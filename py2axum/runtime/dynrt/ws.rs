//! WebSocket routes (`@app.websocket`, `@router.websocket`), in two layers as in Python:
//! - the server, playing uvicorn's part (0.54, `websockets_sansio_impl`): a queue of ASGI messages
//!   (`websocket.connect`, `websocket.receive`, `websocket.disconnect`), the handshake decided by the
//!   application (101, 403 when it closes first, 500 when it fails or returns before, or its denial
//!   response), an I/O task reading the socket while the queue is empty and writing in order; the
//!   application ending without `websocket.close` drops the connection (the client sees 1006), a close
//!   frame waits for the client's reply (10 s);
//! - Starlette's `WebSocket` (1.7): client/application states, `receive`/`send` (ASGI dicts), `accept`,
//!   `receive_text/bytes/json`, `send_text/bytes/json`, `close`, `iter_*`, `send_denial_response`, and the
//!   HTTPConnection attributes.
//! Not reproduced (docs/supported.md): permessage-deflate, uvicorn's keepalive pings.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderName, HeaderValue};
use axum::response::Response;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot, Notify};

use super::v::*;
use super::{ops, Cx};

// ---------------------------------------------------------------- classes

pub static CLS_WS_STATE: Class = Class { name: "WebSocketState", qualname: "WebSocketState", bases: &[], kind: ClassKind::Enum(&ENUM_WS_STATE) };
pub static ENUM_WS_STATE: EnumDesc = EnumDesc {
    name: "WebSocketState",
    class: &CLS_WS_STATE,
    kind: EnumKind::Plain,
    members: &[("CONNECTING", EV::Int(0)), ("CONNECTED", EV::Int(1)), ("DISCONNECTED", EV::Int(2)), ("RESPONSE", EV::Int(3))],
    methods: &[],
    missing: None,
};

const CONNECTING: u8 = 0;
const CONNECTED: u8 = 1;
const DISCONNECTED: u8 = 2;
const RESPONSE: u8 = 3;

fn exc_with(class: &'static Class, args: Vec<V>, attrs: Vec<(&str, V)>) -> Exc {
    let e = Exc::new(class, args);
    {
        let mut a = e.0.attrs.lock();
        for (k, v) in attrs {
            a.insert(k.to_string(), v);
        }
    }
    e
}

fn kwarg<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v))
}

fn check_kwargs(fname: &str, kwargs: &[(String, V)], allowed: &[&str]) -> R<()> {
    match kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(Exc::type_error(format!("{fname}() got an unexpected keyword argument '{k}'"))),
        None => Ok(()),
    }
}

/// `WebSocketDisconnect(code=1000, reason=None)`: `args` are the positional arguments (BaseException.__new__)
pub fn disconnect_exc(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    check_kwargs("WebSocketDisconnect.__init__", &kwargs, &["code", "reason"])?;
    let code = kwarg(&args, &kwargs, 0, "code").cloned().unwrap_or(V::Int(1000));
    let reason = kwarg(&args, &kwargs, 1, "reason").cloned().unwrap_or(V::None);
    let reason = if ops::truthy(&reason)? { reason } else { V::str("") };
    Ok(V::Exc(exc_with(&WS_DISCONNECT, args, vec![("code", code), ("reason", reason)])))
}

/// `WebSocketException(code, reason=None)` (Starlette's and FastAPI's)
pub fn ws_exception(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    check_kwargs("WebSocketException.__init__", &kwargs, &["code", "reason"])?;
    let Some(code) = kwarg(&args, &kwargs, 0, "code").cloned() else {
        return Err(Exc::type_error("WebSocketException.__init__() missing 1 required positional argument: 'code'"));
    };
    let reason = kwarg(&args, &kwargs, 1, "reason").cloned().unwrap_or(V::None);
    let reason = if ops::truthy(&reason)? { reason } else { V::str("") };
    Ok(V::Exc(exc_with(&WS_EXCEPTION, args, vec![("code", code), ("reason", reason)])))
}

fn disconnect(code: i64, reason: Option<String>) -> Exc {
    let r = reason.clone().map(V::str).unwrap_or(V::None);
    exc_with(&WS_DISCONNECT, vec![V::Int(code), r], vec![("code", V::Int(code)), ("reason", V::str(reason.unwrap_or_default()))])
}

fn runtime_error(msg: impl AsRef<str>) -> Exc {
    Exc::msg(&RUNTIME_ERROR, msg)
}

/// FastAPI's check of the solved parameters on a WebSocket route
pub fn check(errs: Vec<super::pyd::ErrDetail>) -> R<()> {
    if errs.is_empty() { Ok(()) } else { Err(Exc::validation(&WS_VALIDATION_ERROR, errs)) }
}

// ---------------------------------------------------------------- the server (uvicorn)

/// a message of the receive queue
#[derive(Clone)]
enum In {
    Connect,
    Text(String),
    Bytes(Bytes),
    /// `reason` is absent when the connection was lost (uvicorn's `connection_lost`)
    Disconnect(i64, Option<String>),
}

