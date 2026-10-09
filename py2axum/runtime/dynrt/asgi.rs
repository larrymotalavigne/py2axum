//! Starlette's middleware stack, outermost first: ServerErrorMiddleware (the `Exception`/500 handler)
//! -> the user middlewares (last added = outermost: CORSMiddleware, BaseHTTPMiddleware subclasses)
//! -> ExceptionMiddleware (handlers by status code, then by the exception's MRO; FastAPI's defaults
//! for HTTPException and RequestValidationError) -> the router.
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::Response;

use super::ops;
use super::v::*;
use super::web::{self, RouteDef};
use super::{Cx, CxInner};

pub enum HKey {
    Class(&'static Class),
    Status(u16),
}

pub enum Mw {
    Cors(Cors),
    /// `dispatch` of a BaseHTTPMiddleware instance, bound
    Dispatch(V),
    /// a raw ASGI middleware instance (`async __call__(scope, receive, send)`), built with
    /// `Native::AsgiApp(slot)` as its `app`: the rest of the stack
    Asgi(V, Arc<Slot>),
    /// starlette_context's RawContextMiddleware
    Context(super::ctxmw::RawContext),
    /// Starlette's HTTPSRedirectMiddleware
    HttpsRedirect,
    /// Starlette's TrustedHostMiddleware
    TrustedHost(TrustedHost),
}

/// Starlette's TrustedHostMiddleware(allowed_hosts=..., www_redirect=...)
pub struct TrustedHost {
    pub hosts: Vec<String>,
    pub www_redirect: bool,
}

/// starlette._utils.parse_host_header: (host, port) of a valid Host header
fn parse_host(h: Option<String>) -> Option<(String, Option<String>)> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)^(?P<host>[a-z0-9._~%!$&'()*+,;=-]+|\[(?:(?P<ipv6>[a-f0-9]*:[a-f0-9.:]+)|(?-i:v)[a-f0-9]+\.[a-z0-9._~!$&'()*+,;=:-]+)\])(?::(?P<port>[0-9]+))?$").unwrap()
    });
    let h = h?;
    let c = re.captures(&h)?;
    if let Some(ip) = c.name("ipv6") {
        ip.as_str().parse::<std::net::Ipv6Addr>().ok()?;
    }
    Some((c["host"].to_string(), c.name("port").map(|p| p.as_str().to_string())))
}

/// `URL(scope=scope)` of a request whose Host header parsed (scheme http, the decoded path, the raw query)
fn scope_url(cx: &Cx, netloc: &str) -> String {
    let mut u = format!("http://{netloc}{}", web::unquote(&cx.req.path));
    if !cx.req.raw_query.is_empty() {
        u += "?";
        u += &cx.req.raw_query;
    }
    u
}

fn redirect(url: String) -> R<Response> {
    let obj = super::resp::new("RedirectResponse", &[V::str(url)], &[("status_code".to_string(), V::Int(307))])?;
    let V::Native(n) = &obj else { unreachable!() };
    let Native::RespObj(r) = &**n else { unreachable!() };
    super::resp::into_response(r)
}

fn invalid_host() -> R<Response> {
    Ok(Response::builder().status(400).header("content-type", "text/plain; charset=utf-8")
        .body(axum::body::Body::from("Invalid host header")).unwrap())
}

impl TrustedHost {
    fn check(&self, cx: &Cx) -> Option<R<Response>> {
        if self.hosts.iter().any(|h| h == "*") {
            return None;
        }
        let Some((host, port)) = parse_host(cx.req.header("host")) else { return Some(invalid_host()) };
        let mut www = false;
        for p in &self.hosts {
            if host == *p || (p.starts_with('*') && host.ends_with(&p[1..])) {
                return None;
            }
            if format!("www.{host}") == *p {
                www = true;
            }
        }
        if www && self.www_redirect {
            let netloc = match &port { Some(p) => format!("{host}:{p}"), None => host };
            return Some(redirect(scope_url(cx, &format!("www.{netloc}"))));
        }
        Some(invalid_host())
    }
}

/// HTTPSRedirectMiddleware: every request is redirected (307) to its https URL, without the port if it is 80 or 443
fn https_redirect(cx: &Cx) -> R<Response> {
    let Some((host, port)) = parse_host(cx.req.header("host")) else {
        return Err(Exc::runtime("py2axum: HTTPSRedirectMiddleware with an invalid Host header (Starlette builds the URL from the server address)"));
    };
    let port_n = match &port {
        Some(p) => Some(p.parse::<u32>().ok().filter(|n| *n <= 65535).ok_or_else(|| Exc::value_error(format!("Port out of range 0-65535")))?),
        None => None,
    };
    let netloc = if matches!(port_n, Some(80) | Some(443)) {
        host.trim_start_matches('[').trim_end_matches(']').to_lowercase()
    } else {
        match &port { Some(p) => format!("{host}:{p}"), None => host }
    };
    let u = scope_url(cx, &netloc);
    redirect(format!("https{}", &u["http".len()..]))
}

