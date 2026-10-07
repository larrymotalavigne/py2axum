//! py2axum dynamic runtime: compiled Python operates on `V` values with CPython semantics.
#![allow(dead_code, unused_imports, unused_variables, clippy::all)]

pub mod aio;
pub mod asgi;
pub mod auth;
pub mod decimal;
pub mod dt;
pub mod http;
pub mod ini;
pub mod email;
pub mod stdlib;
pub mod thread;
pub mod types;
pub mod sysmon;
pub mod mail;
pub mod resp;
pub mod pathio;
pub mod pickle;
pub mod rds;
pub mod files;
pub mod fernet;
pub mod google;
pub mod itsd;
pub mod jose;
pub mod libs;
pub mod methods;
pub mod ops;
pub mod orm;
pub mod pyd;
pub mod v;
pub mod web;
pub mod webpush;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use v::{Exc, Key, Native, R, V};

pub type BoxFut<'a> = Pin<Box<dyn Future<Output = R> + Send + 'a>>;
pub type FnVal = Arc<dyn for<'a> Fn(&'a Cx, Vec<V>) -> BoxFut<'a> + Send + Sync>;
/// A callable taking keyword arguments (CPython binding done by the callee).
pub type KwFn = Arc<dyn for<'a> Fn(&'a Cx, Vec<V>, Vec<(String, V)>) -> BoxFut<'a> + Send + Sync>;

/// Process-wide state.
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub expire_on_commit: bool,
    pub autoflush: bool,
    /// the session dependency commits after the endpoint (`yield s; await s.commit()`)
    pub commit_after: bool,
}

/// Per-request state (or the process-level context of module globals).
pub struct CxInner {
    pub app: Arc<AppState>,
    pub req: Arc<web::ReqCell>,
    pub resp: Arc<web::RespCell>,
    pub session: tokio::sync::OnceCell<V>,
    pub deps: web::DepCache,
    /// generator dependencies waiting for their code after `yield` (run in reverse order)
    pub teardowns: parking_lot::Mutex<Vec<web::DepTeardown>>,
    /// the request's BackgroundTasks, created on first use
    pub background: std::sync::OnceLock<V>,
}

pub type Cx = Arc<CxInner>;

impl CxInner {
    pub fn new(app: Arc<AppState>, req: web::ReqCell) -> CxInner {
        CxInner {
            app,
            req: Arc::new(req),
            resp: Arc::new(web::RespCell::default()),
            session: tokio::sync::OnceCell::new(),
            deps: parking_lot::Mutex::new(std::collections::HashMap::new()),
            teardowns: parking_lot::Mutex::new(Vec::new()),
            background: std::sync::OnceLock::new(),
        }
    }
}

/// The request's `AsyncSession` (FastAPI caches the dependency per request).
pub async fn session(cx: &Cx) -> R {
    Ok(cx
        .session
        .get_or_init(|| async { V::Session(orm::Session::new(cx.app.pool.clone(), cx.app.expire_on_commit, cx.app.autoflush, Arc::downgrade(cx))) })
        .await
        .clone())
}

pub fn request(cx: &Cx) -> V {
    V::native(Native::Request(cx.req.clone()))
}

pub fn response(cx: &Cx) -> V {
    V::native(Native::Response(cx.resp.clone()))
}

/// A module-level global, evaluated once on first use (like a module import side effect).
/// A module-level variable: initialised on first use, rebound by functions that declare it `global`.
pub struct Global {
    cell: tokio::sync::OnceCell<parking_lot::Mutex<V>>,
}