impl In {
    fn to_v(&self) -> R {
        let t = |s: &str| (V::str("type"), V::str(s));
        V::dict_from(match self {
            In::Connect => vec![t("websocket.connect")],
            In::Text(s) => vec![t("websocket.receive"), (V::str("text"), V::str(s))],
            In::Bytes(b) => vec![t("websocket.receive"), (V::str("bytes"), V::Bytes(Arc::from(&b[..])))],
            In::Disconnect(c, None) => vec![t("websocket.disconnect"), (V::str("code"), V::Int(*c))],
            In::Disconnect(c, Some(r)) => vec![t("websocket.disconnect"), (V::str("code"), V::Int(*c)), (V::str("reason"), V::str(r))],
        })
    }
}

enum Out {
    Msg(Message),
    Close(u16, String),
    /// the application has returned
    End,
}

enum Handshake {
    Accept { subprotocol: Option<String>, headers: Vec<(Vec<u8>, Vec<u8>)>, out: mpsc::UnboundedReceiver<Out> },
    Reject(Response),
}

/// One WebSocket connection.
pub struct Session {
    pub req: Arc<super::web::ReqCell>,
    subprotocols: Vec<String>,
    // Starlette's WebSocket
    client_state: AtomicU8,
    application_state: AtomicU8,
    // uvicorn's protocol
    queue: Mutex<VecDeque<In>>,
    arrived: Notify,
    drained: Notify,
    hs: Mutex<Option<oneshot::Sender<Handshake>>>,
    handshake_complete: AtomicBool,
    close_sent: AtomicBool,
    /// the peer has closed (its close frame arrived): sends fail (websockets' InvalidState)
    peer_closed: AtomicBool,
    disconnected: AtomicBool,
    initial_response: Mutex<Option<(u16, Vec<(Vec<u8>, Vec<u8>)>, Vec<u8>)>>,
    out: Mutex<Option<mpsc::UnboundedSender<Out>>>,
}

impl Session {
    fn push(&self, m: In) {
        self.queue.lock().push_back(m);
        self.arrived.notify_one();
    }

    /// uvicorn's `receive()`
    async fn srv_receive(&self) -> In {
        loop {
            let got = {
                let mut q = self.queue.lock();
                let m = q.pop_front();
                if m.is_some() && q.is_empty() {
                    self.drained.notify_one();
                }
                m
            };
            if let Some(m) = got {
                return m;
            }
            self.arrived.notified().await;
        }
    }

    fn decide(&self, h: Handshake) -> bool {
        match self.hs.lock().take() {
            Some(tx) => tx.send(h).is_ok(),
            None => false,
        }
    }

    fn client_disconnected() -> Exc {
        Exc::new(&CLIENT_DISCONNECTED, vec![])
    }

