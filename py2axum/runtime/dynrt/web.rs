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
            if matches!(td, TD::List(_)) || matches!(td, TD::Optional(TD::List(_))) {
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

/// FastAPI reads and JSON-decodes the body before solving dependencies.
pub fn read_body(cx: &Cx) -> R<Option<V>> {
    let bytes = &cx.req.body;
    if bytes.is_empty() {
        return Ok(None);
    }
    let is_json = match cx.req.header("content-type") {
        None => true,
        Some(ct) => {
            let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            match main.split_once('/') {
                Some(("application", sub)) => sub == "json" || sub.ends_with("+json"),
                _ => false,
            }
        }
    };
    if !is_json {
        return Ok(Some(V::Bytes(Arc::from(&bytes[..]))));
    }
    let text = String::from_utf8_lossy(bytes);
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => Ok(Some(pyd::from_serde(&v))),
        Err(e) => {
            let pos = char_offset(&text, e.line(), e.column());
            Err(Exc::validation(
                &REQUEST_VALIDATION_ERROR,
                vec![ErrDetail {
                    kind: "json_invalid",
                    loc: vec![V::str("body"), V::Int(pos as i64)],
                    msg: "JSON decode error".into(),
                    input: V::empty_dict(),
                    ctx: Some(vec![("error", V::str(e.to_string()))]),
                }],
            ))
        }
    }
}

fn char_offset(text: &str, line: usize, column: usize) -> usize {
    let mut cur = 1;
    for (i, (_, c)) in text.char_indices().enumerate() {
        if cur == line {
            return (i + column.saturating_sub(1)).min(text.chars().count());
        }
        if c == '\n' {
            cur += 1;
        }
    }
    text.chars().count()
}

