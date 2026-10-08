//! Starlette / FastAPI routing objects as values, for code that inspects the application at run time:
//! `request.app`, `app.router.routes` (FastAPI's docs routes, `_IncludedRouter`s with their
//! `original_router`, `APIRoute`s, the `Route`s of `app.add_route`), `route.matches(scope)` with
//! `starlette.routing.Match`, `request.scope` (a snapshot), and the app given to project functions
//! while the middleware stack is built (`configure(app)` in the factory: `app.add_middleware`,
//! `app.add_route`).
//!
//! The tree is emitted by the transpiler in registration order (FastAPI 0.141+ keeps an
//! `_IncludedRouter` per `include_router`); `matches` follows Starlette (`Route`: GET implies HEAD,
//! `PARTIAL` on another method) and FastAPI (`APIRoute` without implicit HEAD; `_IncludedRouter`: the
//! best match among its effective routes, under its own include prefix, with an empty child scope).
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};

use super::asgi::Mw;
use super::v::*;
use super::{ops, Cx};

/// A node of the route tree emitted at compile time.
pub enum Node {
    /// a Starlette `Route` FastAPI adds for its docs (`/openapi.json`, `/docs`...): GET and HEAD
    Starlette { path: &'static str, pattern: &'static str, name: &'static str },
    /// an `APIRoute`: its path within its router (the router's prefix included, not the include prefixes)
    Api { path: &'static str, pattern: &'static str, methods: &'static [&'static str], name: &'static str },
    /// an `APIWebSocketRoute` (`@app.websocket`): matches the `websocket` scope only
    Ws { path: &'static str, pattern: &'static str, name: &'static str },
    /// `include_router(router, prefix=...)`
    Included { prefix: &'static str, router: &'static RouterDef },
}

pub struct RouterDef {
    pub prefix: &'static str,
    pub routes: &'static [&'static Node],
    /// why the tree is unknown (a non-literal `FastAPI(docs_url=...)`), raised when it is inspected
    pub error: &'static str,
}

/// A `Route` added at run time (`app.add_route`), tried after the declared routes.
pub struct Added {
    pub path: String,
    pub re: regex::Regex,
    pub methods: Vec<String>,
    pub endpoint: V,
    pub name: String,
}

pub enum RObj {
    App,
    AppRouter,
    Router(&'static RouterDef),
    Node(&'static Node),
    Added(Arc<Added>),
}

static APP: OnceLock<&'static RouterDef> = OnceLock::new();
static ADDED: RwLock<Vec<Arc<Added>>> = RwLock::new(Vec::new());
/// the middlewares `app.add_middleware` registers while the stack is being built
static BUILD: Mutex<Option<Vec<Mw>>> = Mutex::new(None);

pub fn set_app(r: &'static RouterDef) {
    let _ = APP.set(r);
}

fn app_def() -> &'static RouterDef {
    static EMPTY: RouterDef = RouterDef { prefix: "", routes: &[], error: "" };
    APP.get().copied().unwrap_or(&EMPTY)
}

fn obj(o: RObj) -> V {
    V::native(Native::Routing(Arc::new(o)))
}

/// the application object
pub fn app() -> V {
    obj(RObj::App)
}

pub static CLS_MATCH: Class = Class { name: "Match", qualname: "starlette.routing.Match", bases: &[], kind: ClassKind::Enum(&ENUM_MATCH) };
pub static ENUM_MATCH: EnumDesc = EnumDesc {
    name: "Match",
    class: &CLS_MATCH,
    kind: EnumKind::Plain,
    members: &[("NONE", EV::Int(0)), ("PARTIAL", EV::Int(1)), ("FULL", EV::Int(2))],
    methods: &[],
    missing: None,
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum M {
    None = 0,
    Partial = 1,
    Full = 2,
}

fn regex(pattern: &str) -> Arc<regex::Regex> {
    static CACHE: OnceLock<RwLock<HashMap<String, Arc<regex::Regex>>>> = OnceLock::new();
    let c = CACHE.get_or_init(Default::default);
    if let Some(r) = c.read().get(pattern) {
        return r.clone();
    }
    let r = Arc::new(regex::Regex::new(pattern).expect("route pattern"));
    c.write().insert(pattern.to_string(), r.clone());
    r
}

/// Starlette's `compile_path` (`str` and `path` convertors)
pub fn compile_path(path: &str) -> R<String> {
    let mut out = String::from("^");
    let mut rest = path;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        out += &regex::escape(&rest[..i]);
        let param = &rest[i + 1..i + j];
        let (name, conv) = param.split_once(':').unwrap_or((param, "str"));
        let body = match conv {
            "str" => "[^/]+",
            "path" => ".*",
            c => return Err(Exc::type_error(format!("py2axum: path convertor `:{c}` is not supported"))),
        };
        out += &format!("(?P<{name}>{body})");
        rest = &rest[i + j + 1..];
    }
    out += &regex::escape(rest);
    out.push('$');
    Ok(out)
}

/// the path a route sees (`get_route_path`): `scope["path"]` without `scope["root_path"]`
fn route_path(scope: &V) -> R<Option<(String, String)>> {
    let get = |k: &str| -> R<Option<V>> {
        match scope {
            V::Dict(d) => Ok(d.lock().get(&Key::of(&V::str(k))?).map(|(_, v)| v.clone())),
            o => Err(Exc::type_error(format!("py2axum: matches() expects a scope dict, not {}", o.type_name()))),
        }
    };
    if get("type")?.as_ref().and_then(|t| t.as_str().map(String::from)).as_deref() != Some("http") {
        return Ok(None);
    }
    let path = match get("path")? {
        Some(V::Str(s)) => s.to_string(),
        _ => return Err(Exc::new(&KEY_ERROR, vec![V::str("path")])),
    };
    let method = match get("method")? {
        Some(V::Str(s)) => s.to_string(),
        _ => return Err(Exc::new(&KEY_ERROR, vec![V::str("method")])),
    };
    let root = match get("root_path")? {
        Some(V::Str(s)) => s.to_string(),
        _ => String::new(),
    };
    let p = if root.is_empty() {
        path
    } else if path.starts_with(&format!("{}/", root.trim_end_matches('/'))) || path == root {
        path[root.trim_end_matches('/').len()..].to_string()
    } else {
        path
    };
    Ok(Some((p, method)))
}

fn method_match(methods: &[&str], method: &str) -> M {
    if methods.contains(&method) {
        M::Full
    } else {
        M::Partial
    }
}

/// the best match of an `_IncludedRouter` (prefix included) on `path`
fn included_match(prefix: &str, r: &RouterDef, path: &str, method: &str) -> M {
    let mut best = M::None;
    for n in r.routes {
        let m = match n {
            Node::Api { pattern, methods, .. } => {
                if regex(&format!("^{}{}", regex::escape(prefix), &pattern[1..])).is_match(path) {
                    method_match(methods, method)
                } else {
                    M::None
                }
            }
            Node::Starlette { pattern, .. } => {
                if regex(&format!("^{}{}", regex::escape(prefix), &pattern[1..])).is_match(path) {
                    method_match(&["GET", "HEAD"], method)
                } else {
                    M::None
                }
            }
            // the request being matched is an HTTP one
            Node::Ws { .. } => M::None,
            Node::Included { prefix: p2, router } => {
                // under the including router's own prefix, as `APIRouter.include_router` adds them
                included_match(&format!("{prefix}{}{}", r.prefix, super::web::runtime_prefix(p2)), router, path, method)
            }
        };
        if m == M::Full {
            return M::Full;
        }
        best = best.max(m);
    }
    best
}

fn match_value(m: M) -> V {
    V::Enum(&ENUM_MATCH, m as u16)
}

fn params(re: &regex::Regex, path: &str) -> R {
    let caps = re.captures(path);
    let mut items = Vec::new();
    if let Some(c) = caps {
        for n in re.capture_names().flatten() {
            if let Some(m) = c.name(n) {
                items.push((V::str(n), V::str(m.as_str())));
            }
        }
    }
    V::dict_from(items)
}

/// `route.matches(scope)` -> `(Match, child_scope)`
fn matches(o: &Arc<RObj>, scope: &V) -> R {
    if let RObj::Node(Node::Ws { pattern, .. }) = &**o {
        // WebSocketRoute.matches: the `websocket` scope only, FULL on the path
        let d = |k: &str| -> R<Option<V>> {
            match scope {
                V::Dict(d) => Ok(d.lock().get(&Key::of(&V::str(k))?).map(|(_, v)| v.clone())),
                o => Err(Exc::type_error(format!("py2axum: matches() expects a scope dict, not {}", o.type_name()))),
            }
        };
        if d("type")?.as_ref().and_then(|t| t.as_str().map(String::from)).as_deref() == Some("websocket") {
            let path = match d("path")? {
                Some(V::Str(s)) => s.to_string(),
                _ => return Err(Exc::new(&KEY_ERROR, vec![V::str("path")])),
            };
            let re = regex(pattern);
            if re.is_match(&path) {
                let child = V::dict_from(vec![
                    (V::str("endpoint"), V::None),
                    (V::str("path_params"), params(&re, &path)?),
                    (V::str("route"), V::native(Native::Routing(o.clone()))),
                ])?;
                return Ok(V::tuple(vec![match_value(M::Full), child]));
            }
        }
        return Ok(V::tuple(vec![match_value(M::None), V::empty_dict()]));
    }
    let Some((path, method)) = route_path(scope)? else {
        return Ok(V::tuple(vec![match_value(M::None), V::empty_dict()]));
    };
    let (m, child) = match &**o {
        RObj::Node(Node::Included { prefix, router }) => {
            (included_match(&super::web::runtime_prefix(prefix), router, &path, &method), V::empty_dict())
        }
        RObj::Node(Node::Api { pattern, methods, .. }) => {
            let re = regex(pattern);
            if re.is_match(&path) {
                let child = V::dict_from(vec![
                    (V::str("endpoint"), V::None),
                    (V::str("path_params"), params(&re, &path)?),
                    (V::str("route"), V::native(Native::Routing(o.clone()))),
                ])?;
                (method_match(methods, &method), child)
            } else {
                (M::None, V::empty_dict())
            }
        }
        RObj::Node(Node::Starlette { pattern, .. }) => {
            let re = regex(pattern);
            if re.is_match(&path) {
                let child = V::dict_from(vec![(V::str("endpoint"), V::None), (V::str("path_params"), params(&re, &path)?)])?;
                (method_match(&["GET", "HEAD"], &method), child)
            } else {
                (M::None, V::empty_dict())
            }
        }
        RObj::Added(a) => {
            if a.re.is_match(&path) {
                let child = V::dict_from(vec![(V::str("endpoint"), a.endpoint.clone()), (V::str("path_params"), params(&a.re, &path)?)])?;
                let ms: Vec<&str> = a.methods.iter().map(|s| s.as_str()).collect();
                (method_match(&ms, &method), child)
            } else {
                (M::None, V::empty_dict())
            }
        }
        _ => return Err(Exc::attr_error(format!("'{}' object has no attribute 'matches'", type_name(o)))),
    };
    Ok(V::tuple(vec![match_value(m), child]))
}

fn routes_of(r: &'static RouterDef, app: bool) -> R {
    if !r.error.is_empty() {
        return Err(Exc::type_error(r.error));
    }
    let mut out: Vec<V> = r.routes.iter().map(|n| obj(RObj::Node(n))).collect();
    if app {
        out.extend(ADDED.read().iter().map(|a| obj(RObj::Added(a.clone()))));
    }
    Ok(V::list(out))
}

fn method_set(ms: &[&str]) -> R {
    let mut items: Vec<V> = ms.iter().map(V::str).collect();
    items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    let mut m = indexmap::IndexMap::new();
    for v in items {
        m.insert(Key::of(&v)?, v);
    }
    Ok(V::Set(Arc::new(Mutex::new(m))))
}

pub fn type_name(o: &RObj) -> &'static str {
    match o {
        RObj::App => "FastAPI",
        RObj::AppRouter | RObj::Router(_) => "APIRouter",
        RObj::Node(Node::Starlette { .. }) | RObj::Added(_) => "Route",
        RObj::Node(Node::Api { .. }) => "APIRoute",
        RObj::Node(Node::Ws { .. }) => "APIWebSocketRoute",
        RObj::Node(Node::Included { .. }) => "_IncludedRouter",
    }
}

/// `isinstance(v, starlette.routing.Route)` and the like
pub fn isinstance(v: &V, class: &str) -> bool {
    let V::Native(n) = v else { return false };
    let Native::Routing(o) = &**n else { return false };
    match class {
        "starlette.routing.BaseRoute" => matches!(&**o, RObj::Node(_) | RObj::Added(_)),
        "starlette.routing.Route" => matches!(&**o, RObj::Node(Node::Starlette { .. } | Node::Api { .. }) | RObj::Added(_)),
        "fastapi.routing.APIRoute" => matches!(&**o, RObj::Node(Node::Api { .. })),
        "starlette.routing.WebSocketRoute" | "fastapi.routing.APIWebSocketRoute" => matches!(&**o, RObj::Node(Node::Ws { .. })),
        "fastapi.FastAPI" | "starlette.applications.Starlette" => matches!(&**o, RObj::App),
        "fastapi.APIRouter" | "starlette.routing.Router" => matches!(&**o, RObj::AppRouter | RObj::Router(_)),
        _ => false,
    }
}

fn no_attr(o: &RObj, name: &str) -> Exc {
    Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(o)))
}

