//! A raw ASGI application as a route (`app.add_route(path, obj)`, `app.router.add_route(...)` with an
//! object whose class defines `async def __call__(self, scope, receive, send)`, which Starlette runs as an
//! ASGI app since it is neither a function nor a method): the request becomes an ASGI `scope` dict, a
//! `receive` callable (the body in one `http.request` message, then nothing until the client leaves) and a
//! `send` callable whose `http.response.start` / `http.response.body` messages make the axum response
//! (streamed when `more_body` is true). Starlette `Response` objects are ASGI apps too.
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::v::*;
use super::Cx;

pub enum Msg {
    Start(u16, Vec<(Vec<u8>, Vec<u8>)>),
    Body(Bytes, bool),
    /// a whole response from the inner stack, passed through unchanged (`await self.app(scope, receive, send)`
    /// with the layer's own `send`)
    Whole(Response),
}

/// The channel between one ASGI app (a raw route or a raw middleware layer) and the server: `receive` reads
/// the request body, `send` posts the messages that make the response.
pub struct Chan {
    tx: Mutex<Option<mpsc::UnboundedSender<Msg>>>,
    body: Mutex<Option<Bytes>>,
    /// the response is complete (uvicorn's `response_complete`): `receive` then returns `http.disconnect`
    done: tokio::sync::watch::Sender<bool>,
    /// `http.response.start` (or a whole response) was sent
    started: std::sync::atomic::AtomicBool,
}