/// Where the `app` a raw middleware was built with leads: (the stack, the next layer), set once the
/// stack is complete (Starlette builds the whole stack before the first request)
#[derive(Default)]
pub struct Slot {
    at: std::sync::OnceLock<(std::sync::Weak<Stack>, usize, &'static [RouteDef])>,
}

impl Slot {
    pub fn new() -> Arc<Slot> {
        Arc::new(Slot::default())
    }
}

/// The application's middlewares and exception handlers, built once (Starlette builds its stack on the
/// first request).
#[derive(Default)]
pub struct Stack {
    pub mws: Vec<Mw>,
    pub handlers: Vec<(HKey, V)>,
    /// handler of `Exception` / 500: ServerErrorMiddleware's
    pub server: Option<V>,
}

impl Stack {
    /// `app.add_middleware(...)`: Starlette inserts at 0, the last added runs first.
    pub fn add_middleware(&mut self, mw: Mw) {
        self.mws.insert(0, mw);
    }
    /// `FastAPI(middleware=[...])`: Starlette's `user_middleware` starts as that list, so its entries stay
    /// inside every `add_middleware`, in list order
    pub fn push_middleware(&mut self, mw: Mw) {
        self.mws.push(mw);
    }
    /// `@app.exception_handler(key)` (a dict: registering a key again replaces its handler)
    pub fn exception_handler(&mut self, key: &V, handler: V) -> R<()> {
        let k = match key {
            V::Class(c) if std::ptr::eq(*c, &EXCEPTION) => {
                self.server = Some(handler);
                return Ok(());
            }
            V::Int(500) => {
                self.server = Some(handler);
                return Ok(());
            }
            V::Class(c) => HKey::Class(c),
            V::Int(i) => HKey::Status(*i as u16),
            o => return Err(Exc::type_error(format!("py2axum: exception_handler({}) is not supported", o.type_name()))),
        };
        self.handlers.retain(|(h, _)| !same(h, &k));
        self.handlers.push((k, handler));
        Ok(())
    }
}

fn same(a: &HKey, b: &HKey) -> bool {
    match (a, b) {
        (HKey::Class(x), HKey::Class(y)) => std::ptr::eq(*x, *y),
        (HKey::Status(x), HKey::Status(y)) => x == y,
        _ => false,
    }
}

pub type StackFn = for<'a> fn(&'a Cx) -> Pin<Box<dyn Future<Output = R<Stack>> + Send + 'a>>;

static STACK: tokio::sync::OnceCell<Arc<Stack>> = tokio::sync::OnceCell::const_new();

/// The stack, built on the first request; each raw middleware's `app` then leads to the layer after it.
async fn get_stack(stack_fn: StackFn, routes: &'static [RouteDef]) -> R<Arc<Stack>> {
    STACK
        .get_or_try_init(|| async move {
            let st = Arc::new(stack_fn(&super::root_cx()).await?);
            for (i, mw) in st.mws.iter().enumerate() {
                if let Mw::Asgi(_, slot) = mw {
                    let _ = slot.at.set((Arc::downgrade(&st), i + 1, routes));
                }
            }
            Ok(st)
        })
        .await
        .cloned()
}

fn plain_500() -> Response {
    Response::builder()
        .status(500)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from("Internal Server Error"))
        .unwrap()
}

fn log_exc(e: &Exc) {
    eprintln!("ERROR:py2axum:Exception in ASGI application: {:?}", e);
}

/// The ASGI application: one request through the whole stack.
static PYTHON_SIDE: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();

/// `--python-side mount`: the prefixes of the app's last `app.mount()`s, left in Python
static MOUNTS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();

/// The paths left to the Python application (`--python-side`): relayed to `PY2AXUM_PYTHON_URL` when set.
/// `mounts`: the app ends with `app.mount()`s left in Python, so a request under one of their prefixes that
/// no translated route fully matches goes there too (Starlette's router: the mount's full match beats an
/// earlier partial one, so a 405 or a HEAD on a GET route reaches the mounted app; the prefix itself is
/// relayed as well, for the 307 to `prefix/`).
pub fn set_python_side(paths: &[&'static str], mounts: &[&'static str]) {
    let _ = PYTHON_SIDE.set(paths.to_vec());
    let _ = MOUNTS.set(mounts.to_vec());
}