impl Global {
    pub const fn new() -> Global {
        Global { cell: tokio::sync::OnceCell::const_new() }
    }
    pub async fn get<'a, F>(&'a self, cx: &'a Cx, init: F) -> R
    where
        F: FnOnce(&'a Cx) -> BoxFut<'a>,
    {
        let cell = self.cell.get_or_try_init(|| async move { init(cx).await.map(parking_lot::Mutex::new) }).await?;
        Ok(cell.lock().clone())
    }
    /// `global x; x = v` (the getter runs first, so the initial value exists as in Python)
    pub fn set(&self, v: V) {
        *self.cell.get().expect("global read before rebinding").lock() = v;
    }
}

/// Inside `try:` bodies, a failing expression jumps to the handlers instead of returning.
#[macro_export]
macro_rules! tri {
    ($e:expr, $lbl:lifetime, $slot:ident) => {
        match $e {
            Ok(v) => v,
            Err(e) => {
                $slot = Some(e);
                break $lbl;
            }
        }
    };
}

/// `http.HTTPStatus(code).phrase` (CPython >= 3.13 names: 413 Content Too Large, 422 Unprocessable Content...)
pub fn status_phrase(code: u16) -> Option<&'static str> {
    Some(match code {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a Teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => return None,
    })
}

pub fn func(f: FnVal) -> V {
    V::native(Native::Func(f))
}

/// A project function as a value (`def` of the project, decorated or nested): CPython's function
/// object with its writable attributes (`__name__`, `__wrapped__`... — what `functools.wraps` copies).
pub fn pyfn(module: &str, qualname: &str, doc: Option<&str>, is_async: bool, call: KwFn) -> V {
    let name = qualname.rsplit('.').next().unwrap_or(qualname);
    let attrs = vec![
        ("__module__".to_string(), V::str(module)),
        ("__name__".to_string(), V::str(name)),
        ("__qualname__".to_string(), V::str(qualname)),
        ("__doc__".to_string(), doc.map(V::str).unwrap_or(V::None)),
    ];
    V::native(Native::PyFn(v::PyFn { call, is_async, attrs: parking_lot::Mutex::new(attrs) }))
}

/// `inspect.iscoroutinefunction(obj)` (after `functools.partial`/`__wrapped__`-free unwrapping: CPython
/// only follows partials and methods)
pub fn iscoroutinefunction(args: &[V], kwargs: &[(String, V)]) -> R {
    let ([f], true) = (args, kwargs.is_empty()) else {
        return Err(Exc::type_error("iscoroutinefunction() takes 1 positional argument"));
    };
    Ok(V::Bool(match f {
        V::Native(n) => match &**n {
            Native::PyFn(p) => p.is_async,
            Native::Func(_) => false,
            Native::Bound(..) => return Err(Exc::type_error("py2axum: inspect.iscoroutinefunction() of a bound method is not supported")),
            _ => false,
        },
        _ => false,
    }))
}

/// a function object with its `__annotations__` (type values, see `types`)
pub fn with_annotations(f: V, items: Vec<(V, V)>) -> R {
    methods::setattr(&f, "__annotations__", V::dict_from(items)?)?;
    Ok(f)
}

/// `functools.wraps(wrapped)`: the decorator copying `WRAPPER_ASSIGNMENTS`, updating `__dict__` and
/// setting `__wrapped__` on the wrapper it receives (attributes the wrapped object lacks are skipped).
pub fn functools_wraps(args: &[V], kwargs: &[(String, V)]) -> R {
    if !kwargs.is_empty() {
        return Err(Exc::type_error("py2axum: functools.wraps(assigned=, updated=) is not supported"));
    }
    let [wrapped] = args else {
        return Err(Exc::type_error(format!("wraps() takes 1 positional argument but {} were given", args.len())));
    };
    let wrapped = wrapped.clone();
    Ok(func(Arc::new(move |cx: &Cx, args: Vec<V>| -> BoxFut<'_> {
        let wrapped = wrapped.clone();
        Box::pin(async move {
            let [wrapper] = &args[..] else {
                return Err(Exc::type_error("update_wrapper() missing 1 required positional argument: 'wrapper'"));
            };
            for a in ["__module__", "__name__", "__qualname__", "__doc__", "__annotations__"] {
                match methods::getattr(cx, &wrapped, a).await {
                    Ok(v) => methods::setattr(wrapper, a, v)?,
                    Err(e) if e.isinstance(&v::ATTRIBUTE_ERROR) => {}
                    Err(e) => return Err(e),
                }
            }
            if let V::Native(n) = &wrapped {
                if let Native::PyFn(f) = &**n {
                    let extra: Vec<(String, V)> = f.attrs.lock().iter().filter(|(k, _)| !v::PyFn::SLOTS.contains(&k.as_str())).cloned().collect();
                    for (k, v) in extra {
                        methods::setattr(wrapper, &k, v)?;
                    }
                }
            }
            methods::setattr(wrapper, "__wrapped__", wrapped.clone())?;
            Ok(wrapper.clone())
        })
    })))
}