impl Chan {
    fn new(tx: mpsc::UnboundedSender<Msg>, body: Bytes) -> Arc<Chan> {
        Arc::new(Chan {
            tx: Mutex::new(Some(tx)),
            body: Mutex::new(Some(body)),
            done: tokio::sync::watch::channel(false).0,
            started: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn complete(&self) {
        self.done.send_replace(true);
    }
}

/// is `f` run by Starlette as an ASGI app rather than as a `request -> response` endpoint?
pub fn is_asgi_app(f: &V) -> bool {
    matches!(f, V::Obj(_) | V::Inst(_))
}

pub(crate) fn d(items: Vec<(&str, V)>) -> R {
    V::dict_from(items.into_iter().map(|(k, v)| (V::str(k), v)).collect())
}

fn bytes_of(v: &V, what: &str) -> R<Vec<u8>> {
    match v {
        V::Bytes(b) => Ok(b.to_vec()),
        V::Str(s) => Err(Exc::type_error(format!("py2axum: ASGI {what} must be bytes, not str ({s:?})"))),
        o => Err(Exc::type_error(format!("py2axum: ASGI {what} must be bytes, not {}", o.type_name()))),
    }
}

pub(crate) fn get(m: &V, k: &str) -> R<Option<V>> {
    match m {
        V::Dict(dm) => Ok(dm.lock().get(&Key::Str(Arc::from(k))).map(|(_, v)| v.clone())),
        o => Err(Exc::type_error(format!("py2axum: an ASGI message must be a dict, not {}", o.type_name()))),
    }
}

/// `await receive()`: the whole body in one `http.request` message, then (uvicorn) nothing until the
/// response is complete, then `http.disconnect`
pub async fn receive(c: &Arc<Chan>) -> R {
    let body = c.body.lock().take();
    match body {
        Some(b) => d(vec![("type", V::str("http.request")), ("body", V::Bytes(Arc::from(&b[..]))), ("more_body", V::Bool(false))]),
        None => {
            let mut rx = c.done.subscribe();
            let _ = rx.wait_for(|done| *done).await;
            d(vec![("type", V::str("http.disconnect"))])
        }
    }
}

/// `await send(message)`
pub async fn send(c: &Arc<Chan>, msg: &V) -> R {
    let ty = match get(msg, "type")? {
        Some(V::Str(s)) => s.to_string(),
        _ => return Err(Exc::new(&KEY_ERROR, vec![V::str("type")])),
    };
    let m = match ty.as_str() {
        "http.response.start" => {
            let status = match get(msg, "status")? {
                Some(V::Int(i)) => u16::try_from(i).unwrap_or(0),
                _ => return Err(Exc::new(&KEY_ERROR, vec![V::str("status")])),
            };
            let mut headers = Vec::new();
            if let Some(h) = get(msg, "headers")? {
                for pair in super::ops::iter(&h)? {
                    let kv = super::ops::iter(&pair)?;
                    let [k, v] = &kv[..] else {
                        return Err(Exc::value_error("py2axum: an ASGI header must be a (name, value) pair"));
                    };
                    headers.push((bytes_of(k, "header name")?, bytes_of(v, "header value")?));
                }
            }
            if !(100..=599).contains(&status) {
                return Err(super::resp::status_drop(status));
            }
            c.started.store(true, std::sync::atomic::Ordering::Relaxed);
            Msg::Start(status, headers)
        }
        "http.response.body" => {
            let body = match get(msg, "body")? {
                Some(b) => bytes_of(&b, "body")?,
                None => Vec::new(),
            };
            let more = matches!(get(msg, "more_body")?, Some(V::Bool(true)));
            Msg::Body(Bytes::from(body), more)
        }
        other => return Err(Exc::runtime(format!("py2axum: ASGI message type '{other}' is not supported"))),
    };
    let tx = c.tx.lock().clone();
    if let Some(tx) = tx {
        // the client left: CPython's server ignores late messages too
        let _ = tx.send(m);
    }
    Ok(V::None)
}

/// `await send(...)` of a whole response from the inner stack, with a layer's own `send`: passed through
/// unchanged; returns once its body is complete (a streamed one: when the server has taken its last part)
pub async fn forward(c: &Arc<Chan>, resp: Response) -> R<()> {
    c.started.store(true, std::sync::atomic::Ordering::Relaxed);
    let (tx_end, rx_end) = tokio::sync::oneshot::channel::<()>();
    let resp = on_end(resp, move || {
        let _ = tx_end.send(());
    });
    let tx = c.tx.lock().clone();
    if let Some(tx) = tx {
        let _ = tx.send(Msg::Whole(resp));
    }
    let _ = rx_end.await;
    Ok(())
}

/// `hook` runs when the response's body is complete: now for a body of known size, else when the server
/// has taken its last part or dropped it (the client left)
pub fn on_end(resp: Response, hook: impl FnOnce() + Send + 'static) -> Response {
    use axum::body::HttpBody;
    use futures_util::StreamExt;
    let (parts, body) = resp.into_parts();
    if body.size_hint().exact().is_some() {
        hook();
        return Response::from_parts(parts, body);
    }
    struct Hook<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Hook<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    let s = futures_util::stream::unfold((body.into_data_stream(), Hook(Some(hook))), |(mut s, h)| async move {
        s.next().await.map(|item| (item, (s, h)))
    });
    Response::from_parts(parts, Body::from_stream(s.fuse()))
}

/// The ASGI scope of the request being served (the keys uvicorn and Starlette's router set)
fn scope(cx: &Cx, endpoint: &V) -> R {
    let s = super::routing::scope(cx)?;
    if let V::Dict(m) = &s {
        let mut m = m.lock();
        let asgi = d(vec![("version", V::str("3.0")), ("spec_version", V::str("2.4"))])?;
        m.insert(Key::Str(Arc::from("asgi")), (V::str("asgi"), asgi));
        if !m.contains_key(&Key::Str(Arc::from("state"))) {
            m.insert(Key::Str(Arc::from("state")), (V::str("state"), V::dict_from(vec![])?));
        }
        m.insert(Key::Str(Arc::from("endpoint")), (V::str("endpoint"), endpoint.clone()));
        let pp: Vec<(V, V)> = cx.req.path_params.lock().iter().map(|(k, v)| (V::str(k), V::str(v))).collect();
        m.insert(Key::Str(Arc::from("path_params")), (V::str("path_params"), V::dict_from(pp)?));
    }
    *cx.asgi_scope.lock() = Some(s.clone());
    Ok(s)
}

/// The scope a raw ASGI middleware receives: one dict per request, the same object for every layer (and
/// for a raw route under them, which adds the router's keys)
pub fn mw_scope(cx: &Cx) -> R {
    if let Some(s) = cx.asgi_scope.lock().clone() {
        return Ok(s);
    }
    let s = super::routing::scope(cx)?;
    if let V::Dict(m) = &s {
        let mut m = m.lock();
        let asgi = d(vec![("version", V::str("3.0")), ("spec_version", V::str("2.4"))])?;
        m.insert(Key::Str(Arc::from("asgi")), (V::str("asgi"), asgi));
        let st: Vec<(V, V)> = cx.req.state.lock().iter().map(|(k, v)| (V::str(k), v.clone())).collect();
        m.insert(Key::Str(Arc::from("state")), (V::str("state"), V::dict_from(st)?));
    }
    *cx.asgi_scope.lock() = Some(s.clone());
    Ok(s)
}

/// Runs the raw ASGI app `app` of the current request (a route).
pub async fn serve(cx: &Cx, app: &V) -> R<Response> {
    let scope = scope(cx, app)?;
    run(cx, app, scope, cx.req.body.clone()).await
}

/// Runs `app(scope, receive, send)` in its own task and makes the response of its messages. The task is
/// never aborted: after the response, its code goes on (a `finally` after `await self.app(...)` runs).
pub async fn run(cx: &Cx, app: &V, scope: V, body: Bytes) -> R<Response> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let chan = Chan::new(tx, body);
    let args = vec![scope, V::native(Native::AsgiReceive(chan.clone())), V::native(Native::AsgiSend(chan.clone()))];
    let (cx2, app2, chan2) = (cx.clone(), app.clone(), chan.clone());
    let mut task = tokio::spawn(async move {
        let r = match super::methods::call_value(&cx2, &app2, args, vec![]).await {
            Ok(v) => super::aio::await_value(v).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &r {
            if chan2.started.load(std::sync::atomic::Ordering::Relaxed) {
                // the response has started: the server logs the exception (ServerErrorMiddleware re-raises it)
                eprintln!("ERROR:    Exception in ASGI application\n{e:?}");
            }
        }
        r
    });
    let mut finished = false;
    // the start message (or a whole response), or the app ending without one
    let (status, headers) = loop {
        tokio::select! {
            biased;
            m = rx.recv() => match m {
                Some(Msg::Start(s, h)) => break (s, h),
                Some(Msg::Whole(r)) => {
                    let c = chan.clone();
                    return Ok(on_end(r, move || c.complete()));
                }
                Some(Msg::Body(..)) => return Err(Exc::runtime("Expected ASGI message 'http.response.start', but got 'http.response.body'.")),
                None => {}
            },
            r = &mut task, if !finished => {
                finished = true;
                let r = joined(r);
                if rx.is_empty() {
                    r?;
                    // uvicorn: "ASGI callable returned without starting response"
                    chan.complete();
                    return Ok(plain_500());
                }
            }
        }
    };
    let mut builder = Response::builder().status(StatusCode::from_u16(status).map_err(|e| Exc::value_error(e.to_string()))?);
    let mut sized = false;
    for (k, v) in &headers {
        let k = HeaderName::from_bytes(k).map_err(|e| Exc::value_error(e.to_string()))?;
        let v = HeaderValue::from_bytes(v).map_err(|e| Exc::value_error(e.to_string()))?;
        sized |= k == axum::http::header::CONTENT_LENGTH;
        builder = builder.header(k, v);
    }
    // the body: whole when the first part says so, else streamed
    let first = loop {
        tokio::select! {
            biased;
            m = rx.recv() => match m {
                Some(Msg::Body(b, more)) => break Some((b, more)),
                Some(Msg::Start(..)) => return Err(Exc::runtime("Unexpected ASGI message 'http.response.start' sent, after response already started.")),
                Some(Msg::Whole(_)) => return Err(Exc::runtime("py2axum: a response sent after http.response.start")),
                None => break None,
            },
            r = &mut task, if !finished => {
                finished = true;
                let _ = joined(r);
                if rx.is_empty() {
                    break None;
                }
            }
        }
    };
    let Some((first, more)) = first else {
        chan.complete();
        return Ok(builder.body(Body::empty()).unwrap_or_else(super::web::bad_response));
    };
    if !more {
        chan.complete();
        if sized {
            return Ok(builder.body(Body::from(first)).unwrap_or_else(super::web::bad_response));
        }
        // no content-length: uvicorn sends it chunked
        let one = futures_util::stream::iter([Ok::<Bytes, std::io::Error>(first)]);
        return Ok(builder.body(Body::from_stream(one)).unwrap_or_else(super::web::bad_response));
    }
    struct Done(Arc<Chan>);
    impl Drop for Done {
        fn drop(&mut self) {
            self.0.complete();
        }
    }
    let stream = futures_util::stream::unfold((Some(first), rx, Done(chan), false), |(pending, mut rx, done, finished)| async move {
        if let Some(b) = pending {
            return Some((Ok::<Bytes, std::io::Error>(b), (None, rx, done, finished)));
        }
        if finished {
            return None;
        }
        loop {
            match rx.recv().await {
                Some(Msg::Body(b, more)) => return Some((Ok(b), (None, rx, done, !more))),
                Some(Msg::Start(..) | Msg::Whole(_)) => continue,
                None => return None,
            }
        }
    });
    use futures_util::StreamExt;
    Ok(builder.body(Body::from_stream(stream.fuse())).unwrap_or_else(super::web::bad_response))
}

/// the app task's result; a panic (a status uvicorn has no line for: the connection is dropped) goes on
fn joined(r: Result<R, tokio::task::JoinError>) -> R {
    match r {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(Exc::runtime(format!("ASGI task: {e}"))),
    }
}

/// The response of the inner stack handed to a `send` the middleware wrapped: the messages a Starlette
/// response sends (`http.response.start` with its raw headers, then one `http.response.body` for a body of
/// known size, or one per part with `more_body` and an empty last one for a streamed body)
pub async fn deliver(cx: &Cx, resp: Response, send_fn: &V) -> R<()> {
    use axum::body::HttpBody;
    use futures_util::StreamExt;
    if let V::Native(n) = send_fn {
        if let Native::AsgiSend(c) = &**n {
            return forward(c, resp).await; // a layer's own `send`: passed through as is
        }
    }
    let (parts, body) = resp.into_parts();
    let status = parts.status.as_u16();
    let size = body.size_hint().exact();
    let mut hs: Vec<(String, Vec<u8>)> = parts.headers.iter().map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec())).collect();
    if let Some(n) = size {
        if !hs.iter().any(|(k, _)| k == "content-length") && !(status < 200 || status == 204 || status == 304) {
            hs.insert(0, ("content-length".into(), n.to_string().into_bytes()));
        }
    }
    let start = d(vec![("type", V::str("http.response.start")), ("status", V::Int(status as i64)), ("headers", raw_headers(hs))])?;
    super::aio::await_value(super::methods::call_value_boxed(cx, send_fn, vec![start]).await?).await?;
    if size.is_some() {
        let b = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| Exc::runtime(e.to_string()))?;
        let msg = d(vec![("type", V::str("http.response.body")), ("body", V::Bytes(Arc::from(&b[..])))])?;
        super::aio::await_value(super::methods::call_value_boxed(cx, send_fn, vec![msg]).await?).await?;
        return Ok(());
    }
    let mut s = body.into_data_stream();
    while let Some(part) = s.next().await {
        let b = part.map_err(|e| Exc::runtime(e.to_string()))?;
        let msg = d(vec![("type", V::str("http.response.body")), ("body", V::Bytes(Arc::from(&b[..]))), ("more_body", V::Bool(true))])?;
        super::aio::await_value(super::methods::call_value_boxed(cx, send_fn, vec![msg]).await?).await?;
    }
    let msg = d(vec![("type", V::str("http.response.body")), ("body", V::Bytes(Arc::from(&b""[..]))), ("more_body", V::Bool(false))])?;
    super::aio::await_value(super::methods::call_value_boxed(cx, send_fn, vec![msg]).await?).await?;
    Ok(())
}