/// The single body parameter (not embedded): `loc = ["body", ...]`.
pub async fn body_param(cx: &Cx, body: &Option<V>, td: &'static TD, required: bool, default: fn() -> V, errs: &mut Vec<ErrDetail>) -> R {
    match body {
        None => {
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
    let got = match body {
        Some(V::Dict(d)) => d.lock().get(&Key::Str(Arc::from(name))).map(|(_, v)| v.clone()),
        _ => None,
    };
    match got {
        None => {
            if required {
                errs.push(ErrDetail { kind: "missing", loc: vec![V::str("body"), V::str(name)], msg: "Field required".into(), input: body.clone().unwrap_or(V::None), ctx: None });
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
};

pub enum Security {
    /// `OAuth2PasswordBearer(tokenUrl=...)`: the bearer token (str)
    OAuth2Bearer,
    /// `HTTPBearer()`: HTTPAuthorizationCredentials
    HttpBearer,
}

/// A security scheme used as a dependency, as FastAPI 0.142 calls it: `Authorization` header split on
/// the first space (`get_authorization_scheme_param`), 401 `Not authenticated` with `WWW-Authenticate:
/// Bearer`, or None when `auto_error=False`.
pub fn security(cx: &Cx, kind: Security, auto_error: bool) -> R {
    let authorization = cx.req.header("authorization").unwrap_or_default();
    let present = !authorization.is_empty();
    let (scheme, param) = authorization.split_once(' ').unwrap_or((&authorization, ""));
    let param = param.trim();
    let ok = match kind {
        Security::OAuth2Bearer => present && scheme.eq_ignore_ascii_case("bearer"),
        Security::HttpBearer => present && !scheme.is_empty() && !param.is_empty() && scheme.eq_ignore_ascii_case("bearer"),
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
    let raw = if matches!(td, TD::List(_)) || matches!(td, TD::Optional(TD::List(_))) {
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
    let content = match model {
        Some(td) => {
            let prepared = prepare(&ret)?;
            let mut errs = Vec::new();
            match pyd::validate(cx, &prepared, td, &[V::str("response")], &mut errs).await? {
                Some(v) => pyd::dump(&v, pyd::DumpOpts { json: true, by_alias: true, ..Default::default() })?,
                _ => {
                    let detail: Vec<String> = errs.iter().map(|e| format!("{}: {}", e.kind, e.msg)).collect();
                    return Err(Exc::runtime(format!("ResponseValidationError: {}", detail.join("; "))));
                }
            }
        }
        None => pyd::jsonable(&ret)?,
    };
    Ok(json_body(status, pyd::to_json(&content, &pyd::RESPONSE, false)?, &headers))
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
    let obj = &e.0;
    if let Some((code, detail, headers)) = &e.http_info() {
        let body = V::dict_from(vec![(V::str("detail"), pyd::jsonable(detail).unwrap_or(V::None))]).unwrap();
        if no_body(*code) {
            return Response::builder().status(*code).body(Body::empty()).unwrap();
        }
        return json_body(*code, pyd::to_json(&body, &pyd::RESPONSE, true).unwrap_or_default(), headers);
    }
    if e.isinstance(&REQUEST_VALIDATION_ERROR) {
        let errs: Vec<V> = obj.errors.as_ref().map(|v| v.iter().map(|e| e.to_v()).collect()).unwrap_or_default();
        let body = V::dict_from(vec![(V::str("detail"), pyd::jsonable(&V::list(errs)).unwrap_or(V::None))]).unwrap();
        return json_body(422, pyd::to_json(&body, &pyd::RESPONSE, true).unwrap_or_default(), &[]);
    }
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
struct TeardownGuard(Option<Cx>);

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        if let Some(cx) = &self.0 {
            cx.teardowns.lock().clear();
        }
    }
}

// ---------------------------------------------------------------- async generators

pub struct Yielder(pub tokio::sync::mpsc::Sender<V>);

impl Yielder {
    pub async fn send(&self, v: V) -> R<V> {
        self.0.send(v).await.map_err(|_| Exc::new(&GENERATOR_EXIT, vec![]))?;
        Ok(V::None)
    }
}

/// Calling an `async def` generator: its body runs as a task feeding a bounded channel.
pub fn spawn_gen<F>(f: F) -> V
where
    F: FnOnce(Yielder) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'static>>,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<V>(1);
    let fut = f(Yielder(tx));
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            if !e.isinstance(&GENERATOR_EXIT) {
                eprintln!("ERROR:py2axum:exception in async generator: {:?}", e);
            }
        }
    });
    V::native(Native::Gen(Mutex::new(Some(rx))))
}

pub fn streaming_response(content: V, media_type: Option<String>, status: u16, headers: Vec<(String, String)>) -> R {
    let rx = match &content {
        V::Native(n) => match &**n {
            Native::Gen(g) => g.lock().take().ok_or_else(|| Exc::runtime("generator already consumed"))?,
            _ => return Err(Exc::type_error("StreamingResponse needs an async generator")),
        },
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

pub fn log(name: &str, method: &str, args: &[V]) -> R {
    let (level_name, level) = match method {
        "debug" => ("DEBUG", 10),
        "info" => ("INFO", 20),
        "warning" | "warn" => ("WARNING", 30),
        "error" | "exception" => ("ERROR", 40),
        "critical" | "fatal" => ("CRITICAL", 50),
        // logger configuration: the binary's log level comes from PY2AXUM_LOG_LEVEL (documented)
        "setLevel" | "addHandler" | "removeHandler" | "addFilter" | "removeFilter" => return Ok(V::None),
        "isEnabledFor" => return Ok(V::Bool(args.first().and_then(|a| if let V::Int(i) = a { Some(*i) } else { None }).unwrap_or(0) >= min_level() as i64)),
        _ => return Err(Exc::attr_error(format!("'Logger' object has no attribute '{method}'"))),
    };
    if level < min_level() {
        return Ok(V::None);
    }
    let msg = match args.split_first() {
        None => String::new(),
        Some((fmt, rest)) => {
            let f = ops::str_(fmt)?;
            if rest.is_empty() {
                f
            } else {
                ops::percent_format(&f, &V::tuple(rest.to_vec()))?
            }
        }
    };
    eprintln!("{level_name}:{name}:{msg}");
    Ok(V::None)
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
}

static ROUTE_RES: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();

/// `urllib.parse.unquote` (what uvicorn puts in `scope["path"]`).
fn unquote(s: &str) -> String {
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
    let res = ROUTE_RES.get_or_init(|| routes.iter().map(|r| regex::Regex::new(r.pattern).expect("route pattern")).collect());
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
                return run_route(cx, r.run).await;
            }
            partial.get_or_insert(r);
        }
    }
    if let Some(r) = partial {
        return Err(Exc::http(405, V::str("Method Not Allowed"), vec![("allow".into(), r.method.into())]));
    }
    if path != "/" {
        let alt = match path.strip_suffix('/') {
            Some(p) => p.to_string(),
            None => format!("{path}/"),
        };
        if res.iter().any(|re| re.is_match(&alt)) {
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
}

/// `await task`: its result (or exception) once it has finished
pub async fn task_result(t: &Arc<Task>) -> R {
    loop {
        let wait = t.finished.notified();
        if let Some(r) = t.result.lock().clone() {
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
    let task = Arc::new(Task { state: Mutex::new((false, Vec::new())), result: Mutex::new(None), finished: tokio::sync::Notify::new() });
    let tv = V::native(Native::Task(task.clone()));
    let (cx2, tv2) = (cx.clone(), tv.clone());
    tokio::spawn(async move {
        let r = fut.await;
        if let Err(e) = &r {
            eprintln!("ERROR:asyncio:Task exception was never retrieved: {:?}", e);
        }
        *task.result.lock() = Some(r);
        task.finished.notify_waiters();
        let callbacks = {
            let mut st = task.state.lock();
            st.0 = true;
            std::mem::take(&mut st.1)
        };
        for cb in callbacks {
            if let Err(e) = super::methods::call_value(&cx2, &cb, vec![tv2.clone()], vec![]).await {
                eprintln!("ERROR:asyncio:Exception in callback: {:?}", e);
            }
        }
    });
    Ok(tv)
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