/// `HTTPException(status_code, detail, headers)` as an exception value.
pub fn http_exc(status: &V, detail: V, headers: &V) -> R {
    let code = match status {
        V::Int(i) => *i as u16,
        other => return Err(Exc::type_error(format!("status_code must be an int, got {}", other.type_name()))),
    };
    let detail = if detail.is_none() {
        let phrase = status_phrase(code).unwrap_or("");
        V::str(phrase)
    } else {
        detail
    };
    Ok(V::Exc(Exc::http(code, detail, dyn_headers(headers)?)))
}

pub fn dyn_headers(h: &V) -> R<Vec<(String, String)>> {
    match h {
        V::None => Ok(vec![]),
        V::Dict(d) => d.lock().values().map(|(k, v)| Ok((ops::str_(k)?, ops::str_(v)?))).collect(),
        other => Err(Exc::type_error(format!("headers must be a dict, got {}", other.type_name()))),
    }
}

/// `raise <value>`: an exception instance or class.
pub fn raise_v(v: &V) -> Exc {
    match v {
        V::Exc(e) => e.clone(),
        V::Class(c) => Exc::new(c, vec![]),
        other => Exc::type_error(format!("exceptions must derive from BaseException, not {}", other.type_name())),
    }
}

pub fn streaming(content: V, media: &V, status: &V, headers: &V) -> R {
    let media = match media {
        V::None => None,
        m => Some(ops::str_(m)?),
    };
    let status = match status {
        V::Int(i) => *i as u16,
        _ => 200,
    };
    web::streaming_response(content, media, status, dyn_headers(headers)?)
}

/// Keyword arguments with `**mapping` expansions, in call order.
pub fn kwargs(fixed: Vec<(String, V)>, spreads: Vec<V>) -> R<Vec<(String, V)>> {
    let mut out = fixed;
    for s in spreads {
        match &s {
            V::Dict(d) => {
                for (k, v) in d.lock().values() {
                    let k = ops::str_(k)?;
                    if out.iter().any(|(x, _)| *x == k) {
                        return Err(Exc::type_error(format!("got multiple values for keyword argument '{k}'")));
                    }
                    out.push((k, v.clone()));
                }
            }
            other => return Err(Exc::type_error(format!("argument after ** must be a mapping, not {}", other.type_name()))),
        }
    }
    Ok(out)
}

/// Positional arguments with `*iterable` expansions.
pub fn args(parts: Vec<(bool, V)>) -> R<Vec<V>> {
    let mut out = Vec::new();
    for (star, v) in parts {
        if star {
            out.extend(ops::iter(&v)?);
        } else {
            out.push(v);
        }
    }
    Ok(out)
}

pub fn kwargs_dict(kw: Vec<(String, V)>) -> R {
    V::dict_from(kw.into_iter().map(|(k, v)| (V::str(k), v)).collect())
}

/// pydantic-settings `BaseSettings()`: fields read from the environment (case-insensitive).
pub async fn settings(cx: &Cx, desc: &'static pyd::SchemaDesc, prefix: &str, kw: Vec<(String, V)>) -> R {
    let env: Vec<(String, String)> = std::env::vars().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    let mut items = Vec::new();
    for f in desc.fields {
        if let Some((_, v)) = kw.iter().find(|(k, _)| k == f.name) {
            items.push((V::str(f.name), v.clone()));
            continue;
        }
        let key = format!("{}{}", prefix.to_ascii_lowercase(), f.env.unwrap_or(f.name).to_ascii_lowercase());
        if let Some((_, raw)) = env.iter().find(|(k, _)| *k == key) {
            let complex = matches!(f.td, pyd::TD::Dict(_) | pyd::TD::List(_) | pyd::TD::Set(_) | pyd::TD::Tuple(_) | pyd::TD::Schema(_));
            let v = if complex { pyd::loads(raw)? } else { V::str(raw) };
            items.push((V::str(f.name), v));
            continue;
        }
        // pydantic-settings validates the defaults too (validate_default=True)
        match &f.default {
            pyd::Dflt::Required => {}
            pyd::Dflt::Value(g) | pyd::Dflt::Factory(g) => items.push((V::str(f.name), g())),
            pyd::Dflt::Dyn(g) => items.push((V::str(f.name), g(cx, V::None, vec![]).await?)),
        }
    }
    pyd::construct(cx, desc, V::dict_from(items)?).await
}