    /// uvicorn's `send(message)`
    async fn srv_send(&self, msg: &V) -> R<()> {
        if self.disconnected.load(Ordering::SeqCst) {
            return Err(Self::client_disconnected());
        }
        let ty = msg_type(msg)?;
        let initial = self.initial_response.lock().is_some();
        if !self.handshake_complete.load(Ordering::SeqCst) && !initial {
            match ty.as_str() {
                "websocket.accept" => {
                    let subprotocol = match get(msg, "subprotocol")? {
                        Some(V::None) | None => None,
                        Some(v) => Some(ops::str_(&v)?),
                    };
                    let mut headers = vec![];
                    if let Some(h) = get(msg, "headers")? {
                        if !h.is_none() {
                            for pair in ops::iter(&h)? {
                                let kv = ops::iter(&pair)?;
                                let [k, v] = &kv[..] else { return Err(Exc::value_error("too many values to unpack (expected 2)")) };
                                headers.push((bytes_of(k)?, bytes_of(v)?));
                            }
                        }
                    }
                    let (tx, rx) = mpsc::unbounded_channel();
                    *self.out.lock() = Some(tx);
                    self.handshake_complete.store(true, Ordering::SeqCst);
                    if !self.decide(Handshake::Accept { subprotocol, headers, out: rx }) {
                        self.disconnected.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                }
                "websocket.close" => {
                    self.push(In::Disconnect(1006, None));
                    self.close_sent.store(true, Ordering::SeqCst);
                    self.handshake_complete.store(true, Ordering::SeqCst);
                    self.decide(Handshake::Reject(plain(403, "")));
                    Ok(())
                }
                "websocket.http.response.start" => {
                    let status = match get(msg, "status")? {
                        Some(V::Int(i)) => i,
                        Some(o) => return Err(Exc::type_error(format!("py2axum: the status must be an int, not {}", o.type_name()))),
                        None => return Err(Exc::new(&KEY_ERROR, vec![V::str("status")])),
                    };
                    if !(100..600).contains(&status) {
                        return Err(runtime_error(format!("Invalid HTTP status code '{status}' in response.")));
                    }
                    let mut headers = vec![];
                    if let Some(h) = get(msg, "headers")? {
                        for pair in ops::iter(&h)? {
                            let kv = ops::iter(&pair)?;
                            let [k, v] = &kv[..] else { return Err(Exc::value_error("too many values to unpack (expected 2)")) };
                            headers.push((bytes_of(k)?, bytes_of(v)?));
                        }
                    }
                    *self.initial_response.lock() = Some((status as u16, headers, vec![]));
                    Ok(())
                }
                t => Err(runtime_error(format!(
                    "Expected ASGI message 'websocket.accept', 'websocket.close' or 'websocket.http.response.start' but got '{t}'."
                ))),
            }
        } else if !self.close_sent.load(Ordering::SeqCst) && !initial {
            if self.peer_closed.load(Ordering::SeqCst) {
                return Err(Self::client_disconnected());
            }
            let out = match ty.as_str() {
                "websocket.send" => {
                    let b = get(msg, "bytes")?.filter(|v| !v.is_none());
                    let t = get(msg, "text")?.filter(|v| !v.is_none());
                    if let Some(b) = b {
                        Some(Out::Msg(Message::Binary(Bytes::from(bytes_of(&b)?))))
                    } else if let Some(t) = t {
                        Some(Out::Msg(Message::Text(ops::str_(&t)?.into())))
                    } else {
                        None
                    }
                }
                "websocket.close" => {
                    let code = match get(msg, "code")? {
                        Some(V::Int(i)) => i,
                        None => 1000,
                        Some(o) => return Err(Exc::type_error(format!("py2axum: the close code must be an int, not {}", o.type_name()))),
                    };
                    let reason = match get(msg, "reason")? {
                        Some(v) if ops::truthy(&v)? => ops::str_(&v)?,
                        _ => String::new(),
                    };
                    self.push(In::Disconnect(code, Some(reason.clone())));
                    self.close_sent.store(true, Ordering::SeqCst);
                    Some(Out::Close(code as u16, reason))
                }
                t => return Err(runtime_error(format!("Expected ASGI message 'websocket.send' or 'websocket.close', but got '{t}'."))),
            };
            if let Some(o) = out {
                let tx = self.out.lock().clone();
                if tx.map(|t| t.send(o).is_err()).unwrap_or(true) {
                    return Err(Self::client_disconnected());
                }
            }
            Ok(())
        } else if initial {
            if ty != "websocket.http.response.body" {
                return Err(runtime_error(format!("Expected ASGI message 'websocket.http.response.body' but got '{ty}'.")));
            }
            let body = match get(msg, "body")? {
                Some(b) => bytes_of(&b)?,
                None => vec![],
            };
            let more = match get(msg, "more_body")? {
                Some(v) => ops::truthy(&v)?,
                None => false,
            };
            let done = {
                let mut ir = self.initial_response.lock();
                let r = ir.as_mut().unwrap();
                r.2.extend_from_slice(&body);
                if more { None } else { Some(r.clone()) }
            };
            if let Some((status, headers, body)) = done {
                self.push(In::Disconnect(1006, None));
                self.close_sent.store(true, Ordering::SeqCst);
                self.handshake_complete.store(true, Ordering::SeqCst);
                self.decide(Handshake::Reject(denial(status, &headers, body)));
            }
            Ok(())
        } else {
            Err(runtime_error(format!("Unexpected ASGI message '{ty}', after sending 'websocket.close'.")))
        }
    }

    /// the application task has ended (its exception already through the exception handlers)
    fn finished(&self, r: R<()>) {
        if let Err(e) = &r {
            if !e.isinstance(&CLIENT_DISCONNECTED) {
                eprintln!("ERROR:py2axum:Exception in ASGI application: {:?}", e);
                if self.initial_response.lock().is_none() && !self.handshake_complete.load(Ordering::SeqCst) {
                    self.decide(Handshake::Reject(plain(500, "Internal Server Error")));
                }
            }
        } else if !self.handshake_complete.load(Ordering::SeqCst) {
            eprintln!("ERROR:py2axum:ASGI callable returned without completing handshake.");
            if self.initial_response.lock().is_none() {
                self.decide(Handshake::Reject(plain(500, "Internal Server Error")));
            }
        }
        // a handshake never decided (the denial response unfinished): the connection is dropped
        self.hs.lock().take();
        if let Some(tx) = self.out.lock().take() {
            let _ = tx.send(Out::End);
        }
    }
}

fn msg_type(m: &V) -> R<String> {
    match get(m, "type")? {
        Some(V::Str(s)) => Ok(s.to_string()),
        Some(o) => Err(Exc::type_error(format!("py2axum: an ASGI message type must be a str, not {}", o.type_name()))),
        None => Err(Exc::new(&KEY_ERROR, vec![V::str("type")])),
    }
}

fn get(m: &V, k: &str) -> R<Option<V>> {
    match m {
        V::Dict(dm) => Ok(dm.lock().get(&Key::Str(Arc::from(k))).map(|(_, v)| v.clone())),
        o => Err(Exc::type_error(format!("py2axum: an ASGI message must be a dict, not {}", o.type_name()))),
    }
}

fn bytes_of(v: &V) -> R<Vec<u8>> {
    match v {
        V::Bytes(b) => Ok(b.to_vec()),
        o => Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
    }
}

/// websockets' `ServerProtocol.reject(status, text)`
fn plain(status: u16, text: &str) -> Response {
    Response::builder()
        .status(status)
        .header("connection", "close")
        .header("content-length", text.len().to_string())
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(text.to_string()))
        .unwrap()
}

