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
    /// the peer address (`request.client`)
    pub client: Option<(String, u16)>,
    pub body: Bytes,
    pub state: Mutex<IndexMap<String, V>>,
    pub disconnected: AtomicBool,
}

impl ReqCell {
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
            body: Bytes::new(),
            state: Mutex::new(IndexMap::new()),
            disconnected: AtomicBool::new(false),
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
            body,
            state: Mutex::new(IndexMap::new()),
            disconnected: AtomicBool::new(false),
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
    let raw = match source {
        "path" => cx.req.path_params.lock().iter().find(|(k, _)| k == alias).map(|(_, v)| V::str(v)),
        "header" => cx.req.header(alias).map(V::str),
        _ => {
            if matches!(td.bare(), TD::List(_)) || matches!(td, TD::Optional(t) if matches!(t.bare(), TD::List(_))) {
                let vals: Vec<V> = cx.req.query.iter().filter(|(k, _)| k == alias).map(|(_, v)| V::str(v)).collect();
                if vals.is_empty() { None } else { Some(V::list(vals)) }
            } else {
                cx.req.query.iter().rev().find(|(k, _)| k == alias).map(|(_, v)| V::str(v))
            }
        }
    };
    let _ = name;
    match raw {
        None => {
            if required {
                errs.push(ErrDetail { kind: "missing", loc: loc2(source, alias), msg: "Field required".into(), input: V::None, ctx: None });
                Ok(V::None)
            } else {
                Ok(default())
            }
        }
        Some(v) => Ok(pyd::validate(cx, &v, td, &loc2(source, alias), errs).await?.unwrap_or(V::None)),
    }
}

static RESPONSE_DUMP_JSON: AtomicBool = AtomicBool::new(true);

/// FastAPI 0.130+ serializes a response_model with pydantic's `dump_json` (set by main.rs).
pub fn set_response_dump_json(on: bool) {
    RESPONSE_DUMP_JSON.store(on, Ordering::Relaxed);
}

fn response_dump_json() -> bool {
    RESPONSE_DUMP_JSON.load(Ordering::Relaxed)
}

static STARLETTE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

/// The project's locked Starlette (set by main.rs): form parsing changed between versions.
pub fn set_starlette(major: u32, minor: u32) {
    let _ = STARLETTE.set((major, minor));
}

fn starlette() -> (u32, u32) {
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
    let bytes = &cx.req.body;
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
    let text = super::libs::json_text(bytes).map_err(|_| bad())?;
    let chars: Vec<char> = text.chars().collect();
    match super::pyjson::decode(&chars) {
        Ok(v) => Ok(Some(v)),
        Err(super::pyjson::Fail::Recursion(_)) => Err(bad()),
        Err(super::pyjson::Fail::Overflow(t)) => Err(super::pyjson::overflow(&t)),
        Err(super::pyjson::Fail::Decode(msg, pos)) => Err(Exc::validation(
            &REQUEST_VALIDATION_ERROR,
            vec![ErrDetail {
                kind: "json_invalid",
                loc: vec![V::str("body"), V::Int(pos as i64)],
                msg: "JSON decode error".into(),
                input: V::empty_dict(),
                ctx: Some(vec![("error", V::str(msg))]),
            }],
        )),
    }
}

