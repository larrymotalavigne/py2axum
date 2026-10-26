//! FastAPI/Starlette request plumbing: request context, parameter extraction with the same 422
//! errors, response serialisation, exception -> response, async generators and streaming, queues.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use indexmap::IndexMap;
use parking_lot::Mutex;

use super::pyd::{self, ErrDetail, TD};
use super::v::*;
use super::{ops, Cx, CxInner};

// ---------------------------------------------------------------- request / response state

pub struct ReqCell {
    pub method: String,
    pub path: String,
    pub raw_query: String,
    pub headers: Vec<(String, String)>,
    /// set by the router, once the middlewares have run
    pub path_params: Mutex<Vec<(String, String)>>,
    /// the declared route the router chose (`scope["route"]`)
    pub route: Mutex<Option<&'static super::routing::Node>>,
    pub query: Vec<(String, String)>,
    /// the peer address (`request.client`), X-Forwarded-For applied (uvicorn's --proxy-headers)
    pub client: Option<(String, u16)>,
    /// "http", "https" (X-Forwarded-Proto from a trusted proxy)
    pub scheme: String,
    /// "1.1", "1.0", "2" (the access log line)
    pub http_version: &'static str,
    pub body: Bytes,
    pub state: Mutex<IndexMap<String, V>>,
    pub disconnected: AtomicBool,
    /// the body a custom route class's Request subclass gives (its `body()` override, e.g. decompressed), what
    /// the route's handler parses while it runs
    pub body_override: Mutex<Option<Bytes>>,
    /// that Request subclass's instance: the endpoint's `request` parameter while the handler runs
    pub custom_request: Mutex<Option<V>>,
    /// Starlette's `_body` of each Request object of the request (REQ_ROUTE, REQ_EXC, REQ_SERVER, REQ_MW + layer):
    /// set by its `body()`/`json()`, or assigned in a BaseHTTPMiddleware's dispatch (what the inner app then reads)
    pub bodies: Mutex<Vec<(u32, Bytes)>>,
    /// `request.form()` streamed the route's Request object's body (no `_body`)
    pub form_read: AtomicBool,
}

/// the Request objects Starlette builds for one HTTP request: the route's (`request_response`), the one
/// ExceptionMiddleware hands its handlers when no route ran (404, 405), ServerErrorMiddleware's `Request(scope)`
/// (no receive channel), and one `_CachedRequest` per BaseHTTPMiddleware (REQ_MW + its index in the stack)
pub const REQ_ROUTE: u32 = 1;
pub const REQ_EXC: u32 = 2;
pub const REQ_SERVER: u32 = 3;
/// a `Request(scope)` built by translated code without a receive channel (a raw ASGI middleware reading headers)
pub const REQ_NORECV: u32 = 4;
pub const REQ_MW: u32 = 100;

impl ReqCell {
    /// the body the route parses (`body_override` while a custom request's handler runs)
    pub fn body_bytes(&self) -> Bytes {
        self.body_override.lock().clone().unwrap_or_else(|| self.incoming(REQ_ROUTE))
    }

    /// what the receive channel of a Request object yields: the `_body` of the closest BaseHTTPMiddleware around
    /// it that has one (its `_CachedRequest.wrapped_receive` hands that on), else the client's body
    pub fn incoming(&self, obj: u32) -> Bytes {
        // the middlewares around `obj`: all of them for the route's and the handlers' objects, the outer ones for a
        // middleware's (a lower index is further out)
        let around = |o: u32| o >= REQ_MW && (obj < REQ_MW || o < obj);
        let b = self.bodies.lock();
        b.iter().filter(|(o, _)| around(*o)).max_by_key(|(o, _)| *o).map(|(_, x)| x.clone()).unwrap_or_else(|| self.body.clone())
    }

    /// `request._body` (an AttributeError until it is read or assigned)
    pub fn cached_body(&self, obj: u32) -> Option<Bytes> {
        self.bodies.lock().iter().find(|(o, _)| *o == obj).map(|(_, x)| x.clone())
    }

    pub fn set_body(&self, obj: u32, body: Bytes) {
        let mut b = self.bodies.lock();
        match b.iter_mut().find(|(o, _)| *o == obj) {
            Some(slot) => slot.1 = body,
            None => b.push((obj, body)),
        }
    }

    /// `await request.body()` on a Request object: its `_body`, read once from its receive channel
    pub fn read_body(&self, obj: u32) -> R<Bytes> {
        if let Some(b) = self.cached_body(obj) {
            return Ok(b);
        }
        if obj == REQ_ROUTE && self.form_read.load(Ordering::Relaxed) {
            return Err(Exc::runtime("Stream consumed"));
        }
        if obj == REQ_SERVER || obj == REQ_NORECV {
            // ServerErrorMiddleware's `Request(scope)`: Starlette's `empty_receive`
            return Err(Exc::runtime("Receive channel has not been made available"));
        }
        let b = if obj == REQ_ROUTE { self.body_bytes() } else { self.incoming(obj) };
        self.set_body(obj, b.clone());
        Ok(b)
    }

    /// Starlette's `Request.stream()` on a Request object: its `_body` then b"" when read already; else what its
    /// receive channel yields (one chunk here: the body is whole), the stream then consumed (`body()` raises)
    pub fn stream_items(&self, obj: u32) -> R<Vec<V>> {
        let bytes = |b: &[u8]| V::Bytes(Arc::from(b));
        if let Some(b) = self.cached_body(obj) {
            return Ok(vec![bytes(&b), bytes(b"")]);
        }
        if obj == REQ_SERVER || obj == REQ_NORECV {
            return Err(Exc::runtime("Receive channel has not been made available"));
        }
        if obj == REQ_ROUTE && self.form_read.swap(true, Ordering::Relaxed) {
            return Err(Exc::runtime("Stream consumed"));
        }
        let b = if obj == REQ_ROUTE { self.body_bytes() } else { self.incoming(obj) };
        let mut out = Vec::new();
        if !b.is_empty() {
            out.push(bytes(&b));
        }
        out.push(bytes(b""));
        Ok(out)
    }

    pub fn empty() -> ReqCell {
        ReqCell {
            method: "GET".into(),
            path: "/".into(),
            raw_query: String::new(),
            headers: vec![],
            path_params: Mutex::new(vec![]),
            route: Mutex::new(None),
            query: vec![],
            client: None,
            scheme: "http".into(),
            http_version: "1.1",
            body: Bytes::new(),
            state: Mutex::new(IndexMap::new()),
            disconnected: AtomicBool::new(false),
            body_override: Mutex::new(None),
            custom_request: Mutex::new(None),
            bodies: Mutex::new(vec![]),
            form_read: AtomicBool::new(false),
        }
    }
    pub fn from_parts(method: &str, uri: &axum::http::Uri, headers: &HeaderMap, path_params: Vec<(String, String)>, body: Bytes) -> ReqCell {
        let raw_query = uri.query().unwrap_or("").to_string();
        let query = form_urlencoded::parse(raw_query.as_bytes()).into_owned().collect();
        ReqCell {
            method: method.to_string(),
            path: uri.path().to_string(),
            raw_query,
            headers: headers
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect(),
            path_params: Mutex::new(path_params),
            route: Mutex::new(None),
            query,
            client: None,
            scheme: "http".into(),
            http_version: "1.1",
            body,
            state: Mutex::new(IndexMap::new()),
            disconnected: AtomicBool::new(false),
            body_override: Mutex::new(None),
            custom_request: Mutex::new(None),
            bodies: Mutex::new(vec![]),
            form_read: AtomicBool::new(false),
        }
    }
    pub fn header(&self, name: &str) -> Option<String> {
        let n = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone())
    }
}

#[derive(Default)]
pub struct RespCell {
    pub status: Mutex<Option<u16>>,
    pub headers: Mutex<Vec<(String, String)>>,
}

// ---------------------------------------------------------------- parameters

fn loc2(a: &str, b: &str) -> Vec<V> {
    vec![V::str(a), V::str(b)]
}

/// A path/query/header parameter, validated like FastAPI (`loc = [source, name]`).
pub async fn param(cx: &Cx, source: &str, name: &str, alias: &str, td: &'static TD, required: bool, default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    param_after(cx, source, name, alias, td, required, default, &[], errs).await
}

/// A parameter with `Annotated[T, AfterValidator(f), ...]`: once its value passed its type, each function in
/// turn; a ValueError or an AssertionError there is the parameter's error (its input: the raw value), like
/// pydantic's field validators (a default other than None is validated too, as FastAPI does).
pub async fn param_after(cx: &Cx, source: &str, name: &str, alias: &str, td: &'static TD, required: bool, default: fn() -> V,
                         afters: &[pyd::MethodFn], errs: &mut Vec<ErrDetail>) -> R {
    let _ = name;
    let loc = loc2(source, alias);
    // FastAPI 0.142 (`_get_multidict_value`, `_validate_value_with_model_field`): a missing parameter takes a copy
    // of its default, validated like a value unless it is None; a computed default (V::Unbound) is the route's
    let raw = match param_raw(cx, source, alias, td) {
        Some(v) => v,
        None if required => {
            errs.push(ErrDetail { kind: "missing", loc, msg: "Field required".into(), input: V::None, ctx: None, custom: false });
            return Ok(V::None);
        }
        None => match default() {
            d @ (V::None | V::Unbound) => return Ok(d),
            // FastAPI 0.148: defaults are applied at validation, validated only with `validate_default=True`
            d if super::fastapi_at_least(148) => return Ok(d),
            d => d,
        },
    };
    let n = errs.len();
    let mut cur = pyd::validate(cx, &raw, td, &loc, errs).await?.unwrap_or(V::None);
    if errs.len() > n {
        return Ok(V::None);
    }
    for f in afters {
        match f(cx, V::None, vec![cur.clone()]).await {
            Ok(v) => cur = v,
            Err(x) => {
                errs.push(pyd::validator_error(x, loc.clone(), raw.clone())?);
                return Ok(V::None);
            }
        }
    }
    Ok(cur)
}

/// the raw value of a path/query/header/cookie parameter (a list parameter: every occurrence)
fn param_raw(cx: &Cx, source: &str, alias: &str, td: &'static TD) -> Option<V> {
    // a list parameter takes every occurrence (FastAPI: `query_params.getlist`, `headers.getlist`)
    let list = matches!(td.bare(), TD::List(_)) || matches!(td, TD::Optional(t) if matches!(t.bare(), TD::List(_)));
    let raw = match source {
        "path" => cx.req.path_params.lock().iter().find(|(k, _)| k == alias).map(|(_, v)| V::str(v)),
        "header" if list => {
            let n = alias.to_ascii_lowercase();
            let vals: Vec<V> = cx.req.headers.iter().filter(|(k, _)| *k == n).map(|(_, v)| V::str(v)).collect();
            if vals.is_empty() { None } else { Some(V::list(vals)) }
        }
        "header" => cx.req.header(alias).map(V::str),
        // `Cookie()`: `request.cookies` (Starlette's parser), the parameter's name as is
        "cookie" => super::methods::cookie_map(&cx.req).get(alias).map(|v| V::str(v)),
        _ => {
            if list {
                let vals: Vec<V> = cx.req.query.iter().filter(|(k, _)| k == alias).map(|(_, v)| V::str(v)).collect();
                if vals.is_empty() { None } else { Some(V::list(vals)) }
            } else {
                cx.req.query.iter().rev().find(|(k, _)| k == alias).map(|(_, v)| V::str(v))
            }
        }
    };
    raw
}

static RESPONSE_DUMP_JSON: AtomicBool = AtomicBool::new(true);

/// FastAPI 0.130+ serializes a response_model with pydantic's `dump_json` (set by main.rs).
pub fn set_response_dump_json(on: bool) {
    RESPONSE_DUMP_JSON.store(on, Ordering::Relaxed);
}

fn response_dump_json() -> bool {
    RESPONSE_DUMP_JSON.load(Ordering::Relaxed)
}

static ROOT_PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// `FastAPI(root_path=...)` (set by main.rs): the scope's `root_path` of every request.
pub fn set_root_path(p: &str) {
    let _ = ROOT_PATH.set(p.to_string());
}

pub fn root_path() -> &'static str {
    ROOT_PATH.get().map(String::as_str).unwrap_or("")
}

/// Starlette's `get_route_path`: the path without the root_path it starts with.
pub fn route_path(path: &str) -> &str {
    let root = root_path();
    if root.is_empty() || !path.starts_with(root) {
        return path;
    }
    if path.len() == root.len() {
        return "";
    }
    if path.as_bytes()[root.len()] == b'/' { &path[root.len()..] } else { path }
}

static STARLETTE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

/// The project's locked Starlette (set by main.rs): form parsing changed between versions.
pub fn set_starlette(major: u32, minor: u32) {
    let _ = STARLETTE.set((major, minor));
}

pub fn starlette() -> (u32, u32) {
    STARLETTE.get().copied().unwrap_or((1, 7))
}

static STRICT_CONTENT_TYPE: AtomicBool = AtomicBool::new(true);

/// `FastAPI(strict_content_type=...)`: its default, True since FastAPI 0.132 (set by main.rs).
pub fn set_strict_content_type(strict: bool) {
    STRICT_CONTENT_TYPE.store(strict, Ordering::Relaxed);
}

fn strict_content_type() -> bool {
    STRICT_CONTENT_TYPE.load(Ordering::Relaxed)
}

/// FastAPI reads and JSON-decodes the body before solving dependencies (fastapi/routing.py): JSON when the
/// content type is application/json or +json, or missing or empty (unless `strict_content_type`); decoded by
/// `request.json()` (json.loads of the bytes: BOM and UTF-16/32 detected); a JSONDecodeError is a 422
/// `json_invalid` at its character position, any other error (bad UTF-8, nesting too deep) a 400.
pub fn read_body(cx: &Cx) -> R<Option<V>> {
    let bytes = &cx.req.body_bytes();
    // `await request.body()` on the route's Request object: its `_body` (an exception handler of the route sees it)
    if cx.req.cached_body(REQ_ROUTE).is_none() {
        cx.req.set_body(REQ_ROUTE, bytes.clone());
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let is_json = match cx.req.header("content-type").filter(|ct| !ct.is_empty()) {
        None => !strict_content_type(),
        Some(ct) => {
            let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            match main.split_once('/') {
                Some(("application", sub)) if !sub.contains('/') => sub == "json" || sub.ends_with("+json"),
                _ => false,
            }
        }
    };
    if !is_json {
        return Ok(Some(V::Bytes(Arc::from(&bytes[..]))));
    }
    // Starlette's `request.json()` is `json.loads(body)`: any error but JSONDecodeError (undecodable
    // bytes, RecursionError) is FastAPI's 400
    let bad = || Exc::http(400, V::str("There was an error parsing the body"), vec![]);
    // the decoded text dropped as soon as it is copied: two copies of the body alive, not three
    let chars: Vec<char> = super::libs::json_text(bytes).map_err(|_| bad())?.chars().collect();
    match super::pyjson::decode(&chars) {
        Ok(v) => Ok(Some(v)),
        Err(super::pyjson::Fail::Recursion(_)) => Err(bad()),
        // past int()'s 4300 digits CPython's decoder raises ValueError: Starlette's 400, as for a nesting too deep
        Err(super::pyjson::Fail::Overflow(t)) if t.trim_start_matches('-').len() > 4300 => Err(bad()),
        Err(super::pyjson::Fail::Overflow(t)) => Err(super::pyjson::overflow(&t)),
        Err(super::pyjson::Fail::Decode(msg, pos)) => Err(Exc::validation(
            &REQUEST_VALIDATION_ERROR,
            vec![ErrDetail {
                kind: "json_invalid",
                loc: vec![V::str("body"), V::Int(pos as i64)],
                msg: "JSON decode error".into(),
                input: V::empty_dict(),
                ctx: Some(vec![("error", V::str(msg))]), custom: false }],
        )),
    }
}

/// The single body parameter (not embedded): `loc = ["body", ...]`.
pub async fn body_param(cx: &Cx, body: &Option<V>, td: &'static TD, required: bool, default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    match body {
        // FastAPI 0.151: a JSON `null` body is a value, validated
        Some(V::None) if super::fastapi_at_least(151) => Ok(pyd::validate(cx, &V::None, td, &[V::str("body")], errs).await?.unwrap_or(V::None)),
        // a JSON `null` body is no body (FastAPI's _validate_value_with_model_field): missing, or the default
        None | Some(V::None) => {
            if required {
                errs.push(ErrDetail { kind: "missing", loc: vec![V::str("body")], msg: "Field required".into(), input: V::None, ctx: None, custom: false });
                Ok(V::None)
            } else {
                Ok(default())
            }
        }
        Some(v) => Ok(pyd::validate(cx, v, td, &[V::str("body")], errs).await?.unwrap_or(V::None)),
    }
}