/// the application's denial response, with uvicorn's defaults
fn denial(status: u16, headers: &[(Vec<u8>, Vec<u8>)], body: Vec<u8>) -> Response {
    let mut b = Response::builder().status(status);
    let has = |n: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(n.as_bytes()));
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k), HeaderValue::from_bytes(v)) {
            b = b.header(k, v);
        }
    }
    if !has("connection") {
        b = b.header("connection", "close");
    }
    if !has("content-length") {
        b = b.header("content-length", body.len().to_string());
    }
    if !has("content-type") {
        b = b.header("content-type", "text/plain; charset=utf-8");
    }
    b.body(Body::from(body)).unwrap_or_else(|_| plain(500, "Internal Server Error"))
}

/// the I/O of an accepted connection: reads while the queue is empty, writes in order
async fn io(socket: WebSocket, s: Arc<Session>, mut out: mpsc::UnboundedReceiver<Out>) {
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut app_done = false;
    loop {
        let can_read = s.queue.lock().is_empty() || s.close_sent.load(Ordering::SeqCst);
        tokio::select! {
            biased;
            cmd = out.recv(), if !app_done || deadline.is_none() => match cmd {
                Some(Out::Msg(m)) => {
                    let _ = sink.send(m).await;
                }
                Some(Out::Close(code, reason)) => {
                    let _ = sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
                    deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                }
                Some(Out::End) | None => {
                    app_done = true;
                    if deadline.is_none() {
                        break; // no close frame: the connection is dropped
                    }
                }
            },
            m = stream.next(), if can_read => match m {
                Some(Ok(Message::Text(t))) => {
                    if !s.close_sent.load(Ordering::SeqCst) {
                        s.push(In::Text(t.to_string()));
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if !s.close_sent.load(Ordering::SeqCst) {
                        s.push(In::Bytes(b));
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(f))) => {
                    if deadline.is_some() {
                        break; // the reply to our close frame
                    }
                    let (code, reason) = f.map(|f| (f.code as i64, f.reason.to_string())).unwrap_or((1005, String::new()));
                    s.peer_closed.store(true, Ordering::SeqCst);
                    s.push(In::Disconnect(code, Some(reason)));
                    let _ = sink.flush().await; // tungstenite's reply
                    break;
                }
                Some(Err(_)) | None => {
                    if deadline.is_none() {
                        s.push(In::Disconnect(1005, None));
                    }
                    break;
                }
            },
            _ = s.drained.notified(), if !can_read => {}
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => break,
        }
    }
    s.disconnected.store(true, Ordering::SeqCst);
    drop(sink);
    drop(stream);
}

/// Pushes `websocket.disconnect` 1006 when the client leaves before the handshake is decided (the axum
/// future is dropped), as uvicorn's `connection_lost` does.
struct LostGuard(Option<Arc<Session>>);

impl Drop for LostGuard {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            if !s.handshake_complete.load(Ordering::SeqCst) || s.out.lock().is_none() {
                s.disconnected.store(true, Ordering::SeqCst);
                s.push(In::Disconnect(1006, None));
            }
        }
    }
}