pub fn attr(o: &Arc<RObj>, name: &str) -> R {
    match (&**o, name) {
        (RObj::App, "router") => Ok(obj(RObj::AppRouter)),
        // Starlette's `app.state`: one State for the process (set in the lifespan, read by `request.app.state`)
        (RObj::App, "state") => {
            static STATE: std::sync::LazyLock<Arc<super::web::ReqCell>> = std::sync::LazyLock::new(|| Arc::new(super::web::ReqCell::empty()));
            Ok(V::native(Native::State(STATE.clone())))
        }
        // the binary has no overrides (tests set them, a server does not): always empty
        (RObj::App, "dependency_overrides") => V::dict_from(vec![]),
        (RObj::App | RObj::AppRouter, "routes") => routes_of(app_def(), true),
        (RObj::AppRouter, "prefix") => Ok(V::str("")),
        (RObj::Router(r), "routes") => routes_of(r, false),
        (RObj::Router(r), "prefix") => Ok(V::str(r.prefix)),
        (RObj::Node(Node::Included { router, .. }), "original_router") => Ok(obj(RObj::Router(router))),
        (RObj::Node(Node::Api { path, .. } | Node::Starlette { path, .. } | Node::Ws { path, .. }), "path" | "path_format") => Ok(V::str(path)),
        (RObj::Node(Node::Api { name, .. } | Node::Starlette { name, .. } | Node::Ws { name, .. }), "name") => Ok(V::str(name)),
        (RObj::Node(Node::Api { methods, .. }), "methods") => method_set(methods),
        (RObj::Node(Node::Starlette { .. }), "methods") => method_set(&["GET", "HEAD"]),
        (RObj::Added(a), "path" | "path_format") => Ok(V::str(&a.path)),
        (RObj::Added(a), "name") => Ok(V::str(&a.name)),
        (RObj::Added(a), "endpoint") => Ok(a.endpoint.clone()),
        (RObj::Added(a), "methods") => method_set(&a.methods.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
        _ => Err(no_attr(o, name)),
    }
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// `app.add_route(path, endpoint, methods=None, name=None, include_in_schema=True)`
fn add_route(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !["path", "route", "methods", "name", "include_in_schema"].contains(&k.as_str())) {
        return Err(Exc::type_error(format!("Starlette.add_route() got an unexpected keyword argument '{k}'")));
    }
    let path = args.first().or_else(|| kw(&kwargs, "path")).cloned().ok_or_else(|| Exc::type_error("Starlette.add_route() missing 2 required positional arguments: 'path' and 'route'"))?;
    let endpoint = args.get(1).or_else(|| kw(&kwargs, "route")).cloned().ok_or_else(|| Exc::type_error("Starlette.add_route() missing 1 required positional argument: 'route'"))?;
    if matches!(endpoint, V::Class(_)) {
        // Starlette runs a class as an ASGI app (`HTTPEndpoint`): not supported
        return Err(Exc::type_error("py2axum: add_route() with a class endpoint (HTTPEndpoint) is not supported"));
    }
    let path = match path {
        V::Str(s) => s.to_string(),
        o => return Err(Exc::type_error(format!("py2axum: a route path must be a str, not {}", o.type_name()))),
    };
    if !path.starts_with('/') {
        return Err(Exc::value_error("Routed paths must start with '/'"));
    }
    let mut methods: Vec<String> = match args.get(2).or_else(|| kw(&kwargs, "methods")) {
        None | Some(V::None) => vec!["GET".into()],
        Some(v) => ops::iter(v)?.iter().map(|m| ops::str_(m).map(|s| s.to_uppercase())).collect::<R<_>>()?,
    };
    if methods.iter().any(|m| m == "GET") && !methods.iter().any(|m| m == "HEAD") {
        methods.push("HEAD".into());
    }
    let name = match args.get(3).or_else(|| kw(&kwargs, "name")) {
        Some(V::Str(s)) => s.to_string(),
        _ => match getattr_name(&endpoint) {
            Some(n) => n,
            None => "endpoint".into(),
        },
    };
    let re = regex::Regex::new(&compile_path(&path)?).map_err(|e| Exc::value_error(e.to_string()))?;
    ADDED.write().push(Arc::new(Added { path, re, methods, endpoint, name }));
    Ok(V::None)
}

fn getattr_name(f: &V) -> Option<String> {
    if let V::Native(n) = f {
        if let Native::PyFn(p) = &**n {
            return p.attrs.lock().iter().find(|(k, _)| k == "__name__").and_then(|(_, v)| v.as_str().map(String::from));
        }
    }
    None
}

/// `app.add_middleware(Cls, ...)` from project code: the transpiler built the instance (the stack is being
/// built: Starlette instantiates on the first request), its bound `dispatch` is registered
pub fn add_dispatch(app: &V, dispatch: V) -> R {
    if !matches!(app, V::Native(n) if matches!(&**n, Native::Routing(o) if matches!(&**o, RObj::App))) {
        return Err(Exc::attr_error(format!("'{}' object has no attribute 'add_middleware'", app.type_name())));
    }
    match BUILD.lock().as_mut() {
        Some(b) => b.push(Mw::Dispatch(dispatch)),
        None => return Err(Exc::runtime("Cannot add middleware after an application has started")),
    }
    Ok(V::None)
}

/// the stack function starts / ends collecting `app.add_middleware` calls
pub fn begin_build() {
    *BUILD.lock() = Some(Vec::new());
    HANDLERS.lock().clear();
}

pub fn take_built() -> Vec<Mw> {
    BUILD.lock().as_mut().map(std::mem::take).unwrap_or_default()
}

/// the exception handlers a function given the application registered (in order)
pub fn take_handlers() -> Vec<(V, V)> {
    std::mem::take(&mut *HANDLERS.lock())
}

pub fn end_build() {
    *BUILD.lock() = None;
}

static HANDLERS: Mutex<Vec<(V, V)>> = Mutex::new(Vec::new());

fn building() -> R<()> {
    if BUILD.lock().is_none() {
        return Err(Exc::runtime("Cannot add middleware after an application has started"));
    }
    Ok(())
}

/// `app.add_middleware(RawAsgiClass, ...)` from project code: the transpiler built the instance with the
/// slot as its `app`
pub fn add_asgi(app: &V, inst: V, slot: Arc<super::asgi::Slot>) -> R {
    if !matches!(app, V::Native(n) if matches!(&**n, Native::Routing(o) if matches!(&**o, RObj::App))) {
        return Err(Exc::attr_error(format!("'{}' object has no attribute 'add_middleware'", app.type_name())));
    }
    building()?;
    if let Some(b) = BUILD.lock().as_mut() {
        b.push(Mw::Asgi(inst, slot));
    }
    Ok(V::None)
}

/// `@app.exception_handler(key)` in a function given the application: registers the function, returns it
fn exception_handler_deco<'a>(_cx: &'a Cx, key: V, args: Vec<V>) -> super::BoxFut<'a> {
    Box::pin(async move {
        let (args, _) = super::unpack(args);
        let [f] = &args[..] else { return Err(Exc::type_error("decorator() takes exactly one argument")) };
        HANDLERS.lock().push((key, f.clone()));
        Ok(f.clone())
    })
}