/// An embedded body parameter (several body params): `loc = ["body", name, ...]`.
pub async fn body_field(cx: &Cx, body: &Option<V>, name: &str, td: &'static TD, required: bool, default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    // fastapi request_body_to_args: `body.get(alias)`; a body without `.get` (a list, a str...) is a missing
    // field even when the parameter has a default; a `null` value counts as absent
    let missing = |errs: &mut Vec<ErrDetail>| errs.push(ErrDetail { kind: "missing", loc: vec![V::str("body"), V::str(name)], msg: "Field required".into(), input: V::None, ctx: None, custom: false });
    // FastAPI 0.151: a `null` field is a value, validated; a `null` body has no `.get`: each field missing
    let null_is_value = super::fastapi_at_least(151);
    let got = match body {
        Some(V::Dict(d)) => d.lock().get(&Key::Str(Arc::from(name))).map(|(_, v)| v.clone()).filter(|v| null_is_value || !matches!(v, V::None)),
        Some(V::None) if null_is_value => {
            missing(errs);
            return Ok(V::None);
        }
        None | Some(V::None) | Some(V::Bytes(_)) => None,
        Some(_) => {
            missing(errs);
            return Ok(V::None);
        }
    };
    match got {
        None => {
            if required {
                missing(errs);
                Ok(V::None)
            } else {
                Ok(default())
            }
        }
        Some(v) => Ok(pyd::validate(cx, &v, td, &[V::str("body"), V::str(name)], errs).await?.unwrap_or(V::None)),
    }
}

// ---------------------------------------------------------------- fastapi.security

static STR_TD: TD = TD::Str(pyd::NO_STR);
static CREDENTIALS_CLASS: Class =
    Class { name: "HTTPAuthorizationCredentials", qualname: "HTTPAuthorizationCredentials", bases: &[], kind: ClassKind::Schema(&CREDENTIALS) };