/// Starlette's `raw_headers` order: content-length, content-type, then the others
fn raw_headers(mut hs: Vec<(String, Vec<u8>)>) -> V {
    hs.sort_by_key(|(k, _)| match k.as_str() {
        "content-length" => 0,
        "content-type" => 1,
        _ => 2,
    });
    V::list(hs.into_iter().map(|(k, v)| V::tuple(vec![V::Bytes(Arc::from(k.as_bytes())), V::Bytes(Arc::from(&v[..]))])).collect())
}

fn plain_500() -> Response {
    eprintln!("ERROR:    ASGI callable returned without starting response.");
    Response::builder()
        .status(500)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from("Internal Server Error"))
        .unwrap()
}

/// `Request(scope, receive)` inside an ASGI app: the request being served (its own scope only)
pub fn request_from_scope(cx: &Cx, scope: &V) -> R {
    let same = match (&*cx.asgi_scope.lock(), scope) {
        (Some(V::Dict(a)), V::Dict(b)) => Arc::ptr_eq(a, b),
        _ => false,
    };
    if !same {
        return Err(Exc::type_error("py2axum: Request(scope) is only supported with the scope of the request being served"));
    }
    Ok(super::request(cx))
}

/// `response(scope, receive, send)`: a Starlette response object sends itself
pub async fn send_response(cx: &Cx, resp: &V, args: &[V]) -> R {
    let [_, _, send_fn] = args else {
        return Err(Exc::type_error(format!("Response.__call__() takes 4 positional arguments but {} were given", args.len() + 1)));
    };
    deliver(cx, super::asgi::to_response(resp)?, send_fn).await?;
    Ok(V::None)
}