/// under a mount left in Python (`scope["path"]`, decoded, like Starlette's `Mount`)
fn under_mount(raw_path: &str) -> bool {
    let Some(ms) = MOUNTS.get() else { return false };
    if ms.is_empty() {
        return false;
    }
    let path = web::unquote(raw_path);
    ms.iter().any(|m| path.starts_with(&format!("{m}/")) || (!m.is_empty() && path == *m))
}

/// a route pattern (`/items/{id}`, `{p:path}`) or a mount prefix matching the request path
fn python_side(path: &str) -> bool {
    let Some(pats) = PYTHON_SIDE.get() else { return false };
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    pats.iter().any(|p| {
        // a router prefix read from the settings (`\x01n\x01` marker), known once the globals are set
        let resolved = if p.contains('\u{1}') { Some(super::web::runtime_prefix(p)) } else { None };
        let p = resolved.as_deref().unwrap_or(p);
        if path.starts_with(&format!("{}/", p.trim_end_matches('/'))) && !p.contains('{') {
            return true; // under a mounted application
        }
        let ps: Vec<&str> = p.trim_end_matches('/').split('/').collect();
        let mut i = 0;
        for (k, s) in ps.iter().enumerate() {
            if s.starts_with('{') && s.ends_with(":path}") {
                return k <= segs.len();
            }
            match segs.get(i) {
                Some(x) if (s.starts_with('{') && s.ends_with('}') && !x.is_empty()) || x == s => i += 1,
                _ => return false,
            }
        }
        i == segs.len()
    })
}

/// `PY2AXUM_MAX_BODY` (bytes): optional cap on a request body, answered 413 before it is buffered whole.
/// Unset by default, as uvicorn and Starlette have none (equivalence); set it when no proxy in front caps
/// bodies (ingress-nginx's `proxy-body-size`), since the binary buffers a body before routing.
fn max_body() -> Option<usize> {
    static MAX: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| std::env::var("PY2AXUM_MAX_BODY").ok().and_then(|v| v.trim().parse().ok()))
}

fn too_large() -> Response {
    Response::builder()
        .status(413)
        .header("content-type", "text/plain; charset=utf-8")
        .header("connection", "close")
        .body(Body::from("Request Entity Too Large"))
        .unwrap()
}

/// the whole request body, or the response to give instead (400 on a broken body, 413 over the cap)
async fn read_body(headers: &axum::http::HeaderMap, body: Body) -> Result<axum::body::Bytes, Response> {
    let Some(max) = max_body() else {
        return axum::body::to_bytes(body, usize::MAX).await.map_err(|_| Response::builder().status(400).body(Body::empty()).unwrap());
    };
    let declared = headers.get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max as u64) {
        return Err(too_large());
    }
    axum::body::to_bytes(body, max).await.map_err(|e| {
        // http-body-util's LengthLimitError, seen through axum's error (not a dependency of its own)
        let over = std::error::Error::source(&e).is_some_and(|s| s.to_string() == "length limit exceeded");
        if over { too_large() } else { Response::builder().status(400).body(Body::empty()).unwrap() }
    })
}