/// `x += y`: lists are extended in place (aliases see it), everything else is `x = x + y`.
pub fn iadd(a: &V, b: &V) -> R {
    if let V::List(l) = a {
        let items = ops::iter(b)?;
        l.lock().extend(items);
        return Ok(a.clone());
    }
    ops::add(a, b)
}

static ROOT: std::sync::OnceLock<Arc<AppState>> = std::sync::OnceLock::new();

/// Called once by main(): the process-level context used outside requests.
pub fn set_root(app: Arc<AppState>) {
    let _ = ROOT.set(app);
}

static PYTHON: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

/// The CPython version the project runs on (messages that changed between versions).
pub fn set_python(major: u32, minor: u32) {
    let _ = PYTHON.set((major, minor));
}

pub fn python() -> (u32, u32) {
    PYTHON.get().copied().unwrap_or((3, 12))
}

static PYDANTIC: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The project's locked pydantic version, "major.minor" (the documentation URL of each error).
pub fn set_pydantic(version: &str) {
    let _ = PYDANTIC.set(version.to_string());
}

pub fn pydantic() -> &'static str {
    PYDANTIC.get().map(|s| s.as_str()).unwrap_or("2.13")
}

pub fn root_cx() -> Cx {
    let app = ROOT.get().expect("py2axum runtime not initialised").clone();
    Arc::new(CxInner::new(app, web::ReqCell::empty()))
}


// ---------------------------------------------------------------- argument binding

/// Keyword arguments travel to a compiled method as a trailing `Native::Kwargs`.
pub fn pack(mut args: Vec<V>, kwargs: Vec<(String, V)>) -> Vec<V> {
    if !kwargs.is_empty() {
        args.push(V::native(Native::Kwargs(kwargs)));
    }
    args
}

pub fn unpack(mut args: Vec<V>) -> (Vec<V>, Vec<(String, V)>) {
    if let Some(V::Native(n)) = args.last() {
        if let Native::Kwargs(kw) = &**n {
            let kw = kw.clone();
            args.pop();
            return (args, kw);
        }
    }
    (args, Vec::new())
}

pub const P_POS: u8 = 0;
pub const P_VARARG: u8 = 1;
pub const P_KWONLY: u8 = 2;
pub const P_KWARG: u8 = 3;

fn names_list(names: &[&str]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    match q.len() {
        1 => q[0].clone(),
        2 => format!("{} and {}", q[0], q[1]),
        k => format!("{}, and {}", q[..k - 1].join(", "), q[k - 1]),
    }
}