/// `receive` wrapped by a middleware, read to the end of the body (`http.request` messages until
/// `more_body` is false, or `http.disconnect`)
pub async fn drain(cx: &Cx, receive: &V) -> R<Bytes> {
    let mut out = Vec::new();
    loop {
        let m = super::aio::await_value(super::methods::call_value_boxed(cx, receive, vec![]).await?).await?;
        match get(&m, "type")? {
            Some(V::Str(t)) if &*t == "http.request" => {
                if let Some(b) = get(&m, "body")? {
                    out.extend_from_slice(&bytes_of(&b, "body")?);
                }
                if !matches!(get(&m, "more_body")?, Some(V::Bool(true))) {
                    break;
                }
            }
            Some(V::Str(t)) if &*t == "http.disconnect" => break,
            _ => return Err(Exc::runtime("py2axum: a wrapped receive() must return http.request or http.disconnect messages")),
        }
    }
    Ok(Bytes::from(out))
}

fn str_of(v: Option<V>, what: &str) -> R<String> {
    match v {
        Some(V::Str(s)) => Ok(s.to_string()),
        Some(o) => Err(Exc::type_error(format!("py2axum: scope[{what:?}] must be a str, not {}", o.type_name()))),
        None => Err(Exc::new(&KEY_ERROR, vec![V::str(what)])),
    }
}