/// `@app.middleware("http")` in a function given the application
fn middleware_deco<'a>(_cx: &'a Cx, _kind: V, args: Vec<V>) -> super::BoxFut<'a> {
    Box::pin(async move {
        let (args, _) = super::unpack(args);
        let [f] = &args[..] else { return Err(Exc::type_error("decorator() takes exactly one argument")) };
        building()?;
        if let Some(b) = BUILD.lock().as_mut() {
            b.push(Mw::Dispatch(f.clone()));
        }
        Ok(f.clone())
    })
}


pub async fn method(cx: &Cx, o: &Arc<RObj>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    match (&**o, name) {
        (RObj::Node(_) | RObj::Added(_), "matches") => {
            let [scope] = &args[..] else {
                return Err(Exc::type_error(format!("{}.matches() takes 2 positional arguments but {} were given", type_name(o), args.len() + 1)));
            };
            matches(o, scope)
        }
        (RObj::App | RObj::AppRouter, "add_route") => add_route(args, kwargs),
        (RObj::App, "exception_handler") => {
            let [key] = &args[..] else {
                return Err(Exc::type_error("Starlette.exception_handler() takes 2 positional arguments"));
            };
            Ok(V::native(Native::Bound(exception_handler_deco, key.clone())))
        }
        (RObj::App, "add_exception_handler") => {
            let (key, f) = match (&args[..], kw(&kwargs, "exc_class_or_status_code"), kw(&kwargs, "handler")) {
                ([k, f], None, None) => (k.clone(), f.clone()),
                ([k], None, Some(f)) => (k.clone(), f.clone()),
                ([], Some(k), Some(f)) => (k.clone(), f.clone()),
                _ => return Err(Exc::type_error("Starlette.add_exception_handler() takes 3 positional arguments")),
            };
            HANDLERS.lock().push((key, f));
            Ok(V::None)
        }
        (RObj::App, "middleware") => {
            if !matches!(&args[..], [V::Str(s)] if &**s == "http") {
                return Err(Exc::type_error("py2axum: only @app.middleware(\"http\") is supported"));
            }
            Ok(V::native(Native::Bound(middleware_deco, args[0].clone())))
        }
        (RObj::App, "add_middleware") => {
            let _ = (cx, &kwargs);
            Err(Exc::type_error("py2axum: app.add_middleware() of a class known only at run time is not supported"))
        }
        _ => Err(no_attr(o, name)),
    }
}

