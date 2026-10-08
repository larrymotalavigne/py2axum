//! sentry-sdk 2.x (`sentry_sdk`) on the Rust SDK (crate `sentry` 0.46).
//!
//! The crate provides the client: DSN, transport thread, rate limits, flush on shutdown. Everything the
//! Python SDK decides is done here the way it does it, so Sentry receives the same events: the scopes
//! (an isolation scope per request forked from the one of the import, `new_scope`/`push_scope`), the
//! FastAPI/Starlette integration (unhandled exceptions, HTTPException 5xx, request data, a transaction
//! per route), the logging integration (breadcrumbs, events from ERROR), dedupe, the event scrubber, the
//! serializer (databag limits, `max_value_length`, `_meta`) and the `before_send` callbacks, which are
//! the project's own functions, called with the event as a dict.
//!
//! What cannot be the same is the platform: there is no Python stack (an exception carries one frame,
//! its route handler), no `modules`/`sys.argv`/runtime context, no child spans. Every event and
//! transaction carries the tag `py2axum.source` (the route's `file.py:line`, or the call site outside
//! a request).
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Map, Value as J};

use super::v::*;
use super::{ops, Cx};

// ---------------------------------------------------------------- client options

#[derive(Clone, Copy, PartialEq)]
enum BodySize {
    Never,
    Small,
    Medium,
    Always,
}

pub struct Opts {
    client: Arc<sentry::Client>,
    public_key: String,
    environment: String,
    release: Option<String>,
    server_name: Option<String>,
    dist: Option<String>,
    pii: bool,
    max_value_length: Option<usize>,
    max_breadcrumbs: usize,
    sample_rate: f64,
    traces_sample_rate: Option<f64>,
    traces_sampler: Option<V>,
    before_send: Option<V>,
    before_send_transaction: Option<V>,
    body_size: BodySize,
    /// StarletteIntegration / FastApiIntegration (explicit or auto-enabled)
    starlette: bool,
    /// LoggingIntegration: (breadcrumb level, event level)
    logging: Option<(Option<i64>, Option<i64>)>,
    integrations: Vec<&'static str>,
}

impl Opts {
    fn tracing(&self) -> bool {
        self.traces_sample_rate.is_some() || self.traces_sampler.is_some()
    }
}

static ON: AtomicBool = AtomicBool::new(false);
static OPTS: RwLock<Option<Arc<Opts>>> = RwLock::new(None);
/// set while the module globals are evaluated (Python's import): AsyncioIntegration has no loop to patch
static IMPORTING: AtomicBool = AtomicBool::new(false);

/// The project's sentry-sdk: before 2.67 the request's sensitive headers are removed (`""` annotated
/// `!config x`) rather than replaced by `[Filtered]`. Its Starlette integration reads form bodies only when
/// python-multipart is installed.
static OLD_HEADERS: AtomicBool = AtomicBool::new(false);
static MULTIPART: AtomicBool = AtomicBool::new(true);

pub fn set_sdk(version: &str, multipart: bool) {
    let mut it = version.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    let (major, minor) = (it.next().unwrap_or(2), it.next().unwrap_or(0));
    OLD_HEADERS.store((major, minor) < (2, 67), Ordering::Relaxed);
    MULTIPART.store(multipart, Ordering::Relaxed);
}

fn opts() -> Option<Arc<Opts>> {
    if !ON.load(Ordering::Relaxed) {
        return None;
    }
    OPTS.read().clone()
}

/// Is a client with a DSN installed? (one atomic load: the request hooks cost nothing otherwise)
#[inline]
pub fn active() -> bool {
    ON.load(Ordering::Relaxed)
}

pub fn set_importing(v: bool) {
    IMPORTING.store(v, Ordering::Relaxed);
}

/// The process ends: what is queued is sent within `shutdown_timeout` (2 s by default, the Python SDK's
/// atexit integration).
pub fn shutdown() {
    if let Some(o) = opts() {
        o.client.close(None);
    }
}

// ---------------------------------------------------------------- integrations

/// An integration object (`FastApiIntegration()`...), read by `init(integrations=[...])`.
pub struct Integ {
    pub kind: &'static str,
    level: Option<i64>,
    event_level: Option<i64>,
}

fn kwarg<'a>(kw: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kw.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn level_arg(v: Option<&V>, default: i64, what: &str) -> R<Option<i64>> {
    match v {
        None => Ok(Some(default)),
        Some(V::None) => Ok(None),
        Some(V::Int(i)) => Ok(Some(*i)),
        Some(o) => Err(Exc::type_error(format!("py2axum: LoggingIntegration({what}=) must be an int or None, not {}", o.type_name()))),
    }
}

/// `sentry_sdk.integrations.<x>.<X>Integration(...)`: the options are checked at transpile time.
pub fn integration(kind: &'static str, kw: Vec<(String, V)>) -> R {
    let mut i = Integ { kind, level: None, event_level: None };
    match kind {
        "fastapi" | "starlette" => {
            if let Some(ts) = kwarg(&kw, "transaction_style") {
                if ts.as_str() != Some("url") {
                    return Err(Exc::value_error(format!(
                        "py2axum: {kind} integration: transaction_style={} is not supported (\"url\" only)",
                        ops::repr(ts)?
                    )));
                }
            }
        }
        "logging" => {
            i.level = level_arg(kwarg(&kw, "level"), 20, "level")?;
            i.event_level = level_arg(kwarg(&kw, "event_level"), 40, "event_level")?;
        }
        _ => {}
    }
    Ok(V::native(Native::Sentry(Arc::new(Obj::Integration(i)))))
}

// ---------------------------------------------------------------- init

fn opt_str(v: Option<&V>) -> R<Option<String>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(x) => Ok(Some(ops::str_(x)?)),
    }
}

fn opt_rate(v: Option<&V>, what: &str) -> R<Option<f64>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(V::Bool(b)) => Ok(Some(if *b { 1.0 } else { 0.0 })),
        Some(V::Int(i)) => Ok(Some(*i as f64)),
        Some(V::Float(f)) => Ok(Some(*f)),
        Some(o) => Err(Exc::type_error(format!("py2axum: sentry_sdk.init({what}=) must be a number, not {}", o.type_name()))),
    }
}