/// `fastapi.security.HTTPAuthorizationCredentials` (a Pydantic model: `.scheme`, `.credentials`)
pub static CREDENTIALS: pyd::SchemaDesc = pyd::SchemaDesc {
    name: "HTTPAuthorizationCredentials",
    class: &CREDENTIALS_CLASS,
    fields: &[
        pyd::FieldDesc { name: "scheme", alias: None, ser: None, paths: &[], td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
        pyd::FieldDesc { name: "credentials", alias: None, ser: None, paths: &[], td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
    ],
    from_attributes: false,
    extra: pyd::Extra::Ignore,
    validators: &[],
    validate_assignment: false,
    populate_by_name: false,
    methods: &[],
    open: false,
    model_after: &[],
    before: &[],
    model_before: &[],
    has_before: false,
    frozen: false,
    post_init: None,
    hash: pyd::HashKind::Unhashable,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    ser_bytes: pyd::BytesMode::Utf8,
    computed: &[],
    json_schema: None,
    x: pyd::SchemaX::NONE,
};

static SCOPES_CLASS: Class =
    Class { name: "SecurityScopes", qualname: "SecurityScopes", bases: &[], kind: ClassKind::Schema(&SCOPES) };
/// `fastapi.security.SecurityScopes` given to a dependency (`.scopes`, `.scope_str`)
pub static SCOPES: pyd::SchemaDesc = pyd::SchemaDesc {
    name: "SecurityScopes",
    class: &SCOPES_CLASS,
    fields: &[
        pyd::FieldDesc { name: "scopes", alias: None, ser: None, paths: &[], td: &ANY_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
        pyd::FieldDesc { name: "scope_str", alias: None, ser: None, paths: &[], td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
    ],
    from_attributes: false,
    extra: pyd::Extra::Ignore,
    validators: &[],
    validate_assignment: false,
    populate_by_name: false,
    methods: &[],
    open: false,
    model_after: &[],
    before: &[],
    model_before: &[],
    has_before: false,
    frozen: false,
    post_init: None,
    hash: pyd::HashKind::Unhashable,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    ser_bytes: pyd::BytesMode::Utf8,
    computed: &[],
    json_schema: None,
    x: pyd::SchemaX::NONE,
};

static ANY_TD: TD = TD::Any;

/// `SecurityScopes(scopes=...)` as FastAPI builds it for a dependency's parameter: the scopes of the
/// dependency chain down to it, without repeats, in order
pub fn security_scopes(scopes: &[&str]) -> V {
    V::Inst(Arc::new(pyd::Inst { raw: false,
        desc: &SCOPES,
        vals: super::pyd::Slots::new(vec![V::list(scopes.iter().map(|x| V::str(*x)).collect()), V::str(scopes.join(" "))]),
        set: Mutex::new(vec![true, true]),
        extra: super::pyd::Slots::new(IndexMap::new()),
    }))
}

pub enum Security {
    /// `OAuth2PasswordBearer(tokenUrl=...)`: the bearer token (str)
    OAuth2Bearer,
    /// `HTTPBearer()`: HTTPAuthorizationCredentials
    HttpBearer,
    /// `HTTPBasic(realm=...)`: HTTPBasicCredentials
    HttpBasic(Option<&'static str>),
}

static BASIC_CLASS: Class = Class { name: "HTTPBasicCredentials", qualname: "HTTPBasicCredentials", bases: &[], kind: ClassKind::Schema(&BASIC) };
/// `fastapi.security.HTTPBasicCredentials` (a Pydantic model: `.username`, `.password`)
pub static BASIC: pyd::SchemaDesc = pyd::SchemaDesc {
    name: "HTTPBasicCredentials",
    class: &BASIC_CLASS,
    fields: &[
        pyd::FieldDesc { name: "username", alias: None, ser: None, paths: &[], td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
        pyd::FieldDesc { name: "password", alias: None, ser: None, paths: &[], td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
    ],
    from_attributes: false,
    extra: pyd::Extra::Ignore,
    validators: &[],
    validate_assignment: false,
    populate_by_name: false,
    methods: &[],
    open: false,
    model_after: &[],
    before: &[],
    model_before: &[],
    has_before: false,
    frozen: false,
    post_init: None,
    hash: pyd::HashKind::Unhashable,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    ser_bytes: pyd::BytesMode::Utf8,
    computed: &[],
    json_schema: None,
    x: pyd::SchemaX::NONE,
};

/// `HTTPBasic.__call__`: a missing or non-Basic header is a 401 (None without auto_error); a payload that
/// is not base64 of ASCII `user:password` is a 401 whatever auto_error says.
fn http_basic(cx: &Cx, realm: Option<&str>, auto_error: bool) -> R {
    let challenge = match realm {
        Some(r) => format!("Basic realm=\"{r}\""),
        None => "Basic".into(),
    };
    let denied = || Err(Exc::http_fastapi(401, V::str("Not authenticated"), vec![("WWW-Authenticate".into(), challenge.clone())]));
    let authorization = cx.req.header("authorization").unwrap_or_default();
    let (scheme, param) = authorization.split_once(' ').unwrap_or((&authorization, ""));
    if authorization.is_empty() || !scheme.eq_ignore_ascii_case("basic") {
        return if auto_error { denied() } else { Ok(V::None) };
    }
    let param = param.trim();
    if !param.is_ascii() {
        return denied();
    }
    let Ok(data) = super::stdlib::b64dec(param.as_bytes(), false) else { return denied() };
    if !data.is_ascii() {
        return denied();
    }
    let data = String::from_utf8(data).unwrap_or_default();
    let Some((user, password)) = data.split_once(':') else { return denied() };
    Ok(V::Inst(Arc::new(pyd::Inst { raw: false,
        desc: &BASIC,
        vals: super::pyd::Slots::new(vec![V::str(user), V::str(password)]),
        set: Mutex::new(vec![true, true]),
        extra: super::pyd::Slots::new(IndexMap::new()),
    })))
}

/// A security scheme used as a dependency, as FastAPI 0.142 calls it: `Authorization` header split on
/// the first space (`get_authorization_scheme_param`), 401 `Not authenticated` with `WWW-Authenticate:
/// Bearer`, or None when `auto_error=False`.
pub fn security(cx: &Cx, kind: Security, auto_error: bool) -> R {
    if let Security::HttpBasic(realm) = kind {
        return http_basic(cx, realm, auto_error);
    }
    let authorization = cx.req.header("authorization").unwrap_or_default();
    let present = !authorization.is_empty();
    let (scheme, param) = authorization.split_once(' ').unwrap_or((&authorization, ""));
    let param = param.trim();
    let ok = match kind {
        Security::OAuth2Bearer => present && scheme.eq_ignore_ascii_case("bearer"),
        Security::HttpBearer => present && !scheme.is_empty() && !param.is_empty() && scheme.eq_ignore_ascii_case("bearer"),
        Security::HttpBasic(_) => unreachable!(),
    };
    if !ok {
        if auto_error {
            return Err(Exc::http_fastapi(401, V::str("Not authenticated"), vec![("WWW-Authenticate".into(), "Bearer".into())]));
        }
        return Ok(V::None);
    }
    Ok(match kind {
        Security::OAuth2Bearer => V::str(param),
        Security::HttpBearer => V::Inst(Arc::new(pyd::Inst { raw: false,
            desc: &CREDENTIALS,
            vals: super::pyd::Slots::new(vec![V::str(scheme), V::str(param)]),
            set: Mutex::new(vec![true, true]),
            extra: super::pyd::Slots::new(IndexMap::new()),
        })),
        Security::HttpBasic(_) => unreachable!(),
    })
}

/// A project subclass of a security scheme: its `make_not_authenticated_error()` (the exception raised, instead
/// of the 401) or `make_authenticate_headers()` (the 401's headers), called where FastAPI calls them: every
/// refusal of the scheme, `auto_error` or not for HTTPBasic's malformed credentials.
pub async fn security_with(cx: &Cx, kind: Security, auto_error: bool, error: Option<pyd::MethodFn>,
                           headers: Option<pyd::MethodFn>) -> R {
    match security(cx, kind, auto_error) {
        Err(x) if x.0.http.as_ref().map(|h| h.0) == Some(401) => {
            if let Some(f) = error {
                let v = f(cx, V::None, vec![]).await?;
                return Err(super::raise_v(&v));
            }
            let Some(h) = headers else { return Err(x) };
            let hv = h(cx, V::None, vec![]).await?;
            let V::Dict(d) = &hv else {
                return Err(Exc::type_error("py2axum: make_authenticate_headers() must return a dict"));
            };
            let mut out = Vec::new();
            for (_, (k, v)) in d.lock().iter() {
                out.push((super::ops::str_(k)?, super::ops::str_(v)?));
            }
            Err(Exc::http_fastapi(401, V::str("Not authenticated"), out))
        }
        r => r,
    }
}

// ---------------------------------------------------------------- forms (Form(), File())

pub enum FormVal {
    Text(String),
    File(V),
}

/// `await request.form()` as Starlette parses it: multipart or urlencoded, anything else is empty.
pub async fn read_form(cx: &Cx) -> R<Vec<(String, FormVal)>> {
    // the route's Request object streamed its body: a later `body()` on it raises "Stream consumed"
    if cx.req_obj.load(Ordering::Relaxed) == REQ_ROUTE {
        cx.req.form_read.store(true, Ordering::Relaxed);
    }
    let ct = cx.req.header("content-type").unwrap_or_default();
    let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let bad = || Exc::http(400, V::str("There was an error parsing the body"), vec![]);
    match main.as_str() {
        "multipart/form-data" => {
            // Starlette's MultiPartParser: no boundary parameter, then an empty one
            let param = ct.split(';').skip(1).find_map(|p| {
                let (k, v) = p.split_once('=')?;
                k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string())
            });
            match param.as_deref() {
                None => return Err(Exc::http(400, V::str("Missing boundary in multipart."), vec![])),
                // Starlette 1.7+ wraps python-multipart's parse error; before, it reaches FastAPI's catch-all
                Some("") if starlette() >= (1, 7) => return Err(Exc::http(400, V::str("Invalid multipart data."), vec![])),
                Some("") => return Err(bad()),
                _ => {}
            }
            let boundary = multer::parse_boundary(&ct).map_err(|_| bad())?;
            let body = cx.req.body_bytes();
            let stream = futures_util::stream::once(async move { Ok::<Bytes, std::convert::Infallible>(Bytes::from(body.to_vec())) });
            let mut mp = multer::Multipart::new(stream, boundary);
            let mut out = Vec::new();
            // Starlette's MultiPartParser limits (max_files, max_fields: 1000; max_part_size: 1 MiB for a
            // field that is not a file), checked as its callbacks do: the count when a part's headers end
            let (mut files, mut fields) = (0, 0);
            // python-multipart keeps what it parsed from a truncated or malformed body
            while let Ok(Some(field)) = mp.next_field().await {
                let name = field.name().unwrap_or("").to_string();
                // python-multipart's parse_options_header (0.0.2x): the quoted value unescaped (multer keeps
                // `\\` and `\"` as sent), then an IE-style full Windows path keeps its last part
                let filename = field.file_name().map(|raw| {
                    let f = raw.replace("\\\\", "\\").replace("\\\"", "\"");
                    if f.get(1..3) == Some(":\\") || f.starts_with("\\\\") { f.rsplit('\\').next().unwrap_or("").to_string() } else { f }
                });
                if filename.is_some() {
                    files += 1;
                    if files > 1000 {
                        return Err(Exc::http(400, V::str("Too many files. Maximum number of files is 1000."), vec![]));
                    }
                } else {
                    fields += 1;
                    if fields > 1000 {
                        return Err(Exc::http(400, V::str("Too many fields. Maximum number of fields is 1000."), vec![]));
                    }
                }
                let ctype = field.content_type().map(|m| m.to_string());
                let headers: Vec<(String, String)> =
                    field.headers().iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())).collect();
                let Ok(data) = field.bytes().await else { break };
                if filename.is_none() && data.len() > 1024 * 1024 {
                    return Err(Exc::http(400, V::str("Part exceeded maximum size of 1024KB."), vec![]));
                }
                out.push((
                    name,
                    match filename {
                        Some(f) => FormVal::File(super::files::upload(Some(f), ctype, headers, data.to_vec())),
                        None => FormVal::Text(String::from_utf8_lossy(&data).to_string()),
                    },
                ));
            }
            Ok(out)
        }
        "application/x-www-form-urlencoded" => {
            // Starlette's FormParser limits (1.4+: max_part_size, name + value as sent; max_fields), checked
            // field by field like python-multipart's callbacks
            let mut fields = 0;
            let limits = starlette() >= (1, 4);
            for part in cx.req.body_bytes().split(|b| *b == b'&').filter(|p| !p.is_empty() && limits) {
                if part.len() - usize::from(part.contains(&b'=')) > 1024 * 1024 {
                    return Err(Exc::http(400, V::str("Field exceeded maximum size of 1024KB."), vec![]));
                }
                fields += 1;
                if fields > 1000 {
                    return Err(Exc::http(400, V::str("Too many fields. Maximum number of fields is 1000."), vec![]));
                }
            }
            Ok(form_urlencoded::parse(&cx.req.body_bytes()).map(|(k, v)| (k.to_string(), FormVal::Text(v.to_string()))).collect())
        }
        _ => Ok(Vec::new()),
    }
}

fn form_value(v: &FormVal) -> V {
    match v {
        FormVal::Text(t) => V::str(t),
        FormVal::File(f) => f.clone(),
    }
}

/// FastAPI 0.150: an empty file input is absent, as an empty string is
fn form_absent(v: &FormVal) -> bool {
    match v {
        FormVal::Text(t) => t.is_empty(),
        FormVal::File(f) => super::fastapi_at_least(150) && super::files::empty_upload(f),
    }
}

/// FastAPI 0.150: the empty file inputs left out of a list
fn form_list<'a>(vals: Vec<&'a FormVal>) -> Vec<&'a FormVal> {
    if !super::fastapi_at_least(150) {
        return vals;
    }
    vals.into_iter().filter(|v| !matches!(v, FormVal::File(f) if super::files::empty_upload(f))).collect()
}

/// FastAPI 0.147 to 0.161: a `bool` (or `bool | None`) form field absent from the form is False (an unchecked
/// checkbox; 0.162 dropped it)
fn checkbox_inference() -> bool {
    super::fastapi_at_least(147) && !super::fastapi_at_least(162)
}

fn form_bool_absent(form: &[(String, FormVal)], alias: &str, td: &TD) -> bool {
    checkbox_inference()
        && matches!(td.bare(), TD::Bool | TD::Optional(TD::Bool))
        && !form.iter().any(|(k, _)| k == alias)
}

/// A `Form()` parameter: last value, or all of them for a list; "" counts as missing (FastAPI).
pub async fn form_field(cx: &Cx, form: &[(String, FormVal)], alias: &str, td: &'static TD, required: bool, default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    let loc = vec![V::str("body"), V::str(alias)];
    let vals: Vec<&FormVal> = form.iter().filter(|(k, _)| k == alias).map(|(_, v)| v).collect();
    let raw = if form_bool_absent(form, alias, td) {
        Some(V::Bool(false))
    } else if matches!(td.bare(), TD::List(_)) || matches!(td, TD::Optional(t) if matches!(t.bare(), TD::List(_))) {
        let vals = form_list(vals);
        if vals.is_empty() { None } else { Some(V::list(vals.iter().map(|v| form_value(v)).collect())) }
    } else {
        match vals.last() {
            None => None,
            Some(v) if form_absent(v) => None,
            Some(v) => Some(form_value(v)),
        }
    };
    match raw {
        None if required => {
            errs.push(ErrDetail { kind: "missing", loc, msg: "Field required".into(), input: V::None, ctx: None, custom: false });
            Ok(V::None)
        }
        None => Ok(default()),
        Some(v) => Ok(pyd::validate(cx, &v, td, &loc, errs).await?.unwrap_or(V::None)),
    }
}

/// FastAPI's `field_annotation_is_sequence`: a list, set or tuple, or a union holding one.
fn td_is_sequence(td: &TD) -> bool {
    match td.bare() {
        TD::List(_) | TD::Set(_) | TD::Tuple(_) => true,
        TD::Optional(t) | TD::Strict(t, _) | TD::StrictTop(t) => td_is_sequence(t),
        TD::Union(ts) => ts.iter().any(|t| td_is_sequence(t)),
        _ => false,
    }
}

/// A Pydantic model of query, header, cookie or form parameters (`Annotated[Model, Query()]`, ...), as
/// FastAPI's `request_params_to_args` / `_extract_form_body`: each field read under its alias (a header's
/// underscores turned into hyphens unless `convert_underscores=False`; a sequence field gets every value, an
/// absent one its default when it has one), then every other received key (one value, or the list of them),
/// and the whole dict validated as the model at `[source]` (`["body"]` for a form).
pub async fn param_model(cx: &Cx, source: &str, td: &'static TD, convert: bool, form: Option<&[(String, FormVal)]>,
                         errs: &mut Vec<ErrDetail>) -> R {
    let desc = match td {
        TD::Schema(d) => *d,
        _ => return Err(Exc::type_error("a model of parameters must be a Pydantic model")),
    };
    let received: Vec<(String, V)> = match source {
        "query" => cx.req.query.iter().map(|(k, v)| (k.clone(), V::str(v))).collect(),
        "header" => cx.req.headers.iter().map(|(k, v)| (k.clone(), V::str(v))).collect(),
        "cookie" => super::methods::cookie_map(&cx.req).into_iter().map(|(k, v)| (k.to_string(), V::str(v))).collect(),
        _ => form.unwrap_or(&[]).iter().map(|(k, v)| (k.clone(), form_value(v))).collect(),
    };
    // Starlette: `Headers.get` is the first value, a multidict's (query, form) the last; cookies are a dict
    let multi = source != "cookie";
    let getlist = |key: &str| -> Vec<V> { received.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect() };
    let get = |key: &str| -> Option<V> {
        let mut it = received.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone());
        if source == "header" { it.next() } else { it.last() }
    };
    let mut values: IndexMap<String, V> = IndexMap::new();
    let mut processed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // FastAPI 0.148: an absent field is left to the model's default (no longer put in the input)
    let fill_defaults = !super::fastapi_at_least(148);
    // FastAPI 0.149: "" (0.150: an empty file input) read from a form is absent for a model's fields too
    let form_empty_absent = source == "form" && super::fastapi_at_least(149);
    // FastAPI 0.152: the errors of a header model name the headers (`save-data`), not the fields
    let mut header_aliases: Vec<(&'static str, String)> = Vec::new();
    for f in desc.fields {
        let valias = f.alias.unwrap_or(f.name);
        let alias = if source == "header" && convert {
            Some(if valias == f.name { valias.replace('_', "-") } else { valias.to_string() })
        } else {
            None
        };
        let look = alias.clone().unwrap_or_else(|| valias.to_string());
        let look = if source == "header" { look.to_ascii_lowercase() } else { look };
        if source == "header" && alias.as_deref().is_some_and(|a| a != valias) && super::fastapi_at_least(152) {
            header_aliases.push((f.name, alias.clone().unwrap_or_default()));
        }
        let seq = td_is_sequence(f.td);
        let empty = |v: &V| form_empty_absent && (matches!(v, V::Str(s) if s.is_empty()) || super::fastapi_at_least(150) && super::files::empty_upload(v));
        let raw = if source == "form" && checkbox_inference() && matches!(f.td.bare(), TD::Bool | TD::Optional(TD::Bool))
            && !received.iter().any(|(k, _)| *k == look) {
            Some(V::Bool(false))
        } else if seq && multi {
            let mut all = getlist(&look);
            if form_empty_absent && super::fastapi_at_least(150) {
                all.retain(|v| !super::files::empty_upload(v));
            }
            Some(V::list(all))
        } else {
            get(&look).filter(|v| !empty(v))
        };
        let missing = match &raw {
            None => true,
            Some(V::List(l)) if seq => l.lock().is_empty(),
            _ => false,
        };
        let value = if !missing {
            raw
        } else if !fill_defaults {
            None
        } else {
            match &f.default {
                pyd::Dflt::Required => None,
                pyd::Dflt::Value(d) | pyd::Dflt::Factory(d) => Some(d()),
                pyd::Dflt::Dyn(d) => Some(d(cx, V::None, vec![]).await?),
                pyd::Dflt::Data(_) => return Err(Exc::type_error("py2axum: a default_factory taking data in a parameter model is not supported")),
            }
        };
        if let Some(v) = value {
            if !matches!(v, V::None) {
                values.insert(valias.to_string(), v);
            }
        }
        processed.insert(alias.map(|a| if source == "header" { a.to_ascii_lowercase() } else { a }).unwrap_or_else(|| valias.to_string()));
        processed.insert(valias.to_string());
    }
    for (key, _) in received.iter() {
        if processed.contains(key) || values.contains_key(key) {
            continue;
        }
        let v = if multi {
            let mut all = getlist(key);
            if all.len() == 1 { all.remove(0) } else { V::list(all) }
        } else {
            get(key).unwrap_or(V::None)
        };
        values.insert(key.clone(), v);
    }
    let input = V::dict_from(values.into_iter().map(|(k, v)| (V::str(k), v)).collect())?;
    let loc = vec![V::str(if source == "form" { "body" } else { source })];
    let n = errs.len();
    let out = pyd::validate(cx, &input, td, &loc, errs).await?.unwrap_or(V::None);
    if !header_aliases.is_empty() {
        for e in &mut errs[n..] {
            if let Some(V::Str(name)) = e.loc.get(1) {
                if let Some((_, h)) = header_aliases.iter().find(|(f, _)| **name == **f) {
                    e.loc[1] = V::str(h);
                }
            }
        }
    }
    Ok(out)
}

/// A `File()` parameter typed `bytes` (or `bytes | None`, `list[bytes]`), as FastAPI's `_extract_form_body`:
/// an uploaded file is read whole; a text field is validated as is (Pydantic encodes a str); in a list, a text
/// field has no `read()` (an AttributeError, a 500 in FastAPI).
pub async fn form_file_bytes(cx: &Cx, form: &[(String, FormVal)], alias: &str, td: &'static TD, required: bool,
                             default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    let loc = vec![V::str("body"), V::str(alias)];
    let vals: Vec<&FormVal> = form.iter().filter(|(k, _)| k == alias).map(|(_, v)| v).collect();
    let seq = td_is_sequence(td);
    // `_get_multidict_value`: a File() is a Form(): an empty string is absent; an empty list too
    let raw: Option<V> = if seq {
        let vals = form_list(vals);
        if vals.is_empty() {
            None
        } else {
            let mut out = Vec::new();
            for v in vals {
                match v {
                    FormVal::File(f) => out.push(V::Bytes(Arc::from(super::files::upload_read_all(f).unwrap_or_default()))),
                    // FastAPI 0.144: a text field is kept as it is
                    FormVal::Text(t) if super::fastapi_at_least(144) => out.push(V::str(t)),
                    FormVal::Text(_) => return Err(Exc::attr_error("'str' object has no attribute 'read'")),
                }
            }
            Some(V::list(out))
        }
    } else {
        match vals.last() {
            None => None,
            Some(v) if form_absent(v) => None,
            Some(FormVal::Text(t)) => Some(V::str(t)),
            Some(FormVal::File(f)) => Some(V::Bytes(Arc::from(super::files::upload_read_all(f).unwrap_or_default()))),
        }
    };
    match raw {
        None if required => {
            errs.push(ErrDetail { kind: "missing", loc, msg: "Field required".into(), input: V::None, ctx: None, custom: false });
            Ok(V::None)
        }
        None => Ok(default()),
        Some(v) => Ok(pyd::validate(cx, &v, td, &loc, errs).await?.unwrap_or(V::None)),
    }
}

/// A `File()` / `UploadFile` parameter (one file, or `list[UploadFile]`).
pub fn form_file(form: &[(String, FormVal)], alias: &str, list: bool, required: bool, dflt: fn() -> V, errs: &mut Vec<ErrDetail>) -> V {
    let loc = vec![V::str("body"), V::str(alias)];
    let vals: Vec<&FormVal> = form.iter().filter(|(k, _)| k == alias).map(|(_, v)| v).collect();
    // FastAPI's `_get_multidict_value`: an empty string is absent for a single field, a list keeps it
    let present: Vec<&FormVal> = if list { form_list(vals) } else { vals.into_iter().filter(|v| !form_absent(v)).collect() };
    if present.is_empty() {
        if required {
            errs.push(ErrDetail { kind: "missing", loc, msg: "Field required".into(), input: V::None, ctx: None, custom: false });
            return V::None;
        }
        return dflt();
    }
    let check = |v: &FormVal, loc: Vec<V>, errs: &mut Vec<ErrDetail>| -> V {
        match v {
            FormVal::File(f) => f.clone(),
            FormVal::Text(t) => {
                errs.push(ErrDetail {
                    kind: "value_error",
                    loc,
                    msg: "Value error, Expected UploadFile, received: <class 'str'>".into(),
                    input: V::str(t),
                    ctx: Some(vec![("error", V::empty_dict())]), custom: false });
                V::None
            }
        }
    };
    if list {
        V::list(present.iter().enumerate().map(|(i, v)| check(v, vec![V::str("body"), V::str(alias), V::Int(i as i64)], errs)).collect())
    } else {
        check(present[present.len() - 1], loc, errs)
    }
}

pub fn check(cx: &Cx, errs: Vec<ErrDetail>) -> R<()> {
    if errs.is_empty() {
        return Ok(());
    }
    let e = Exc::validation(&REQUEST_VALIDATION_ERROR, errs);
    endpoint_context(cx, &e);
    Err(e)
}

/// A route's RequestValidationError with FastAPI's `exc.body`: the decoded JSON body (or its bytes), None
/// without one (`raise RequestValidationError(errors, body=body)` in fastapi/routing.py).
pub fn check_body(cx: &Cx, errs: Vec<ErrDetail>, body: &Option<V>) -> R<()> {
    if errs.is_empty() {
        return Ok(());
    }
    let e = Exc::validation(&REQUEST_VALIDATION_ERROR, errs);
    endpoint_context(cx, &e);
    e.0.attrs.lock().insert("body".into(), body.clone().unwrap_or(V::None));
    Err(e)
}

// ---------------------------------------------------------------- responses

pub struct Streaming {
    pub rx: tokio::sync::mpsc::Receiver<V>,
    /// fired at the body's first read: Starlette iterates the content only once the response is sent,
    /// after the function-scoped dependencies closed (the producer task waits for it)
    pub go: tokio::sync::oneshot::Sender<()>,
    pub media_type: Option<String>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

pub type GenRx = tokio::sync::mpsc::Receiver<V>;

/// FastAPI validates the returned value itself: an instance of the response model's own class is kept
/// (its private attributes with it); other instances go through their dump.
fn prepare_for(v: &V, td: &'static TD) -> R {
    match (v, td) {
        (V::Inst(i), TD::Schema(d)) if std::ptr::eq(i.desc, *d) => Ok(v.clone()),
        (V::Inst(i), TD::Optional(TD::Schema(d))) if std::ptr::eq(i.desc, *d) => Ok(v.clone()),
        (V::List(l), TD::List(Some(inner))) => Ok(V::list(l.lock().clone().iter().map(|x| prepare_for(x, inner)).collect::<R<Vec<_>>>()?)),
        _ => prepare(v),
    }
}

fn prepare(v: &V) -> R {
    super::stack_guard()?;
    Ok(match v {
        V::Inst(_) => pyd::dump(v, pyd::DumpOpts { by_alias: true, ..Default::default() })?,
        V::List(l) => V::list(l.lock().clone().iter().map(prepare).collect::<R<Vec<_>>>()?),
        V::Tuple(t) => V::list(t.iter().map(prepare).collect::<R<Vec<_>>>()?),
        V::Dict(d) => {
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            V::dict_from(items.into_iter().map(|(k, x)| Ok((k, prepare(&x)?))).collect::<R<Vec<_>>>()?)?
        }
        _ => v.clone(),
    })
}

fn json_body(status: u16, body: String, extra: &[(String, String)]) -> Response {
    let mut b = Response::builder().status(status).header(header::CONTENT_TYPE, "application/json");
    for (k, v) in extra {
        b = b.header(k.as_str(), v.as_str());
    }
    b.body(Body::from(body)).unwrap_or_else(bad_response)
}

fn no_body(status: u16) -> bool {
    status < 200 || status == 204 || status == 304
}

/// The route's `response_model_include=`, `_exclude=` (top-level field names of a model),
/// `_exclude_unset=`, `_exclude_none=`: FastAPI hands them to the response field's serializer.
pub struct RespDump {
    pub include: Option<&'static [&'static str]>,
    pub exclude: Option<&'static [&'static str]>,
    pub exclude_unset: bool,
    pub exclude_none: bool,
}

pub static RESP_DUMP: RespDump = RespDump { include: None, exclude: None, exclude_unset: false, exclude_none: false };

/// Endpoint return value -> response (response_model validation + Pydantic JSON, or jsonable_encoder).
pub async fn respond(cx: &Cx, ret: V, model: Option<&'static TD>, status: u16) -> R<Response> {
    respond_as(cx, ret, model, Some(status), None, &RESP_DUMP).await
}

/// Same, with the route's `response_class=` (`cls` = its class name, None = JSONResponse) and its
/// `status_code=` (None = not given: the class default, 307 for RedirectResponse). FastAPI builds
/// `response_class(content, status_code=..., background=...)` from the encoded value, empties the body
/// for a no-body status, then appends the `response` parameter's headers.
pub async fn respond_as(cx: &Cx, ret: V, model: Option<&'static TD>, status: Option<u16>, cls: Option<&'static str>,
                        opts: &'static RespDump) -> R<Response> {
    respond_custom(cx, ret, model, status, cls, None, None, opts).await
}

/// `response_class=` a project subclass of a response class: its `media_type` (`media`) and its
/// `render(self, content)` (`render`, given the encoded value; its bytes are the body).
#[allow(clippy::too_many_arguments)]
pub async fn respond_custom(cx: &Cx, ret: V, model: Option<&'static TD>, status: Option<u16>, cls: Option<&'static str>,
                            media: Option<&'static str>, render: Option<pyd::MethodFn>, opts: &'static RespDump) -> R<Response> {
    let custom = media.is_some() || render.is_some();
    let Some(cls) = cls.filter(|c| *c != "JSONResponse" || custom) else {
        // the class a custom route handler sees (`repr(response)`): the endpoint's Response, else JSONResponse
        let mut r = respond_json(cx, ret, model, status.unwrap_or(200), opts).await?;
        if r.extensions().get::<super::resp::RespClass>().is_none() {
            r.extensions_mut().insert(super::resp::RespClass("starlette.responses.JSONResponse"));
        }
        return Ok(r);
    };
    if let V::Native(n) = &ret {
        if matches!(&**n, Native::RespObj(_) | Native::Streaming(_)) {
            return respond_json(cx, ret, model, status.unwrap_or(200), opts).await;
        }
    }
    if cls == "StreamingResponse" {
        return Err(Exc::runtime("py2axum: response_class=StreamingResponse with an endpoint that does not return a response is not supported"));
    }
    let content = encode(cx, &ret, model, opts).await?;
    let mut kwargs = Vec::new();
    if let Some(st) = cx.resp.status.lock().or(status) {
        kwargs.push(("status_code".to_string(), V::Int(st as i64)));
    }
    let key = if cls == "RedirectResponse" { "url" } else if cls == "FileResponse" { "path" } else { "content" };
    let (cls, content) = match render {
        Some(f) => {
            // Starlette's `self.body = self.render(content)`, under the subclass's media type (else its base's)
            let body = f(cx, V::None, vec![content]).await?;
            if !matches!(body, V::Bytes(_)) {
                return Err(Exc::type_error(format!("py2axum: render() returned {}, not bytes", body.type_name())));
            }
            if let Some(m) = media.or(super::resp::default_media(cls)) {
                kwargs.push(("media_type".to_string(), V::str(m)));
            }
            ("Response", body)
        }
        None => {
            if let Some(m) = media {
                kwargs.push(("media_type".to_string(), V::str(m)));
            }
            (cls, content)
        }
    };
    let obj = super::resp::new(cls, &[], &[vec![(key.to_string(), content)], kwargs].concat())?;
    let V::Native(n) = &obj else { unreachable!() };
    let Native::RespObj(r) = &**n else { unreachable!() };
    let st = *r.status.lock();
    if u16::try_from(st).is_ok_and(no_body) {
        if let super::resp::RespBody::Bytes(_) = &r.body {
            let r2 = super::resp::RespObj { status: parking_lot::Mutex::new(st), headers: parking_lot::Mutex::new(r.headers.lock().clone()),
                                            body: super::resp::RespBody::Bytes(vec![]), media: r.media.clone(), cls: r.cls };
            r2.headers.lock().extend(cx.resp.headers.lock().iter().cloned());
            return super::resp::into_response(&r2);
        }
    }
    r.headers.lock().extend(cx.resp.headers.lock().iter().cloned());
    super::resp::into_response(r)
}

/// `serialize_response`: the response_model's JSON dump, or jsonable_encoder.
async fn encode(cx: &Cx, ret: &V, model: Option<&'static TD>, opts: &RespDump) -> R<V> {
    match model {
        Some(td) => {
            let prepared = prepare_for(ret, td)?;
            let mut errs = Vec::new();
            match pyd::validate(cx, &prepared, td, &[V::str("response")], &mut errs).await? {
                Some(v) => {
                    let o = pyd::DumpOpts { json: true, by_alias: true, exclude_unset: opts.exclude_unset, exclude_none: opts.exclude_none, ..Default::default() };
                    let d = pyd::dump(&v, o).map_err(pyd::ser_json_error)?;
                    if opts.include.is_none() && opts.exclude.is_none() {
                        return Ok(d);
                    }
                    // a model's top-level fields (the translator refuses them on any other response model)
                    let (V::Inst(i), V::Dict(m)) = (&v, &d) else { return Ok(d) };
                    let key = |n: &&str| -> &'static str {
                        i.desc.fields.iter().find(|f| f.name == *n).map(|f| f.ser_key()).unwrap_or("")
                    };
                    let inc: Option<Vec<&str>> = opts.include.map(|l| l.iter().map(key).collect());
                    let exc: Vec<&str> = opts.exclude.map(|l| l.iter().map(key).collect()).unwrap_or_default();
                    let kept: Vec<(V, V)> = m.lock().values().filter(|(k, _)| match k {
                        V::Str(s) => inc.as_ref().is_none_or(|inc| inc.contains(&&**s)) && !exc.contains(&&**s),
                        _ => true,
                    }).cloned().collect();
                    V::dict_from(kept)
                }
                _ => {
                    Err(response_validation_error(cx, errs))
                }
            }
        }
        None => pyd::jsonable(ret),
    }
}