/// hop-by-hop headers (RFC 9110 §7.6.1), never relayed in either direction, plus the ones the `Connection`
/// header names; `content-length` is recomputed by the client from the buffered body
fn relayed(headers: &axum::http::HeaderMap, extra: &[&str]) -> Vec<(axum::http::HeaderName, axum::http::HeaderValue)> {
    const HOP: [&str; 9] =
        ["connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization", "proxy-authenticate", "proxy-connection"];
    let named: Vec<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|t| t.trim().to_ascii_lowercase()))
        .filter(|t| !t.is_empty())
        .collect();
    headers
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            !HOP.contains(&k) && !extra.contains(&k) && !named.iter().any(|n| n == k)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// relays the request to the Python application, streaming its response back. The headers go through
/// unchanged apart from the hop-by-hop ones (X-Forwarded-* included: see docs/security.md for the
/// sidecar's `--forwarded-allow-ips`); the client ignores HTTP(S)_PROXY, the sidecar is local
async fn proxy(upstream: &str, req: axum::extract::Request) -> Response {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let http = HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .no_deflate()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap()
    });
    let (parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let url = format!("{}{}", upstream.trim_end_matches('/'), pq);
    let mut rb = http.request(parts.method.clone(), &url);
    for (k, v) in relayed(&parts.headers, &["content-length"]) {
        rb = rb.header(k, v);
    }
    let body = match read_body(&parts.headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    match rb.body(body).send().await {
        Ok(r) => {
            let mut out = Response::builder().status(r.status().as_u16());
            for (k, v) in relayed(r.headers(), &[]) {
                out = out.header(k, v);
            }
            out.body(Body::from_stream(r.bytes_stream())).unwrap_or_else(|_| plain_500())
        }
        Err(e) => {
            // the path only: a query string can carry a token
            eprintln!("ERROR:py2axum:python-side upstream {}: {}", parts.uri.path(), e.without_url());
            Response::builder().status(502).header("content-type", "text/plain; charset=utf-8").body(Body::from("Bad Gateway")).unwrap()
        }
    }
}

pub async fn app(
    app: Arc<super::AppState>,
    req: axum::extract::Request,
    routes: &'static [RouteDef],
    ws_routes: &'static [super::ws::WsRouteDef],
    stack_fn: StackFn,
) -> Response {
    // a WebSocket handshake: the scope `websocket` (the HTTP middlewares let it through; never relayed
    // to the Python side)
    if super::ws::is_upgrade(&req) {
        if let Err(e) = get_stack(stack_fn, routes).await {
            log_exc(&e);
            return plain_500();
        }
        return super::ws::serve(app, req, ws_routes, ws_handle).await;
    }
    if python_side(req.uri().path()) {
        return match std::env::var("PY2AXUM_PYTHON_URL") {
            Ok(up) => proxy(&up, req).await,
            // no Python process: the path is not the binary's (the ingress routes it elsewhere), and a
            // translated route must not answer in its place (`/items/export` would reach `/items/{id}`)
            Err(_) => web::not_found().await,
        };
    }
    if under_mount(req.uri().path()) {
        if let Ok(up) = std::env::var("PY2AXUM_PYTHON_URL") {
            // the stack registers the routes `app.add_route` adds: built before matching
            if get_stack(stack_fn, routes).await.is_ok()
                && !web::full_match(req.uri().path(), req.method().as_str(), routes)
            {
                return proxy(&up, req).await;
            }
        }
    }
    let client = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|c| (c.0.ip().to_canonical().to_string(), c.0.port()));
    let (parts, body) = req.into_parts();
    let bytes = match read_body(&parts.headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mut cell = web::ReqCell::from_parts(parts.method.as_str(), &parts.uri, &parts.headers, vec![], bytes);
    cell.client = client;
    let stack = match get_stack(stack_fn, routes).await {
        Ok(s) => s,
        Err(e) => {
            log_exc(&e);
            return plain_500();
        }
    };
    let cx: Cx = Arc::new(CxInner::new(app, cell));
    // sentry_sdk's FastAPI integration (SentryAsgiMiddleware): nothing to do while it is not initialised
    let traced = super::sentry::active() && super::sentry::request_start(&cx).await;
    let r = match chain(&cx, &stack, 0, routes).await {
        Ok(r) => r,
        Err(e) if super::resp::is_drop(&e) => {
            panic!("py2axum: status code outside 100..599: no status line, the connection is dropped (as uvicorn does)")
        }
        Err(e) => {
            let r = server_error(&cx, &stack, e.clone()).await;
            if traced {
                super::sentry::unhandled(&cx, &e).await;
            }
            r
        }
    };
    if traced {
        super::sentry::request_end(&cx, r.status().as_u16()).await;
    }
    r
}

type RespFut<'a> = Pin<Box<dyn Future<Output = R<Response>> + Send + 'a>>;

/// The stack from layer `i` on. Each layer boxes only its own future (one `async` block for every kind
/// of layer would be as large as the largest, the router's, and copied whole at every layer).
fn chain<'a>(cx: &'a Cx, stack: &'a Arc<Stack>, i: usize, routes: &'static [RouteDef]) -> RespFut<'a> {
    match stack.mws.get(i) {
        None => Box::pin(async move {
            match web::route(cx, routes).await {
                Ok(r) => Ok(r),
                Err(e) => handle(cx, stack, e).await,
            }
        }),
        Some(Mw::Cors(c)) => Box::pin(c.call(cx, chain(cx, stack, i + 1, routes))),
        Some(Mw::Dispatch(f)) => Box::pin(async move {
            let next = V::native(Native::CallNext(Arc::new(Next { stack: stack.clone(), i: i + 1, routes })));
            let ret = super::methods::call_value(cx, f, vec![super::request(cx), next], vec![]).await?;
            to_response(&ret)
        }),
        Some(Mw::Asgi(inst, _)) => Box::pin(async move {
            let scope = super::rawasgi::mw_scope(cx)?;
            super::rawasgi::run(cx, inst, scope, cx.req.body.clone()).await
        }),
        Some(Mw::Context(c)) => Box::pin(c.call(cx, chain(cx, stack, i + 1, routes))),
        Some(Mw::HttpsRedirect) => Box::pin(std::future::ready(https_redirect(cx))),
        Some(Mw::TrustedHost(t)) => match t.check(cx) {
            Some(r) => Box::pin(std::future::ready(r)),
            None => chain(cx, stack, i + 1, routes),
        },
    }
}