fn opt_fn(v: Option<&V>) -> Option<V> {
    v.filter(|x| !x.is_none()).cloned()
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

/// `sentry_sdk.init(...)`: a DSN (argument or `SENTRY_DSN`) installs a client; without one the SDK
/// stays inactive, like Python's (every capture returns None).
pub async fn init(cx: &Cx, args: Vec<V>, kw: Vec<(String, V)>) -> R {
    let dsn = match args.first().or_else(|| kwarg(&kw, "dsn")) {
        Some(V::None) | None => env_nonempty("SENTRY_DSN"),
        Some(v) => Some(ops::str_(v)?),
    };
    // a new init replaces the client
    if let Some(old) = OPTS.write().take() {
        ON.store(false, Ordering::Relaxed);
        old.client.close(Some(std::time::Duration::from_secs(2)));
    }
    let integ_list: Vec<V> = match kwarg(&kw, "integrations") {
        None | Some(V::None) => vec![],
        Some(V::List(l)) => l.lock().clone(),
        Some(V::Tuple(t)) => t.to_vec(),
        Some(o) => return Err(Exc::type_error(format!("py2axum: sentry_sdk.init(integrations=) must be a list, not {}", o.type_name()))),
    };
    let defaults = match kwarg(&kw, "default_integrations") {
        Some(v) => ops::truthy(v)?,
        None => true,
    };
    let auto = defaults
        && match kwarg(&kw, "auto_enabling_integrations") {
            Some(v) => ops::truthy(v)?,
            None => true,
        };
    let mut starlette = auto;
    let mut logging = if defaults { Some((Some(20), Some(40))) } else { None };
    let mut names: Vec<&'static str> = vec![];
    if defaults {
        names.extend(["argv", "atexit", "dedupe", "excepthook", "logging", "modules", "stdlib", "threading"]);
    }
    if auto {
        names.extend(["fastapi", "starlette"]);
    }
    for it in &integ_list {
        let V::Native(n) = it else {
            return Err(Exc::type_error(format!("py2axum: sentry_sdk.init(integrations=): {} is not a supported integration", it.type_name())));
        };
        let Native::Sentry(o) = &**n else {
            return Err(Exc::type_error(format!("py2axum: sentry_sdk.init(integrations=): {} is not a supported integration", it.type_name())));
        };
        let Obj::Integration(i) = &**o else {
            return Err(Exc::type_error("py2axum: sentry_sdk.init(integrations=) expects integration objects"));
        };
        match i.kind {
            "fastapi" | "starlette" => {
                starlette = true;
                names.extend(["fastapi", "starlette"]);
            }
            "logging" => {
                logging = Some((i.level, i.event_level));
                names.push("logging");
            }
            "asyncio" => {
                // it patches the running loop's task factory: at import there is none, and Python only
                // logs a warning (the case of the ATOM projects). Inside the loop it would fork the
                // isolation scope per task and capture task exceptions: not reproduced.
                if !IMPORTING.load(Ordering::Relaxed) {
                    return Err(Exc::msg(
                        &NOT_IMPLEMENTED_ERROR,
                        "py2axum: AsyncioIntegration is only supported when sentry_sdk.init runs at import time \
                         (it then has no running loop to patch, as in Python)",
                    ));
                }
                names.push("asyncio");
            }
            "sqlalchemy" => names.push("sqlalchemy"),
            _ => {}
        }
    }
    names.sort();
    names.dedup();
    let Some(dsn) = dsn.filter(|d| !d.is_empty()) else {
        return Ok(V::None);
    };
    let parsed: sentry::types::Dsn = dsn
        .parse()
        .map_err(|e| Exc::msg(&VALUE_ERROR, format!("BadDsn: {e}")))?;
    let enable_tracing = match kwarg(&kw, "enable_tracing") {
        Some(V::None) | None => None,
        Some(v) => Some(ops::truthy(v)?),
    };
    let mut traces_sample_rate = opt_rate(kwarg(&kw, "traces_sample_rate"), "traces_sample_rate")?;
    if enable_tracing == Some(true) && traces_sample_rate.is_none() {
        traces_sample_rate = Some(1.0);
    }
    let traces_sampler = opt_fn(kwarg(&kw, "traces_sampler"));
    if enable_tracing == Some(false) {
        traces_sample_rate = None;
    }
    let body_size = match kwarg(&kw, "max_request_body_size").map(ops::str_).transpose()?.as_deref() {
        None | Some("medium") => BodySize::Medium,
        Some("never") => BodySize::Never,
        Some("small") => BodySize::Small,
        Some("always") => BodySize::Always,
        Some(o) => return Err(Exc::value_error(format!("py2axum: max_request_body_size={o:?} is not supported"))),
    };
    let max_value_length = match kwarg(&kw, "max_value_length") {
        None | Some(V::None) => None,
        Some(V::Int(i)) => Some((*i).max(0) as usize),
        Some(o) => return Err(Exc::type_error(format!("py2axum: max_value_length must be an int, not {}", o.type_name()))),
    };
    let max_breadcrumbs = match kwarg(&kw, "max_breadcrumbs") {
        None | Some(V::None) => 100,
        Some(V::Int(i)) => (*i).max(0) as usize,
        Some(o) => return Err(Exc::type_error(format!("py2axum: max_breadcrumbs must be an int, not {}", o.type_name()))),
    };
    let release = match opt_str(kwarg(&kw, "release"))? {
        Some(r) => Some(r),
        None => default_release(),
    };
    let environment = opt_str(kwarg(&kw, "environment"))?
        .or_else(|| env_nonempty("SENTRY_ENVIRONMENT"))
        .unwrap_or_else(|| "production".into());
    let server_name = match opt_str(kwarg(&kw, "server_name"))? {
        Some(s) => Some(s),
        None => hostname(),
    };
    let pii = match kwarg(&kw, "send_default_pii") {
        Some(v) => ops::truthy(v)?,
        None => false,
    };
    let sample_rate = opt_rate(kwarg(&kw, "sample_rate"), "sample_rate")?.unwrap_or(1.0);
    let shutdown = opt_rate(kwarg(&kw, "shutdown_timeout"), "shutdown_timeout")?.unwrap_or(2.0);
    let copts = sentry::ClientOptions {
        dsn: Some(parsed.clone()),
        shutdown_timeout: std::time::Duration::from_secs_f64(shutdown.max(0.0)),
        transport: Some(Arc::new(|o: &sentry::ClientOptions| {
            Arc::new(sentry::transports::ReqwestHttpTransport::new(o)) as Arc<dyn sentry::Transport>
        })),
        ..Default::default()
    };
    let client = Arc::new(sentry::Client::with_options(copts));
    let o = Opts {
        client,
        public_key: parsed.public_key().to_string(),
        environment: environment.trim().to_string(),
        release: release.map(|r| r.trim().to_string()),
        server_name: server_name.map(|s| s.trim().to_string()),
        dist: opt_str(kwarg(&kw, "dist"))?,
        pii,
        max_value_length,
        max_breadcrumbs,
        sample_rate,
        traces_sample_rate,
        traces_sampler,
        before_send: opt_fn(kwarg(&kw, "before_send")),
        before_send_transaction: opt_fn(kwarg(&kw, "before_send_transaction")),
        body_size,
        starlette,
        logging,
        integrations: names,
    };
    let _ = cx;
    *OPTS.write() = Some(Arc::new(o));
    ON.store(true, Ordering::Relaxed);
    Ok(V::None)
}

/// `get_default_release()` without `git rev-parse` (a deployed binary has no checkout)
fn default_release() -> Option<String> {
    for k in [
        "SENTRY_RELEASE",
        "HEROKU_BUILD_COMMIT",
        "HEROKU_SLUG_COMMIT",
        "SOURCE_VERSION",
        "CODEBUILD_RESOLVED_SOURCE_VERSION",
        "CIRCLE_SHA1",
        "GAE_DEPLOYMENT_ID",
        "K_REVISION",
    ] {
        if let Some(v) = env_nonempty(k) {
            return Some(v);
        }
    }
    None
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r != 0 {
        return None;
    }
    let n = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

// ---------------------------------------------------------------- scopes

#[derive(Clone)]
struct Crumb {
    /// the crumb dict as built by `add_breadcrumb` (timestamp and type included)
    fields: Vec<(String, V)>,
    ts: f64,
}

/// What a `sentry_sdk.Scope` holds that this runtime reproduces.
#[derive(Clone, Default)]
pub struct ScopeData {
    level: Option<String>,
    user: Option<V>,
    tags: IndexMap<String, V>,
    contexts: IndexMap<String, V>,
    extras: IndexMap<String, V>,
    fingerprint: Option<V>,
    breadcrumbs: VecDeque<Crumb>,
    truncated: usize,
}

impl ScopeData {
    fn update_from(&mut self, o: &ScopeData) {
        if o.level.is_some() {
            self.level = o.level.clone();
        }
        if o.fingerprint.is_some() {
            self.fingerprint = o.fingerprint.clone();
        }
        if o.user.is_some() {
            self.user = o.user.clone();
        }
        for (k, v) in &o.tags {
            self.tags.insert(k.clone(), v.clone());
        }
        for (k, v) in &o.contexts {
            self.contexts.insert(k.clone(), v.clone());
        }
        for (k, v) in &o.extras {
            self.extras.insert(k.clone(), v.clone());
        }
        self.breadcrumbs.extend(o.breadcrumbs.iter().cloned());
        self.truncated += o.truncated;
    }
}

type Scope = Arc<Mutex<ScopeData>>;

/// The request (and its tasks) or the import: Python's isolation scope with its propagation context,
/// the stack of `new_scope()` forks of the current scope, and the request's transaction.
pub struct Iso {
    data: Scope,
    current: Mutex<Vec<Scope>>,
    trace: Mutex<Trace>,
    /// DedupeIntegration: the last exception captured
    last_exc: Mutex<Option<Exc>>,
    last_event_id: Mutex<Option<String>>,
}

#[derive(Clone, Default)]
struct Trace {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    /// a request: the `http.server` span (sampled or not); None for a HEAD/OPTIONS request or the import
    http: bool,
    sampled: Option<bool>,
    sample_rate: Option<f64>,
    sample_rand: f64,
    status: Option<&'static str>,
    status_code: Option<u16>,
    /// (transaction name, source)
    name: Option<(String, &'static str)>,
    start: Option<chrono::DateTime<chrono::Utc>>,
    /// the matched route: (`file.py:line`, function, module)
    route: Option<(&'static str, &'static str, &'static str)>,
    /// the route handler ran: the request body and cookies join the request data
    in_route: bool,
    /// a request served while Sentry was active: its events carry the request data
    in_request: bool,
}

fn hex32() -> String {
    format!("{:032x}", rand::random::<u128>())
}

fn hex16() -> String {
    format!("{:016x}", rand::random::<u64>())
}

impl Iso {
    fn new(data: ScopeData) -> Iso {
        Iso {
            data: Arc::new(Mutex::new(data)),
            current: Mutex::new(vec![]),
            trace: Mutex::new(Trace { trace_id: hex32(), span_id: hex16(), sample_rand: rand::random::<f64>(), ..Default::default() }),
            last_exc: Mutex::new(None),
            last_event_id: Mutex::new(None),
        }
    }
}

/// The isolation scope of the import (module globals, lifespan, background threads).
static MAIN: LazyLock<Arc<Iso>> = LazyLock::new(|| Arc::new(Iso::new(ScopeData::default())));

pub fn main_iso() -> Arc<Iso> {
    MAIN.clone()
}

/// The request's isolation scope: a fork of the import's, made when the request starts (or on first use
/// while Sentry is inactive). SentryAsgiMiddleware clears the breadcrumbs.
fn fork_main() -> Iso {
    let mut d = MAIN.data.lock().clone();
    d.breadcrumbs.clear();
    d.truncated = 0;
    Iso::new(d)
}

fn iso(cx: &Cx) -> Arc<Iso> {
    cx.sentry.get_or_init(|| Arc::new(fork_main())).clone()
}

/// The scope `sentry_sdk.set_*` writes to (the isolation scope), and the current scope (the innermost
/// `new_scope()`), merged at capture time.
fn current(i: &Iso) -> Option<Scope> {
    i.current.lock().last().cloned()
}

fn level_str(v: &V) -> R<String> {
    ops::str_(v)
}

fn key_str(k: &V) -> R<String> {
    ops::str_(k)
}

fn scope_set_tag(s: &Scope, k: &V, v: &V) -> R {
    s.lock().tags.insert(key_str(k)?, v.clone());
    Ok(V::None)
}

fn scope_set_tags(s: &Scope, d: &V) -> R {
    let V::Dict(m) = d else {
        return Err(Exc::type_error(format!("py2axum: set_tags() expects a dict, not {}", d.type_name())));
    };
    let items: Vec<(V, V)> = m.lock().values().cloned().collect();
    let mut s = s.lock();
    for (k, v) in items {
        s.tags.insert(key_str(&k)?, v);
    }
    Ok(V::None)
}

fn scope_set_user(s: &Scope, v: &V) -> R {
    s.lock().user = if v.is_none() { None } else { Some(v.clone()) };
    Ok(V::None)
}

fn scope_set_context(s: &Scope, k: &V, v: &V) -> R {
    s.lock().contexts.insert(key_str(k)?, v.clone());
    Ok(V::None)
}

fn scope_set_extra(s: &Scope, k: &V, v: &V) -> R {
    s.lock().extras.insert(key_str(k)?, v.clone());
    Ok(V::None)
}

fn scope_set_level(s: &Scope, v: &V) -> R {
    s.lock().level = if v.is_none() { None } else { Some(level_str(v)?) };
    Ok(V::None)
}

fn now_ts() -> f64 {
    let n = chrono::Utc::now();
    n.timestamp() as f64 + n.timestamp_subsec_micros() as f64 / 1e6
}

/// `add_breadcrumb(crumb=None, hint=None, **kwargs)`: `dict(crumb)` updated with the keywords, then
/// the timestamp and type defaults (before_breadcrumb is refused at init).
fn scope_add_breadcrumb(s: &Scope, args: &[V], kw: &[(String, V)]) -> R {
    if !active() {
        return Ok(V::None);
    }
    let mut fields: Vec<(String, V)> = vec![];
    let crumb = args.first().or_else(|| kwarg(kw, "crumb"));
    if let Some(c) = crumb.filter(|c| !c.is_none()) {
        let V::Dict(m) = c else {
            return Err(Exc::type_error(format!("py2axum: add_breadcrumb(crumb=) expects a dict, not {}", c.type_name())));
        };
        for (k, v) in m.lock().values() {
            fields.push((key_str(k)?, v.clone()));
        }
    }
    for (k, v) in kw {
        if k == "crumb" || k == "hint" {
            continue;
        }
        match fields.iter_mut().find(|(n, _)| n == k) {
            Some(e) => e.1 = v.clone(),
            None => fields.push((k.clone(), v.clone())),
        }
    }
    if fields.is_empty() {
        return Ok(V::None);
    }
    push_crumb(s, fields);
    Ok(V::None)
}

fn push_crumb(s: &Scope, mut fields: Vec<(String, V)>) {
    let ts = now_ts();
    if !fields.iter().any(|(k, _)| k == "timestamp") {
        fields.push(("timestamp".into(), V::Float(ts)));
    }
    if !fields.iter().any(|(k, _)| k == "type") {
        fields.push(("type".into(), V::str("default")));
    }
    let max = opts().map(|o| o.max_breadcrumbs).unwrap_or(100);
    let mut s = s.lock();
    s.breadcrumbs.push_back(Crumb { fields, ts });
    while s.breadcrumbs.len() > max {
        s.breadcrumbs.pop_front();
        s.truncated += 1;
    }
}

// ---------------------------------------------------------------- the module API

pub fn set_tag(cx: &Cx, k: &V, v: &V) -> R {
    scope_set_tag(&iso(cx).data, k, v)
}
pub fn set_tags(cx: &Cx, d: &V) -> R {
    scope_set_tags(&iso(cx).data, d)
}
pub fn set_user(cx: &Cx, v: &V) -> R {
    scope_set_user(&iso(cx).data, v)
}
pub fn set_context(cx: &Cx, k: &V, v: &V) -> R {
    scope_set_context(&iso(cx).data, k, v)
}
pub fn set_extra(cx: &Cx, k: &V, v: &V) -> R {
    scope_set_extra(&iso(cx).data, k, v)
}
pub fn set_level(cx: &Cx, v: &V) -> R {
    scope_set_level(&iso(cx).data, v)
}
pub fn add_breadcrumb(cx: &Cx, args: Vec<V>, kw: Vec<(String, V)>) -> R {
    scope_add_breadcrumb(&iso(cx).data, &args, &kw)
}

pub fn last_event_id(cx: &Cx) -> R {
    Ok(iso(cx).last_event_id.lock().clone().map(V::str).unwrap_or(V::None))
}

pub fn is_initialized() -> R {
    Ok(V::Bool(active()))
}

pub async fn flush(args: Vec<V>, kw: Vec<(String, V)>) -> R {
    let t = opt_rate(args.first().or_else(|| kwarg(&kw, "timeout")), "timeout")?;
    if let Some(o) = opts() {
        let c = o.client.clone();
        let d = t.map(|s| std::time::Duration::from_secs_f64(s.max(0.0)));
        let _ = tokio::task::spawn_blocking(move || c.flush(d)).await;
    }
    Ok(V::None)
}

/// `new_scope()` / `push_scope()`: a context manager forking the current scope while its block runs.
pub fn new_scope(cx: &Cx) -> R {
    Ok(V::native(Native::Sentry(Arc::new(Obj::ScopeCm(iso(cx), Mutex::new(None))))))
}

pub fn get_isolation_scope(cx: &Cx) -> R {
    Ok(V::native(Native::Sentry(Arc::new(Obj::Scope(iso(cx).data.clone())))))
}

/// The current scope outside a `new_scope()` block is the request's own (empty unless written through
/// this object).
pub fn get_current_scope(cx: &Cx) -> R {
    let i = iso(cx);
    let s = {
        let mut cur = i.current.lock();
        if cur.is_empty() {
            cur.push(Arc::new(Mutex::new(ScopeData::default())));
        }
        cur.last().unwrap().clone()
    };
    Ok(V::native(Native::Sentry(Arc::new(Obj::Scope(s)))))
}

/// Native objects of this module.
pub enum Obj {
    Integration(Integ),
    /// a `Scope` (the one yielded by `new_scope()`, or the isolation scope)
    Scope(Scope),
    /// `new_scope()` before/while it is entered: the scope pushed
    ScopeCm(Arc<Iso>, Mutex<Option<Scope>>),
}

pub fn type_name(o: &Obj) -> &'static str {
    match o {
        Obj::Integration(i) => match i.kind {
            "fastapi" => "FastApiIntegration",
            "starlette" => "StarletteIntegration",
            "logging" => "LoggingIntegration",
            "asyncio" => "AsyncioIntegration",
            _ => "SqlalchemyIntegration",
        },
        Obj::Scope(_) => "Scope",
        Obj::ScopeCm(..) => "_GeneratorContextManager",
    }
}

pub fn enter(o: &Obj) -> R {
    let Obj::ScopeCm(iso, slot) = o else {
        return Err(Exc::attr_error(format!("__enter__ of '{}'", type_name(o))));
    };
    let base = current(iso).map(|s| s.lock().clone()).unwrap_or_default();
    let s: Scope = Arc::new(Mutex::new(base));
    iso.current.lock().push(s.clone());
    *slot.lock() = Some(s.clone());
    Ok(V::native(Native::Sentry(Arc::new(Obj::Scope(s)))))
}

pub fn exit(o: &Obj) -> R {
    if let Obj::ScopeCm(iso, slot) = o {
        if let Some(s) = slot.lock().take() {
            let mut cur = iso.current.lock();
            if let Some(p) = cur.iter().rposition(|x| Arc::ptr_eq(x, &s)) {
                cur.remove(p);
            }
        }
    }
    Ok(V::Bool(false))
}

/// Methods of a `Scope` object.
pub async fn method(cx: &Cx, o: &Obj, name: &str, args: Vec<V>, kw: Vec<(String, V)>, src: Option<&'static str>) -> R {
    let Obj::Scope(s) = o else {
        return Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(o))));
    };
    let a = |i: usize, k: &str| -> R<V> {
        args.get(i)
            .or_else(|| kwarg(&kw, k))
            .cloned()
            .ok_or_else(|| Exc::type_error(format!("Scope.{name}() missing required argument: '{k}'")))
    };
    match name {
        "set_tag" => scope_set_tag(s, &a(0, "key")?, &a(1, "value")?),
        "set_tags" => scope_set_tags(s, &a(0, "tags")?),
        "remove_tag" => {
            s.lock().tags.shift_remove(&key_str(&a(0, "key")?)?);
            Ok(V::None)
        }
        "set_user" => scope_set_user(s, &a(0, "value")?),
        "set_context" => scope_set_context(s, &a(0, "key")?, &a(1, "value")?),
        "remove_context" => {
            s.lock().contexts.shift_remove(&key_str(&a(0, "key")?)?);
            Ok(V::None)
        }
        "set_extra" => scope_set_extra(s, &a(0, "key")?, &a(1, "value")?),
        "remove_extra" => {
            s.lock().extras.shift_remove(&key_str(&a(0, "key")?)?);
            Ok(V::None)
        }
        "set_level" => scope_set_level(s, &a(0, "value")?),
        "add_breadcrumb" => scope_add_breadcrumb(s, &args, &kw),
        "clear_breadcrumbs" => {
            let mut d = s.lock();
            d.breadcrumbs.clear();
            d.truncated = 0;
            Ok(V::None)
        }
        "clear" => {
            *s.lock() = ScopeData::default();
            Ok(V::None)
        }
        "capture_message" => capture_message_in(cx, src, args, kw, Some(s.clone())).await,
        "capture_exception" => capture_exception_in(cx, src, args, kw, Some(s.clone())).await,
        _ => Err(Exc::msg(&NOT_IMPLEMENTED_ERROR, format!("py2axum: sentry_sdk Scope.{name}() is not supported"))),
    }
}