async fn respond_json(cx: &Cx, ret: V, model: Option<&'static TD>, status: u16, opts: &RespDump) -> R<Response> {
    if let V::Native(n) = &ret {
        // a Response object is sent as is (no serialization, the `response` parameter is not merged)
        if let Native::RespObj(r) = &**n {
            return super::resp::into_response(r);
        }
        if let Native::Streaming(s) = &**n {
            let s = s.lock().take().ok_or_else(|| Exc::runtime("response already consumed"))?;
            return Ok(stream_response(s, &cx.resp.headers.lock().clone()));
        }
        if let Native::ListStream(l) = &**n {
            let l = l.lock().take().ok_or_else(|| Exc::runtime("response already consumed"))?;
            let status = cx.resp.status.lock().unwrap_or(status);
            let headers = cx.resp.headers.lock().clone();
            return stream_list(cx, l, status, headers).await;
        }
    }
    let status = cx.resp.status.lock().unwrap_or(status);
    let headers = cx.resp.headers.lock().clone();
    if no_body(status) {
        let mut b = Response::builder().status(status).header(header::CONTENT_TYPE, "application/json");
        for (k, v) in &headers {
            b = b.header(k.as_str(), v.as_str());
        }
        return Ok(b.body(Body::empty()).unwrap_or_else(bad_response));
    }
    let content = encode(cx, &ret, model, opts).await?;
    let style = if model.is_some() && response_dump_json() { &pyd::DUMP_JSON } else { &pyd::RESPONSE };
    Ok(json_body(status, pyd::to_json(&content, style, false)?, &headers))
}

/// The route's endpoint may return its list as a `ListStream` (orm::defer_list): set before calling it.
pub fn stream_list_ok(cx: &Cx, fname: &'static str, model: &'static TD) {
    *cx.stream_list.lock() = Some((fname, model));
}

/// Blocks queued between the query task and the socket (back-pressure bound).
const STREAM_QUEUE: usize = 4;

enum Block {
    /// the whole body fitted in one block: answered with a content-length, like the buffered path
    Complete(Bytes),
    /// first block of a body that is streamed
    Start(Bytes),
    Chunk(Bytes),
}

/// A `ListStream` as the response: rows read from Postgres as a stream, each validated by the item schema
/// and serialized like `encode` does for the whole list, block by block, so memory stays bounded
/// (~STREAM_QUEUE blocks per request) whatever the size. Same bytes as the buffered path. An exception
/// before the first block is the endpoint's (500); after it, the connection can only be aborted.
async fn stream_list(cx: &Cx, l: super::orm::ListStream, status: u16, extra: Vec<(String, String)>) -> R<Response> {
    use futures_util::{StreamExt, TryStreamExt};
    let (chunk, _) = super::orm::stream_cfg();
    let style = if response_dump_json() { &pyd::DUMP_JSON } else { &pyd::RESPONSE };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Block, Exc>>(STREAM_QUEUE);
    let cx2 = cx.clone();
    tokio::spawn(async move {
        let super::orm::ListStream { pool, sql, args, model, item } = l;
        let cx = cx2;
        let run = async {
            let mut conn = pool.acquire().await.map_err(|e| super::orm::stream_sql_error(e, &sql))?;
            let mut rows = sqlx::query_with(&sql, args).fetch(&mut *conn);
            let mut buf = String::with_capacity(chunk + 4096);
            buf.push('[');
            let mut started = false;
            let mut i = 0i64;
            while let Some(row) = rows.try_next().await.map_err(|e| super::orm::stream_sql_error(e, &sql))? {
                let obj = super::orm::row_object(&cx, model, &row).await?;
                let mut errs = Vec::new();
                let v = match pyd::validate(&cx, &prepare_for(&obj, item)?, item, &[V::str("response"), V::Int(i)], &mut errs).await? {
                    Some(v) => v,
                    None => {
                        return Err(response_validation_error(&cx, errs));
                    }
                };
                let content = pyd::dump(&v, pyd::DumpOpts { json: true, by_alias: true, ..Default::default() }).map_err(pyd::ser_json_error)?;
                if i > 0 {
                    buf.push(',');
                }
                buf.push_str(&pyd::to_json(&content, style, false)?);
                i += 1;
                if buf.len() >= chunk {
                    let block = Bytes::from(std::mem::replace(&mut buf, String::with_capacity(chunk + 4096)));
                    let msg = if started { Block::Chunk(block) } else { Block::Start(block) };
                    started = true;
                    if tx.send(Ok(msg)).await.is_err() {
                        return Ok(()); // the client went away: dropping `rows` cancels the query
                    }
                }
            }
            buf.push(']');
            let block = Bytes::from(buf);
            let _ = tx.send(Ok(if started { Block::Chunk(block) } else { Block::Complete(block) })).await;
            Ok::<(), Exc>(())
        };
        if let Err(e) = run.await {
            let _ = tx.send(Err(e)).await;
        }
    });
    let mut b = Response::builder().status(status).header(header::CONTENT_TYPE, "application/json");
    for (k, v) in &extra {
        b = b.header(k.as_str(), v.as_str());
    }
    match rx.recv().await {
        Some(Ok(Block::Complete(body))) => Ok(b.body(Body::from(body)).unwrap_or_else(bad_response)),
        Some(Ok(Block::Start(first) | Block::Chunk(first))) => {
            let rest = futures_util::stream::unfold(rx, |mut rx| async move {
                match rx.recv().await {
                    Some(Ok(Block::Chunk(b) | Block::Start(b) | Block::Complete(b))) => Some((Ok::<Bytes, std::io::Error>(b), rx)),
                    Some(Err(e)) => {
                        super::uvicorn::exception(&e);
                        Some((Err(std::io::Error::other("list response aborted")), rx))
                    }
                    None => None,
                }
            });
            // fuse(): the compression layer may poll again after the end
            let body = futures_util::stream::once(async move { Ok::<Bytes, std::io::Error>(first) }).chain(rest).fuse();
            Ok(b.body(Body::from_stream(body)).unwrap_or_else(bad_response))
        }
        Some(Err(e)) => Err(e),
        None => Err(Exc::runtime("py2axum: list stream ended without a result")),
    }
}

fn stream_response(s: Streaming, extra: &[(String, String)]) -> Response {
    use futures_util::StreamExt;
    let mut b = Response::builder().status(s.status);
    if let Some(mt) = &s.media_type {
        let ct = if mt.starts_with("text/") && !mt.contains("charset") { format!("{mt}; charset=utf-8") } else { mt.clone() };
        b = b.header(header::CONTENT_TYPE, ct);
    }
    for (k, v) in s.headers.iter().chain(extra.iter()) {
        b = b.header(k.to_ascii_lowercase().as_str(), v.as_str());
    }
    let stream = futures_util::stream::unfold((Some(s.rx), Some(s.go)), |(rx, go)| async move {
        if let Some(go) = go {
            let _ = go.send(());
        }
        // after a failed chunk the body ends (the generator is released with the channel)
        let mut rx = rx?;
        rx.recv().await.map(|v| match v {
            V::Str(s) => (Ok::<Bytes, std::io::Error>(Bytes::from(s.to_string())), (Some(rx), None)),
            V::Bytes(b) => (Ok(Bytes::from(b.to_vec())), (Some(rx), None)),
            // Starlette's stream_response: `chunk.encode(self.charset)` for anything but bytes/memoryview; the
            // response has started: logged, the connection dropped
            other => {
                let e = Exc::attr_error(format!("'{}' object has no attribute 'encode'", other.type_name()));
                super::uvicorn::exception(&super::trace::streaming_error(&e));
                (Err(std::io::Error::other("streaming response failed")), (None, None))
            }
        })
    })
    .fuse();
    b.body(Body::from_stream(stream)).unwrap_or_else(bad_response)
}

/// Exception escaping the endpoint -> HTTP response (FastAPI's default exception handlers).
pub fn error_response(e: Exc) -> Response {
    try_error_response(e).unwrap_or_else(internal_error)
}

/// FastAPI's handlers for HTTPException and RequestValidationError. Their JSONResponse fails on
/// NaN/inf (json.dumps(allow_nan=False), in a detail or in a validation error's raw `input`): the
/// ValueError leaves the ExceptionMiddleware, ServerErrorMiddleware answers 500 outside every other
/// middleware.
/// an exception raised by FastAPI's default handler while it handles `handled` (`inf` in a 422's input: the
/// encoder's ValueError): its `__context__`, as CPython chains it
fn handling(x: Exc, handled: &Exc) -> Exc {
    x.0.attrs.lock().entry("__context__".into()).or_insert_with(|| V::Exc(handled.clone()));
    x
}

pub fn try_error_response(e: Exc) -> R<Response> {
    let obj = &e.0;
    if let Some((code, detail, headers)) = &e.http_info() {
        let body = V::dict_from(vec![(V::str("detail"), pyd::jsonable(detail).unwrap_or(V::None))]).unwrap();
        if no_body(*code) {
            // `Response(status_code=..., headers=exc.headers)`
            let mut b = Response::builder().status(*code);
            for (k, v) in headers {
                b = b.header(k.as_str(), v.as_str());
            }
            return Ok(b.body(Body::empty()).unwrap_or_else(bad_response));
        }
        return Ok(json_body(*code, pyd::to_json(&body, &pyd::RESPONSE, true).map_err(|x| handling(x, &e))?, headers));
    }
    if e.isinstance(&REQUEST_VALIDATION_ERROR) {
        let errs: Vec<V> = obj.errors.as_ref().map(|v| v.iter().map(|e| e.to_v()).collect()).unwrap_or_default();
        let body = V::dict_from(vec![(V::str("detail"), pyd::jsonable(&V::list(errs)).unwrap_or(V::None))]).unwrap();
        return Ok(json_body(422, pyd::to_json(&body, &pyd::RESPONSE, true).map_err(|x| handling(x, &e))?, &[]));
    }
    Ok(internal_error(e))
}