/// a decoded path as the raw path the router decodes again
fn quote_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        if b == b'%' || b == b'?' || b == b'#' || b <= b' ' || b >= 0x7f {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    out
}

/// The request the scope handed to `self.app(...)` describes, when it is not the one being served
/// (headers, method, path or query string rewritten, or a body read through a wrapped `receive`): a new
/// request context for the rest of the stack (ContextVars and `request.state` carried over).
pub fn derive(cx: &Cx, scope: &V, body: Option<Bytes>) -> R<Option<Cx>> {
    let r = &cx.req;
    let method = str_of(get(scope, "method")?, "method")?;
    let path = str_of(get(scope, "path")?, "path")?;
    let query = match get(scope, "query_string")? {
        Some(q) => String::from_utf8_lossy(&bytes_of(&q, "query_string")?).into_owned(),
        None => String::new(),
    };
    if let Some(rp) = get(scope, "root_path")? {
        if !matches!(&rp, V::Str(s) if s.is_empty()) {
            return Err(Exc::runtime("py2axum: a raw middleware setting scope['root_path'] is not supported"));
        }
    }
    let mut headers = Vec::new();
    if let Some(h) = get(scope, "headers")? {
        for pair in super::ops::iter(&h)? {
            let kv = super::ops::iter(&pair)?;
            let [k, v] = &kv[..] else {
                return Err(Exc::value_error("py2axum: an ASGI header must be a (name, value) pair"));
            };
            headers.push((String::from_utf8_lossy(&bytes_of(k, "header name")?).into_owned(), String::from_utf8_lossy(&bytes_of(v, "header value")?).into_owned()));
        }
    }
    let same = body.is_none() && method == r.method && query == r.raw_query && path == super::web::unquote(&r.path) && headers == r.headers;
    if same {
        return Ok(None);
    }
    let raw = if path == super::web::unquote(&r.path) { r.path.clone() } else { quote_path(&path) };
    let cell = super::web::ReqCell {
        method,
        path: raw,
        query: form_urlencoded::parse(query.as_bytes()).into_owned().collect(),
        raw_query: query,
        headers,
        path_params: Mutex::new(vec![]),
        route: Mutex::new(None),
        client: r.client.clone(),
        body: body.unwrap_or_else(|| r.body.clone()),
        state: Mutex::new(r.state.lock().clone()),
        disconnected: std::sync::atomic::AtomicBool::new(false),
    };
    let inner = super::CxInner::new(cx.app.clone(), cell);
    *inner.ctxvars.lock() = cx.ctxvars.lock().clone();
    if let Some(s) = cx.sentry.get() {
        let _ = inner.sentry.set(s.clone());
    }
    Ok(Some(Arc::new(inner)))
}