/// the request is a WebSocket upgrade (what uvicorn's HTTP protocol checks before switching)
pub fn is_upgrade(req: &axum::extract::Request) -> bool {
    req.headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

pub type WsRunFn = for<'a> fn(&'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<()>> + Send + 'a>>;

/// One `@app.websocket` route: Starlette path regex, compiled endpoint.
pub struct WsRouteDef {
    pub pattern: &'static str,
    pub run: WsRunFn,
    pub node: Option<&'static super::routing::Node>,
}

/// `max_size`: uvicorn's `ws_max_size` default
const MAX_SIZE: usize = 16 * 1024 * 1024;

/// Serve one upgrade request. `handle` runs the exception handlers (ExceptionMiddleware) on the route's
/// exception.
pub async fn serve(app: Arc<super::AppState>, req: axum::extract::Request, routes: &'static [WsRouteDef], handle: HandleFn) -> Response {
    use axum::extract::FromRequestParts;
    let client = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| (c.0.ip().to_canonical().to_string(), c.0.port()));
    let (mut parts, _body) = req.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(rej) => return axum::response::IntoResponse::into_response(rej),
    };
    let mut cell = super::web::ReqCell::from_parts("GET", &parts.uri, &parts.headers, vec![], Bytes::new());
    cell.client = client;
    let subprotocols: Vec<String> = parts
        .headers
        .get_all("sec-websocket-protocol")
        .iter()
        .flat_map(|v| String::from_utf8_lossy(v.as_bytes()).split(',').map(|t| t.trim().to_string()).collect::<Vec<_>>())
        .collect();
    let cx: Cx = Arc::new(super::CxInner::new(app, cell));
    // Starlette's router: the first WebSocket route whose path matches, else `WebSocketClose()` (403)
    static RES: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let res = RES.get_or_init(|| routes.iter().map(|r| super::web::route_regex(r.pattern)).collect());
    let path = super::web::unquote(&cx.req.path);
    let Some((route, re)) = routes.iter().zip(res).find(|(_, re)| re.is_match(&path)) else {
        return plain(403, "");
    };
    if let Some(caps) = re.captures(&path) {
        *cx.req.path_params.lock() = re.capture_names().flatten().filter_map(|n| caps.name(n).map(|m| (n.to_string(), m.as_str().to_string()))).collect();
    }
    *cx.req.route.lock() = route.node;
    let (tx, rx) = oneshot::channel();
    let s = Arc::new(Session {
        req: cx.req.clone(),
        subprotocols,
        client_state: AtomicU8::new(CONNECTING),
        application_state: AtomicU8::new(CONNECTING),
        queue: Mutex::new(VecDeque::from([In::Connect])),
        arrived: Notify::new(),
        drained: Notify::new(),
        hs: Mutex::new(Some(tx)),
        handshake_complete: AtomicBool::new(false),
        close_sent: AtomicBool::new(false),
        peer_closed: AtomicBool::new(false),
        disconnected: AtomicBool::new(false),
        initial_response: Mutex::new(None),
        out: Mutex::new(None),
    });
    let _ = cx.ws.set(V::native(Native::WebSocket(s.clone())));
    let run = route.run;
    let task_s = s.clone();
    tokio::spawn(async move {
        let r = match super::web::run_ws_route(&cx, run).await {
            Ok(()) => Ok(()),
            Err(e) => handle(&cx, e).await,
        };
        task_s.finished(r);
    });
    let mut guard = LostGuard(Some(s.clone()));
    let decision = rx.await;
    guard.0 = None;
    match decision {
        Ok(Handshake::Accept { subprotocol, headers, out }) => {
            let s2 = s.clone();
            let s3 = s.clone();
            let mut resp = upgrade
                .max_message_size(MAX_SIZE)
                .max_frame_size(MAX_SIZE)
                .on_failed_upgrade(move |_| {
                    s3.disconnected.store(true, Ordering::SeqCst);
                    s3.push(In::Disconnect(1006, None));
                })
                .on_upgrade(move |socket| io(socket, s2, out));
            let h = resp.headers_mut();
            for (k, v) in headers {
                if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(&k.to_ascii_lowercase()), HeaderValue::from_bytes(&v)) {
                    h.append(k, v);
                }
            }
            if let Some(p) = subprotocol {
                if let Ok(v) = HeaderValue::from_str(&p) {
                    h.insert("sec-websocket-protocol", v);
                }
            }
            resp
        }
        Ok(Handshake::Reject(r)) => r,
        // the application ended without deciding (an unfinished denial response): no response
        Err(_) => plain(500, "Internal Server Error"),
    }
}

pub type HandleFn = for<'a> fn(&'a Cx, Exc) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<()>> + Send + 'a>>;

// ---------------------------------------------------------------- Starlette's WebSocket

/// the WebSocket of the connection being served (a `websocket: WebSocket` parameter)
pub fn current(cx: &Cx) -> R {
    cx.ws.get().cloned().ok_or_else(|| Exc::type_error("py2axum: a WebSocket parameter outside a WebSocket route"))
}

impl Session {
    fn cs(&self) -> u8 {
        self.client_state.load(Ordering::SeqCst)
    }
    fn aps(&self) -> u8 {
        self.application_state.load(Ordering::SeqCst)
    }

    async fn receive_in(&self) -> R<In> {
        match self.cs() {
            CONNECTING => {
                let m = self.srv_receive().await;
                if !matches!(m, In::Connect) {
                    let t = msg_type(&m.to_v()?)?;
                    return Err(runtime_error(format!("Expected ASGI message \"websocket.connect\", but got '{t}'")));
                }
                self.client_state.store(CONNECTED, Ordering::SeqCst);
                Ok(m)
            }
            CONNECTED => {
                let m = self.srv_receive().await;
                match m {
                    In::Text(_) | In::Bytes(_) => {}
                    In::Disconnect(..) => self.client_state.store(DISCONNECTED, Ordering::SeqCst),
                    In::Connect => {
                        return Err(runtime_error(
                            "Expected ASGI message \"websocket.receive\" or \"websocket.disconnect\", but got 'websocket.connect'",
                        ))
                    }
                }
                Ok(m)
            }
            _ => Err(Exc::msg(&WS_DISCONNECTED, "Cannot call \"receive\" once a disconnect message has been received.")),
        }
    }