/// `fastapi.exception_handlers.http_exception_handler(request, exc)` and
/// `request_validation_exception_handler(request, exc)`: FastAPI's default handlers, called by the application's
/// own (`await http_exception_handler(request, exc)`); an exception of another kind is their AttributeError.
pub fn default_exc_handler(exc: &V, which: &str) -> R {
    let V::Exc(e) = exc else {
        return Err(Exc::attr_error(format!("'{}' object has no attribute '{}'", exc.type_name(),
                                           if which == "http" { "status_code" } else { "errors" })));
    };
    let ok = if which == "http" { e.http_info().is_some() } else { e.isinstance(&REQUEST_VALIDATION_ERROR) };
    if !ok {
        return Err(Exc::attr_error(format!("'{}' object has no attribute '{}'", e.0.class.name,
                                           if which == "http" { "status_code" } else { "errors" })));
    }
    Ok(super::resp::from_response(try_error_response(e.clone())?))
}

/// a response the application gave headers HTTP cannot carry (CR, LF, NUL in a value, a bad name):
/// uvicorn's h11 refuses it at send time and answers 500, never reaching the client either
pub fn bad_response(e: axum::http::Error) -> Response {
    super::uvicorn::error(&format!("Exception in ASGI application\nRuntimeError: Invalid HTTP header value: {e}"));
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("Internal Server Error"))
        .unwrap()
}

fn internal_error(e: Exc) -> Response {
    super::uvicorn::exception(&e);
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("Internal Server Error"))
        .unwrap()
}

/// Run the matched route on the request's context: teardowns, session commit, background tasks.
/// An exception escapes after the dependencies' exit code (for the exception handlers).
/// PEP 479: a StopIteration escaping a coroutine (the endpoint, a dependency, AnyIO's worker of a `def` endpoint)
/// becomes `RuntimeError: coroutine raised StopIteration`, the StopIteration its cause
fn pep479(e: Exc) -> Exc {
    if !e.0.class.is_subclass(&STOP_ITERATION) {
        return e;
    }
    let r = Exc::runtime("coroutine raised StopIteration");
    r.0.attrs.lock().insert("__cause__".into(), V::Exc(e));
    r
}

async fn run_route(cx: &Cx, run: RunFn) -> R<Response> {
    let mut guard = TeardownGuard(Some(cx.clone()));
    let r = match run(cx).await {
        Ok(r) => r,
        // the exception is raised at the dependencies' `yield` (function scope first), then the session
        // dependency: rollback (the transaction is dropped)
        Err(e) => return Err(throw_teardowns(cx, pep479(e)).await),
    };
    // `Depends(scope="function")`: its exit code runs before the response is sent, an exception there
    // goes through the request-scoped ones
    if let Some(e) = run_fn_teardowns(cx).await {
        return Err(throw_teardowns(cx, e).await);
    }
    let session = match cx.session.get() {
        Some(V::Session(s)) if super::commit_after(cx) => Some(s.clone()),
        _ => None,
    };
    // the dependencies' exit code (and `yield s; await s.commit()`): FastAPI (>= 0.121, scope "request")
    // runs it once the response is sent; a failure is logged and the client keeps its response. A full
    // body is committed before being returned (same response, no read-after-write race), a stream at its end.
    use axum::body::HttpBody as _;
    if r.body().size_hint().exact().is_some() || (session.is_none() && cx.teardowns.lock().is_empty()) {
        run_teardowns(cx, true).await;
        if let Some(session) = session {
            if let Err(e) = session.commit().await {
                super::uvicorn::exception(&e);
            }
        }
        spawn_background(cx);
        return Ok(phase_while_streaming(cx, r));
    }
    use futures_util::StreamExt;
    let (parts, body) = r.into_parts();
    // the stream's tail runs them; a body dropped before its end (the client left) drops the tail unpolled,
    // and the guard with it: the generators are released (else each task waits for its `resume`, held in
    // `cx.teardowns` by the context it holds itself: task, context, session and connection never freed)
    guard.0 = None;
    let cx2 = cx.clone();
    let cx = cx.clone();
    let left = TeardownGuard(Some(cx.clone()));
    let tail = futures_util::stream::once(async move {
        let _left = left;
        run_teardowns(&cx, true).await;
        match session {
            Some(session) => {
                if let Err(e) = session.commit().await {
                    super::uvicorn::exception(&e);
                }
                super::resp::run_background(cx.clone()).await;
            }
            None => {
                spawn_background(&cx);
            }
        }
        drop(cx);
        Ok::<Bytes, axum::Error>(Bytes::new())
    });
    let phase = Phase::new(&cx2);
    Ok(Response::from_parts(parts, Body::from_stream(body.into_data_stream().chain(tail).map(move |c| {
        let _ = &phase;
        c
    }))))
}

// ---------------------------------------------------------------- generator dependencies

/// The `yield` of a dependency: hands the value to the request, then waits for its end.
pub struct DepYield {
    val: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<V>>>,
    resume: parking_lot::Mutex<Option<tokio::sync::oneshot::Receiver<Option<Exc>>>>,
}

impl DepYield {
    pub async fn yield_(&self, v: V) -> R {
        let tx = self.val.lock().take().ok_or_else(|| Exc::runtime("generator didn't stop"))?;
        let _ = tx.send(v);
        let rx = self.resume.lock().take().expect("resume channel");
        match rx.await {
            Ok(None) => Ok(V::None),
            // FastAPI throws the request's exception into the generator (`gen.athrow`): raised at the `yield`
            Ok(Some(e)) => Err(e),
            // dropped (the request was abandoned): the code after `yield` does not run, `finally` blocks do
            Err(_) => Err(Exc::new(&GENERATOR_EXIT, vec![])),
        }
    }
}

pub struct DepTeardown {
    resume: tokio::sync::oneshot::Sender<Option<Exc>>,
    task: tokio::task::JoinHandle<R>,
    /// `Depends(scope="function")`: closed when the endpoint returns, before the response is sent
    function: bool,
}

/// Solving a dependency written as a generator: its body runs as a task up to its `yield`.
pub async fn dep_gen<F>(cx: &Cx, function: bool, f: F) -> R
where
    F: FnOnce(Cx, Arc<DepYield>) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'static>>,
{
    let (vtx, vrx) = tokio::sync::oneshot::channel();
    let (rtx, rrx) = tokio::sync::oneshot::channel();
    let y = Arc::new(DepYield { val: parking_lot::Mutex::new(Some(vtx)), resume: parking_lot::Mutex::new(Some(rrx)) });
    let task = tokio::spawn(f(cx.clone(), y));
    match vrx.await {
        Ok(v) => {
            cx.teardowns.lock().push(DepTeardown { resume: rtx, task, function });
            Ok(v)
        }
        Err(_) => match task.await {
            Ok(Err(e)) => Err(e),
            Ok(Ok(_)) => Err(Exc::runtime("generator didn't yield")),
            Err(e) => Err(Exc::runtime(format!("generator dependency task: {e}"))),
        },
    }
}

fn take_teardowns(cx: &Cx, function: bool) -> Vec<DepTeardown> {
    let mut all = cx.teardowns.lock();
    let (take, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut *all).into_iter().partition(|t| t.function == function);
    *all = keep;
    take
}

/// Close generators, last solved first (an AsyncExitStack): `exc` is thrown into each, what it raises
/// replaces it, a generator that catches it without raising suppresses it (contextlib).
/// Returns the exception left.
async fn close_teardowns(tds: Vec<DepTeardown>, mut exc: Option<Exc>) -> Option<Exc> {
    for td in tds.into_iter().rev() {
        // contextlib's `__aexit__`: the exception thrown in that comes back out keeps its own traceback
        let thrown = exc.as_ref().map(|e| (e.clone(), e.0.ptb.lock().len()));
        let _ = td.resume.send(exc.clone());
        match td.task.await {
            Ok(Ok(_)) => exc = None,
            Ok(Err(e)) => {
                if let Some((t, n)) = &thrown {
                    if Arc::ptr_eq(&t.0, &e.0) {
                        e.0.ptb.lock().truncate(*n);
                    }
                }
                exc = Some(e)
            }
            Err(e) => exc = Some(Exc::runtime(format!("generator dependency task: {e}"))),
        }
    }
    exc
}

/// FastAPI's `request_response`: an exception swallowed by a dependency with yield leaves no response.
fn not_awaited() -> Exc {
    Exc::msg(&FASTAPI_ERROR, "Response not awaited. There's a high chance that the application code is raising an \
exception and a dependency with yield has a block with a bare except, or a block with except Exception, and is not \
raising the exception again. Read more about it in the docs: \
https://fastapi.tiangolo.com/tutorial/dependencies/dependencies-with-yield/#dependencies-with-yield-and-except")
}

/// The request failed with `e`: it goes through the function-scoped generators, then the request-scoped
/// ones; the exception that escapes is the one the exception handlers see.
pub async fn throw_teardowns(cx: &Cx, e: Exc) -> Exc {
    let exc = close_teardowns(take_teardowns(cx, true), Some(e)).await;
    close_teardowns(take_teardowns(cx, false), exc).await.unwrap_or_else(not_awaited)
}

/// The endpoint returned: the function-scoped generators close (an exception there fails the request).
pub async fn run_fn_teardowns(cx: &Cx) -> Option<Exc> {
    let tds = take_teardowns(cx, true);
    if tds.is_empty() {
        return None;
    }
    close_teardowns(tds, None).await
}

/// End of the request: the dependencies' code after `yield`, last solved first (FastAPI's exit stack).
/// The response is sent: an exception there is only logged.
pub async fn run_teardowns(cx: &Cx, ok: bool) {
    let tds = std::mem::take(&mut *cx.teardowns.lock());
    for td in tds.into_iter().rev() {
        if ok {
            let _ = td.resume.send(None);
        } else {
            drop(td.resume);
        }
        match td.task.await {
            Ok(Err(e)) if !e.isinstance(&GENERATOR_EXIT) => super::uvicorn::exception(&e),
            Err(e) => super::uvicorn::error(&format!("Exception in ASGI application\n{e}")),
            _ => {}
        }
    }
}

/// A request future dropped (client gone): the pending generators are released, not leaked.
/// Run a WebSocket route: FastAPI's `websocket_session` closes the dependencies' exit stack once the
/// endpoint returns (the session dependency commits there); an exception escapes after it.
pub async fn run_ws_route(cx: &Cx, run: super::ws::WsRunFn) -> R<()> {
    let _guard = TeardownGuard(Some(cx.clone()));
    if let Err(e) = run(cx).await {
        return Err(throw_teardowns(cx, e).await);
    }
    if let Some(e) = run_fn_teardowns(cx).await {
        return Err(throw_teardowns(cx, e).await);
    }
    run_teardowns(cx, true).await;
    if let Some(V::Session(s)) = cx.session.get() {
        if super::commit_after(cx) {
            s.commit().await?;
        }
    }
    Ok(())
}

/// A stage of the request's life (the route, its streamed body, its background tasks). The last one to end
/// frees `request.state` once no `Request` object outlives the request: CPython frees the scope, and the state
/// dict in it, by reference count when the ASGI call returns, so a task or an async generator kept there is
/// collected then (« Task exception was never retrieved », the generator's `finally`). Here the task, or the
/// generator's body, holds the context that holds the state: a cycle nothing would ever free. With no
/// `Request` left, no code can read that state any more: freeing it is not observable.
pub struct Phase(Cx);

impl Phase {
    pub fn new(cx: &Cx) -> Phase {
        cx.phases.fetch_add(1, Ordering::AcqRel);
        Phase(cx.clone())
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        let last = self.0.phases.fetch_sub(1, Ordering::AcqRel) == 1;
        if last && Arc::strong_count(&self.0.req) == 1 {
            // taken under the locks, dropped after them (a task's drop logs, a generator's `finally` runs); the
            // raw ASGI scope of the request holds a copy of the state (`scope["state"]`)
            let state = std::mem::take(&mut *self.0.req.state.lock());
            let scope = self.0.asgi_scope.lock().take();
            let seen = self.0.asgi_seen.lock().take();
            drop((state, scope, seen));
        }
    }
}

/// The body of a streamed response keeps the request's stage open until it is sent (or dropped).
pub fn phase_while_streaming(cx: &Cx, r: Response) -> Response {
    use axum::body::HttpBody as _;
    use futures_util::StreamExt;
    if r.body().size_hint().exact().is_some() {
        return r;
    }
    let phase = Phase::new(cx);
    let (parts, body) = r.into_parts();
    Response::from_parts(parts, Body::from_stream(body.into_data_stream().map(move |c| {
        let _ = &phase;
        c
    })))
}

/// the request's background tasks, run after the response was sent (a stage of the request)
pub fn spawn_background(cx: &Cx) {
    // none created (the usual case): no task to start
    if cx.background.get().is_none() {
        return;
    }
    let phase = Phase::new(cx);
    let cx = cx.clone();
    tokio::spawn(async move {
        super::resp::run_background(cx).await;
        drop(phase);
    });
}

struct TeardownGuard(Option<Cx>);

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        if let Some(cx) = &self.0 {
            cx.teardowns.lock().clear();
        }
    }
}

// ---------------------------------------------------------------- async generators

pub use super::agen::{spawn_gen, Yielder};

pub fn streaming_response(content: V, media_type: Option<String>, status: u16, headers: Vec<(String, String)>) -> R {
    let (go, started) = tokio::sync::oneshot::channel();
    let rx = match &content {
        V::Native(n) if matches!(&**n, Native::Gen(_)) => match &**n {
            Native::Gen(g) => super::agen::into_channel(g.clone(), started),
            _ => unreachable!(),
        },
        // a synchronous iterable (a list, `io.StringIO(...)`: its lines), which Starlette iterates in a
        // thread pool; anything not iterable is its TypeError
        _ => {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let items = ops::iter(&content)?;
            tokio::spawn(async move {
                if started.await.is_err() {
                    return;
                }
                for it in items {
                    if tx.send(it).await.is_err() {
                        break;
                    }
                }
            });
            rx
        }
    };
    Ok(V::native(Native::Streaming(Mutex::new(Some(Streaming { rx, go, media_type, status, headers })))))
}

// ---------------------------------------------------------------- asyncio

pub struct AQueue {
    pub items: Mutex<VecDeque<V>>,
    pub maxsize: usize,
    pub notify: tokio::sync::Notify,
    /// woken when an item is taken (a full bounded queue's `await put()` waits for room)
    pub room: tokio::sync::Notify,
}

impl AQueue {
    pub fn new(maxsize: usize) -> V {
        V::native(Native::Queue(Arc::new(AQueue { items: Mutex::new(VecDeque::new()), maxsize, notify: tokio::sync::Notify::new(), room: tokio::sync::Notify::new() })))
    }
    pub fn put_nowait(&self, v: V) -> R<()> {
        let mut q = self.items.lock();
        if self.maxsize > 0 && q.len() >= self.maxsize {
            return Err(Exc::new(&QUEUE_FULL, vec![]));
        }
        q.push_back(v);
        drop(q);
        self.notify.notify_one();
        Ok(())
    }
    /// `await queue.put(v)`: waits for room in a full bounded queue
    pub async fn put(&self, v: V) -> R<()> {
        loop {
            {
                let mut q = self.items.lock();
                if self.maxsize == 0 || q.len() < self.maxsize {
                    q.push_back(v);
                    drop(q);
                    self.notify.notify_one();
                    return Ok(());
                }
            }
            self.room.notified().await;
        }
    }
    pub async fn get(&self) -> R {
        loop {
            if let Some(v) = self.items.lock().pop_front() {
                self.room.notify_one();
                return Ok(v);
            }
            self.notify.notified().await;
        }
    }
    pub fn get_nowait(&self) -> R {
        let v = self.items.lock().pop_front().ok_or_else(|| Exc::new(&QUEUE_EMPTY, vec![]))?;
        self.room.notify_one();
        Ok(v)
    }
}