/// `await self.app(scope, receive, send)` in a raw middleware: the rest of the stack for the request the
/// scope describes (a rewritten scope or a wrapped `receive` makes a new request), its response delivered
/// through `send`; an exception it lets through is raised here, in the middleware.
pub async fn call_app(cx: &Cx, slot: &Slot, args: Vec<V>) -> R {
    let [scope, receive, send] = &args[..] else {
        return Err(Exc::type_error(format!("py2axum: an ASGI app takes 3 positional arguments (scope, receive, send) but {} were given", args.len())));
    };
    let Some((st, i, routes)) = slot.at.get() else {
        return Err(Exc::runtime("py2axum: the middleware stack is not built yet"));
    };
    let Some(stack) = st.upgrade() else { return Err(Exc::runtime("py2axum: the middleware stack is gone")) };
    if !matches!(super::rawasgi::get(scope, "type")?, Some(V::Str(t)) if &*t == "http") {
        return Err(Exc::runtime("py2axum: a raw middleware handing a non-http scope to the application is not supported"));
    }
    // the body: the request's, unless `receive` is a wrapper (read whole, like the server does)
    let body = match receive {
        V::Native(n) if matches!(&**n, Native::AsgiReceive(_)) => None,
        f => Some(super::rawasgi::drain(cx, f).await?),
    };
    let inner = super::rawasgi::derive(cx, scope, body)?;
    let icx = inner.as_ref().unwrap_or(cx);
    let prev = icx.asgi_scope.lock().replace(scope.clone());
    super::rawasgi::state_in(icx, scope)?;
    let r = chain(icx, &stack, *i, routes).await;
    super::rawasgi::state_out(icx, scope)?;
    if let Some(icx) = &inner {
        super::rawasgi::merge_back(cx, icx);
    } else {
        *cx.asgi_scope.lock() = prev;
    }
    super::rawasgi::deliver(cx, r?, send).await?;
    Ok(V::None)
}

/// `call_next` handed to a BaseHTTPMiddleware's `dispatch`.
pub struct Next {
    stack: Arc<Stack>,
    i: usize,
    routes: &'static [RouteDef],
}

/// `await call_next(request)`: the response of the inner stack; an exception escaping it is raised here.
pub async fn call_next(cx: &Cx, n: &Next) -> R {
    let saved = super::sentry::context_enter(cx);
    let r = chain(cx, &n.stack, n.i, n.routes).await;
    super::sentry::context_exit(cx, saved);
    let r = r?;
    Ok(super::resp::from_response(r))
}

pub fn to_response(v: &V) -> R<Response> {
    if let V::Native(n) = v {
        if let Native::RespObj(r) = &**n {
            return super::resp::into_response(r);
        }
    }
    Err(Exc::type_error(format!("'{}' object is not callable", v.type_name())))
}

/// `type(exc).__mro__`, depth-first left to right without repeats (C3 for the usual hierarchies).
fn mro(c: &'static Class, out: &mut Vec<&'static Class>) {
    if !out.iter().any(|x| std::ptr::eq(*x, c)) {
        out.push(c);
    }
    for b in c.bases {
        mro(b, out);
    }
}

/// ExceptionMiddleware: a handler by status code (HTTPException), else the first class of the MRO
/// with a handler; FastAPI registers HTTPException and RequestValidationError by default.
async fn handle(cx: &Cx, stack: &Stack, e: Exc) -> R<Response> {
    if super::resp::is_drop(&e) {
        return Err(e); // raised by the server's send: no handler sees it
    }
    let mut found: Option<V> = None;
    if let Some((code, ..)) = &e.http_info() {
        found = stack.handlers.iter().find(|(k, _)| matches!(k, HKey::Status(s) if s == code)).map(|(_, h)| h.clone());
    }
    if found.is_none() {
        let mut classes = Vec::new();
        mro(e.0.class, &mut classes);
        for c in classes {
            if let Some((_, h)) = stack.handlers.iter().find(|(k, _)| matches!(k, HKey::Class(x) if std::ptr::eq(*x, c))) {
                found = Some(h.clone());
                break;
            }
            if std::ptr::eq(c, &HTTP_EXCEPTION) || std::ptr::eq(c, &REQUEST_VALIDATION_ERROR) {
                if super::sentry::active() {
                    super::sentry::handled_exception(cx, &e).await;
                }
                return web::try_error_response(e);
            }
        }
    }
    match found {
        Some(h) => {
            if super::sentry::active() {
                super::sentry::handled_exception(cx, &e).await;
            }
            let ret = super::methods::call_value(cx, &h, vec![super::request(cx), V::Exc(e)], vec![]).await?;
            to_response(&ret)
        }
        None => Err(e),
    }
}