    async fn send_msg(&self, msg: &V) -> R<()> {
        let ty = || msg_type(msg);
        match self.aps() {
            CONNECTING => {
                let t = ty()?;
                match t.as_str() {
                    "websocket.close" => self.application_state.store(DISCONNECTED, Ordering::SeqCst),
                    "websocket.http.response.start" => self.application_state.store(RESPONSE, Ordering::SeqCst),
                    "websocket.accept" => self.application_state.store(CONNECTED, Ordering::SeqCst),
                    _ => {
                        return Err(runtime_error(format!(
                            "Expected ASGI message \"websocket.accept\", \"websocket.close\" or \"websocket.http.response.start\", but got '{t}'"
                        )))
                    }
                }
                self.srv_send(msg).await
            }
            CONNECTED => {
                let t = ty()?;
                if t != "websocket.send" && t != "websocket.close" {
                    return Err(runtime_error(format!("Expected ASGI message \"websocket.send\" or \"websocket.close\", but got '{t}'")));
                }
                if t == "websocket.close" {
                    self.application_state.store(DISCONNECTED, Ordering::SeqCst);
                }
                match self.srv_send(msg).await {
                    Err(e) if e.isinstance(&OS_ERROR) => {
                        self.application_state.store(DISCONNECTED, Ordering::SeqCst);
                        Err(exc_with(&WS_DISCONNECT, vec![], vec![("code", V::Int(1006)), ("reason", V::str(""))]))
                    }
                    r => r,
                }
            }
            RESPONSE => {
                let t = ty()?;
                if t != "websocket.http.response.body" {
                    return Err(runtime_error(format!("Expected ASGI message \"websocket.http.response.body\", but got '{t}'")));
                }
                if !matches!(get(msg, "more_body")?, Some(v) if ops::truthy(&v)?) {
                    self.application_state.store(DISCONNECTED, Ordering::SeqCst);
                }
                self.srv_send(msg).await
            }
            _ => Err(Exc::msg(&WS_DISCONNECTED, "Cannot call \"send\" once a close message has been sent.")),
        }
    }

    fn need_connected(&self) -> R<()> {
        if self.aps() != CONNECTED {
            return Err(Exc::msg(&WS_DISCONNECTED, "WebSocket is not connected. Need to call \"accept\" first."));
        }
        Ok(())
    }

    /// receive_text / receive_bytes / receive_json: the message's payload, `WebSocketDisconnect` on a
    /// disconnect message
    async fn receive_data(&self) -> R<In> {
        self.need_connected()?;
        let m = self.receive_in().await?;
        if let In::Disconnect(code, reason) = &m {
            return Err(disconnect(*code, reason.clone()));
        }
        Ok(m)
    }

    async fn receive_text(&self) -> R {
        match self.receive_data().await? {
            In::Text(t) => Ok(V::str(t)),
            _ => Err(Exc::new(&KEY_ERROR, vec![V::str("text")])),
        }
    }

    async fn receive_bytes(&self) -> R {
        match self.receive_data().await? {
            In::Bytes(b) => Ok(V::Bytes(Arc::from(&b[..]))),
            _ => Err(Exc::new(&KEY_ERROR, vec![V::str("bytes")])),
        }
    }

    async fn receive_json(&self, mode: &str) -> R {
        if mode != "text" && mode != "binary" {
            return Err(runtime_error("The \"mode\" argument should be \"text\" or \"binary\"."));
        }
        let m = self.receive_data().await?;
        let text = match (mode, m) {
            ("text", In::Text(t)) => t,
            ("text", _) => return Err(Exc::new(&KEY_ERROR, vec![V::str("text")])),
            (_, In::Bytes(b)) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(_) => return Err(Exc::msg(&VALUE_ERROR, "'utf-8' codec can't decode bytes")),
            },
            _ => return Err(Exc::new(&KEY_ERROR, vec![V::str("bytes")])),
        };
        super::pyd::loads(&text)
    }

    async fn send_dict(&self, items: Vec<(&str, V)>) -> R<()> {
        let d = V::dict_from(items.into_iter().map(|(k, v)| (V::str(k), v)).collect())?;
        self.send_msg(&d).await
    }

    async fn accept(&self, subprotocol: V, headers: V) -> R<()> {
        let headers = if ops::truthy(&headers)? { headers } else { V::list(vec![]) };
        if self.cs() == CONNECTING {
            self.receive_in().await?;
        }
        self.send_dict(vec![("type", V::str("websocket.accept")), ("subprotocol", subprotocol), ("headers", headers)]).await
    }

    async fn close(&self, code: V, reason: V) -> R<()> {
        let reason = if ops::truthy(&reason)? { reason } else { V::str("") };
        self.send_dict(vec![("type", V::str("websocket.close")), ("code", code), ("reason", reason)]).await
    }
}

fn json_text(data: &V) -> R<String> {
    let kw = vec![
        ("separators".to_string(), V::tuple(vec![V::str(","), V::str(":")])),
        ("ensure_ascii".to_string(), V::Bool(false)),
    ];
    ops::str_(&super::libs::json_dumps(data, &kw)?)
}