/// A delay as asyncio reads it (`delay <= 0`, then `loop.time() + delay`): None for "no deadline" (inf),
/// Some(0.0) for a delay <= 0; a Decimal fails the addition to the loop's float time unless the loop is
/// uvloop's (it rounds `delay * 1000`); NaN, whose handling varies with the Python and the uvloop
/// version, is refused at run time.
fn aio_delay(what: &str, v: &V, uvloop: bool) -> R<Option<f64>> {
    let le0 = |t: &str| Exc::type_error(format!("'<=' not supported between instances of '{t}' and 'int'"));
    let t = match v {
        V::Bool(b) => *b as i64 as f64,
        V::Int(i) => *i as f64,
        V::Float(f) => *f,
        V::Decimal(d) => {
            let f = d.to_f64();
            if f.is_nan() {
                return Err(Exc::type_error(format!("py2axum: {what} with a Decimal NaN is not supported")));
            }
            if f > 0.0 && !uvloop {
                return Err(Exc::type_error("unsupported operand type(s) for +: 'float' and 'decimal.Decimal'"));
            }
            f
        }
        V::Native(n) if matches!(&**n, Native::Fraction(_)) => {
            let Native::Fraction(f) = &**n else { unreachable!() };
            use num_traits::ToPrimitive;
            f.num.to_f64().unwrap_or(f64::NAN) / f.den.to_f64().unwrap_or(f64::NAN)
        }
        o => return Err(le0(o.type_name())),
    };
    if t.is_nan() {
        return Err(Exc::type_error(format!("py2axum: {what} with a NaN delay is not supported")));
    }
    if t <= 0.0 {
        return Ok(Some(0.0));
    }
    Ok(std::time::Duration::try_from_secs_f64(t).ok().map(|_| t))
}

/// `await asyncio.wait_for(coro, timeout)`: a timeout <= 0 cancels the coroutine before it starts
/// (TimeoutError, nothing run: asyncio wraps it in a task that never got a turn); a Decimal timeout fails
/// on any loop (asyncio.timeout adds it to `loop.time()`, a float on uvloop too)
pub async fn wait_for<F>(timeout: &V, fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    if timeout.is_none() {
        return fut.await;
    }
    match aio_delay("asyncio.wait_for()", timeout, false)? {
        None => fut.await,
        Some(t) if t <= 0.0 => Err(Exc::new(&TIMEOUT_ERROR, vec![])),
        Some(t) => match tokio::time::timeout(std::time::Duration::from_secs_f64(t), fut).await {
            Ok(r) => r,
            Err(_) => Err(Exc::new(&TIMEOUT_ERROR, vec![])),
        },
    }
}

/// `await asyncio.sleep(delay, result=None)` (and `anyio.sleep(delay)`, asyncio's underneath): `result`
/// once the delay is over; a delay <= 0 yields to the loop once
pub async fn sleep(secs: &V, result: V, uvloop: bool) -> R {
    match aio_delay("asyncio.sleep()", secs, uvloop)? {
        None => std::future::pending::<()>().await,
        Some(t) if t <= 0.0 => tokio::task::yield_now().await,
        Some(t) => tokio::time::sleep(std::time::Duration::from_secs_f64(t)).await,
    }
    Ok(result)
}

pub fn dep_cache_get(cx: &Cx, key: &'static str) -> Option<V> {
    cx.deps.lock().get(key).cloned()
}

pub fn dep_cache_put(cx: &Cx, key: &'static str, v: V) {
    cx.deps.lock().insert(key, v);
}

pub type DepCache = Mutex<HashMap<&'static str, V>>;