/// ExceptionMiddleware on a WebSocket route: a handler by status code, else by the exception's MRO,
/// called with `(websocket, exc)`, its response sent on the connection; FastAPI's defaults for
/// HTTPException / RequestValidationError (a JSON response), WebSocketRequestValidationError (close
/// 1008) and Starlette's for WebSocketException (close with its code and reason). Without one the
/// exception escapes (ServerErrorMiddleware lets the `websocket` scope through).
fn ws_handle(cx: &Cx, e: Exc) -> Pin<Box<dyn Future<Output = R<()>> + Send + '_>> {
    Box::pin(async move {
        use super::ws;
        let Some(stack) = STACK.get().cloned() else { return Err(e) };
        let mut found: Option<V> = None;
        if let Some((code, ..)) = &e.http_info() {
            found = stack.handlers.iter().find(|(k, _)| matches!(k, HKey::Status(s) if s == code)).map(|(_, h)| h.clone());
        }
        if found.is_none() {
            let mut classes = Vec::new();
            mro(e.0.class, &mut classes);
            for c in classes {
                if let Some((_, h)) = stack.handlers.iter().find(|(k, _)| matches!(k, HKey::Class(x) if std::ptr::eq(*x, c))) {
                    found = Some(h.clone());
                    break;
                }
                if std::ptr::eq(c, &HTTP_EXCEPTION) || std::ptr::eq(c, &REQUEST_VALIDATION_ERROR) {
                    let Some(s) = ws::session(cx) else { return Err(e) };
                    return ws::send_raw_response(&s, web::error_response(e)).await;
                }
                if std::ptr::eq(c, &WS_EXCEPTION) {
                    let attr = |k: &str| e.0.attrs.lock().get(k).cloned().unwrap_or(V::None);
                    return ws::close_with(cx, attr("code"), attr("reason")).await;
                }
                if std::ptr::eq(c, &WS_VALIDATION_ERROR) {
                    let errs: Vec<V> = e.0.errors.as_ref().map(|v| v.iter().map(|x| x.to_v()).collect()).unwrap_or_default();
                    return ws::close_with(cx, V::Int(1008), super::pyd::jsonable(&V::list(errs))?).await;
                }
            }
        }
        match found {
            Some(h) => {
                let ret = super::methods::call_value(cx, &h, vec![ws::current(cx)?, V::Exc(e)], vec![]).await?;
                ws::handler_response(cx, &ret).await
            }
            None => Err(e),
        }
    })
}

/// ServerErrorMiddleware: the `Exception` handler's response (it bypasses the user middlewares), the
/// exception logged anyway; without one, a plain 500.
async fn server_error(cx: &Cx, stack: &Stack, e: Exc) -> Response {
    if let Some(h) = &stack.server {
        let r = super::methods::call_value(cx, h, vec![super::request(cx), V::Exc(e.clone())], vec![]).await;
        log_exc(&e);
        return match r.and_then(|v| to_response(&v)) {
            Ok(r) => r,
            Err(e2) => {
                log_exc(&e2);
                plain_500()
            }
        };
    }
    log_exc(&e);
    plain_500()
}

// ---------------------------------------------------------------- CORSMiddleware

const ALL_METHODS: &[&str] = &["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT", "QUERY"];
const SAFELISTED_HEADERS: &[&str] = &["Accept", "Accept-Language", "Content-Language", "Content-Type"];

/// starlette.middleware.cors.CORSMiddleware, its options evaluated at startup like `__init__`, as the
/// project's Starlette has it (`v17`: 1.7+, which adds `Vary: Origin` to every response and the QUERY
/// method; before, a request without `Origin` passes through untouched).
pub struct Cors {
    v17: bool,
    allow_origins: Vec<String>,
    allow_methods: Vec<String>,
    allow_headers: Vec<String>,
    allow_all_origins: bool,
    allow_all_headers: bool,
    allow_credentials: bool,
    preflight_explicit_allow_origin: bool,
    allow_origin_regex: Option<fancy_regex::Regex>,
    allow_private_network: bool,
    simple_headers: Vec<(String, String)>,
    preflight_headers: Vec<(String, String)>,
}