pub fn attr(cx: &Cx, s: &Arc<Session>, name: &str) -> R {
    match name {
        "client_state" => Ok(V::Enum(&ENUM_WS_STATE, s.cs() as u16)),
        "application_state" => Ok(V::Enum(&ENUM_WS_STATE, s.aps() as u16)),
        "headers" => Ok(V::native(Native::Headers(s.req.clone()))),
        "state" => Ok(V::native(Native::State(s.req.clone()))),
        "url" => Ok(V::native(Native::Url(s.req.clone()))),
        "scope" => scope(cx, s),
        "query_params" => V::dict_from(s.req.query.iter().map(|(k, x)| (V::str(k), V::str(x))).collect()),
        "cookies" => super::methods::cookies(&s.req),
        "client" => Ok(s.req.client.as_ref().map(|(h, p)| V::native(Native::Address(h.clone(), *p))).unwrap_or(V::None)),
        "path_params" => V::dict_from(s.req.path_params.lock().iter().map(|(k, x)| (V::str(k), V::str(x))).collect()),
        "app" => Ok(super::routing::app()),
        _ => Err(Exc::attr_error(format!("'WebSocket' object has no attribute '{name}'"))),
    }
}

/// `websocket.scope` (the keys the binary has)
fn scope(cx: &Cx, s: &Arc<Session>) -> R {
    let r = &s.req;
    let headers: Vec<V> = r.headers.iter().map(|(k, v)| V::tuple(vec![V::Bytes(Arc::from(k.as_bytes())), V::Bytes(Arc::from(v.as_bytes()))])).collect();
    let pp: Vec<(V, V)> = r.path_params.lock().iter().map(|(k, v)| (V::str(k), V::str(v))).collect();
    let _ = cx;
    V::dict_from(vec![
        (V::str("type"), V::str("websocket")),
        (V::str("http_version"), V::str("1.1")),
        (V::str("scheme"), V::str("ws")),
        (V::str("client"), r.client.as_ref().map(|(h, p)| V::tuple(vec![V::str(h), V::Int(*p as i64)])).unwrap_or(V::None)),
        (V::str("root_path"), V::str("")),
        (V::str("path"), V::str(super::web::unquote(&r.path))),
        (V::str("raw_path"), V::Bytes(Arc::from(r.path.as_bytes()))),
        (V::str("query_string"), V::Bytes(Arc::from(r.raw_query.as_bytes()))),
        (V::str("headers"), V::list(headers)),
        (V::str("subprotocols"), V::list(s.subprotocols.iter().map(V::str).collect())),
        (V::str("app"), super::routing::app()),
        (V::str("endpoint"), V::None),
        (V::str("path_params"), V::dict_from(pp)?),
    ])
}

pub async fn method(cx: &Cx, s: &Arc<Session>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = |i: usize, k: &str| kwarg(&args, &kwargs, i, k).cloned();
    let f = |allowed: &[&str]| check_kwargs(&format!("WebSocket.{name}"), &kwargs, allowed);
    match name {
        "accept" => {
            f(&["subprotocol", "headers"])?;
            s.accept(a(0, "subprotocol").unwrap_or(V::None), a(1, "headers").unwrap_or(V::None)).await.map(|_| V::None)
        }
        "receive" => s.receive_in().await?.to_v(),
        "send" => {
            f(&["message"])?;
            let m = a(0, "message").ok_or_else(|| Exc::type_error("WebSocket.send() missing 1 required positional argument: 'message'"))?;
            s.send_msg(&m).await.map(|_| V::None)
        }
        "receive_text" => s.receive_text().await,
        "receive_bytes" => s.receive_bytes().await,
        "receive_json" => {
            f(&["mode"])?;
            let mode = match a(0, "mode") {
                Some(v) => ops::str_(&v)?,
                None => "text".into(),
            };
            s.receive_json(&mode).await
        }
        "send_text" | "send_bytes" => {
            f(&["data"])?;
            let d = a(0, "data").ok_or_else(|| Exc::type_error(format!("WebSocket.{name}() missing 1 required positional argument: 'data'")))?;
            let key = if name == "send_text" { "text" } else { "bytes" };
            s.send_dict(vec![("type", V::str("websocket.send")), (key, d)]).await.map(|_| V::None)
        }
        "send_json" => {
            f(&["data", "mode"])?;
            let d = a(0, "data").ok_or_else(|| Exc::type_error("WebSocket.send_json() missing 1 required positional argument: 'data'"))?;
            let mode = match a(1, "mode") {
                Some(v) => ops::str_(&v)?,
                None => "text".into(),
            };
            if mode != "text" && mode != "binary" {
                return Err(runtime_error("The \"mode\" argument should be \"text\" or \"binary\"."));
            }
            let text = json_text(&d)?;
            let item = if mode == "text" { ("text", V::str(text)) } else { ("bytes", V::Bytes(Arc::from(text.as_bytes()))) };
            s.send_dict(vec![("type", V::str("websocket.send")), item]).await.map(|_| V::None)
        }
        "close" => {
            f(&["code", "reason"])?;
            s.close(a(0, "code").unwrap_or(V::Int(1000)), a(1, "reason").unwrap_or(V::None)).await.map(|_| V::None)
        }
        "iter_text" | "iter_bytes" | "iter_json" => Ok(V::native(Native::WsIter(s.clone(), name[5..].to_string()))),
        "send_denial_response" => {
            f(&["response"])?;
            let r = a(0, "response").ok_or_else(|| Exc::type_error("WebSocket.send_denial_response() missing 1 required positional argument: 'response'"))?;
            send_response(cx, s, &r).await.map(|_| V::None)
        }
        _ => Err(Exc::attr_error(format!("'WebSocket' object has no attribute '{name}'"))),
    }
}