/// after the inner stack ran on a derived request: its ContextVars and `request.state` are the caller's
/// (one task in Python)
pub fn merge_back(cx: &Cx, inner: &Cx) {
    let vars = inner.ctxvars.lock().clone();
    cx.ctxvars.lock().extend(vars);
    *cx.req.state.lock() = inner.req.state.lock().clone();
}

/// `scope["state"]` is what `request.state` reads (Starlette): set before the inner stack runs...
pub fn state_in(cx: &Cx, scope: &V) -> R<()> {
    if let Some(V::Dict(m)) = get(scope, "state")? {
        let items: Vec<(V, V)> = m.lock().values().cloned().collect();
        let mut st = cx.req.state.lock();
        for (k, v) in items {
            if let V::Str(k) = k {
                st.insert(k.to_string(), v);
            }
        }
    }
    Ok(())
}

/// ...and what the request stored there, back in the scope after it
pub fn state_out(cx: &Cx, scope: &V) -> R<()> {
    if let Some(V::Dict(m)) = get(scope, "state")? {
        let items: Vec<(String, V)> = cx.req.state.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut d = m.lock();
        for (k, v) in items {
            d.insert(Key::Str(Arc::from(k.as_str())), (V::str(&k), v));
        }
    }
    Ok(())
}