pub fn disconnected(cx: &Cx) -> bool {
    cx.req.disconnected.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------- routing (Starlette's)

pub type RunFn = for<'a> fn(&'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<Response>> + Send + 'a>>;

/// One FastAPI route: method, Starlette path regex (`^/tasks/(?P<task_id>[^/]+)$`), compiled endpoint.
pub struct RouteDef {
    pub method: &'static str,
    pub pattern: &'static str,
    pub run: RunFn,
    /// its `APIRoute` in the route tree (`request.scope["route"]`)
    pub node: Option<&'static super::routing::Node>,
    /// the route's path (runtime prefix markers included): Sentry's transaction name
    pub path: &'static str,
    /// the handler: `file.py:line`, function, module (Sentry's `py2axum.source` and frame)
    pub src: (&'static str, &'static str, &'static str),
    /// a `def` endpoint (FastAPI runs it in a thread, in a copy of the context)
    pub sync_endpoint: bool,
    /// the line `inspect.getsourcelines` gives (its first decorator's): FastAPI's endpoint context
    pub first_line: u32,
    /// a custom `route_class`: `run` calls its `get_route_handler()`'s handler, this is the route's own
    /// handler (what `super().get_route_handler()` returns)
    pub raw: Option<RunFn>,
    /// FastAPI reads the request's body before the endpoint (a body, form or file parameter)
    pub body: bool,
}

/// `super().get_route_handler()` in an `APIRoute` subclass: the route's own handler, a callable taking the request
/// and returning its Response (exceptions raised as they are: RequestValidationError, HTTPException...)
pub fn original_handler(cx: &Cx) -> R {
    match cx.route_def.get().and_then(|r| r.raw) {
        Some(run) => Ok(V::native(Native::RouteOrig(run))),
        None => Err(Exc::runtime("py2axum: get_route_handler() called outside a request of its route")),
    }
}

/// `await original_route_handler(request)`: the route's handler on that request, its response as a Response
/// object (headers mutable)
pub async fn call_original(cx: &Cx, run: RunFn, args: &[V]) -> R {
    let [req] = args else {
        return Err(Exc::type_error(format!("app() takes 1 positional argument but {} were given", args.len())));
    };
    // a project subclass of Request (`GzipRequest(request.scope, request.receive)`): the endpoint's request; when
    // FastAPI reads the body (a body or form parameter), what its `body()` returns is the body the route parses,
    // any error there a 400 (`There was an error parsing the body`)
    match req {
        V::Native(n) if matches!(&**n, Native::Request(..)) => return Ok(super::resp::from_response(run(cx).await?)),
        V::Inst(i) if i.extra.lock().contains_key(INNER_REQUEST) => {}
        _ => return Err(Exc::type_error(format!("py2axum: a route handler called with a '{}' (the request only)", req.type_name()))),
    }
    let mut body = None;
    if cx.route_def.get().is_some_and(|r| r.body) {
        match Box::pin(super::methods::call_method(cx, req, "body", vec![], vec![])).await {
            Ok(V::Bytes(b)) => body = Some(Bytes::copy_from_slice(&b)),
            Ok(other) => return Err(Exc::type_error(format!("py2axum: {}.body() returned a '{}', not bytes", req.type_name(), other.type_name()))),
            Err(e) if e.0.http.is_some() => return Err(e),
            Err(e) => {
                let x = Exc::http(400, V::str("There was an error parsing the body"), vec![]);
                x.0.attrs.lock().insert("__cause__".into(), V::Exc(e));
                return Err(x);
            }
        }
    }
    let prev_body = cx.req.body_override.lock().clone();
    if let Some(b) = body {
        *cx.req.body_override.lock() = Some(b);
    }
    let prev_req = cx.req.custom_request.lock().replace(req.clone());
    let r = run(cx).await;
    *cx.req.body_override.lock() = prev_body;
    *cx.req.custom_request.lock() = prev_req;
    Ok(super::resp::from_response(r?))
}

/// the hidden attribute of a Request subclass's instance: the request it wraps
pub const INNER_REQUEST: &str = "__py2axum_request";

/// `Request(scope, receive[, send])` for a project subclass of Request without `__init__`: the instance wraps this
/// request (its attributes and methods fall back to it); the scope must be this request's
pub async fn request_subclass(cx: &Cx, s: &'static pyd::SchemaDesc, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if !kwargs.is_empty() || !(2..=3).contains(&args.len()) {
        return Err(Exc::type_error(format!("py2axum: {}(scope, receive) is the only supported form", s.name)));
    }
    if !matches!(&args[0], V::Dict(_)) {
        return Err(Exc::type_error(format!("py2axum: {}(): the scope must be the request's `request.scope`", s.name)));
    }
    let inst = super::methods::schema_new(cx, s, vec![], vec![]).await?;
    if let V::Inst(i) = &inst {
        i.extra.lock().insert(INNER_REQUEST.to_string(), super::request(cx));
    }
    Ok(inst)
}

/// `super().name(...)` in a Request subclass: the wrapped request's method
pub async fn request_super(cx: &Cx, me: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let inner = match me {
        V::Inst(i) => i.extra.lock().get(INNER_REQUEST).cloned(),
        _ => None,
    };
    let inner = inner.ok_or_else(|| Exc::runtime("py2axum: super() outside a Request subclass instance"))?;
    Box::pin(super::methods::call_method(cx, &inner, name, args, kwargs)).await
}

/// A route of a custom `route_class`: the handler its `get_route_handler()` made (once per route, like FastAPI's
/// `APIRoute.__init__`), called with the request; its returned Response is sent
pub async fn custom_route(cx: &Cx, cache: &'static std::sync::OnceLock<V>, make: MakeHandler) -> R<Response> {
    let h = match cache.get() {
        Some(h) => h.clone(),
        None => {
            let h = make(cx).await?;
            cache.get_or_init(|| h).clone()
        }
    };
    let ret = super::methods::call_value(cx, &h, vec![super::request(cx)], vec![]).await?;
    super::asgi::to_response(&ret)
}

pub type MakeHandler = for<'a> fn(&'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<V>> + Send + 'a>>;

static ROUTE_RES: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
/// `include_router(prefix=<runtime value>)`: their values (raw), in the place of `\u{1}<index>\u{1}` in the patterns
static PREFIXES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// The runtime router prefixes (settings, env), read at startup after the module globals, checked as
/// FastAPI's `include_router` does (a failure stops the application, as an import error does). Each value
/// comes with the path operation of an empty route path under it, if any.
pub fn set_prefixes(vals: Vec<(V, Option<&'static str>)>) -> R<()> {
    let mut out = Vec::new();
    for (v, empty_route) in vals {
        let Some(p) = v.as_str() else {
            return Err(Exc::type_error(format!("can only concatenate str (not \"{}\") to str", v.type_name())));
        };
        if !p.is_empty() {
            if !p.starts_with('/') {
                return Err(Exc::msg(&super::v::ASSERTION_ERROR, "A path prefix must start with '/'"));
            }
            if p.ends_with('/') {
                return Err(Exc::msg(&super::v::ASSERTION_ERROR, "A path prefix must not end with '/', as the routes will start with '/'"));
            }
            if p.contains('{') {
                return Err(Exc::msg(&super::v::NOT_IMPLEMENTED_ERROR, format!("py2axum: the router prefix {p:?} has a path parameter: not supported")));
            }
        } else if let Some(op) = empty_route {
            return Err(Exc::msg(&super::v::RUNTIME_ERROR, format!("FastAPIError: Prefix and path cannot be both empty (path operation: {op})")));
        }
        out.push(p.to_string());
    }
    let _ = PREFIXES.set(out);
    Ok(())
}

/// A prefix that may hold runtime prefix markers, with their values (`_IncludedRouter` of the route tree).
pub fn runtime_prefix(prefix: &str) -> String {
    let mut out = prefix.to_string();
    if let Some(ps) = PREFIXES.get() {
        for (i, p) in ps.iter().enumerate() {
            out = out.replace(&format!("\u{1}{i}\u{1}"), p);
        }
    }
    out
}

pub fn route_regex(pattern: &str) -> regex::Regex {
    let mut pat = pattern.to_string();
    if let Some(ps) = PREFIXES.get() {
        for (i, p) in ps.iter().enumerate() {
            pat = pat.replace(&format!("\u{1}{i}\u{1}"), &regex::escape(p));
        }
    }
    regex::Regex::new(&pat).expect("route pattern")
}

/// `urllib.parse.unquote` (what uvicorn puts in `scope["path"]`).
pub fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `urllib.parse.quote(url, safe=":/%#?=@[]!$&'()*+,;")` (Starlette's RedirectResponse).
fn quote_url(s: &str) -> String {
    let mut out = String::new();
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"_.-~:/%#?=@[]!$&'()*+,;".contains(&c) {
            out.push(c as char);
        } else {
            out += &format!("%{c:02X}");
        }
    }
    out
}

/// Routes under an `if` around their `include_router` that is off at startup (`if settings.ENABLE_X:`), by index
/// in ROUTES: never registered in Python, so never matched. Unset when no route is conditional.
static DISABLED: std::sync::OnceLock<Vec<bool>> = std::sync::OnceLock::new();

pub fn set_disabled(off: Vec<bool>) {
    let _ = DISABLED.set(off);
}

#[inline]
fn route_off(i: usize) -> bool {
    DISABLED.get().is_some_and(|d| d[i])
}

/// Starlette's Router: routes tried in declaration order on the decoded path; the first full match
/// (path and method) runs, else the first path-only match raises a 405 HTTPException with its methods,
/// else the path with/without a trailing slash is tried (307 redirect), else a 404 HTTPException
/// (FastAPI apps raise them, for the exception handlers).
pub async fn route(cx: &Cx, routes: &'static [RouteDef]) -> R<Response> {
    let res = ROUTE_RES.get_or_init(|| routes.iter().map(|r| route_regex(r.pattern)).collect());
    let full = unquote(&cx.req.path);
    let path = route_path(&full);
    let mut partial: Option<&RouteDef> = None;
    for (i, (r, re)) in routes.iter().zip(res).enumerate() {
        if route_off(i) {
            continue;
        }
        if let Some(caps) = re.captures(&path) {
            if r.method == cx.req.method {
                let pp: Vec<(String, String)> = re
                    .capture_names()
                    .flatten()
                    .filter_map(|n| caps.name(n).map(|m| (n.to_string(), m.as_str().to_string())))
                    .collect();
                *cx.req.path_params.lock() = pp;
                *cx.req.route.lock() = r.node;
                let _ = cx.route_def.set(r);
                // a `def` endpoint runs in AnyIO's worker threads (logging's `threadName`)
                if r.sync_endpoint {
                    cx.worker.store(true, Ordering::Relaxed);
                }
                if !super::sentry::active() {
                    return run_route(cx, r.run).await;
                }
                super::sentry::route_matched(cx, runtime_prefix(r.path), Some(r.src), true);
                if !r.sync_endpoint {
                    return run_route(cx, r.run).await;
                }
                let saved = super::sentry::context_enter(cx);
                let res = run_route(cx, r.run).await;
                super::sentry::context_exit(cx, saved);
                return res;
            }
            partial.get_or_insert(r);
        }
    }
    if let Some(r) = partial {
        if super::sentry::active() {
            super::sentry::route_matched(cx, runtime_prefix(r.path), Some(r.src), false);
        }
        // the APIRoute's methods (several for `api_route(methods=[...])`; FastAPI joins a set, in no fixed order)
        let allow = match r.node {
            Some(super::routing::Node::Api { methods, .. }) if methods.len() > 1 => methods.join(", "),
            _ => r.method.into(),
        };
        return Err(Exc::http(405, V::str("Method Not Allowed"), vec![("allow".into(), allow)]));
    }
    // routes added at run time (`app.add_route`) come after the declared ones
    if let Some((a, full)) = super::routing::added_match(&path, &cx.req.method) {
        if !full {
            return Err(Exc::http(405, V::str("Method Not Allowed"), vec![("allow".into(), a.methods.join(", "))]));
        }
        let pp: Vec<(String, String)> = match a.re.captures(&path) {
            Some(caps) => a.re.capture_names().flatten().filter_map(|n| caps.name(n).map(|m| (n.to_string(), m.as_str().to_string()))).collect(),
            None => vec![],
        };
        *cx.req.path_params.lock() = pp;
        if super::sentry::active() {
            super::sentry::route_matched(cx, a.path.clone(), None, true);
        }
        if super::rawasgi::is_asgi_app(&a.endpoint) {
            return super::rawasgi::serve(cx, &a.endpoint).await;
        }
        let ret = super::methods::call_value(cx, &a.endpoint, vec![super::request(cx)], vec![]).await?;
        return super::asgi::to_response(&ret);
    }
    if path != "/" {
        // Starlette: `path.rstrip("/")`, every trailing slash (`/tasks/%2F` decodes to `/tasks//` -> `/tasks`), on
        // the scope's path (root_path included), matched without the root_path
        let alt_full = if path.ends_with('/') { full.trim_end_matches('/').to_string() } else { format!("{full}/") };
        let alt = route_path(&alt_full);
        if res.iter().enumerate().any(|(i, re)| !route_off(i) && re.is_match(alt)) || super::routing::added_is_match(alt) {
            let host = cx.req.header("host").unwrap_or_default();
            let mut url = format!("http://{host}{alt_full}");
            if !cx.req.raw_query.is_empty() {
                url += "?";
                url += &cx.req.raw_query;
            }
            return Ok(Response::builder()
                .status(307)
                .header(header::LOCATION, quote_url(&url))
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap());
        }
    }
    Err(Exc::http(404, V::str("Not Found"), vec![]))
}

/// prometheus_fastapi_instrumentator's `get_route_name`: the template of the route matching the request (a full
/// match, else the first path-only match), declared then added at run time; with no match, the path itself when
/// it matches with or without its trailing slash (`redirect_slashes`); None otherwise.
pub fn route_template(cx: &Cx, routes: &'static [RouteDef]) -> Option<String> {
    let res = ROUTE_RES.get_or_init(|| routes.iter().map(|r| route_regex(r.pattern)).collect());
    let lookup = |path: &str| -> Option<String> {
        let mut partial: Option<String> = None;
        for (i, (r, re)) in routes.iter().zip(res).enumerate() {
            if route_off(i) || !re.is_match(path) {
                continue;
            }
            if r.method == cx.req.method {
                return Some(runtime_prefix(r.path));
            }
            partial.get_or_insert_with(|| runtime_prefix(r.path));
        }
        partial.or_else(|| super::routing::added_match(path, &cx.req.method).map(|(a, _)| a.path.clone()))
    };
    let full = unquote(&cx.req.path);
    let path = route_path(&full).to_string();
    if let Some(t) = lookup(&path) {
        return Some(t);
    }
    let toggled = if let Some(p) = path.strip_suffix('/') { p.to_string() } else { format!("{path}/") };
    if !toggled.is_empty() && toggled != path && lookup(&toggled).is_some() {
        return Some(path);
    }
    None
}

/// A route that `route` would run (path and method): declared, then added at run time.
pub fn full_match(raw_path: &str, method: &str, routes: &'static [RouteDef]) -> bool {
    let res = ROUTE_RES.get_or_init(|| routes.iter().map(|r| route_regex(r.pattern)).collect());
    let path = unquote(raw_path);
    if routes.iter().zip(res).enumerate().any(|(i, (r, re))| !route_off(i) && r.method == method && re.is_match(&path)) {
        return true;
    }
    if routes.iter().zip(res).enumerate().any(|(i, (_, re))| !route_off(i) && re.is_match(&path)) {
        return false; // `route` would answer 405 without looking at the added routes: Python decides
    }
    matches!(super::routing::added_match_native(&path, method), Some((_, true)))
}

pub async fn not_found() -> Response {
    json_body(404, r#"{"detail":"Not Found"}"#.to_string(), &[])
}

pub async fn method_not_allowed() -> Response {
    json_body(405, r#"{"detail":"Method Not Allowed"}"#.to_string(), &[])
}

// ---------------------------------------------------------------- asyncio tasks

/// `asyncio.create_task(coro)`: the coroutine runs on its own (it keeps the request's context alive);
/// done callbacks run after it, in order, with the task.
pub struct Task {
    /// (done, pending callbacks)
    state: Mutex<(bool, Vec<V>)>,
    result: Mutex<Option<R>>,
    finished: tokio::sync::Notify,
    /// `task.cancel()`: the coroutine stops at its current await (its future is dropped)
    cancel: tokio::sync::Notify,
    cancelled: std::sync::atomic::AtomicBool,
    /// its exception was read (`await task`): otherwise asyncio logs it when the task is collected
    retrieved: AtomicBool,
    /// the request's context, for that log (Sentry's scope): kept only while an exception may still need it,
    /// so that a finished task stored in the request's own state (`request.state.t = create_task(...)`) is no
    /// cycle through it (CPython's GC collects that one)
    cx: Mutex<Option<Cx>>,
    /// the `name=` given, else asyncio's default `Task-N` (its number taken at creation)
    name: Option<String>,
    no: u64,
    /// the coroutine function, when known at translation: that log's `coro=<f() done, defined at file:line>`
    coro: Option<&'static TaskCoro>,
}

/// A coroutine function of the project as asyncio's task repr shows it.
pub struct TaskCoro {
    pub qualname: &'static str,
    /// relative to the source root
    pub file: &'static str,
    /// `co_firstlineno` (its first decorator's line when decorated)
    pub line: u32,
}

impl Task {
    /// asyncio's « Task exception was never retrieved » message, with the `future: <Task finished ...>` line
    /// of the default exception handler when the coroutine is known
    fn never_retrieved(&self, e: &Exc) -> String {
        let repr = ops::repr(&V::Exc(e.clone())).unwrap_or_default();
        let name = self.name.clone().unwrap_or_else(|| format!("Task-{}", self.no));
        match self.coro {
            Some(c) => format!("Task exception was never retrieved\nfuture: <Task finished name='{name}' coro=<{}() done, \
defined at {}:{}> exception={repr}>", c.qualname, super::logging::source_path(c.file), c.line),
            None => format!("Task exception was never retrieved\nfuture: <Task finished name='{name}' exception={repr}>"),
        }
    }
}

/// asyncio's task names: `Task-1`, `Task-2`... (one counter for the process)
static TASK_NO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Drop for Task {
    fn drop(&mut self) {
        if self.retrieved.load(Ordering::Relaxed) {
            return;
        }
        if let Some(Err(e)) = self.result.lock().take() {
            if !e.isinstance(&CANCELLED_ERROR) {
                let msg = self.never_retrieved(&e);
                super::logging::emit_sync_not_to_sentry("asyncio", super::logging::ERROR, &msg, Some(e.clone()), &super::logging::NO_LOC);
                if let Some(cx) = self.cx.lock().take() {
                    super::sentry::task_never_retrieved(cx, e, msg);
                }
            }
        }
    }
}

/// `await task`: its result (or exception) once it has finished
pub async fn task_result(t: &Arc<Task>) -> R {
    loop {
        let wait = t.finished.notified();
        if let Some(r) = t.result.lock().clone() {
            t.retrieved.store(true, Ordering::Relaxed);
            t.cx.lock().take();
            return r;
        }
        wait.await;
    }
}

/// The coroutine of `create_task(f(args...))`: `f` and its arguments, evaluated by the caller.
pub fn spawn_task(cx: &Cx, f: V, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    spawn_task_at(cx, f, args, kwargs, None, None)
}

/// `asyncio.create_task(f(...), name=...)` of the project: `coro` = `f` when known at translation
pub fn spawn_task_at(cx: &Cx, f: V, args: Vec<V>, kwargs: Vec<(String, V)>, coro: Option<&'static TaskCoro>, name: Option<V>) -> R {
    let cx2 = cx.clone();
    // asyncio: `str(name)` (None: the next `Task-N`)
    let name = match name {
        None | Some(V::None) => None,
        Some(n) => Some(ops::str_(&n)?),
    };
    spawn_named(cx, Box::pin(async move { super::methods::call_value(&cx2, &f, args, kwargs).await }), coro, name)
}

/// A task running `fut` on its own: result kept for `await task`, done callbacks called after.
pub fn spawn_future(cx: &Cx, fut: super::BoxFut<'static>) -> R {
    spawn_named(cx, fut, None, None)
}

fn spawn_named(cx: &Cx, fut: super::BoxFut<'static>, coro: Option<&'static TaskCoro>, name: Option<String>) -> R {
    let task = Arc::new(Task {
        state: Mutex::new((false, Vec::new())),
        result: Mutex::new(None),
        finished: tokio::sync::Notify::new(),
        cancel: tokio::sync::Notify::new(),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        retrieved: AtomicBool::new(false),
        cx: Mutex::new(Some(cx.clone())),
        no: if name.is_none() { TASK_NO.fetch_add(1, Ordering::Relaxed) } else { 0 },
        name,
        coro,
    });
    let tv = V::native(Native::Task(task.clone()));
    let (cx2, tv2) = (cx.clone(), tv.clone());
    tokio::spawn(async move {
        let r = tokio::select! {
            r = fut => r,
            _ = task.cancel.notified() => Err(Exc::new(&CANCELLED_ERROR, vec![])),
        };
        // a result (or a cancellation, never logged) needs no context any more
        if !matches!(&r, Err(e) if !e.isinstance(&CANCELLED_ERROR)) {
            task.cx.lock().take();
        }
        *task.result.lock() = Some(r);
        // done before anyone waiting on the result runs again
        let callbacks = {
            let mut st = task.state.lock();
            st.0 = true;
            std::mem::take(&mut st.1)
        };
        task.finished.notify_waiters();
        for cb in callbacks {
            if let Err(e) = super::methods::call_value(&cx2, &cb, vec![tv2.clone()], vec![]).await {
                super::logging::emit_sync("asyncio", super::logging::ERROR, "Exception in callback", vec![], vec![], Some(e.clone()), &super::logging::NO_LOC);
            }
        }
    });
    Ok(tv)
}

// ---------------------------------------------------------------- lifespan

/// `FastAPI(lifespan=f)` at startup: `f(app)` entered (an async generator function is wrapped like
/// Starlette does); the context manager is kept for the shutdown
pub async fn lifespan_start(cx: &Cx, f: V) -> R {
    let cm = super::methods::call_value(cx, &f, vec![super::routing::app()], vec![]).await?;
    let cm = match super::agen::as_gen(&cm) {
        Some(g) => V::native(Native::Acm(g)),
        None => cm,
    };
    super::aio::aenter(cx, &cm).await?;
    Ok(cm)
}

/// after the server stopped: the lifespan's code after `yield`
pub async fn lifespan_end(cx: &Cx, cm: V) -> R<()> {
    super::aio::aexit(cx, &cm, None).await?;
    Ok(())
}

static SHUTDOWN: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> = std::sync::OnceLock::new();
/// the signal that started the shutdown, raised again once the process is cleaned up (uvicorn's
/// `capture_signals`): the exit status is the signal's, as for the Python server
static SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn shutdown_tx() -> &'static tokio::sync::watch::Sender<bool> {
    SHUTDOWN.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// true once SIGTERM/SIGINT arrived (WebSocket sessions close with 1012, like uvicorn's `shutdown()`)
pub fn shutdown_watch() -> tokio::sync::watch::Receiver<bool> {
    shutdown_tx().subscribe()
}

/// the next SIGINT or SIGTERM; its number
async fn next_signal() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => 15,
            _ = int.recv() => 2,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        2
    }
}

/// SIGINT or SIGTERM (what uvicorn handles): the server stops accepting and drains
pub async fn shutdown_signal() {
    let sig = next_signal().await;
    SIGNAL.store(sig, std::sync::atomic::Ordering::SeqCst);
    super::uvicorn::info("Shutting down");
    let _ = shutdown_tx().send(true);
}

/// resolves PY2AXUM_SHUTDOWN_TIMEOUT seconds (default 25) after the shutdown signal, or at a second signal
/// (uvicorn's force exit)
pub async fn shutdown_deadline() {
    let mut rx = shutdown_tx().subscribe();
    if rx.wait_for(|v| *v).await.is_err() {
        return std::future::pending().await;
    }
    // --timeout-graceful-shutdown (or PY2AXUM_SHUTDOWN_TIMEOUT); uvicorn waits without limit when it is
    // not given, the binary 25 s (an endless stream would hold the shutdown forever)
    let secs: f64 = super::uvicorn::opts().timeout_graceful_shutdown.unwrap_or(25.0);
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs_f64(secs.max(0.0))) => {
            let n = TASKS.load(std::sync::atomic::Ordering::SeqCst).max(1);
            super::logging::emit_sync("uvicorn.error", super::logging::ERROR, "Cancel %s running task(s), timeout graceful shutdown exceeded", vec![V::Int(n as i64)], vec![], None, &super::logging::NO_LOC);
        }
        _ = next_signal() => {}
    }
}

static TASKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static TASKS_DONE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Held by work the shutdown waits for once the server stopped accepting (uvicorn's `server_state.tasks`):
/// WebSocket sessions, which hyper's graceful shutdown no longer tracks once upgraded.
pub struct TaskGuard(());

impl TaskGuard {
    pub fn new() -> Self {
        TASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        TaskGuard(())
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if TASKS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            TASKS_DONE.notify_waiters();
        }
    }
}

/// the server has stopped: wait for the tasks still running (bounded by the caller's deadline)
pub async fn drain_tasks() {
    loop {
        let done = TASKS_DONE.notified();
        tokio::pin!(done);
        done.as_mut().enable();
        if TASKS.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return;
        }
        done.await;
    }
}

/// The end of a graceful shutdown: the pool's connections closed (Terminate sent to PostgreSQL, not a dropped
/// socket), then the signal raised again with its default action, so the process ends the way uvicorn's does.
pub async fn exit_after_shutdown(pool: &sqlx::PgPool) -> ! {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pool.close()).await;
    let sig = SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
    #[cfg(unix)]
    if sig != 0 {
        unsafe extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
            fn raise(sig: i32) -> i32;
        }
        // SAFETY: restores SIG_DFL (0 on Linux and macOS) for the signal, then delivers it to this process
        unsafe {
            signal(sig, 0);
            raise(sig);
        }
    }
    std::process::exit(if sig == 0 { 0 } else { 128 + sig })
}

pub async fn task_method(cx: &Cx, t: &Arc<Task>, recv: &V, name: &str, args: &[V]) -> R {
    match name {
        "add_done_callback" => {
            let cb = args.first().cloned().ok_or_else(|| Exc::type_error("add_done_callback() missing 1 required positional argument: 'fn'"))?;
            let done = {
                let mut st = t.state.lock();
                if !st.0 {
                    st.1.push(cb.clone());
                }
                st.0
            };
            if done {
                // already finished: called now (asyncio schedules it with call_soon)
                if let Err(e) = Box::pin(super::methods::call_value(cx, &cb, vec![recv.clone()], vec![])).await {
                    super::logging::emit_sync("asyncio", super::logging::ERROR, "Exception in callback", vec![], vec![], Some(e.clone()), &super::logging::NO_LOC);
                }
            }
            Ok(V::None)
        }
        "done" => Ok(V::Bool(t.state.lock().0)),
        "cancel" => {
            if t.state.lock().0 {
                return Ok(V::Bool(false));
            }
            t.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            t.cancel.notify_one();
            Ok(V::Bool(true))
        }
        "cancelled" => Ok(V::Bool(t.state.lock().0 && t.cancelled.load(std::sync::atomic::Ordering::SeqCst))),
        _ => Err(Exc::attr_error(format!("'_asyncio.Task' object has no attribute '{name}'"))),
    }
}