/// a Starlette Response called as an ASGI app on the websocket scope (`websocket.http.response.*`)
async fn send_response(cx: &Cx, s: &Arc<Session>, r: &V) -> R<()> {
    let _ = cx;
    let resp = match r {
        V::Native(n) => match &**n {
            Native::RespObj(o) => super::resp::into_response(o)?,
            _ => return Err(Exc::type_error(format!("'{}' object is not callable", r.type_name()))),
        },
        _ => return Err(Exc::type_error(format!("'{}' object is not callable", r.type_name()))),
    };
    let (parts, body) = resp.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| runtime_error(e.to_string()))?;
    let headers: Vec<V> = parts
        .headers
        .iter()
        .map(|(k, v)| V::tuple(vec![V::Bytes(Arc::from(k.as_str().as_bytes())), V::Bytes(Arc::from(v.as_bytes()))]))
        .collect();
    s.send_dict(vec![("type", V::str("websocket.http.response.start")), ("status", V::Int(parts.status.as_u16() as i64)), ("headers", V::list(headers))])
        .await?;
    s.send_dict(vec![("type", V::str("websocket.http.response.body")), ("body", V::Bytes(Arc::from(&body[..])))]).await
}

/// ExceptionMiddleware's response to an exception on a WebSocket route: sent on the connection like
/// any ASGI response (a denial response before `accept`, RuntimeError after)
pub async fn handler_response(cx: &Cx, r: &V) -> R<()> {
    let s = match current(cx)? {
        V::Native(n) => match &*n {
            Native::WebSocket(s) => s.clone(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    if r.is_none() {
        return Ok(());
    }
    // the raw `send`, not WebSocket.send: uvicorn's own checks apply
    let resp = match r {
        V::Native(n) => match &**n {
            Native::RespObj(o) => super::resp::into_response(o)?,
            _ => return Err(Exc::type_error(format!("'{}' object is not callable", r.type_name()))),
        },
        _ => return Err(Exc::type_error(format!("'{}' object is not callable", r.type_name()))),
    };
    send_raw_response(&s, resp).await
}

/// FastAPI's `http_exception_handler` response (`JSONResponse` / `Response`), sent raw
pub async fn send_raw_response(s: &Arc<Session>, resp: Response) -> R<()> {
    let (parts, body) = resp.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| runtime_error(e.to_string()))?;
    let headers: Vec<V> = parts
        .headers
        .iter()
        .map(|(k, v)| V::tuple(vec![V::Bytes(Arc::from(k.as_str().as_bytes())), V::Bytes(Arc::from(v.as_bytes()))]))
        .collect();
    let start = V::dict_from(vec![(V::str("type"), V::str("websocket.http.response.start")), (V::str("status"), V::Int(parts.status.as_u16() as i64)), (V::str("headers"), V::list(headers))])?;
    s.srv_send(&start).await?;
    let b = V::dict_from(vec![(V::str("type"), V::str("websocket.http.response.body")), (V::str("body"), V::Bytes(Arc::from(&body[..])))])?;
    s.srv_send(&b).await
}

/// ExceptionMiddleware's `websocket_exception`: `await websocket.close(code=exc.code, reason=exc.reason)`
pub async fn close_with(cx: &Cx, code: V, reason: V) -> R<()> {
    match current(cx)? {
        V::Native(n) => match &*n {
            Native::WebSocket(s) => s.close(code, reason).await,
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

/// an `HTTPConnection` parameter: the WebSocket on a WebSocket route, else the Request
pub fn connection(cx: &Cx) -> V {
    cx.ws.get().cloned().unwrap_or_else(|| super::request(cx))
}

/// the session of the connection being served
pub fn session(cx: &Cx) -> Option<Arc<Session>> {
    match cx.ws.get()? {
        V::Native(n) => match &**n {
            Native::WebSocket(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `async for x in websocket.iter_text()` (and `anext`): the next item, None once disconnected
pub async fn iter_next(s: &Arc<Session>, kind: &str) -> R<Option<V>> {
    let r = match kind {
        "text" => s.receive_text().await,
        "bytes" => s.receive_bytes().await,
        _ => s.receive_json("text").await,
    };
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.isinstance(&WS_DISCONNECT) => Ok(None),
        Err(e) => Err(e),
    }
}