// ---------------------------------------------------------------- capture

/// A value of the event before serialization: built here, or a Python value of the scopes.
#[derive(Clone)]
enum N {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Map(Vec<(String, N)>, Option<V>),
    List(Vec<N>, Option<V>),
    /// AnnotatedValue: the value and its `_meta`
    Ann(Box<N>, Vec<(&'static str, J)>),
    Py(V),
}

fn s(x: impl Into<String>) -> N {
    N::Str(x.into())
}

fn map(items: Vec<(&str, N)>) -> N {
    N::Map(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect(), None)
}

impl N {
    fn get_mut(&mut self, k: &str) -> Option<&mut N> {
        match self {
            N::Map(m, _) => m.iter_mut().find(|(n, _)| n == k).map(|(_, v)| v),
            _ => None,
        }
    }
    fn get(&self, k: &str) -> Option<&N> {
        match self {
            N::Map(m, _) => m.iter().find(|(n, _)| n == k).map(|(_, v)| v),
            _ => None,
        }
    }
    fn set(&mut self, k: &str, v: N) {
        if let N::Map(m, _) = self {
            match m.iter_mut().find(|(n, _)| n == k) {
                Some(e) => e.1 = v,
                None => m.push((k.to_string(), v)),
            }
        }
    }
    fn remove(&mut self, k: &str) {
        if let N::Map(m, _) = self {
            m.retain(|(n, _)| n != k);
        }
    }
    fn setdefault_map(&mut self, k: &str) -> &mut N {
        if self.get(k).is_none() {
            self.set(k, N::Map(vec![], None));
        }
        let v = self.get_mut(k).unwrap();
        if let N::Py(p) = v {
            *v = shallow(&p.clone());
        }
        v
    }
}

/// A Python dict as a map whose values stay Python values (the scrubber looks at the first level only).
fn shallow(v: &V) -> N {
    match v {
        V::Dict(m) => {
            let items: Vec<(V, V)> = m.lock().values().cloned().collect();
            N::Map(items.into_iter().map(|(k, x)| (ops::str_(&k).unwrap_or_default(), N::Py(x))).collect(), Some(v.clone()))
        }
        _ => N::Py(v.clone()),
    }
}

fn fmt_ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

fn ts_from_f64(t: f64) -> chrono::DateTime<chrono::Utc> {
    let secs = t.floor();
    chrono::DateTime::from_timestamp(secs as i64, ((t - secs) * 1e6).round() as u32 * 1000).unwrap_or_default()
}

fn level_name(levelno: i64) -> String {
    match levelno {
        0 => "notset".into(),
        10 => "debug".into(),
        20 => "info".into(),
        30 => "warning".into(),
        40 => "error".into(),
        50 => "fatal".into(),
        n => format!("level {n}"),
    }
}

/// `type(exc).__module__` when it is not builtins: the project module, or the library's (a table of
/// the library exceptions this runtime raises).
fn exc_module(c: &'static Class) -> Option<&'static str> {
    if let ClassKind::UserException(d) = &c.kind {
        return Some(d.module);
    }
    let table: &[(&'static Class, &'static str)] = &[
        (&HTTP_EXCEPTION, "fastapi.exceptions"),
        (&REQUEST_VALIDATION_ERROR, "fastapi.exceptions"),
        (&VALIDATION_ERROR, "pydantic_core._pydantic_core"),
        (&SQLALCHEMY_ERROR, "sqlalchemy.exc"),
        (&DBAPI_ERROR, "sqlalchemy.exc"),
        (&INTEGRITY_ERROR, "sqlalchemy.exc"),
        (&STATEMENT_ERROR, "sqlalchemy.exc"),
        (&COMPILE_ERROR, "sqlalchemy.exc"),
        (&OPERATIONAL_ERROR, "sqlalchemy.exc"),
        (&DATA_ERROR, "sqlalchemy.exc"),
        (&PROGRAMMING_ERROR, "sqlalchemy.exc"),
        (&INTERNAL_ERROR, "sqlalchemy.exc"),
        (&NOT_SUPPORTED_ERROR, "sqlalchemy.exc"),
        (&NO_RESULT_FOUND, "sqlalchemy.exc"),
        (&MULTIPLE_RESULTS_FOUND, "sqlalchemy.exc"),
        (&MISSING_GREENLET, "sqlalchemy.exc"),
        (&INVALID_REQUEST_ERROR, "sqlalchemy.exc"),
        (&ARGUMENT_ERROR, "sqlalchemy.exc"),
        (&OBJECT_DELETED_ERROR, "sqlalchemy.orm.exc"),
        (&CANCELLED_ERROR, "asyncio.exceptions"),
        (&JSON_DECODE_ERROR, "json.decoder"),
        (&JOSE_ERROR, "jose.exceptions"),
        (&JWS_ERROR, "jose.exceptions"),
        (&JWT_ERROR, "jose.exceptions"),
        (&JWT_CLAIMS_ERROR, "jose.exceptions"),
        (&EXPIRED_SIGNATURE_ERROR, "jose.exceptions"),
        (&JWK_ERROR, "jose.exceptions"),
        (&BAD_DATA, "itsdangerous.exc"),
        (&BAD_SIGNATURE, "itsdangerous.exc"),
        (&BAD_TIME_SIGNATURE, "itsdangerous.exc"),
        (&SIGNATURE_EXPIRED, "itsdangerous.exc"),
        (&BAD_HEADER, "itsdangerous.exc"),
        (&BAD_PAYLOAD, "itsdangerous.exc"),
        (&INVALID_TOKEN, "cryptography.fernet"),
        (&TEMPLATE_NOT_FOUND, "jinja2.exceptions"),
        (&REDIS_ERROR, "redis.exceptions"),
        (&REDIS_CONNECTION_ERROR, "redis.exceptions"),
        (&REDIS_TIMEOUT_ERROR, "redis.exceptions"),
        (&REDIS_DATA_ERROR, "redis.exceptions"),
        (&REDIS_RESPONSE_ERROR, "redis.exceptions"),
        (&TENACITY_RETRY_ERROR, "tenacity"),
        (&WEBPUSH_EXCEPTION, "pywebpush"),
        (&DECIMAL_INVALID_OPERATION, "decimal"),
        (&QUEUE_FULL, "asyncio.queues"),
        (&QUEUE_EMPTY, "asyncio.queues"),
    ];
    table.iter().find(|(k, _)| std::ptr::eq(*k, c)).map(|(_, m)| *m)
}

/// `type.__qualname__` (a project class's is written with its module here)
fn type_name_of(c: &'static Class) -> &'static str {
    if let ClassKind::UserException(d) = &c.kind {
        if let Some(q) = c.qualname.strip_prefix(d.module).and_then(|q| q.strip_prefix('.')) {
            return q;
        }
    }
    c.qualname
}

/// `get_error_message`: `exc.message`, else `exc.detail`, else `str(exc)`.
async fn error_message(cx: &Cx, e: &Exc) -> String {
    let v = V::Exc(e.clone());
    for a in ["message", "detail"] {
        if a == "detail" {
            if let Some((_, d, _)) = e.http_info() {
                if ops::truthy(&d).unwrap_or(false) {
                    return ops::str_(&d).unwrap_or_default();
                }
                continue;
            }
        }
        if !e.0.attrs.lock().contains_key(a) && e.0.class.exc_lookup(a).is_none() {
            continue;
        }
        if let Ok(x) = super::methods::getattr(cx, &v, a).await {
            if ops::truthy(&x).unwrap_or(false) {
                return ops::str_(&x).unwrap_or_default();
            }
        }
    }
    ops::str_(&v).unwrap_or_else(|_| e.message())
}

/// One entry of `exception.values`: the frame is the route handler (no Python traceback here).
async fn exception_entry(cx: &Cx, e: &Exc, mechanism: J, frame: Option<(&'static str, &'static str, &'static str)>) -> N {
    let mut m = vec![
        ("mechanism".to_string(), N::Py(super::pyd::from_serde(&mechanism))),
        ("module".to_string(), exc_module(e.0.class).map(s).unwrap_or(N::Null)),
        ("type".to_string(), s(type_name_of(e.0.class))),
        ("value".to_string(), s(error_message(cx, e).await)),
    ];
    if let Some((src, func, module)) = frame {
        let (file, line) = src.rsplit_once(':').unwrap_or((src, "0"));
        m.push((
            "stacktrace".into(),
            map(vec![(
                "frames",
                N::List(
                    vec![map(vec![
                        ("filename", s(file)),
                        ("abs_path", s(file)),
                        ("function", s(func)),
                        ("module", s(module)),
                        ("lineno", N::Int(line.parse().unwrap_or(0))),
                        ("in_app", N::Bool(true)),
                    ])],
                    None,
                ),
            )]),
        ));
    }
    N::Map(m, None)
}

/// What gets captured: the event so far (built by the API call) and its hint.
struct Capture {
    event: N,
    exc: Option<Exc>,
    is_tx: bool,
    /// the request's trace as it was when the event happened (an event built later)
    trace: Option<Trace>,
}

/// The source tag of an event: the route handler in a request, else the call site.
fn source(i: &Iso, src: Option<&'static str>) -> Option<&'static str> {
    i.trace.lock().route.map(|r| r.0).or(src)
}

fn trace_context(t: &Trace) -> N {
    let mut m = vec![
        ("trace_id", s(&t.trace_id)),
        ("span_id", s(&t.span_id)),
        ("parent_span_id", t.parent_span_id.clone().map(s).unwrap_or(N::Null)),
    ];
    if t.http {
        m.push(("op", s("http.server")));
        m.push(("description", N::Null));
        m.push(("origin", s("auto.http.starlette")));
        if let Some(st) = t.status {
            m.push(("status", s(st)));
        }
        let mut data = vec![];
        if let Some(c) = t.status_code {
            data.push(("http.response.status_code", N::Int(c as i64)));
        }
        m.push(("data", map(data)));
    }
    map(m)
}

// ---------------------------------------------------------------- request data (Starlette's extractor)

const SENSITIVE_HEADERS: &[&str] = &["X_FORWARDED_FOR", "SET_COOKIE", "COOKIE", "AUTHORIZATION", "PROXY_AUTHORIZATION", "X_API_KEY", "X_REAL_IP"];

fn is_json_type(ct: &str) -> bool {
    let m = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    m == "application/json" || (m.starts_with("application/") && m.ends_with("+json"))
}

fn cookies(header: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for chunk in header.split(';') {
        let (k, v) = match chunk.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => ("", chunk.trim()),
        };
        if !k.is_empty() || !v.is_empty() {
            let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
            match out.iter_mut().find(|(n, _)| n == k) {
                Some(e) => e.1 = v.to_string(),
                None => out.push((k.to_string(), v.to_string())),
            }
        }
    }
    out
}

async fn request_info(cx: &Cx, o: &Opts, in_route: bool) -> N {
    let r = &cx.req;
    let mut headers: Vec<(String, String)> = vec![];
    for (k, v) in &r.headers {
        match headers.iter_mut().find(|(n, _)| n == k) {
            Some(e) => {
                e.1.push_str(", ");
                e.1.push_str(v);
            }
            None => headers.push((k.clone(), v.clone())),
        }
    }
    let host = headers.iter().find(|(k, _)| k == "host").map(|(_, v)| v.clone());
    let filtered: Vec<(String, N)> = headers
        .iter()
        .map(|(k, v)| {
            let sensitive = !o.pii && SENSITIVE_HEADERS.contains(&k.to_ascii_uppercase().replace('-', "_").as_str());
            let n = match (sensitive, OLD_HEADERS.load(Ordering::Relaxed)) {
                (false, _) => s(v.clone()),
                (true, false) => s("[Filtered]"),
                (true, true) => N::Ann(Box::new(s("")), vec![("rem", json!([["!config", "x"]]))]),
            };
            (k.clone(), n)
        })
        .collect();
    let path = super::web::unquote(&r.path);
    let url = match &host {
        Some(h) => format!("http://{h}{path}"),
        None => path.clone(),
    };
    let mut m: Vec<(String, N)> = vec![
        ("method".into(), s(&r.method)),
        ("headers".into(), N::Map(filtered, None)),
        ("query_string".into(), if r.raw_query.is_empty() { N::Null } else { s(super::web::unquote(&r.raw_query)) }),
        ("url".into(), s(url)),
    ];
    if o.pii {
        let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        let ip = get("x-forwarded-for")
            .filter(|x| !x.is_empty())
            .map(|x| x.split(',').next().unwrap_or("").trim().to_string())
            .or_else(|| get("x-real-ip").filter(|x| !x.is_empty()).map(|x| x.trim().to_string()))
            .or_else(|| r.client.as_ref().map(|c| c.0.clone()));
        if let Some(ip) = ip {
            m.push(("env".into(), map(vec![("REMOTE_ADDR", s(ip))])));
        }
    }
    if !in_route {
        return N::Map(m, None);
    }
    if o.pii {
        let c = cookies(&headers.iter().find(|(k, _)| k == "cookie").map(|(_, v)| v.clone()).unwrap_or_default());
        m.push(("cookies".into(), N::Map(c.into_iter().map(|(k, v)| (k, s(v))).collect(), None)));
    }
    let Some(len) = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.trim().parse::<usize>().ok()) else {
        return N::Map(m, None);
    };
    if len == 0 {
        return N::Map(m, None);
    }
    let too_big = match o.body_size {
        BodySize::Never => true,
        BodySize::Small => len > 1_000,
        BodySize::Medium => len > 10_000,
        BodySize::Always => false,
    };
    if too_big {
        m.push(("data".into(), N::Ann(Box::new(s("")), vec![("rem", json!([["!config", "x"]]))])));
        return N::Map(m, None);
    }
    let ct = headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.clone()).unwrap_or_default();
    if is_json_type(&ct) {
        match super::pyd::loads(&String::from_utf8_lossy(&r.body)) {
            Ok(v) if ops::truthy(&v).unwrap_or(false) => {
                m.push(("data".into(), N::Py(v)));
                return N::Map(m, None);
            }
            Ok(_) => {}
            // a JSON body that does not parse: Python's extractor stops there
            Err(_) => return N::Map(m, None),
        }
    }
    let lct = ct.to_ascii_lowercase();
    if MULTIPART.load(Ordering::Relaxed) && (lct.starts_with("application/x-www-form-urlencoded") || lct.starts_with("multipart/form-data")) {
        if let Ok(form) = super::web::read_form(cx).await {
            if !form.is_empty() {
                let mut items: Vec<(String, N)> = vec![];
                for (k, v) in form {
                    let n = match v {
                        super::web::FormVal::Text(t) => s(t),
                        _ => N::Ann(Box::new(s("")), vec![("rem", json!([["!raw", "x"]]))]),
                    };
                    match items.iter_mut().find(|(x, _)| *x == k) {
                        Some(e) => e.1 = n,
                        None => items.push((k, n)),
                    }
                }
                m.push(("data".into(), N::Map(items, None)));
                return N::Map(m, None);
            }
        }
    }
    m.push(("data".into(), N::Ann(Box::new(s("")), vec![("rem", json!([["!raw", "x"]]))])));
    N::Map(m, None)
}

// ---------------------------------------------------------------- the event pipeline

/// `client.capture_event` with the merged scopes: scope data, dedupe, request data, scrubber,
/// serializer, `before_send`, then the envelope. Returns the event id, or None when dropped.
async fn capture(cx: &Cx, o: &Opts, i: &Arc<Iso>, extra: Option<Scope>, mut c: Capture, src: Option<&'static str>) -> Option<String> {
    let event_id = format!("{:032x}", rand::random::<u128>() & !(0xf000u128 << 64) | (0x4000u128 << 64));
    // merged scope: isolation, then current, then the scope of the call (capture_message(scope=...))
    let mut sc = i.data.lock().clone();
    if let Some(cur) = current(i) {
        sc.update_from(&cur.lock());
    }
    if let Some(e) = &extra {
        sc.update_from(&e.lock());
    }
    let trace = c.trace.take().unwrap_or_else(|| i.trace.lock().clone());
    let ev = &mut c.event;
    ev.set("event_id", s(&event_id));
    if ev.get("timestamp").is_none() {
        ev.set("timestamp", s(fmt_ts(chrono::Utc::now())));
    }
    // apply_to_event: contexts (trace included), level, fingerprint, user, transaction, tags, extra, breadcrumbs
    {
        let ctx = ev.setdefault_map("contexts");
        for (k, v) in &sc.contexts {
            ctx.set(k, N::Py(v.clone()));
        }
        if ctx.get("trace").is_none() {
            ctx.set("trace", trace_context(&trace));
        }
    }
    if let Some(l) = &sc.level {
        ev.set("level", s(l));
    }
    if ev.get("fingerprint").is_none() {
        if let Some(f) = &sc.fingerprint {
            ev.set("fingerprint", N::Py(f.clone()));
        }
    }
    if ev.get("user").map_or(true, |u| matches!(u, N::Null)) {
        if let Some(u) = &sc.user {
            ev.set("user", shallow(u));
        }
    }
    if ev.get("transaction").is_none() {
        if let Some((n, _)) = &trace.name {
            ev.set("transaction", s(n));
        }
    }
    if ev.get("transaction_info").is_none() {
        ev.set("transaction_info", match &trace.name {
            Some((_, src)) => map(vec![("source", s(*src))]),
            None => map(vec![]),
        });
    }
    {
        let tags = ev.setdefault_map("tags");
        for (k, v) in &sc.tags {
            tags.set(k, N::Py(v.clone()));
        }
        if let Some(src) = source(i, src) {
            tags.set("py2axum.source", s(src));
        }
    }
    if !sc.extras.is_empty() {
        let ex = ev.setdefault_map("extra");
        for (k, v) in &sc.extras {
            ex.set(k, N::Py(v.clone()));
        }
    }
    if !c.is_tx {
        let mut crumbs: Vec<Crumb> = sc.breadcrumbs.iter().cloned().collect();
        crumbs.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        let values: Vec<N> = crumbs
            .into_iter()
            .map(|cr| {
                N::Map(
                    cr.fields
                        .into_iter()
                        .map(|(k, v)| {
                            let n = match (k.as_str(), &v) {
                                ("timestamp", V::Float(t)) => s(fmt_ts(ts_from_f64(*t))),
                                ("data", V::Dict(_)) => shallow(&v),
                                _ => N::Py(v),
                            };
                            (k, n)
                        })
                        .collect(),
                    None,
                )
            })
            .collect();
        let crumbs = map(vec![("values", N::List(values, None))]);
        ev.set("breadcrumbs", if sc.truncated > 0 {
            let len = sc.breadcrumbs.len() + sc.truncated;
            N::Ann(Box::new(crumbs), vec![("len", json!(len))])
        } else {
            crumbs
        });
    }
    // DedupeIntegration: the same exception object twice in a row is sent once
    if let Some(e) = &c.exc {
        let mut last = i.last_exc.lock();
        if last.as_ref().is_some_and(|l| Arc::ptr_eq(&l.0, &e.0)) {
            return None;
        }
        *last = Some(e.clone());
    }
    // the request's event processors (SentryAsgiMiddleware, then Starlette's extractor in the route)
    if trace.in_request {
        let info = request_info(cx, o, trace.in_route).await;
        let req = ev.setdefault_map("request");
        if let N::Map(items, _) = info {
            for (k, v) in items {
                req.set(&k, v);
            }
        }
    }
    if c.is_tx {
        // sample_rate applies to errors only
    } else if o.sample_rate < 1.0 && rand::random::<f64>() >= o.sample_rate {
        return None;
    }
    for k in ["release", "environment", "server_name", "dist"] {
        if ev.get(k).is_none() {
            let v = match k {
                "release" => o.release.clone(),
                "environment" => Some(o.environment.clone()),
                "server_name" => o.server_name.clone(),
                _ => o.dist.clone(),
            };
            if let Some(v) = v {
                ev.set(k, s(v));
            }
        }
    }
    if ev.get("sdk").is_none() {
        let mut integ: Vec<N> = vec![s("py2axum")];
        integ.extend(o.integrations.iter().map(|n| s(*n)));
        ev.set(
            "sdk",
            map(vec![
                ("name", s("sentry.rust")),
                ("version", s(SDK_VERSION)),
                ("integrations", N::List(integ, None)),
                ("packages", N::List(vec![map(vec![("name", s("cargo:sentry")), ("version", s(SDK_VERSION))])], None)),
            ]),
        );
    }
    if ev.get("platform").is_none() {
        ev.set("platform", s("python"));
    }
    scrub(ev, o.pii);
    let mut ser = Ser { path: vec![], meta: Map::new(), max_len: o.max_value_length, keep_bodies: o.body_size == BodySize::Always };
    let mut out = ser.node(ev, None, None, None);
    if !ser.meta.is_empty() {
        if let J::Object(m) = &mut out {
            m.insert("_meta".into(), J::Object(std::mem::take(&mut ser.meta)));
        }
    }
    let hook = if c.is_tx { &o.before_send_transaction } else { &o.before_send };
    if let Some(f) = hook {
        let mut hint: Vec<(V, V)> = vec![];
        if let Some(e) = &c.exc {
            hint.push((V::str("exc_info"), V::tuple(vec![V::Class(e.0.class), V::Exc(e.clone()), V::None])));
        }
        hint.push((V::str("attachments"), V::list(vec![])));
        let hint = V::dict_from(hint).unwrap_or(V::None);
        let ev_v = super::pyd::from_serde(&out);
        match super::methods::call_value(cx, f, vec![ev_v, hint], vec![]).await {
            Ok(V::None) | Err(_) => {
                if c.exc.is_some() {
                    *i.last_exc.lock() = None;
                }
                return None;
            }
            Ok(v) => out = to_json(&v),
        }
    }
    let item_type = if c.is_tx { "transaction" } else { "event" };
    send(o, &event_id, item_type, &out, &trace);
    if !c.is_tx {
        *i.last_event_id.lock() = Some(event_id.clone());
    }
    Some(event_id)
}

const SDK_VERSION: &str = "0.46.2";

/// The envelope: the event item as JSON, sent by the crate's transport.
fn send(o: &Opts, event_id: &str, item_type: &str, payload: &J, trace: &Trace) {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let mut dsc = Map::new();
    dsc.insert("trace_id".into(), J::String(trace.trace_id.clone()));
    dsc.insert("public_key".into(), J::String(o.public_key.clone()));
    dsc.insert("environment".into(), J::String(o.environment.clone()));
    if let Some(r) = &o.release {
        dsc.insert("release".into(), J::String(r.clone()));
    }
    if let (Some(sampled), Some((name, src))) = (trace.sampled, &trace.name) {
        if *src == "route" {
            dsc.insert("transaction".into(), J::String(name.clone()));
        }
        dsc.insert("sampled".into(), J::String(sampled.to_string()));
        if let Some(r) = trace.sample_rate {
            dsc.insert("sample_rate".into(), J::String(r.to_string()));
        }
        dsc.insert("sample_rand".into(), J::String(format!("{:.6}", trace.sample_rand)));
    }
    let head = json!({"event_id": event_id, "sent_at": fmt_ts(chrono::Utc::now()), "trace": J::Object(dsc)});
    let item = json!({"type": item_type, "content_type": "application/json", "length": body.len()});
    let mut raw = serde_json::to_vec(&head).unwrap_or_default();
    raw.push(b'\n');
    raw.extend(serde_json::to_vec(&item).unwrap_or_default());
    raw.push(b'\n');
    raw.extend(body);
    raw.push(b'\n');
    if let Ok(env) = sentry::Envelope::from_bytes_raw(raw) {
        o.client.send_envelope(env);
    }
}

/// The value `before_send` returned, as JSON (sent as is, like `json.dumps(event)`).
fn to_json(v: &V) -> J {
    match v {
        V::None | V::Unbound => J::Null,
        V::Bool(b) => J::Bool(*b),
        V::Int(i) => json!(i),
        V::Float(f) => json!(f),
        V::Str(x) => J::String(x.to_string()),
        V::List(l) => J::Array(l.lock().iter().map(to_json).collect()),
        V::Tuple(t) => J::Array(t.iter().map(to_json).collect()),
        V::Dict(m) => {
            let mut o = Map::new();
            for (k, x) in m.lock().values() {
                o.insert(ops::str_(k).unwrap_or_default(), to_json(x));
            }
            J::Object(o)
        }
        o => J::String(ops::str_(o).unwrap_or_default()),
    }
}

// ---------------------------------------------------------------- EventScrubber

const DENYLIST: &[&str] = &[
    "password", "passwd", "secret", "api_key", "apikey", "auth", "credentials", "mysql_pwd", "privatekey", "private_key", "token",
    "session", "csrftoken", "sessionid", "x_csrftoken", "x_forwarded_for", "set_cookie", "cookie", "authorization",
    "proxy-authorization", "x_api_key", "aiohttp_session", "connect.sid", "csrf_token", "csrf", "_csrf", "_csrf_token",
    "phpsessid", "_session", "symfony", "user_session", "_xsrf", "xsrf-token",
];
const PII_DENYLIST: &[&str] = &["x_forwarded_for", "x_real_ip", "ip_address", "remote_addr"];

fn denied(k: &str, pii: bool) -> bool {
    let l = k.to_lowercase();
    DENYLIST.contains(&l.as_str()) || (!pii && PII_DENYLIST.contains(&l.as_str()))
}

fn filtered() -> N {
    N::Ann(Box::new(s("[Filtered]")), vec![("rem", json!([["!config", "s"]]))])
}

fn scrub_dict(n: &mut N, pii: bool) {
    if let N::Py(v @ V::Dict(_)) = n {
        *n = shallow(&v.clone());
    }
    if let N::Map(m, orig) = n {
        let mut hit = false;
        for (k, v) in m.iter_mut() {
            if denied(k, pii) {
                *v = filtered();
                hit = true;
            }
        }
        if hit {
            *orig = None;
        }
    }
}

fn scrub(ev: &mut N, pii: bool) {
    if let Some(req) = ev.get_mut("request") {
        for k in ["headers", "cookies", "data"] {
            if let Some(x) = req.get_mut(k) {
                scrub_dict(x, pii);
            }
        }
    }
    if let Some(x) = ev.get_mut("extra") {
        scrub_dict(x, pii);
    }
    if let Some(u) = ev.get_mut("user") {
        if !pii {
            if let N::Py(v @ V::Dict(_)) = u {
                *u = shallow(&v.clone());
            }
            if let N::Map(m, orig) = u {
                if m.iter().any(|(k, _)| k == "ip_address") {
                    m.retain(|(k, _)| k != "ip_address");
                    *orig = None;
                }
            }
        }
        scrub_dict(u, pii);
    }
    if let Some(b) = ev.get_mut("breadcrumbs") {
        let b = match b {
            N::Ann(x, _) => &mut **x,
            x => x,
        };
        if let Some(N::List(vals, _)) = b.get_mut("values") {
            for cr in vals {
                if let Some(d) = cr.get_mut("data") {
                    scrub_dict(d, pii);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- serializer (sentry_sdk.serializer)

struct Ser {
    path: Vec<String>,
    meta: Map<String, J>,
    max_len: Option<usize>,
    keep_bodies: bool,
}

const MAX_DEPTH: i64 = 5;
const MAX_BREADTH: i64 = 10;

impl Ser {
    fn annotate(&mut self, items: Vec<(&str, J)>) {
        let mut node = &mut self.meta;
        for seg in &self.path {
            let e = node.entry(seg.clone()).or_insert_with(|| J::Object(Map::new()));
            if !e.is_object() {
                *e = J::Object(Map::new());
            }
            node = e.as_object_mut().unwrap();
        }
        let e = node.entry(String::new()).or_insert_with(|| J::Object(Map::new()));
        if let J::Object(m) = e {
            for (k, v) in items {
                m.insert(k.to_string(), v);
            }
        }
    }

    fn is_request_body(&self) -> Option<bool> {
        let p0 = self.path.first()?;
        if p0 != "request" {
            return Some(false);
        }
        Some(self.path.get(1)? == "data")
    }

    fn is_databag(&self) -> Option<bool> {
        match self.is_request_body() {
            Some(true) => return Some(true),
            None => return None,
            _ => {}
        }
        let p0 = self.path.first()?;
        if p0 == "breadcrumbs" {
            if self.path.get(1)? == "values" {
                self.path.get(2)?;
                return Some(true);
            }
            return Some(false);
        }
        Some(p0 == "extra")
    }

    fn strip(&mut self, v: String) -> J {
        let Some(max) = self.max_len else { return J::String(v) };
        if v.is_empty() {
            return J::String(v);
        }
        let bytes = v.len();
        if bytes > max {
            let cut = &v.as_bytes()[..max.saturating_sub(3).min(bytes)];
            let t = String::from_utf8_lossy(cut);
            let t = t.trim_end_matches('\u{FFFD}').to_string() + "...";
            self.annotate(vec![("len", json!(bytes)), ("rem", json!([["!limit", "x", max as i64 - 3, max]]))]);
            return J::String(t);
        }
        J::String(v)
    }

    fn node(&mut self, n: &N, databag: Option<bool>, depth: Option<i64>, breadth: Option<i64>) -> J {
        let databag = databag.or_else(|| self.is_databag());
        let (mut depth, mut breadth) = (depth, breadth);
        if databag == Some(true) {
            if self.is_request_body() == Some(true) && self.keep_bodies {
                depth = None;
                breadth = None;
            } else {
                depth = depth.or(Some(MAX_DEPTH));
                breadth = breadth.or(Some(MAX_BREADTH));
            }
        }
        let n = match n {
            N::Ann(v, meta) => {
                self.annotate(meta.clone());
                return self.node_inner(v, databag, depth, breadth);
            }
            x => x,
        };
        self.node_inner(n, databag, depth, breadth)
    }

    fn node_inner(&mut self, n: &N, databag: Option<bool>, depth: Option<i64>, breadth: Option<i64>) -> J {
        if let N::Py(v) = n {
            let conv = from_py(v, 0);
            return self.node_inner(&conv, databag, depth, breadth);
        }
        if depth.is_some_and(|d| d <= 0) {
            self.annotate(vec![("rem", json!([["!limit", "x"]]))]);
            if databag == Some(true) {
                return self.strip(repr_n(n));
            }
            return J::Null;
        }
        match n {
            N::Null => J::Null,
            N::Bool(b) => J::Bool(*b),
            N::Int(i) => json!(i),
            N::Float(f) => {
                if f.is_finite() {
                    json!(f)
                } else {
                    J::String(ops::float_repr(*f))
                }
            }
            N::Str(x) => self.strip(x.clone()),
            N::Map(items, _) => {
                let mut out = Map::new();
                for (i, (k, v)) in items.iter().enumerate() {
                    if breadth.is_some_and(|b| i as i64 >= b) {
                        self.annotate(vec![("len", json!(items.len()))]);
                        break;
                    }
                    self.path.push(k.clone());
                    let x = self.node(v, databag, depth.map(|d| d - 1), breadth);
                    self.path.pop();
                    out.insert(k.clone(), x);
                }
                J::Object(out)
            }
            N::List(items, _) => {
                let mut out = vec![];
                for (i, v) in items.iter().enumerate() {
                    if breadth.is_some_and(|b| i as i64 >= b) {
                        self.annotate(vec![("len", json!(items.len()))]);
                        break;
                    }
                    self.path.push(i.to_string());
                    let x = self.node(v, databag, depth.map(|d| d - 1), breadth);
                    self.path.pop();
                    out.push(x);
                }
                J::Array(out)
            }
            N::Ann(v, meta) => {
                self.annotate(meta.clone());
                self.node_inner(v, databag, depth, breadth)
            }
            N::Py(_) => unreachable!(),
        }
    }
}

/// `safe_repr` of what a node was (a dict or list cut by the depth limit)
fn repr_n(n: &N) -> String {
    match n {
        N::Map(_, Some(v)) | N::List(_, Some(v)) => ops::repr(v).unwrap_or_default(),
        N::Str(x) => ops::repr(&V::str(x)).unwrap_or_default(),
        N::Int(i) => i.to_string(),
        N::Float(f) => ops::float_repr(*f),
        N::Bool(b) => if *b { "True" } else { "False" }.into(),
        N::Null => "None".into(),
        _ => String::new(),
    }
}

fn naive_utc(d: &super::dt::DateTime) -> chrono::NaiveDateTime {
    if d.tz.is_some() {
        return d.utc();
    }
    use chrono::TimeZone;
    match chrono::Local.from_local_datetime(&d.wall).earliest() {
        Some(l) => l.naive_utc(),
        None => d.wall,
    }
}

/// A Python value as a node: mappings and sequences walked, `datetime` formatted, strings kept,
/// anything else by its `repr` (the serializer's rules).
fn from_py(v: &V, level: usize) -> N {
    if level > 64 {
        return s("<cyclic>");
    }
    match v {
        V::None | V::Unbound => N::Null,
        V::Bool(b) => N::Bool(*b),
        V::Int(i) => N::Int(*i),
        V::Float(f) => N::Float(*f),
        V::Str(x) => N::Str(x.to_string()),
        V::Bytes(b) => N::Str(String::from_utf8_lossy(b).into_owned()),
        V::DateTime(d) => s(naive_utc(d).and_utc().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()),
        V::Dict(m) => {
            let items: Vec<(V, V)> = m.lock().values().cloned().collect();
            N::Map(items.iter().map(|(k, x)| (ops::str_(k).unwrap_or_default(), from_py(x, level + 1))).collect(), Some(v.clone()))
        }
        V::List(l) => {
            let items = l.lock().clone();
            N::List(items.iter().map(|x| from_py(x, level + 1)).collect(), Some(v.clone()))
        }
        V::Tuple(t) => N::List(t.iter().map(|x| from_py(x, level + 1)).collect(), Some(v.clone())),
        V::Set(m) => {
            let items: Vec<V> = m.lock().values().cloned().collect();
            N::List(items.iter().map(|x| from_py(x, level + 1)).collect(), Some(v.clone()))
        }
        V::Enum(e, i) => match e.kind {
            EnumKind::Str | EnumKind::StrEnum => from_py(&e.value(*i), level + 1),
            EnumKind::Int | EnumKind::IntEnum => from_py(&e.value(*i), level + 1),
            EnumKind::Plain => s(ops::repr(v).unwrap_or_default()),
        },
        o => s(ops::repr(o).unwrap_or_default()),
    }
}

// ---------------------------------------------------------------- capture API

fn level_kw(kw: &[(String, V)]) -> R<Option<String>> {
    match kwarg(kw, "level") {
        None | Some(V::None) => Ok(None),
        Some(v) => Ok(Some(level_str(v)?)),
    }
}

/// `capture_message(message, level=None, scope=None, **scope_kwargs)`
pub async fn capture_message(cx: &Cx, src: Option<&'static str>, args: Vec<V>, kw: Vec<(String, V)>) -> R {
    capture_message_in(cx, src, args, kw, None).await
}

/// The scope keywords of `capture_*` (`tags=`, `extras=`, `contexts=`, `user=`, `fingerprint=`) as a scope
/// merged last.
fn scope_kwargs(kw: &[(String, V)], base: Option<Scope>) -> R<Option<Scope>> {
    let keys = ["tags", "extras", "contexts", "user", "fingerprint"];
    if !kw.iter().any(|(k, _)| keys.contains(&k.as_str())) {
        return Ok(base);
    }
    let s: Scope = Arc::new(Mutex::new(base.map(|b| b.lock().clone()).unwrap_or_default()));
    for (k, v) in kw {
        match k.as_str() {
            "tags" => {
                scope_set_tags(&s, v)?;
            }
            "extras" | "contexts" => {
                let V::Dict(m) = v else { return Err(Exc::type_error(format!("py2axum: {k}= expects a dict"))) };
                let items: Vec<(V, V)> = m.lock().values().cloned().collect();
                for (a, b) in items {
                    if k == "extras" {
                        scope_set_extra(&s, &a, &b)?;
                    } else {
                        scope_set_context(&s, &a, &b)?;
                    }
                }
            }
            "user" => {
                scope_set_user(&s, v)?;
            }
            "fingerprint" => s.lock().fingerprint = Some(v.clone()),
            _ => {}
        }
    }
    Ok(Some(s))
}

async fn capture_message_in(cx: &Cx, src: Option<&'static str>, args: Vec<V>, kw: Vec<(String, V)>, scope: Option<Scope>) -> R {
    let Some(o) = opts() else { return Ok(V::None) };
    let msg = args
        .first()
        .or_else(|| kwarg(&kw, "message"))
        .cloned()
        .ok_or_else(|| Exc::type_error("capture_message() missing 1 required positional argument: 'message'"))?;
    let level = match level_kw(&kw)? {
        Some(l) => l,
        None => "info".into(),
    };
    let extra = scope_kwargs(&kw, scope)?;
    let i = iso(cx);
    let event = N::Map(vec![("message".into(), N::Py(msg)), ("level".into(), s(level))], None);
    let id = capture(cx, &o, &i, extra, Capture { event, exc: None, is_tx: false, trace: None }, src).await;
    Ok(id.map(V::str).unwrap_or(V::None))
}

/// `capture_exception(error=None, scope=None, **scope_kwargs)`: without an error, the exception being
/// handled (`sys.exc_info()`), none outside an `except` block.
pub async fn capture_exception(cx: &Cx, src: Option<&'static str>, args: Vec<V>, kw: Vec<(String, V)>) -> R {
    capture_exception_in(cx, src, args, kw, None).await
}

fn exc_of(v: &V) -> R<Option<Exc>> {
    match v {
        V::None => Ok(None),
        V::Exc(e) => Ok(Some(e.clone())),
        V::Tuple(t) if t.len() == 3 => exc_of(&t[1]),
        o => Err(Exc::type_error(format!("py2axum: capture_exception() expects an exception, not {}", o.type_name()))),
    }
}

async fn capture_exception_in(cx: &Cx, src: Option<&'static str>, args: Vec<V>, kw: Vec<(String, V)>, scope: Option<Scope>) -> R {
    let Some(o) = opts() else { return Ok(V::None) };
    let e = match args.first().or_else(|| kwarg(&kw, "error")) {
        Some(v) => exc_of(v)?,
        None => None,
    };
    let Some(e) = e.or_else(|| cx.handling.lock().last().cloned()) else {
        return Ok(V::None);
    };
    let extra = scope_kwargs(&kw, scope)?;
    let i = iso(cx);
    let id = capture_exc(cx, &o, &i, &e, json!({"type": "generic", "handled": true}), extra, src, None).await;
    Ok(id.map(V::str).unwrap_or(V::None))
}

#[allow(clippy::too_many_arguments)]
async fn capture_exc(cx: &Cx, o: &Opts, i: &Arc<Iso>, e: &Exc, mechanism: J, extra: Option<Scope>, src: Option<&'static str>, more: Option<Vec<(String, N)>>) -> Option<String> {
    capture_exc_at(cx, o, i, e, mechanism, extra, src, more, None).await
}

#[allow(clippy::too_many_arguments)]
async fn capture_exc_at(cx: &Cx, o: &Opts, i: &Arc<Iso>, e: &Exc, mechanism: J, extra: Option<Scope>, src: Option<&'static str>, more: Option<Vec<(String, N)>>, trace: Option<Trace>) -> Option<String> {
    let route = i.trace.lock().route;
    let entry = exception_entry(cx, e, mechanism, route.map(|r| (r.0, r.1, r.2))).await;
    let mut event = N::Map(vec![("level".to_string(), s("error")), ("exception".to_string(), map(vec![("values", N::List(vec![entry], None))]))], None);
    for (k, v) in more.unwrap_or_default() {
        event.set(&k, v);
    }
    capture(cx, o, i, extra, Capture { event, exc: Some(e.clone()), is_tx: false, trace }, src).await
}

// ---------------------------------------------------------------- logging integration

/// A log record that passed the logger's level: an event from `event_level`, then a breadcrumb from
/// `level` (LoggingIntegration's order). `exc` is the record's exc_info.
#[allow(clippy::too_many_arguments)]
pub async fn log_record(cx: &Cx, logger: &str, levelno: i64, msg: &V, args: &[V], formatted: &str, exc: Option<Exc>, extra: Option<V>) {
    log_record_at(cx, logger, levelno, msg, args, formatted, exc, extra, None).await
}

#[allow(clippy::too_many_arguments)]
async fn log_record_at(cx: &Cx, logger: &str, levelno: i64, msg: &V, args: &[V], formatted: &str, exc: Option<Exc>, extra: Option<V>, trace: Option<Trace>) {
    let Some(o) = opts() else { return };
    let Some((crumb_level, event_level)) = o.logging else { return };
    if ["sentry_sdk.errors", "urllib3.connectionpool", "urllib3.connection"].contains(&logger.trim()) {
        return;
    }
    let level = level_name(levelno);
    let extra_v = extra.unwrap_or_else(|| V::dict_from(vec![]).unwrap_or(V::None));
    let i = iso(cx);
    if event_level.is_some_and(|l| levelno >= l) {
        let params = match args {
            [d @ V::Dict(_)] => N::Py(d.clone()),
            _ => N::List(args.iter().map(|a| N::Py(a.clone())).collect(), None),
        };
        let more = vec![
            ("logger".to_string(), s(logger)),
            ("logentry".to_string(), map(vec![("message", s(ops::str_(msg).unwrap_or_default())), ("formatted", s(formatted)), ("params", params)])),
            ("extra".to_string(), shallow(&extra_v)),
        ];
        match &exc {
            Some(e) => {
                let mut more = more;
                more.insert(0, ("level".to_string(), s(&level)));
                let _ = capture_exc_at(cx, &o, &i, e, json!({"type": "logging", "handled": true}), None, None, Some(more), trace).await;
            }
            None => {
                let mut items = vec![("level".to_string(), s(&level))];
                items.extend(more);
                let _ = capture(cx, &o, &i, None, Capture { event: N::Map(items, None), exc: None, is_tx: false, trace }, None).await;
            }
        }
    }
    if crumb_level.is_some_and(|l| levelno >= l) {
        push_crumb(
            &i.data,
            vec![
                ("type".into(), V::str("log")),
                ("level".into(), V::str(level)),
                ("category".into(), V::str(logger)),
                ("message".into(), V::str(formatted)),
                ("data".into(), extra_v),
            ],
        );
    }
}

// ---------------------------------------------------------------- FastAPI / Starlette integration

const TRACED_METHODS: &[&str] = &["CONNECT", "DELETE", "GET", "PATCH", "POST", "PUT", "TRACE"];

fn span_status(code: u16) -> &'static str {
    match code {
        c if c < 400 => "ok",
        403 => "permission_denied",
        404 => "not_found",
        429 => "resource_exhausted",
        413 => "failed_precondition",
        401 => "unauthenticated",
        409 => "already_exists",
        400..=499 => "invalid_argument",
        504 => "deadline_exceeded",
        501 => "unimplemented",
        503 => "unavailable",
        500..=599 => "internal_error",
        _ => "unknown_error",
    }
}

/// SentryAsgiMiddleware at the start of a request: the isolation scope forked (breadcrumbs cleared),
/// the trace continued from `sentry-trace`, the transaction named after the URL until a route
/// matches, the sampling decision (`traces_sampler` called with the ASGI scope). False when inactive.
pub async fn request_start(cx: &Cx) -> bool {
    let Some(o) = opts() else { return false };
    if !o.starlette {
        return false;
    }
    let i = iso(cx);
    let url = {
        let host = cx.req.header("host");
        let path = super::web::unquote(&cx.req.path);
        match host {
            Some(h) => format!("http://{h}{path}"),
            None => path,
        }
    };
    let mut parent_sampled = None;
    {
        let mut t = i.trace.lock();
        if let Some(h) = cx.req.header("sentry-trace") {
            let parts: Vec<&str> = h.trim().split('-').collect();
            if parts.len() >= 2 && parts[0].len() == 32 && parts[1].len() == 16 {
                t.trace_id = parts[0].to_string();
                t.parent_span_id = Some(parts[1].to_string());
                parent_sampled = parts.get(2).map(|x| *x == "1");
            }
        }
        if let Some(b) = cx.req.header("baggage") {
            for kv in b.split(',') {
                if let Some(r) = kv.trim().strip_prefix("sentry-sample_rand=") {
                    if let Ok(x) = r.parse::<f64>() {
                        t.sample_rand = x;
                    }
                }
            }
        }
        t.name = Some((url, "url"));
        t.start = Some(chrono::Utc::now());
        t.in_request = true;
        t.http = TRACED_METHODS.contains(&cx.req.method.as_str());
    }
    if !i.trace.lock().http || !o.tracing() {
        return true;
    }
    let rate = match &o.traces_sampler {
        Some(f) => {
            let ctx = sampling_context(cx, &i, parent_sampled);
            match super::methods::call_value(cx, f, vec![ctx], vec![]).await {
                Ok(V::Bool(b)) => Some(if b { 1.0 } else { 0.0 }),
                Ok(V::Int(n)) => Some(n as f64),
                Ok(V::Float(f)) => Some(f),
                _ => None,
            }
        }
        None => match parent_sampled {
            Some(p) => Some(if p { 1.0 } else { 0.0 }),
            None => o.traces_sample_rate,
        },
    };
    let rate = rate.filter(|r| (0.0..=1.0).contains(r));
    let mut t = i.trace.lock();
    t.sample_rate = rate;
    t.sampled = Some(rate.is_some_and(|r| t.sample_rand < r));
    true
}

fn sampling_context(cx: &Cx, i: &Iso, parent_sampled: Option<bool>) -> V {
    let t = i.trace.lock().clone();
    let scope = super::routing::scope(cx).unwrap_or(V::None);
    let tctx = V::dict_from(vec![
        (V::str("trace_id"), V::str(&t.trace_id)),
        (V::str("span_id"), V::str(&t.span_id)),
        (V::str("parent_span_id"), t.parent_span_id.clone().map(V::str).unwrap_or(V::None)),
        (V::str("same_process_as_parent"), V::Bool(true)),
        (V::str("op"), V::str("http.server")),
        (V::str("description"), V::None),
        (V::str("origin"), V::str("auto.http.starlette")),
        (V::str("name"), V::str(t.name.as_ref().map(|n| n.0.as_str()).unwrap_or(""))),
        (V::str("source"), V::str("url")),
    ])
    .unwrap_or(V::None);
    V::dict_from(vec![
        (V::str("transaction_context"), tctx),
        (V::str("parent_sampled"), parent_sampled.map(V::Bool).unwrap_or(V::None)),
        (V::str("asgi_scope"), scope),
    ])
    .unwrap_or(V::None)
}

/// The router chose a route (Starlette's `scope["route"]`): the transaction takes its path; `src` is the
/// handler (`file.py:line`, function, module) when it is a declared route.
pub fn route_matched(cx: &Cx, path: String, src: Option<(&'static str, &'static str, &'static str)>, in_route: bool) {
    let Some(i) = cx.sentry.get() else { return };
    let mut t = i.trace.lock();
    if !t.in_request {
        return;
    }
    t.name = Some((path, "route"));
    t.route = src;
    t.in_route = in_route;
}

/// ExceptionMiddleware ran a handler for this exception: captured when its `status_code` is a 5xx
/// (`handled: true`).
pub async fn handled_exception(cx: &Cx, e: &Exc) {
    let Some(o) = opts() else { return };
    if !o.starlette || cx.sentry.get().is_none() {
        return;
    }
    let code = match e.http_info() {
        Some((c, ..)) => Some(c as i64),
        None => {
            let has = e.0.attrs.lock().contains_key("status_code") || e.0.class.exc_lookup("status_code").is_some();
            if has {
                match super::methods::getattr(cx, &V::Exc(e.clone()), "status_code").await {
                    Ok(V::Int(c)) => Some(c),
                    _ => None,
                }
            } else {
                None
            }
        }
    };
    if !code.is_some_and(|c| (500..600).contains(&c)) {
        return;
    }
    let i = iso(cx);
    let _ = capture_exc(cx, &o, &i, e, json!({"type": "starlette", "handled": true}), None, None, None).await;
}

/// An exception left the application (ServerErrorMiddleware answered 500): `handled: false`, after the
/// response status is known.
pub async fn unhandled(cx: &Cx, e: &Exc) {
    let Some(o) = opts() else { return };
    if !o.starlette || cx.sentry.get().is_none() {
        return;
    }
    let i = iso(cx);
    if e.http_info().is_some_and(|(c, ..)| c < 500) {
        return;
    }
    {
        let mut t = i.trace.lock();
        if t.http {
            t.status = Some("internal_error");
            t.status_code = Some(500);
        }
    }
    let _ = capture_exc(cx, &o, &i, e, json!({"type": "starlette", "handled": false}), None, None, None).await;
}

/// Code Python runs in a copy of the context (the task of a BaseHTTPMiddleware's `call_next`, the thread
/// of a `def` endpoint): what DedupeIntegration remembers there does not come back.
pub fn context_enter(cx: &Cx) -> Option<Option<Exc>> {
    cx.sentry.get().map(|i| i.last_exc.lock().clone())
}

pub fn context_exit(cx: &Cx, saved: Option<Option<Exc>>) {
    if let (Some(i), Some(s)) = (cx.sentry.get(), saved) {
        *i.last_exc.lock() = s;
    }
}

/// The response is ready: the transaction, when sampled.
pub async fn request_end(cx: &Cx, status: u16) {
    let Some(o) = opts() else { return };
    let Some(i) = cx.sentry.get().cloned() else { return };
    let t = {
        let mut t = i.trace.lock();
        if !t.http {
            return;
        }
        t.status = Some(span_status(status));
        t.status_code = Some(status);
        t.clone()
    };
    if t.sampled != Some(true) {
        return;
    }
    let (name, src) = t.name.clone().unwrap_or_default();
    let event = N::Map(
        vec![
            ("type".into(), s("transaction")),
            ("transaction".into(), s(name)),
            ("transaction_info".into(), map(vec![("source", s(src))])),
            ("contexts".into(), map(vec![("response", map(vec![("status_code", N::Int(status as i64))])), ("trace", trace_context(&t))])),
            ("tags".into(), map(vec![("asgi.type", s("http")), ("http.status_code", s(status.to_string()))])),
            ("timestamp".into(), s(fmt_ts(chrono::Utc::now()))),
            ("start_timestamp".into(), s(fmt_ts(t.start.unwrap_or_else(chrono::Utc::now)))),
            ("spans".into(), N::List(vec![], None)),
            ("measurements".into(), map(vec![])),
        ],
        None,
    );
    let _ = capture(cx, &o, &i, None, Capture { event, exc: None, is_tx: true, trace: None }, None).await;
}

// ---------------------------------------------------------------- asyncio

/// `Task exception was never retrieved`: the task was dropped without its exception being read
/// (asyncio's default exception handler logs it on the `asyncio` logger, the logging integration
/// turns it into an event).
pub fn task_never_retrieved(cx: Cx, e: Exc) {
    if !active() {
        return;
    }
    let Ok(h) = tokio::runtime::Handle::try_current() else { return };
    let trace = cx.sentry.get().map(|i| i.trace.lock().clone());
    h.spawn(async move {
        let repr = ops::repr(&V::Exc(e.clone())).unwrap_or_default();
        let msg = format!("Task exception was never retrieved\nfuture: <Task finished exception={repr}>");
        log_record_at(&cx, "asyncio", 40, &V::str(&msg), &[], &msg, Some(e), None, trace).await;
    });
}