fn strs(v: &V, what: &str) -> R<Vec<String>> {
    match v {
        V::List(l) => l.lock().iter().map(ops::str_).collect(),
        V::Tuple(t) => t.iter().map(ops::str_).collect(),
        V::Set(s) => s.lock().values().map(ops::str_).collect(),
        o => Err(Exc::type_error(format!("py2axum: CORSMiddleware({what}=) must be a list, not {}", o.type_name()))),
    }
}

fn dict_set(d: &mut Vec<(String, String)>, k: &str, v: String) {
    match d.iter_mut().find(|(x, _)| x == k) {
        Some(e) => e.1 = v,
        None => d.push((k.to_string(), v)),
    }
}

impl Cors {
    pub fn new(kwargs: Vec<(String, V)>, starlette: (u32, u32)) -> R<Cors> {
        let v17 = starlette >= (1, 7);
        let mut allow_origins = vec![];
        let mut allow_methods: Vec<String> = vec!["GET".into()];
        let mut allow_headers_in = vec![];
        let mut allow_credentials = false;
        let mut regex = None;
        let mut allow_private_network = false;
        let mut expose_headers = vec![];
        let mut max_age = "600".to_string();
        for (k, v) in &kwargs {
            match k.as_str() {
                "allow_origins" => allow_origins = strs(v, k)?,
                "allow_methods" => allow_methods = strs(v, k)?,
                "allow_headers" => allow_headers_in = strs(v, k)?,
                "allow_credentials" => allow_credentials = ops::truthy(v)?,
                "allow_origin_regex" if !v.is_none() => {
                    let pat = ops::str_(v)?;
                    regex = Some(fancy_regex::Regex::new(&format!("^(?:{pat})$")).map_err(|e| Exc::msg(&RE_ERROR, e.to_string()))?);
                }
                "allow_origin_regex" => {}
                "allow_private_network" => allow_private_network = ops::truthy(v)?,
                "expose_headers" => expose_headers = strs(v, k)?,
                "max_age" => max_age = ops::str_(v)?,
                _ => return Err(Exc::type_error(format!("CORSMiddleware.__init__() got an unexpected keyword argument '{k}'"))),
            }
        }
        if allow_methods.iter().any(|m| m == "*") {
            allow_methods = ALL_METHODS.iter().filter(|m| v17 || **m != "QUERY").map(|m| m.to_string()).collect();
        }
        let allow_all_origins = allow_origins.iter().any(|o| o == "*");
        let allow_all_headers = allow_headers_in.iter().any(|h| h == "*");
        let preflight_explicit_allow_origin = !allow_all_origins || allow_credentials;
        let mut simple_headers = vec![];
        if allow_all_origins {
            simple_headers.push(("Access-Control-Allow-Origin".to_string(), "*".to_string()));
        }
        if allow_credentials {
            simple_headers.push(("Access-Control-Allow-Credentials".into(), "true".into()));
        }
        if !expose_headers.is_empty() {
            simple_headers.push(("Access-Control-Expose-Headers".into(), expose_headers.join(", ")));
        }
        let mut preflight_headers = vec![];
        if v17 {
            preflight_headers.push((
                "Vary".to_string(),
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers, Access-Control-Request-Private-Network".to_string(),
            ));
        } else if preflight_explicit_allow_origin {
            preflight_headers.push(("Vary".to_string(), "Origin".to_string()));
        }
        if !preflight_explicit_allow_origin {
            preflight_headers.push(("Access-Control-Allow-Origin".into(), "*".into()));
        }
        preflight_headers.push(("Access-Control-Allow-Methods".into(), allow_methods.join(", ")));
        preflight_headers.push(("Access-Control-Max-Age".into(), max_age));
        let mut allow_headers: Vec<String> = SAFELISTED_HEADERS.iter().map(|h| h.to_string()).collect();
        for h in allow_headers_in {
            if !allow_headers.contains(&h) {
                allow_headers.push(h);
            }
        }
        allow_headers.sort();
        if !allow_headers.is_empty() && !allow_all_headers {
            preflight_headers.push(("Access-Control-Allow-Headers".into(), allow_headers.join(", ")));
        }
        if allow_credentials {
            preflight_headers.push(("Access-Control-Allow-Credentials".into(), "true".into()));
        }
        Ok(Cors {
            v17,
            allow_origins,
            allow_methods,
            allow_headers: allow_headers.iter().map(|h| h.to_lowercase()).collect(),
            allow_all_origins,
            allow_all_headers,
            allow_credentials,
            preflight_explicit_allow_origin,
            allow_origin_regex: regex,
            allow_private_network,
            simple_headers,
            preflight_headers,
        })
    }