/// CPython's argument binding for a compiled function `qual` (params: name, kind, has a default;
/// `lead` = 1 when a self/cls precedes them): one slot per parameter, None = use the default.
pub fn bind_params(qual: &str, lead: usize, args: Vec<V>, kwargs: Vec<(String, V)>, params: &[(&str, u8, bool)]) -> R<Vec<Option<V>>> {
    let mut slots: Vec<Option<V>> = vec![None; params.len()];
    let pos: Vec<usize> = (0..params.len()).filter(|i| params[*i].1 == P_POS).collect();
    let vararg = params.iter().position(|p| p.1 == P_VARARG);
    let kwarg = params.iter().position(|p| p.1 == P_KWARG);
    let given = args.len();
    let mut extra = Vec::new();
    for (k, a) in args.into_iter().enumerate() {
        match pos.get(k) {
            Some(i) => slots[*i] = Some(a),
            None => extra.push(a),
        }
    }
    if !extra.is_empty() {
        match vararg {
            Some(i) => slots[i] = Some(V::tuple(std::mem::take(&mut extra))),
            None => {
                let req = pos.iter().filter(|i| !params[**i].2).count() + lead;
                let total = pos.len() + lead;
                let takes = if req == total { format!("{total}") } else { format!("from {req} to {total}") };
                let s = if total == 1 { "" } else { "s" };
                let were = if given + lead == 1 { "was" } else { "were" };
                return Err(Exc::type_error(format!("{qual} takes {takes} positional argument{s} but {} {were} given", given + lead)));
            }
        }
    } else if let Some(i) = vararg {
        slots[i] = Some(V::tuple(vec![]));
    }
    let mut rest = Vec::new();
    for (k, v) in kwargs {
        match params.iter().position(|p| p.0 == k && (p.1 == P_POS || p.1 == P_KWONLY)) {
            Some(i) if slots[i].is_some() => return Err(Exc::type_error(format!("{qual} got multiple values for argument '{k}'"))),
            Some(i) => slots[i] = Some(v),
            None if kwarg.is_some() => rest.push((V::str(&k), v)),
            None => return Err(Exc::type_error(format!("{qual} got an unexpected keyword argument '{k}'"))),
        }
    }
    if let Some(i) = kwarg {
        slots[i] = Some(V::dict_from(rest)?);
    }
    for (kind, word) in [(P_POS, "positional"), (P_KWONLY, "keyword-only")] {
        let missing: Vec<&str> = params.iter().zip(&slots).filter(|(p, s)| p.1 == kind && !p.2 && s.is_none()).map(|(p, _)| p.0).collect();
        if !missing.is_empty() {
            let s = if missing.len() == 1 { "" } else { "s" };
            return Err(Exc::type_error(format!("{qual} missing {} required {word} argument{s}: {}", missing.len(), names_list(&missing))));
        }
    }
    Ok(slots)
}

/// `type(obj)` for a classmethod called through an instance.
pub fn class_of(v: &V) -> V {
    match v {
        V::Inst(i) => V::Class(i.desc.class),
        V::Obj(o) => V::Class(o.desc.class),
        V::Enum(e, _) => V::Class(e.class),
        other => other.clone(),
    }
}


/// `super().__init__(*args)` reaching BaseException: rebinds `exc.args`.
/// `super().__init__(status_code, detail=None, headers=None)` in a project HTTPException subclass
pub fn exc_http_init(slf: &V, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let names = ["status_code", "detail", "headers"];
    let mut vals: Vec<Option<V>> = vec![None; 3];
    if args.len() > 3 {
        return Err(Exc::type_error(format!("HTTPException.__init__() takes from 2 to 4 positional arguments but {} were given", args.len() + 1)));
    }
    for (i, a) in args.into_iter().enumerate() {
        vals[i] = Some(a);
    }
    for (k, v) in kwargs {
        let i = names.iter().position(|n| *n == k).ok_or_else(|| Exc::type_error(format!("HTTPException.__init__() got an unexpected keyword argument '{k}'")))?;
        vals[i] = Some(v);
    }
    let status = vals[0].clone().ok_or_else(|| Exc::type_error("HTTPException.__init__() missing 1 required positional argument: 'status_code'"))?;
    let V::Exc(made) = http_exc(&status, vals[1].clone().unwrap_or(V::None), &vals[2].clone().unwrap_or(V::None))? else { unreachable!() };
    match slf {
        V::Exc(e) => {
            *e.0.http_late.lock() = made.http_info();
            Ok(V::None)
        }
        other => Err(Exc::type_error(format!("py2axum: super().__init__() on a {}", other.type_name()))),
    }
}

pub fn exc_set_args(slf: &V, args: Vec<V>) -> R {
    match slf {
        V::Exc(e) => {
            *e.0.new_args.lock() = Some(args);
            Ok(V::None)
        }
        other => Err(Exc::type_error(format!("py2axum: super().__init__() on a {}", other.type_name()))),
    }
}
