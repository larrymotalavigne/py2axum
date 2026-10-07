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
}

pub struct Chan {
    tx: Mutex<Option<mpsc::UnboundedSender<Msg>>>,
    body: Mutex<Option<Bytes>>,
}

/// is `f` run by Starlette as an ASGI app rather than as a `request -> response` endpoint?
pub fn is_asgi_app(f: &V) -> bool {
    matches!(f, V::Obj(_) | V::Inst(_))
}

fn d(items: Vec<(&str, V)>) -> R {
    V::dict_from(items.into_iter().map(|(k, v)| (V::str(k), v)).collect())
}

fn bytes_of(v: &V, what: &str) -> R<Vec<u8>> {
    match v {
        V::Bytes(b) => Ok(b.to_vec()),
        V::Str(s) => Err(Exc::type_error(format!("py2axum: ASGI {what} must be bytes, not str ({s:?})"))),
        o => Err(Exc::type_error(format!("py2axum: ASGI {what} must be bytes, not {}", o.type_name()))),
    }
}

fn get(m: &V, k: &str) -> R<Option<V>> {
    match m {
        V::Dict(dm) => Ok(dm.lock().get(&Key::Str(Arc::from(k))).map(|(_, v)| v.clone())),
        o => Err(Exc::type_error(format!("py2axum: an ASGI message must be a dict, not {}", o.type_name()))),
    }
}

/// `await receive()`
pub async fn receive(c: &Arc<Chan>) -> R {
    let body = c.body.lock().take();
    match body {
        Some(b) => d(vec![("type", V::str("http.request")), ("body", V::Bytes(Arc::from(&b[..]))), ("more_body", V::Bool(false))]),
        // the request is complete: the next message is the client leaving (the task ends with the response)
        None => std::future::pending().await,
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
                Some(V::Int(i)) => i as u16,
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

/// The ASGI scope of the request being served (the keys uvicorn and Starlette's router set)
fn scope(cx: &Cx, endpoint: &V) -> R {
    let s = super::routing::scope(cx)?;
    if let V::Dict(m) = &s {
        let mut m = m.lock();
        let asgi = d(vec![("version", V::str("3.0")), ("spec_version", V::str("2.4"))])?;
        m.insert(Key::Str(Arc::from("asgi")), (V::str("asgi"), asgi));
        m.insert(Key::Str(Arc::from("state")), (V::str("state"), V::dict_from(vec![])?));
        m.insert(Key::Str(Arc::from("endpoint")), (V::str("endpoint"), endpoint.clone()));
        let pp: Vec<(V, V)> = cx.req.path_params.lock().iter().map(|(k, v)| (V::str(k), V::str(v))).collect();
        m.insert(Key::Str(Arc::from("path_params")), (V::str("path_params"), V::dict_from(pp)?));
    }
    *cx.asgi_scope.lock() = Some(s.clone());
    Ok(s)
}

/// Runs the ASGI app `app` for the current request.
pub async fn serve(cx: &Cx, app: &V) -> R<Response> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let chan = Arc::new(Chan { tx: Mutex::new(Some(tx)), body: Mutex::new(Some(cx.req.body.clone())) });
    let scope = scope(cx, app)?;
    let args = vec![scope, V::native(Native::AsgiReceive(chan.clone())), V::native(Native::AsgiSend(chan.clone()))];
    let (cx2, app2) = (cx.clone(), app.clone());
    let mut task = tokio::spawn(async move { super::methods::call_value(&cx2, &app2, args, vec![]).await });
    let mut done: Option<R> = None;
    // the start message, or the app ending without one
    let (status, headers) = loop {
        tokio::select! {
            m = rx.recv() => match m {
                Some(Msg::Start(s, h)) => break (s, h),
                Some(Msg::Body(..)) => return Err(Exc::runtime("py2axum: ASGI body sent before http.response.start")),
                None => {}
            },
            r = &mut task, if done.is_none() => {
                let r = r.unwrap_or_else(|e| Err(Exc::runtime(format!("ASGI task: {e}"))));
                if let Err(e) = r {
                    return Err(e);
                }
                done = Some(Ok(V::None));
                // uvicorn: "ASGI callable returned without starting response"
                if rx.is_empty() {
                    return Ok(plain_500());
                }
            }
        }
    };
    let mut builder = Response::builder().status(StatusCode::from_u16(status).map_err(|e| Exc::value_error(e.to_string()))?);
    for (k, v) in &headers {
        let k = HeaderName::from_bytes(k).map_err(|e| Exc::value_error(e.to_string()))?;
        let v = HeaderValue::from_bytes(v).map_err(|e| Exc::value_error(e.to_string()))?;
        builder = builder.header(k, v);
    }
    // the body: whole when the first part says so, else streamed
    let first = loop {
        tokio::select! {
            m = rx.recv() => match m {
                Some(Msg::Body(b, more)) => break Some((b, more)),
                Some(Msg::Start(..)) => return Err(Exc::runtime("py2axum: http.response.start sent twice")),
                None => break None,
            },
            r = &mut task, if done.is_none() => {
                let r = r.unwrap_or_else(|e| Err(Exc::runtime(format!("ASGI task: {e}"))));
                done = Some(r);
                if rx.is_empty() {
                    break None;
                }
            }
        }
    };
    let Some((first, more)) = first else {
        return Ok(builder.body(Body::empty()).unwrap());
    };
    if !more {
        task.abort();
        return Ok(builder.body(Body::from(first)).unwrap());
    }
    let guard = Abort(task);
    let stream = futures_util::stream::unfold((Some(first), rx, guard, false), |(pending, mut rx, guard, finished)| async move {
        if let Some(b) = pending {
            return Some((Ok::<Bytes, std::io::Error>(b), (None, rx, guard, finished)));
        }
        if finished {
            return None;
        }
        loop {
            match rx.recv().await {
                Some(Msg::Body(b, more)) => return Some((Ok(b), (None, rx, guard, !more))),
                Some(Msg::Start(..)) => continue,
                None => return None,
            }
        }
    });
    Ok(builder.body(Body::from_stream(stream)).unwrap())
}

/// the app's task ends with the response (the client left, or the body is complete)
struct Abort(tokio::task::JoinHandle<R>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
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
    let r = super::asgi::to_response(resp)?;
    let (parts, body) = r.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| Exc::runtime(e.to_string()))?;
    let mut headers = Vec::new();
    // Starlette's init_headers order: content-length, then content-type, then the rest
    let mut hs: Vec<(String, Vec<u8>)> = parts.headers.iter().map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec())).collect();
    if !hs.iter().any(|(k, _)| k == "content-length") {
        hs.insert(0, ("content-length".into(), body.len().to_string().into_bytes()));
    }
    hs.sort_by_key(|(k, _)| match k.as_str() {
        "content-length" => 0,
        "content-type" => 1,
        _ => 2,
    });
    for (k, v) in hs {
        headers.push(V::tuple(vec![V::Bytes(Arc::from(k.as_bytes())), V::Bytes(Arc::from(&v[..]))]));
    }
    let start = d(vec![("type", V::str("http.response.start")), ("status", V::Int(parts.status.as_u16() as i64)), ("headers", V::list(headers))])?;
    super::aio::await_value(super::methods::call_value_boxed(cx, send_fn, vec![start]).await?).await?;
    let msg = d(vec![("type", V::str("http.response.body")), ("body", V::Bytes(Arc::from(&body[..])))])?;
    super::aio::await_value(super::methods::call_value_boxed(cx, send_fn, vec![msg]).await?).await?;
    Ok(V::None)
}