/// Starlette's GZipMiddleware writes `Vary: Accept-Encoding`; tower-http's compression `accept-encoding`
pub async fn starlette_vary(mut r: axum::response::Response) -> axum::response::Response {
    let vals: Vec<String> = r.headers().get_all("vary").iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
    if vals.iter().any(|v| v.contains("accept-encoding")) {
        r.headers_mut().remove("vary");
        for v in vals {
            if let Ok(h) = axum::http::HeaderValue::from_str(&v.replace("accept-encoding", "Accept-Encoding")) {
                r.headers_mut().append("vary", h);
            }
        }
    }
    r
}

// ---------------------------------------------------------------- generator endpoints (FastAPI >= 0.134)

/// How a generator endpoint's items are sent: JSON Lines (the default response class), Server-Sent Events
/// (`response_class=EventSourceResponse`) or as they are (`response_class=StreamingResponse`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GenMode {
    Jsonl,
    Sse,
    Raw,
    /// a StreamingResponse subclass with its own `media_type`
    RawAs(&'static str),
}

/// FastAPI's `_PING_INTERVAL`: an SSE stream idle that long gets a keep-alive comment.
const SSE_PING: std::time::Duration = std::time::Duration::from_secs(15);

/// A generator endpoint's response: `item` is the stream item type of its return annotation
/// (`AsyncIterable[Item]`), each item validated and serialized with it; without one, `jsonable_encoder` and
/// `json.dumps`. The body streams one item at a time; an exception of the generator (or of an item's
/// validation) cuts the connection, as uvicorn does once the response has started.
pub async fn gen_response(cx: &Cx, gen: V, mode: GenMode, item: Option<&'static TD>, status: u16) -> R<Response> {
    let g = super::agen::as_gen(&gen).ok_or_else(|| Exc::type_error("py2axum: the generator endpoint returned no generator"))?;
    let status = cx.resp.status.lock().unwrap_or(status);
    let mut b = Response::builder().status(status);
    match mode {
        GenMode::Jsonl => b = b.header(header::CONTENT_TYPE, "application/jsonl"),
        GenMode::Sse => {
            b = b.header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
                .header("cache-control", "no-cache")
                .header("x-accel-buffering", "no")
        }
        GenMode::Raw => {}
        GenMode::RawAs(m) => b = b.header(header::CONTENT_TYPE, super::resp::with_charset(m)),
    }
    for (k, v) in cx.resp.headers.lock().iter() {
        b = b.header(k.as_str(), v.as_str());
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let cx2 = cx.clone();
    tokio::spawn(async move {
        // the producer pulls one item at a time (`__anext__`), the consumer serializes it (an SSE consumer
        // inserts a ping when the producer is idle)
        let (itx, mut irx) = tokio::sync::mpsc::channel::<R>(1);
        let g2 = g.clone();
        tokio::spawn(async move {
            loop {
                let r = g2.asend(V::None).await;
                let end = r.is_err();
                if itx.send(r).await.is_err() {
                    let _ = g2.aclose().await;
                    break;
                }
                if end {
                    break;
                }
            }
        });
        let fail = |e: &Exc| {
            // FastAPI streams Server-Sent Events in an anyio TaskGroup (the keep-alive pings): the error comes out
            // as an ExceptionGroup
            if mode == GenMode::Sse {
                super::uvicorn::exception(&super::trace::task_group_error(e));
            } else {
                super::uvicorn::exception(&super::trace::streaming_error(e));
            }
            Err(std::io::Error::other("generator endpoint failed"))
        };
        loop {
            let next = if mode == GenMode::Sse {
                match tokio::time::timeout(SSE_PING, irx.recv()).await {
                    Ok(n) => n,
                    Err(_) => {
                        if tx.send(Ok(Bytes::from_static(b": ping\n\n"))).await.is_err() {
                            break;
                        }
                        continue;
                    }
                }
            } else {
                irx.recv().await
            };
            let v = match next {
                None => break,
                Some(Ok(v)) => v,
                Some(Err(e)) if e.isinstance(&STOP_ASYNC_ITERATION) => break,
                Some(Err(e)) => {
                    let _ = tx.send(fail(&e)).await;
                    break;
                }
            };
            let out = match gen_item(&cx2, &v, mode, item).await {
                Ok(bytes) => Ok(bytes),
                Err(e) => fail(&e),
            };
            let stop = out.is_err();
            if tx.send(out).await.is_err() || stop {
                break;
            }
        }
    });
    let body = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) });
    use futures_util::StreamExt;
    Ok(b.body(Body::from_stream(body.fuse())).unwrap_or_else(bad_response))
}

/// FastAPI's `_serialize_data`: the stream item field's validation then `serialize_json`, else
/// `json.dumps(jsonable_encoder(item))`.
async fn gen_data(cx: &Cx, v: &V, item: Option<&'static TD>) -> R<String> {
    match item {
        Some(td) => {
            let prepared = prepare_for(v, td)?;
            let mut errs = Vec::new();
            match pyd::validate(cx, &prepared, td, &[V::str("response")], &mut errs).await? {
                Some(x) => pyd::to_json(&pyd::dump(&x, pyd::DumpOpts { json: true, by_alias: true, ..Default::default() }).map_err(pyd::ser_json_error)?, &pyd::DUMP_JSON, false),
                None => {
                    Err(response_validation_error(cx, errs))
                }
            }
        }
        None => {
            let mut data = pyd::jsonable(v)?;
            // FastAPI 0.144 (`_normalize_non_finite_floats`): NaN and infinities become null (json.dumps wrote NaN)
            if super::fastapi_at_least(144) {
                data = finite_floats(&data);
            }
            let s = super::libs::json_dumps(&data, &[])?;
            Ok(ops::str_(&s)?)
        }
    }
}

/// FastAPI's `_normalize_non_finite_floats` over JSON-compatible data (dict keys kept)
fn finite_floats(v: &V) -> V {
    match v {
        V::Float(f) if !f.is_finite() => V::None,
        V::Dict(d) => {
            let items: Vec<(V, V)> = d.lock().values().map(|(k, x)| (k.clone(), finite_floats(x))).collect();
            V::dict_from(items).unwrap_or_else(|_| v.clone())
        }
        V::List(l) => V::list(l.lock().iter().map(finite_floats).collect()),
        _ => v.clone(),
    }
}

async fn gen_item(cx: &Cx, v: &V, mode: GenMode, item: Option<&'static TD>) -> R<Bytes> {
    match mode {
        GenMode::Jsonl => Ok(Bytes::from(gen_data(cx, v, item).await? + "\n")),
        GenMode::Raw | GenMode::RawAs(_) => match v {
            V::Str(s) => Ok(Bytes::from(s.to_string())),
            V::Bytes(b) => Ok(Bytes::from(b.to_vec())),
            // StreamingResponse: `chunk.encode(self.charset)`
            other => Err(Exc::attr_error(format!("'{}' object has no attribute 'encode'", other.type_name()))),
        },
        GenMode::Sse => {
            if let V::Inst(i) = v {
                if std::ptr::eq(i.desc, &SSE_EVENT) {
                    let get = |k: &str| i.extra.lock().get(k).cloned().unwrap_or(V::None);
                    let opt = |k: &str| -> R<Option<String>> { let x = get(k); if x.is_none() { Ok(None) } else { Ok(Some(ops::str_(&x)?)) } };
                    let data = get("data");
                    let data_str = if let Some(raw) = opt("raw_data")? {
                        Some(raw)
                    } else if !data.is_none() {
                        let is_model = matches!(&data, V::Inst(d) if !d.desc.open && !d.desc.dataclass && d.desc.settings.is_none());
                        Some(if is_model {
                            ops::str_(&super::methods::call_method(cx, &data, "model_dump_json", vec![], vec![]).await?)?
                        } else {
                            gen_data(cx, &data, None).await?
                        })
                    } else {
                        None
                    };
                    let retry = get("retry");
                    return Ok(Bytes::from(sse_format(data_str, opt("event")?, opt("id")?,
                                                     if retry.is_none() { None } else { Some(ops::str_(&retry)?) }, opt("comment")?)));
                }
            }
            Ok(Bytes::from(sse_format(Some(gen_data(cx, v, item).await?), None, None, None, None)))
        }
    }
}

/// fastapi.sse.format_sse_event
fn sse_format(data: Option<String>, event: Option<String>, id: Option<String>, retry: Option<String>, comment: Option<String>) -> String {
    let split = |s: &str| s.replace("\r\n", "\n").replace('\r', "\n").split('\n').map(str::to_string).collect::<Vec<_>>();
    let mut lines = Vec::new();
    if let Some(c) = comment {
        lines.extend(split(&c).into_iter().map(|l| format!(": {l}")));
    }
    if let Some(e) = event {
        lines.push(format!("event: {e}"));
    }
    if let Some(d) = data {
        lines.extend(split(&d).into_iter().map(|l| format!("data: {l}")));
    }
    if let Some(i) = id {
        lines.push(format!("id: {i}"));
    }
    if let Some(r) = retry {
        lines.push(format!("retry: {r}"));
    }
    lines.push(String::new());
    lines.push(String::new());
    lines.join("\n")
}

static SSE_EVENT_CLASS: Class = Class { name: "ServerSentEvent", qualname: "fastapi.sse.ServerSentEvent", bases: &[], kind: ClassKind::Schema(&SSE_EVENT) };
/// `fastapi.sse.ServerSentEvent`: its attributes (data, raw_data, event, id, retry, comment) checked at
/// construction like its validators
pub static SSE_EVENT: pyd::SchemaDesc = pyd::SchemaDesc {
    name: "ServerSentEvent",
    class: &SSE_EVENT_CLASS,
    fields: &[],
    from_attributes: false,
    extra: pyd::Extra::Allow,
    validators: &[],
    validate_assignment: false,
    populate_by_name: false,
    methods: &[],
    open: true,
    model_after: &[],
    before: &[],
    model_before: &[],
    has_before: false,
    frozen: false,
    post_init: None,
    hash: pyd::HashKind::Unhashable,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    ser_bytes: pyd::BytesMode::Utf8,
    computed: &[],
    json_schema: None,
    x: pyd::SchemaX::NONE,
};

/// `ServerSentEvent(data=..., raw_data=..., event=..., id=..., retry=..., comment=...)`: str fields are str or
/// None, retry an int >= 0, event and id one line (id without NUL), data and raw_data exclusive. A value
/// pydantic would coerce (a bytes, a numeric str for retry) is refused (TypeError) rather than guessed.
pub fn sse_event(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("BaseModel.__init__() takes 1 positional argument but 2 were given"));
    }
    let o = pyd::object_new(&SSE_EVENT);
    let fields = ["data", "raw_data", "event", "id", "retry", "comment"];
    for f in fields {
        super::methods::setattr(&o, f, V::None)?;
    }
    // pydantic's ValidationError of the model (its validators raise ValueError: `value_error`, ctx.error)
    let bad = |kind: &'static str, loc: Option<&str>, msg: String, input: V, ctx: Option<Vec<(&'static str, V)>>| {
        Exc::validation_titled(
            &super::v::VALIDATION_ERROR,
            vec![ErrDetail { kind, loc: loc.map(|l| vec![V::str(l)]).unwrap_or_default(), msg, input, ctx, custom: false }],
            "ServerSentEvent",
        )
    };
    let value_error = |loc: Option<&str>, m: &str, input: V| {
        bad("value_error", loc, format!("Value error, {m}"), input, Some(vec![("error", V::Exc(Exc::value_error(m)))]))
    };
    for (k, v) in kwargs {
        if !fields.contains(&k.as_str()) {
            return Err(Exc::type_error(format!("py2axum: ServerSentEvent({k}=) is not supported")));
        }
        match (k.as_str(), v) {
            (_, V::None) | ("data", _) => {}
            ("retry", V::Int(n)) if *n >= 0 => {}
            ("retry", V::Int(_)) => {
                return Err(bad("greater_than_equal", Some("retry"), "Input should be greater than or equal to 0".into(), v.clone(), Some(vec![("ge", V::Int(0))])))
            }
            ("retry", _) => return Err(Exc::type_error("py2axum: ServerSentEvent(retry=) must be an int")),
            (_, V::Str(s)) => {
                if (k == "event" || k == "id") && (s.contains('\r') || s.contains('\n')) {
                    return Err(value_error(Some(k), &format!("SSE '{k}' must be a single line"), v.clone()));
                }
                if k == "id" && s.contains('\0') {
                    return Err(value_error(Some("id"), "SSE 'id' must not contain null characters", v.clone()));
                }
            }
            _ => return Err(Exc::type_error(format!("py2axum: ServerSentEvent({k}=) must be a str or None"))),
        }
        super::methods::setattr(&o, k, v.clone())?;
    }
    let has = |k: &str| kwargs.iter().any(|(n, v)| n == k && !v.is_none());
    if has("data") && has("raw_data") {
        let input = V::dict_from(kwargs.iter().map(|(k, v)| (V::str(k), v.clone())).collect())?;
        return Err(value_error(None, "Cannot set both 'data' and 'raw_data' on the same ServerSentEvent. Use 'data' for \
JSON-serialized payloads or 'raw_data' for pre-formatted strings.", input));
    }
    Ok(o)
}

/// FastAPI's `ResponseValidationError(errors, endpoint_ctx=...)` for the route being served
pub fn response_validation_error(cx: &Cx, errs: Vec<pyd::ErrDetail>) -> Exc {
    let e = Exc::validation(&RESPONSE_VALIDATION_ERROR, errs);
    endpoint_context(cx, &e);
    e
}

/// `endpoint_ctx` of FastAPI's validation errors: the endpoint's file, line, function, "METHOD /path"
pub fn endpoint_context(cx: &Cx, e: &Exc) {
    if let Some(r) = cx.route_def.get() {
        let file = r.src.0.rsplit_once(':').map(|(f, _)| f).unwrap_or(r.src.0);
        let mut a = e.0.attrs.lock();
        a.insert("endpoint_file".into(), V::str(super::logging::source_path(file)));
        a.insert("endpoint_line".into(), V::Int(r.first_line as i64));
        a.insert("endpoint_function".into(), V::str(r.src.1));
        a.insert("endpoint_path".into(), V::str(format!("{} {}", r.method, runtime_prefix(r.path))));
    }
}

/// `str()` of FastAPI's ValidationException: the count, each error dict's repr, the endpoint context
pub fn fastapi_validation_str(e: &Exc, errs: &[pyd::ErrDetail]) -> String {
    let n = errs.len();
    let mut out = format!("{n} validation error{}:\n", if n == 1 { "" } else { "s" });
    for d in errs {
        out += &format!("  {}\n", ops::repr(&d.to_v()).unwrap_or_default());
    }
    let a = e.0.attrs.lock();
    let get = |k: &str| a.get(k).filter(|v| ops::truthy(v).unwrap_or(false)).map(|v| ops::str_(v).unwrap_or_default());
    match (get("endpoint_file"), get("endpoint_line"), get("endpoint_function")) {
        (Some(f), Some(l), Some(func)) => {
            out += &format!("\n  File \"{f}\", line {l}, in {func}");
            if let Some(p) = get("endpoint_path") {
                out += &format!("\n    {p}");
            }
        }
        _ => {
            if let Some(p) = get("endpoint_path") {
                out += &format!("\n  Endpoint: {p}");
            }
        }
    }
    out.trim_end().to_string()
}