    fn is_allowed_origin(&self, origin: &str) -> bool {
        if self.allow_all_origins {
            return true;
        }
        if let Some(re) = &self.allow_origin_regex {
            if re.is_match(origin).unwrap_or(false) {
                return true;
            }
        }
        self.allow_origins.iter().any(|o| o == origin)
    }

    fn preflight(&self, cx: &Cx, origin: &str) -> Response {
        let requested_method = cx.req.header("access-control-request-method").unwrap_or_default();
        let requested_headers = cx.req.header("access-control-request-headers");
        let requested_private_network = cx.req.header("access-control-request-private-network");
        let mut headers = self.preflight_headers.clone();
        let mut failures: Vec<&str> = vec![];
        if self.is_allowed_origin(origin) {
            if self.preflight_explicit_allow_origin {
                dict_set(&mut headers, "Access-Control-Allow-Origin", origin.to_string());
            }
        } else {
            failures.push("origin");
        }
        if !self.allow_methods.iter().any(|m| *m == requested_method) {
            failures.push("method");
        }
        if let Some(rh) = &requested_headers {
            if self.allow_all_headers {
                dict_set(&mut headers, "Access-Control-Allow-Headers", rh.clone());
            } else {
                for h in rh.split(',') {
                    let h = h.to_lowercase();
                    if !self.allow_headers.iter().any(|a| *a == h.trim()) {
                        failures.push("headers");
                        break;
                    }
                }
            }
        }
        if requested_private_network.is_some() {
            if self.allow_private_network {
                dict_set(&mut headers, "Access-Control-Allow-Private-Network", "true".into());
            } else {
                failures.push("private-network");
            }
        }
        let (status, text) = if failures.is_empty() { (200, "OK".to_string()) } else { (400, format!("Disallowed CORS {}", failures.join(", "))) };
        let mut b = Response::builder().status(status);
        for (k, v) in &headers {
            b = b.header(k.to_lowercase(), hv(v));
        }
        b.header("content-length", text.len().to_string())
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(text))
            .unwrap()
    }

    async fn call(&self, cx: &Cx, inner: RespFut<'_>) -> R<Response> {
        let origin = cx.req.header("origin");
        if origin.is_none() && !self.v17 {
            return inner.await;
        }
        if let Some(o) = &origin {
            if cx.req.method == "OPTIONS" && cx.req.header("access-control-request-method").is_some() {
                return Ok(self.preflight(cx, o));
            }
        }
        let mut r = inner.await?;
        let h = r.headers_mut();
        if origin.is_some() {
            for (k, v) in &self.simple_headers {
                set_header(h, k, v);
            }
        }
        match &origin {
            Some(o) if self.allow_all_origins && self.allow_credentials => self.allow_explicit_origin(h, o),
            Some(o) if !self.allow_all_origins && self.is_allowed_origin(o) => self.allow_explicit_origin(h, o),
            _ if self.v17 => add_vary_origin(h),
            _ => {}
        }
        Ok(r)
    }
}

fn hv(v: &str) -> HeaderValue {
    HeaderValue::from_str(v).unwrap_or_else(|_| HeaderValue::from_bytes(v.as_bytes()).unwrap_or(HeaderValue::from_static("")))
}

/// MutableHeaders.__setitem__: one value for the (lower-cased) name
fn set_header(h: &mut HeaderMap, k: &str, v: &str) {
    if let Ok(name) = HeaderName::from_bytes(k.to_lowercase().as_bytes()) {
        h.insert(name, hv(v));
    }
}

fn add_vary_origin(h: &mut HeaderMap) {
    let mut vals: Vec<String> = h.get_all("vary").iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
    vals.push("Origin".into());
    set_header(h, "vary", &vals.join(", "));
}

impl Cors {
    fn allow_explicit_origin(&self, h: &mut HeaderMap, origin: &str) {
        set_header(h, "access-control-allow-origin", origin);
        if self.v17 {
            add_vary_origin(h);
        } else {
            // MutableHeaders.add_vary_header: the first existing value only
            let v = match h.get("vary") {
                Some(e) => format!("{}, Origin", String::from_utf8_lossy(e.as_bytes())),
                None => "Origin".into(),
            };
            set_header(h, "vary", &v);
        }
    }
}