/// a route added by `app.add_route` matching `path`: (route, full match)
pub fn added_match(path: &str, method: &str) -> Option<(Arc<Added>, bool)> {
    let mut partial = None;
    for a in ADDED.read().iter() {
        if a.re.is_match(path) {
            if a.methods.iter().any(|m| m == method) {
                return Some((a.clone(), true));
            }
            partial.get_or_insert_with(|| a.clone());
        }
    }
    partial.map(|a| (a, false))
}

pub fn added_is_match(path: &str) -> bool {
    ADDED.read().iter().any(|a| a.re.is_match(path))
}

/// `request.scope`: a snapshot of the ASGI scope (the keys the binary has; `endpoint` is None)
pub fn scope(cx: &Cx) -> R {
    // under a raw middleware: the scope it handed down (Starlette's router adds its keys to the same dict)
    let passed = cx.asgi_scope.lock().clone();
    if let Some(s @ V::Dict(_)) = passed {
        if let (Some(node), V::Dict(m)) = (*cx.req.route.lock(), &s) {
            let pp: Vec<(V, V)> = cx.req.path_params.lock().iter().map(|(k, v)| (V::str(k), V::str(v))).collect();
            let pp = V::dict_from(pp)?;
            let mut m = m.lock();
            if !m.contains_key(&Key::Str(Arc::from("endpoint"))) {
                m.insert(Key::Str(Arc::from("endpoint")), (V::str("endpoint"), V::None));
            }
            m.insert(Key::Str(Arc::from("path_params")), (V::str("path_params"), pp));
            if matches!(node, Node::Api { .. }) {
                m.insert(Key::Str(Arc::from("route")), (V::str("route"), obj(RObj::Node(node))));
            }
        }
        return Ok(s);
    }
    let r = &cx.req;
    let (path, query) = (super::web::unquote(&r.path), r.raw_query.clone());
    let headers: Vec<V> = r.headers.iter().map(|(k, v)| V::tuple(vec![V::Bytes(Arc::from(k.to_lowercase().as_bytes())), V::Bytes(Arc::from(v.as_bytes()))])).collect();
    let mut items = vec![
        (V::str("type"), V::str("http")),
        (V::str("http_version"), V::str("1.1")),
        (V::str("scheme"), V::str("http")),
        (V::str("method"), V::str(&r.method)),
        (V::str("root_path"), V::str("")),
        (V::str("path"), V::str(&path)),
        (V::str("raw_path"), V::Bytes(Arc::from(r.path.as_bytes()))),
        (V::str("query_string"), V::Bytes(Arc::from(query.as_bytes()))),
        (V::str("headers"), V::list(headers)),
        (V::str("client"), r.client.as_ref().map(|(h, p)| V::tuple(vec![V::str(h), V::Int(*p as i64)])).unwrap_or(V::None)),
        (V::str("app"), app()),
    ];
    // what the router adds once it has run
    if let Some(node) = *r.route.lock() {
        items.push((V::str("endpoint"), V::None));
        let pp: Vec<(V, V)> = r.path_params.lock().iter().map(|(k, v)| (V::str(k), V::str(v))).collect();
        items.push((V::str("path_params"), V::dict_from(pp)?));
        if matches!(node, Node::Api { .. }) {
            items.push((V::str("route"), obj(RObj::Node(node))));
        }
    }
    V::dict_from(items)
}
