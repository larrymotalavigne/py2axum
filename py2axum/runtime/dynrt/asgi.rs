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

/// The paths left to the Python application (`--python-side`): relayed to `PY2AXUM_PYTHON_URL` when set.
pub fn set_python_side(paths: &[&'static str]) {
    let _ = PYTHON_SIDE.set(paths.to_vec());
}

/// a route pattern (`/items/{id}`, `{p:path}`) or a mount prefix matching the request path
fn python_side(path: &str) -> bool {
    let Some(pats) = PYTHON_SIDE.get() else { return false };
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    pats.iter().any(|p| {
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

/// relays the request to the Python application, streaming its response back
async fn proxy(upstream: &str, req: axum::extract::Request) -> Response {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let http = HTTP.get_or_init(|| reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_gzip().no_deflate().build().unwrap());
    let (parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let url = format!("{}{}", upstream.trim_end_matches('/'), pq);
    let hop = ["connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization", "proxy-authenticate"];
    let mut rb = http.request(parts.method.clone(), &url);
    for (k, v) in parts.headers.iter() {
        if !hop.contains(&k.as_str()) {
            rb = rb.header(k, v);
        }
    }
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return Response::builder().status(400).body(Body::empty()).unwrap(),
    };
    match rb.body(body).send().await {
        Ok(r) => {
            let mut out = Response::builder().status(r.status().as_u16());
            for (k, v) in r.headers().iter() {
                if !hop.contains(&k.as_str()) {
                    out = out.header(k, v);
                }
            }
            out.body(Body::from_stream(r.bytes_stream())).unwrap_or_else(|_| plain_500())
        }
        Err(e) => {
            eprintln!("ERROR:py2axum:python-side upstream {url}: {e}");
            Response::builder().status(502).header("content-type", "text/plain; charset=utf-8").body(Body::from("Bad Gateway")).unwrap()
        }
    }
}

pub async fn app(app: Arc<super::AppState>, req: axum::extract::Request, routes: &'static [RouteDef], stack_fn: StackFn) -> Response {
    if python_side(req.uri().path()) {
        if let Ok(up) = std::env::var("PY2AXUM_PYTHON_URL") {
            return proxy(&up, req).await;
        }
    }
    let client = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|c| (c.0.ip().to_canonical().to_string(), c.0.port()));
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return Response::builder().status(400).body(Body::empty()).unwrap(),
    };
    let mut cell = web::ReqCell::from_parts(parts.method.as_str(), &parts.uri, &parts.headers, vec![], bytes);
    cell.client = client;
    let stack = match STACK
        .get_or_try_init(|| async move {
            let rc = super::root_cx();
            stack_fn(&rc).await.map(Arc::new)
        })
        .await
    {
        Ok(s) => s.clone(),
        Err(e) => {
            log_exc(&e);
            return plain_500();
        }
    };
    let cx: Cx = Arc::new(CxInner::new(app, cell));
    match chain(&cx, &stack, 0, routes).await {
        Ok(r) => r,
        Err(e) => server_error(&cx, &stack, e).await,
    }
}

type RespFut<'a> = Pin<Box<dyn Future<Output = R<Response>> + Send + 'a>>;

fn chain<'a>(cx: &'a Cx, stack: &'a Arc<Stack>, i: usize, routes: &'static [RouteDef]) -> RespFut<'a> {
    Box::pin(async move {
        match stack.mws.get(i) {
            None => match web::route(cx, routes).await {
                Ok(r) => Ok(r),
                Err(e) => handle(cx, stack, e).await,
            },
            Some(Mw::Cors(c)) => c.call(cx, chain(cx, stack, i + 1, routes)).await,
            Some(Mw::Dispatch(f)) => {
                let next = V::native(Native::CallNext(Arc::new(Next { stack: stack.clone(), i: i + 1, routes })));
                let ret = super::methods::call_value(cx, f, vec![super::request(cx), next], vec![]).await?;
                to_response(&ret)
            }
        }
    })
}

/// `call_next` handed to a BaseHTTPMiddleware's `dispatch`.
pub struct Next {
    stack: Arc<Stack>,
    i: usize,
    routes: &'static [RouteDef],
}

/// `await call_next(request)`: the response of the inner stack; an exception escaping it is raised here.
pub async fn call_next(cx: &Cx, n: &Next) -> R {
    let r = chain(cx, &n.stack, n.i, n.routes).await?;
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
                return Ok(web::error_response(e));
            }
        }
    }
    match found {
        Some(h) => {
            let ret = super::methods::call_value(cx, &h, vec![super::request(cx), V::Exc(e)], vec![]).await?;
            to_response(&ret)
        }
        None => Err(e),
    }
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