/// The single body parameter (not embedded): `loc = ["body", ...]`.
pub async fn body_param(cx: &Cx, body: &Option<V>, td: &'static TD, required: bool, default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    match body {
        // a JSON `null` body is no body (FastAPI's _validate_value_with_model_field): missing, or the default
        None | Some(V::None) => {
            if required {
                errs.push(ErrDetail { kind: "missing", loc: vec![V::str("body")], msg: "Field required".into(), input: V::None, ctx: None });
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
    let missing = |errs: &mut Vec<ErrDetail>| errs.push(ErrDetail { kind: "missing", loc: vec![V::str("body"), V::str(name)], msg: "Field required".into(), input: V::None, ctx: None });
    let got = match body {
        Some(V::Dict(d)) => d.lock().get(&Key::Str(Arc::from(name))).map(|(_, v)| v.clone()).filter(|v| !matches!(v, V::None)),
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
        pyd::FieldDesc { name: "scheme", alias: None, td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
        pyd::FieldDesc { name: "credentials", alias: None, td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
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
    computed: &[],
    json_schema: None,
};

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
        pyd::FieldDesc { name: "username", alias: None, td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
        pyd::FieldDesc { name: "password", alias: None, td: &STR_TD, default: pyd::Dflt::Required, env: None, validate_default: false },
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
    computed: &[],
    json_schema: None,
};

/// `HTTPBasic.__call__`: a missing or non-Basic header is a 401 (None without auto_error); a payload that
/// is not base64 of ASCII `user:password` is a 401 whatever auto_error says.
fn http_basic(cx: &Cx, realm: Option<&str>, auto_error: bool) -> R {
    let challenge = match realm {
        Some(r) => format!("Basic realm=\"{r}\""),
        None => "Basic".into(),
    };
    let denied = || Err(Exc::http(401, V::str("Not authenticated"), vec![("WWW-Authenticate".into(), challenge.clone())]));
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
    Ok(V::Inst(Arc::new(pyd::Inst {
        desc: &BASIC,
        vals: Mutex::new(vec![V::str(user), V::str(password)]),
        set: Mutex::new(vec![true, true]),
        extra: Mutex::new(IndexMap::new()),
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
            return Err(Exc::http(401, V::str("Not authenticated"), vec![("WWW-Authenticate".into(), "Bearer".into())]));
        }
        return Ok(V::None);
    }
    Ok(match kind {
        Security::OAuth2Bearer => V::str(param),
        Security::HttpBearer => V::Inst(Arc::new(pyd::Inst {
            desc: &CREDENTIALS,
            vals: Mutex::new(vec![V::str(scheme), V::str(param)]),
            set: Mutex::new(vec![true, true]),
            extra: Mutex::new(IndexMap::new()),
        })),
        Security::HttpBasic(_) => unreachable!(),
    })
}

// ---------------------------------------------------------------- forms (Form(), File())

pub enum FormVal {
    Text(String),
    File(V),
}

/// `await request.form()` as Starlette parses it: multipart or urlencoded, anything else is empty.
pub async fn read_form(cx: &Cx) -> R<Vec<(String, FormVal)>> {
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
            let body = cx.req.body.clone();
            let stream = futures_util::stream::once(async move { Ok::<Bytes, std::convert::Infallible>(Bytes::from(body.to_vec())) });
            let mut mp = multer::Multipart::new(stream, boundary);
            let mut out = Vec::new();
            // python-multipart keeps what it parsed from a truncated or malformed body
            while let Ok(Some(field)) = mp.next_field().await {
                let name = field.name().unwrap_or("").to_string();
                let filename = field.file_name().map(str::to_string);
                let ctype = field.content_type().map(|m| m.to_string());
                let headers: Vec<(String, String)> =
                    field.headers().iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())).collect();
                let Ok(data) = field.bytes().await else { break };
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
            for part in cx.req.body.split(|b| *b == b'&').filter(|p| !p.is_empty() && limits) {
                if part.len() - usize::from(part.contains(&b'=')) > 1024 * 1024 {
                    return Err(Exc::http(400, V::str("Field exceeded maximum size of 1024KB."), vec![]));
                }
                fields += 1;
                if fields > 1000 {
                    return Err(Exc::http(400, V::str("Too many fields. Maximum number of fields is 1000."), vec![]));
                }
            }
            Ok(form_urlencoded::parse(&cx.req.body).map(|(k, v)| (k.to_string(), FormVal::Text(v.to_string()))).collect())
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

/// A `Form()` parameter: last value, or all of them for a list; "" counts as missing (FastAPI).
pub async fn form_field(cx: &Cx, form: &[(String, FormVal)], alias: &str, td: &'static TD, required: bool, default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    let loc = vec![V::str("body"), V::str(alias)];
    let vals: Vec<&FormVal> = form.iter().filter(|(k, _)| k == alias).map(|(_, v)| v).collect();
    let raw = if matches!(td.bare(), TD::List(_)) || matches!(td, TD::Optional(t) if matches!(t.bare(), TD::List(_))) {
        if vals.is_empty() { None } else { Some(V::list(vals.iter().map(|v| form_value(v)).collect())) }
    } else {
        match vals.last() {
            None => None,
            Some(FormVal::Text(t)) if t.is_empty() => None,
            Some(v) => Some(form_value(v)),
        }
    };
    match raw {
        None if required => {
            errs.push(ErrDetail { kind: "missing", loc, msg: "Field required".into(), input: V::None, ctx: None });
            Ok(V::None)
        }
        None => Ok(default()),
        Some(v) => Ok(pyd::validate(cx, &v, td, &loc, errs).await?.unwrap_or(V::None)),
    }
}

/// A `File()` / `UploadFile` parameter (one file, or `list[UploadFile]`).
pub fn form_file(form: &[(String, FormVal)], alias: &str, list: bool, required: bool, errs: &mut Vec<ErrDetail>) -> V {
    let loc = vec![V::str("body"), V::str(alias)];
    let vals: Vec<&FormVal> = form.iter().filter(|(k, _)| k == alias).map(|(_, v)| v).collect();
    let present: Vec<&FormVal> = vals.into_iter().filter(|v| !matches!(v, FormVal::Text(t) if t.is_empty())).collect();
    if present.is_empty() {
        if required {
            errs.push(ErrDetail { kind: "missing", loc, msg: "Field required".into(), input: V::None, ctx: None });
        }
        return V::None;
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
                    ctx: Some(vec![("error", V::empty_dict())]),
                });
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

pub fn check(errs: Vec<ErrDetail>) -> R<()> {
    if errs.is_empty() { Ok(()) } else { Err(Exc::validation(&REQUEST_VALIDATION_ERROR, errs)) }
}

// ---------------------------------------------------------------- responses

pub struct Streaming {
    pub rx: tokio::sync::mpsc::Receiver<V>,
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
    b.body(Body::from(body)).unwrap()
}

fn no_body(status: u16) -> bool {
    status < 200 || status == 204 || status == 304
}

/// Endpoint return value -> response (response_model validation + Pydantic JSON, or jsonable_encoder).
pub async fn respond(cx: &Cx, ret: V, model: Option<&'static TD>, status: u16) -> R<Response> {
    respond_as(cx, ret, model, Some(status), None).await
}

/// Same, with the route's `response_class=` (`cls` = its class name, None = JSONResponse) and its
/// `status_code=` (None = not given: the class default, 307 for RedirectResponse). FastAPI builds
/// `response_class(content, status_code=..., background=...)` from the encoded value, empties the body
/// for a no-body status, then appends the `response` parameter's headers.
pub async fn respond_as(cx: &Cx, ret: V, model: Option<&'static TD>, status: Option<u16>, cls: Option<&'static str>) -> R<Response> {
    let Some(cls) = cls.filter(|c| *c != "JSONResponse") else {
        return respond_json(cx, ret, model, status.unwrap_or(200)).await;
    };
    if let V::Native(n) = &ret {
        if matches!(&**n, Native::RespObj(_) | Native::Streaming(_)) {
            return respond_json(cx, ret, model, status.unwrap_or(200)).await;
        }
    }
    if cls == "StreamingResponse" {
        return Err(Exc::runtime("py2axum: response_class=StreamingResponse with an endpoint that does not return a response is not supported"));
    }
    let content = encode(cx, &ret, model).await?;
    let mut kwargs = Vec::new();
    if let Some(st) = cx.resp.status.lock().or(status) {
        kwargs.push(("status_code".to_string(), V::Int(st as i64)));
    }
    let key = if cls == "RedirectResponse" { "url" } else if cls == "FileResponse" { "path" } else { "content" };
    let obj = super::resp::new(cls, &[], &[vec![(key.to_string(), content)], kwargs].concat())?;
    let V::Native(n) = &obj else { unreachable!() };
    let Native::RespObj(r) = &**n else { unreachable!() };
    let st = *r.status.lock();
    if no_body(st) {
        if let super::resp::RespBody::Bytes(_) = &r.body {
            let r2 = super::resp::RespObj { status: parking_lot::Mutex::new(st), headers: parking_lot::Mutex::new(r.headers.lock().clone()),
                                            body: super::resp::RespBody::Bytes(vec![]), media: r.media.clone() };
            r2.headers.lock().extend(cx.resp.headers.lock().iter().cloned());
            return super::resp::into_response(&r2);
        }
    }
    r.headers.lock().extend(cx.resp.headers.lock().iter().cloned());
    super::resp::into_response(r)
}

/// `serialize_response`: the response_model's JSON dump, or jsonable_encoder.
async fn encode(cx: &Cx, ret: &V, model: Option<&'static TD>) -> R<V> {
    match model {
        Some(td) => {
            let prepared = prepare_for(ret, td)?;
            let mut errs = Vec::new();
            match pyd::validate(cx, &prepared, td, &[V::str("response")], &mut errs).await? {
                Some(v) => pyd::dump(&v, pyd::DumpOpts { json: true, by_alias: true, ..Default::default() }),
                _ => {
                    let detail: Vec<String> = errs.iter().map(|e| format!("{}: {}", e.kind, e.msg)).collect();
                    Err(Exc::runtime(format!("ResponseValidationError: {}", detail.join("; "))))
                }
            }
        }
        None => pyd::jsonable(ret),
    }
}

async fn respond_json(cx: &Cx, ret: V, model: Option<&'static TD>, status: u16) -> R<Response> {
    if let V::Native(n) = &ret {
        // a Response object is sent as is (no serialization, the `response` parameter is not merged)
        if let Native::RespObj(r) = &**n {
            return super::resp::into_response(r);
        }
        if let Native::Streaming(s) = &**n {
            let s = s.lock().take().ok_or_else(|| Exc::runtime("response already consumed"))?;
            return Ok(stream_response(s, &cx.resp.headers.lock().clone()));
        }
    }
    let status = cx.resp.status.lock().unwrap_or(status);
    let headers = cx.resp.headers.lock().clone();
    if no_body(status) {
        let mut b = Response::builder().status(status).header(header::CONTENT_TYPE, "application/json");
        for (k, v) in &headers {
            b = b.header(k.as_str(), v.as_str());
        }
        return Ok(b.body(Body::empty()).unwrap());
    }
    let content = encode(cx, &ret, model).await?;
    let style = if model.is_some() && response_dump_json() { &pyd::DUMP_JSON } else { &pyd::RESPONSE };
    Ok(json_body(status, pyd::to_json(&content, style, false)?, &headers))
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
    let stream = futures_util::stream::unfold(s.rx, |mut rx| async move {
        rx.recv().await.map(|v| {
            let bytes = match v {
                V::Str(s) => Bytes::from(s.to_string()),
                V::Bytes(b) => Bytes::from(b.to_vec()),
                other => Bytes::from(ops::str_(&other).unwrap_or_default()),
            };
            (Ok::<Bytes, std::io::Error>(bytes), rx)
        })
    })
    .fuse();
    b.body(Body::from_stream(stream)).unwrap()
}

/// Exception escaping the endpoint -> HTTP response (FastAPI's default exception handlers).
pub fn error_response(e: Exc) -> Response {
    try_error_response(e).unwrap_or_else(internal_error)
}

/// FastAPI's handlers for HTTPException and RequestValidationError. Their JSONResponse fails on
/// NaN/inf (json.dumps(allow_nan=False), in a detail or in a validation error's raw `input`): the
/// ValueError leaves the ExceptionMiddleware, ServerErrorMiddleware answers 500 outside every other
/// middleware.
pub fn try_error_response(e: Exc) -> R<Response> {
    let obj = &e.0;
    if let Some((code, detail, headers)) = &e.http_info() {
        let body = V::dict_from(vec![(V::str("detail"), pyd::jsonable(detail).unwrap_or(V::None))]).unwrap();
        if no_body(*code) {
            return Ok(Response::builder().status(*code).body(Body::empty()).unwrap());
        }
        return Ok(json_body(*code, pyd::to_json(&body, &pyd::RESPONSE, true)?, headers));
    }
    if e.isinstance(&REQUEST_VALIDATION_ERROR) {
        let errs: Vec<V> = obj.errors.as_ref().map(|v| v.iter().map(|e| e.to_v()).collect()).unwrap_or_default();
        let body = V::dict_from(vec![(V::str("detail"), pyd::jsonable(&V::list(errs)).unwrap_or(V::None))]).unwrap();
        return Ok(json_body(422, pyd::to_json(&body, &pyd::RESPONSE, true)?, &[]));
    }
    Ok(internal_error(e))
}

fn internal_error(e: Exc) -> Response {
    eprintln!("ERROR:py2axum:Exception in ASGI application: {:?}", e);
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("Internal Server Error"))
        .unwrap()
}

/// Run the matched route on the request's context: teardowns, session commit, background tasks.
/// An exception escapes after the dependencies' exit code (for the exception handlers).
async fn run_route(cx: &Cx, run: RunFn) -> R<Response> {
    let mut guard = TeardownGuard(Some(cx.clone()));
    let r = match run(cx).await {
        Ok(r) => r,
        // the exception goes through the dependencies' exit code, then the session dependency:
        // rollback (the transaction is dropped)
        Err(e) => {
            run_teardowns(cx, false).await;
            return Err(e);
        }
    };
    let session = match cx.session.get() {
        Some(V::Session(s)) if cx.app.commit_after => s.clone(),
        _ => {
            run_teardowns(cx, true).await;
            tokio::spawn(super::resp::run_background(cx.clone()));
            return Ok(r);
        }
    };
    // `yield s; await s.commit()`: FastAPI (>= 0.121, scope "request") runs it once the response is
    // sent; a failure is logged and the client keeps its response. A full body is committed before
    // being returned (same response, no read-after-write race), a stream at its end.
    use axum::body::HttpBody as _;
    if r.body().size_hint().exact().is_some() {
        run_teardowns(cx, true).await;
        if let Err(e) = session.commit().await {
            eprintln!("ERROR:py2axum:Exception in ASGI application: {:?}", e);
        }
        tokio::spawn(super::resp::run_background(cx.clone()));
        return Ok(r);
    }
    use futures_util::StreamExt;
    let (parts, body) = r.into_parts();
    guard.0 = None; // the stream's tail runs them
    let cx = cx.clone();
    let tail = futures_util::stream::once(async move {
        run_teardowns(&cx, true).await;
        if let Err(e) = session.commit().await {
            eprintln!("ERROR:py2axum:Exception in ASGI application: {:?}", e);
        }
        super::resp::run_background(cx.clone()).await;
        drop(cx);
        Ok::<Bytes, axum::Error>(Bytes::new())
    });
    Ok(Response::from_parts(parts, Body::from_stream(body.into_data_stream().chain(tail))))
}

// ---------------------------------------------------------------- generator dependencies

/// The `yield` of a dependency: hands the value to the request, then waits for its end.
pub struct DepYield {
    val: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<V>>>,
    resume: parking_lot::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl DepYield {
    pub async fn yield_(&self, v: V) -> R {
        let tx = self.val.lock().take().ok_or_else(|| Exc::runtime("generator didn't stop"))?;
        let _ = tx.send(v);
        let rx = self.resume.lock().take().expect("resume channel");
        // dropped (the endpoint failed or the request was abandoned): the code after `yield` does not
        // run, `finally` blocks do (in Python the exception is raised at the `yield`)
        rx.await.map_err(|_| Exc::new(&GENERATOR_EXIT, vec![]))?;
        Ok(V::None)
    }
}

pub struct DepTeardown {
    resume: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<R>,
}

/// Solving a dependency written as a generator: its body runs as a task up to its `yield`.
pub async fn dep_gen<F>(cx: &Cx, f: F) -> R
where
    F: FnOnce(Cx, Arc<DepYield>) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'static>>,
{
    let (vtx, vrx) = tokio::sync::oneshot::channel();
    let (rtx, rrx) = tokio::sync::oneshot::channel();
    let y = Arc::new(DepYield { val: parking_lot::Mutex::new(Some(vtx)), resume: parking_lot::Mutex::new(Some(rrx)) });
    let task = tokio::spawn(f(cx.clone(), y));
    match vrx.await {
        Ok(v) => {
            cx.teardowns.lock().push(DepTeardown { resume: rtx, task });
            Ok(v)
        }
        Err(_) => match task.await {
            Ok(Err(e)) => Err(e),
            Ok(Ok(_)) => Err(Exc::runtime("generator didn't yield")),
            Err(e) => Err(Exc::runtime(format!("generator dependency task: {e}"))),
        },
    }
}

/// End of the request: the dependencies' code after `yield`, last solved first (FastAPI's exit stack).
pub async fn run_teardowns(cx: &Cx, ok: bool) {
    let tds = std::mem::take(&mut *cx.teardowns.lock());
    for td in tds.into_iter().rev() {
        if ok {
            let _ = td.resume.send(());
        } else {
            drop(td.resume);
        }
        match td.task.await {
            Ok(Err(e)) if !e.isinstance(&GENERATOR_EXIT) => eprintln!("ERROR:py2axum:Exception in ASGI application: {:?}", e),
            Err(e) => eprintln!("ERROR:py2axum:Exception in ASGI application: {e}"),
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
        run_teardowns(cx, false).await;
        return Err(e);
    }
    run_teardowns(cx, true).await;
    if let Some(V::Session(s)) = cx.session.get() {
        if cx.app.commit_after {
            s.commit().await?;
        }
    }
    Ok(())
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
    let rx = match &content {
        V::Native(n) if matches!(&**n, Native::Gen(_)) => match &**n {
            Native::Gen(g) => super::agen::into_channel(g.clone()),
            _ => unreachable!(),
        },
        // a synchronous iterable (a list, `io.StringIO(...)`: its lines), which Starlette iterates in a
        // thread pool; anything not iterable is its TypeError
        _ => {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let items = ops::iter(&content)?;
            tokio::spawn(async move {
                for it in items {
                    if tx.send(it).await.is_err() {
                        break;
                    }
                }
            });
            rx
        }
    };
    Ok(V::native(Native::Streaming(Mutex::new(Some(Streaming { rx, media_type, status, headers })))))
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

/// `await asyncio.wait_for(coro, timeout=...)`
pub async fn wait_for<F>(timeout: &V, fut: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let secs = match timeout {
        V::None => return fut.await,
        V::Int(i) => *i as f64,
        V::Float(f) => *f,
        other => return Err(Exc::type_error(format!("timeout must be a number, not {}", other.type_name()))),
    };
    match tokio::time::timeout(std::time::Duration::from_secs_f64(secs.max(0.0)), fut).await {
        Ok(r) => r,
        Err(_) => Err(Exc::new(&TIMEOUT_ERROR, vec![])),
    }
}

pub async fn sleep(secs: &V) -> R {
    let s = match secs {
        V::Int(i) => *i as f64,
        V::Float(f) => *f,
        _ => 0.0,
    };
    tokio::time::sleep(std::time::Duration::from_secs_f64(s.max(0.0))).await;
    Ok(V::None)
}

// ---------------------------------------------------------------- logging

static LEVELS: [(&str, i32); 6] = [("DEBUG", 10), ("INFO", 20), ("WARNING", 30), ("ERROR", 40), ("CRITICAL", 50), ("EXCEPTION", 40)];

fn min_level() -> i32 {
    static L: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *L.get_or_init(|| {
        let name = std::env::var("PY2AXUM_LOG_LEVEL").unwrap_or_else(|_| "INFO".into()).to_uppercase();
        LEVELS.iter().find(|(n, _)| *n == name).map(|(_, l)| *l).unwrap_or(20)
    })
}

/// `Logger.<level>(msg, *args, exc_info=, extra=, stack_info=, stacklevel=)`: printed on stderr when the
/// level passes PY2AXUM_LOG_LEVEL, then handed to Sentry's logging integration (when initialised; its
/// future is boxed so that the common path stays small).
pub async fn log(cx: &Cx, name: &str, method: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    if method == "isEnabledFor" {
        return Ok(V::Bool(args.first().and_then(|a| if let V::Int(i) = a { Some(*i) } else { None }).unwrap_or(0) >= min_level() as i64));
    }
    if let Some(rec) = log_print(cx, name, method, args, kwargs)? {
        Box::pin(super::sentry::log_record(cx, &rec.0, rec.1, &rec.2, &rec.3, &rec.4, rec.5, rec.6)).await;
    }
    Ok(V::None)
}

/// A record for Sentry: logger, level, msg, args, formatted message, exception, extra.
type LogRec = (String, i64, V, Vec<V>, String, Option<Exc>, Option<V>);

fn log_print(cx: &Cx, name: &str, method: &str, args: &[V], kwargs: &[(String, V)]) -> R<Option<LogRec>> {
    if method == "log" {
        // Logger.log(level, msg, *args): the standard levels (CPython names others "Level N")
        let m = match args.first() {
            Some(V::Int(10)) => "debug",
            Some(V::Int(20)) => "info",
            Some(V::Int(30)) => "warning",
            Some(V::Int(40)) => "error",
            Some(V::Int(50)) => "critical",
            Some(V::Int(l)) => return Err(Exc::type_error(format!("py2axum: logging at level {l} is not supported (standard levels only)"))),
            _ => return Err(Exc::type_error("level must be an integer")),
        };
        return log_print(cx, name, m, &args[1..], kwargs);
    }
    let (level_name, level) = match method {
        "debug" => ("DEBUG", 10),
        "info" => ("INFO", 20),
        "warning" | "warn" => ("WARNING", 30),
        "error" | "exception" => ("ERROR", 40),
        "critical" | "fatal" => ("CRITICAL", 50),
        // logger configuration: the binary's log level comes from PY2AXUM_LOG_LEVEL (documented)
        "setLevel" | "addHandler" | "removeHandler" | "addFilter" | "removeFilter" => return Ok(None),
        _ => return Err(Exc::attr_error(format!("'Logger' object has no attribute '{method}'"))),
    };
    let mut exc_info = if method == "exception" { Some(V::Bool(true)) } else { None };
    let mut extra = None;
    for (k, v) in kwargs {
        match k.as_str() {
            "exc_info" => exc_info = Some(v.clone()),
            "extra" => extra = Some(v.clone()),
            "stack_info" | "stacklevel" => {}
            _ => return Err(Exc::type_error(format!("Logger._log() got an unexpected keyword argument '{k}'"))),
        }
    }
    if level < min_level() {
        return Ok(None);
    }
    let msg = match args.split_first() {
        None => String::new(),
        Some((fmt, rest)) => {
            let f = ops::str_(fmt)?;
            match rest {
                [] => f,
                // a single non-empty mapping is the record's args (`log.info("%(a)s", {"a": 1})`)
                [d @ V::Dict(m)] if !m.lock().is_empty() => ops::percent_format(&f, d)?,
                _ => ops::percent_format(&f, &V::tuple(rest.to_vec()))?,
            }
        }
    };
    eprintln!("{level_name}:{name}:{msg}");
    if !super::sentry::active() {
        return Ok(None);
    }
    let exc = match &exc_info {
        Some(V::Exc(e)) => Some(e.clone()),
        Some(V::Tuple(t)) if t.len() == 3 => match &t[1] {
            V::Exc(e) => Some(e.clone()),
            _ => None,
        },
        Some(v) if ops::truthy(v)? => cx.handling.lock().last().cloned(),
        _ => None,
    };
    let extra = match extra {
        Some(V::Dict(m)) => {
            let items: Vec<(V, V)> = m.lock().values().filter(|(k, _)| !k.as_str().is_some_and(|s| s.starts_with('_'))).cloned().collect();
            Some(V::dict_from(items)?)
        }
        _ => None,
    };
    let (fmt, rest) = match args.split_first() {
        Some((f, r)) => (f.clone(), r.to_vec()),
        None => (V::str(""), vec![]),
    };
    Ok(Some((name.to_string(), level as i64, fmt, rest, msg, exc, extra)))
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
}

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

/// Starlette's Router: routes tried in declaration order on the decoded path; the first full match
/// (path and method) runs, else the first path-only match raises a 405 HTTPException with its methods,
/// else the path with/without a trailing slash is tried (307 redirect), else a 404 HTTPException
/// (FastAPI apps raise them, for the exception handlers).
pub async fn route(cx: &Cx, routes: &'static [RouteDef]) -> R<Response> {
    let res = ROUTE_RES.get_or_init(|| routes.iter().map(|r| route_regex(r.pattern)).collect());
    let path = unquote(&cx.req.path);
    let mut partial: Option<&RouteDef> = None;
    for (r, re) in routes.iter().zip(res) {
        if let Some(caps) = re.captures(&path) {
            if r.method == cx.req.method {
                let pp: Vec<(String, String)> = re
                    .capture_names()
                    .flatten()
                    .filter_map(|n| caps.name(n).map(|m| (n.to_string(), m.as_str().to_string())))
                    .collect();
                *cx.req.path_params.lock() = pp;
                *cx.req.route.lock() = r.node;
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
        return Err(Exc::http(405, V::str("Method Not Allowed"), vec![("allow".into(), r.method.into())]));
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
        // Starlette: `path.rstrip("/")`, every trailing slash (`/tasks/%2F` decodes to `/tasks//` -> `/tasks`)
        let alt = if path.ends_with('/') { path.trim_end_matches('/').to_string() } else { format!("{path}/") };
        if res.iter().any(|re| re.is_match(&alt)) || super::routing::added_is_match(&alt) {
            let host = cx.req.header("host").unwrap_or_default();
            let mut url = format!("http://{host}{alt}");
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

/// A route that `route` would run (path and method): declared, then added at run time.
pub fn full_match(raw_path: &str, method: &str, routes: &'static [RouteDef]) -> bool {
    let res = ROUTE_RES.get_or_init(|| routes.iter().map(|r| route_regex(r.pattern)).collect());
    let path = unquote(raw_path);
    if routes.iter().zip(res).any(|(r, re)| r.method == method && re.is_match(&path)) {
        return true;
    }
    if routes.iter().zip(res).any(|(_, re)| re.is_match(&path)) {
        return false; // `route` would answer 405 without looking at the added routes: Python decides
    }
    matches!(super::routing::added_match(&path, method), Some((_, true)))
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
    cx: Cx,
}

impl Drop for Task {
    fn drop(&mut self) {
        if self.retrieved.load(Ordering::Relaxed) {
            return;
        }
        if let Some(Err(e)) = self.result.lock().take() {
            if !e.isinstance(&CANCELLED_ERROR) {
                eprintln!("ERROR:asyncio:Task exception was never retrieved: {:?}", e);
                super::sentry::task_never_retrieved(self.cx.clone(), e);
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
            return r;
        }
        wait.await;
    }
}

/// The coroutine of `create_task(f(args...))`: `f` and its arguments, evaluated by the caller.
pub fn spawn_task(cx: &Cx, f: V, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let cx2 = cx.clone();
    spawn_future(cx, Box::pin(async move { super::methods::call_value(&cx2, &f, args, kwargs).await }))
}

/// A task running `fut` on its own: result kept for `await task`, done callbacks called after.
pub fn spawn_future(cx: &Cx, fut: super::BoxFut<'static>) -> R {
    let task = Arc::new(Task {
        state: Mutex::new((false, Vec::new())),
        result: Mutex::new(None),
        finished: tokio::sync::Notify::new(),
        cancel: tokio::sync::Notify::new(),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        retrieved: AtomicBool::new(false),
        cx: cx.clone(),
    });
    let tv = V::native(Native::Task(task.clone()));
    let (cx2, tv2) = (cx.clone(), tv.clone());
    tokio::spawn(async move {
        let r = tokio::select! {
            r = fut => r,
            _ = task.cancel.notified() => Err(Exc::new(&CANCELLED_ERROR, vec![])),
        };
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
                eprintln!("ERROR:asyncio:Exception in callback: {:?}", e);
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
    eprintln!("INFO:     Shutting down");
    let _ = shutdown_tx().send(true);
}

/// resolves PY2AXUM_SHUTDOWN_TIMEOUT seconds (default 25) after the shutdown signal, or at a second signal
/// (uvicorn's force exit)
pub async fn shutdown_deadline() {
    let mut rx = shutdown_tx().subscribe();
    if rx.wait_for(|v| *v).await.is_err() {
        return std::future::pending().await;
    }
    let secs: f64 = std::env::var("PY2AXUM_SHUTDOWN_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(25.0);
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs_f64(secs.max(0.0))) => {
            eprintln!("WARNING:  py2axum: shutdown timeout, open connections dropped");
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
                    eprintln!("ERROR:asyncio:Exception in callback: {:?}", e);
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
