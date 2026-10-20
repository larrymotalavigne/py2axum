//! stripe-python 14.x (14.0.0 to 14.4.1): the legacy resource API (`stripe.Customer.create(...)`), its module
//! configuration, its errors, its objects and `stripe.Webhook`, as stripe-python sends and builds them.
//!
//! A request is the one `_APIRequestor` makes (options split from the parameters, `_api_encode` + `urlencode`
//! form encoding, the same headers, `requests` timeouts and `HTTPClient` retries), its response the object
//! `_convert_to_stripe_object` builds. A `StripeObject` is a dict subclass: here an ordinary dict registered
//! with its class (as `defaultdict`s are, `ops::default_factory`), so everything a dict does (keys, `len`,
//! JSON, Pydantic, `dict(obj)`) is unchanged and only what the subclass adds looks the tag up: attribute access,
//! `str`/`repr`, the empty-string check of `__setitem__`, and a list object's `__iter__`/`__len__`/`__getitem__`.
//! The tables (classes, resource requests) come from stripe-python itself: tools/gen_stripe.py.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, Weak};

use hmac::Mac;
use indexmap::IndexMap;
use parking_lot::Mutex;

use super::logging::Loc;
use super::stripetab::{CLASS_ATTRS, EXTRA, METHODS, OBJECTS};
use super::v::*;
use super::{methods, ops, Cx};

type DictCell = Mutex<IndexMap<Key, (V, V)>>;

// ---------------------------------------------------------------- version

static VERSION: OnceLock<&'static str> = OnceLock::new();

/// The project's locked stripe-python (main.rs, before the module globals)
pub fn set_version(ver: &'static str) {
    let _ = VERSION.get_or_init(|| ver);
}

fn version() -> &'static str {
    VERSION.get().copied().unwrap_or("14.4.1")
}

fn at_least(major: u32, minor: u32, patch: u32) -> bool {
    let mut it = version().split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0)) >= (major, minor, patch)
}

/// `stripe._api_version._ApiVersion.CURRENT` of the release
fn default_api_version() -> &'static str {
    if at_least(14, 4, 0) {
        "2026-02-25.clover"
    } else if at_least(14, 3, 0) {
        "2026-01-28.clover"
    } else if at_least(14, 1, 0) {
        "2025-12-15.clover"
    } else {
        "2025-11-17.clover"
    }
}

// ---------------------------------------------------------------- module configuration

struct Conf {
    api_key: V,
    api_base: V,
    api_version: Option<V>,
    max_network_retries: V,
    enable_telemetry: V,
    log: V,
}

static CONF: LazyLock<Mutex<Conf>> = LazyLock::new(|| {
    Mutex::new(Conf {
        api_key: V::None,
        api_base: V::str("https://api.stripe.com"),
        api_version: None,
        max_network_retries: V::Int(2),
        enable_telemetry: V::Bool(true),
        log: V::None,
    })
});

/// The module attributes the binary keeps (`stripe.api_key`...): the names libmap maps
pub const CONF_NAMES: [&str; 6] = ["api_key", "api_base", "api_version", "max_network_retries", "enable_telemetry", "log"];

/// `stripe.<name>` read (the names libmap maps)
pub fn conf(name: &str) -> V {
    let c = CONF.lock();
    match name {
        "api_key" => c.api_key.clone(),
        "api_base" => c.api_base.clone(),
        "api_version" => c.api_version.clone().unwrap_or_else(|| V::str(default_api_version())),
        "max_network_retries" => c.max_network_retries.clone(),
        "enable_telemetry" => c.enable_telemetry.clone(),
        "log" => c.log.clone(),
        "VERSION" => V::str(version()),
        _ => V::None,
    }
}

/// `stripe.<name> = v`
pub fn set_conf(name: &str, v: V) -> R {
    let mut c = CONF.lock();
    match name {
        "api_key" => c.api_key = v,
        "api_base" => c.api_base = v,
        "api_version" => c.api_version = Some(v),
        "max_network_retries" => c.max_network_retries = v,
        "enable_telemetry" => c.enable_telemetry = v,
        "log" => c.log = v,
        _ => return Err(Exc::runtime(format!("py2axum: setting stripe.{name} is not supported"))),
    }
    Ok(V::None)
}

// ---------------------------------------------------------------- objects: dicts registered with their class

/// What the class of a StripeObject adds to its dict: the class (index in OBJECTS, then EXTRA) and, for a list
/// or search result, its `_retrieve_params` and requestor (what `next_page` and `auto_paging_iter` reuse).
#[derive(Clone)]
struct Tag {
    cls: u16,
    list: Option<Arc<ListCtx>>,
}

struct ListCtx {
    params: Vec<(String, V)>,
    req: Req,
}

static ANY: AtomicBool = AtomicBool::new(false);
static TAGS: LazyLock<Mutex<std::collections::HashMap<usize, (Weak<DictCell>, Tag)>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn register(d: &Arc<DictCell>, tag: Tag) {
    let mut reg = TAGS.lock();
    if reg.len() >= 64 && reg.len().is_power_of_two() {
        reg.retain(|_, (w, _)| w.strong_count() > 0);
    }
    reg.insert(Arc::as_ptr(d) as usize, (Arc::downgrade(d), tag));
    ANY.store(true, Ordering::Relaxed);
}

fn tag(d: &Arc<DictCell>) -> Option<Tag> {
    if !ANY.load(Ordering::Relaxed) {
        return None;
    }
    let reg = TAGS.lock();
    reg.get(&(Arc::as_ptr(d) as usize)).filter(|(w, _)| w.strong_count() > 0).map(|(_, t)| t.clone())
}

/// The class of a StripeObject (a registered dict), or None for any other value
pub fn class_of(v: &V) -> Option<u16> {
    match v {
        V::Dict(d) => tag(d).map(|t| t.cls),
        _ => None,
    }
}

const STRIPE_OBJECT: u16 = OBJECTS.len() as u16;
const ERROR_OBJECT: u16 = OBJECTS.len() as u16 + 1;
const OAUTH_ERROR_OBJECT: u16 = OBJECTS.len() as u16 + 2;

fn class_name(cls: u16) -> &'static str {
    let i = cls as usize;
    if i < OBJECTS.len() { OBJECTS[i].1 } else { EXTRA[i - OBJECTS.len()] }
}

fn object_class(name: &str) -> u16 {
    // classes added by releases after the lowest supported one
    if !at_least(14, 4, 0) && (name == "radar.payment_evaluation" || name.starts_with("reserve.")) {
        return STRIPE_OBJECT;
    }
    match OBJECTS.binary_search_by(|(k, _)| (*k).cmp(name)) {
        Ok(i) => i as u16,
        Err(_) => STRIPE_OBJECT,
    }
}

fn is_list_class(cls: u16) -> bool {
    let i = cls as usize;
    i < OBJECTS.len() && (OBJECTS[i].0 == "list" || OBJECTS[i].0 == "search_result")
}

fn is_search_class(cls: u16) -> bool {
    let i = cls as usize;
    i < OBJECTS.len() && OBJECTS[i].0 == "search_result"
}

fn skey(s: &str) -> Key {
    Key::Str(Arc::from(s))
}

fn dget(d: &Arc<DictCell>, k: &str) -> Option<V> {
    d.lock().get(&skey(k)).map(|(_, v)| v.clone())
}

/// `_convert_to_stripe_object`: lists element by element, every dict a StripeObject of the class its `object`
/// field names (`klass_` of an inner field is not followed: those are plain StripeObjects here)
fn convert(v: &V, req: &Req, params: Option<&[(String, V)]>, cls: Option<u16>) -> R {
    super::stack_guard()?;
    match v {
        V::List(l) => {
            let items = l.lock().clone();
            Ok(V::list(items.iter().map(|x| convert(x, req, None, None)).collect::<R<Vec<_>>>()?))
        }
        V::Dict(d) if tag(d).is_none() => {
            let pairs: Vec<(V, V)> = d.lock().values().cloned().collect();
            let cls = cls.unwrap_or_else(|| match pairs.iter().find(|(k, _)| matches!(k, V::Str(s) if &**s == "object")) {
                Some((_, V::Str(o))) => object_class(o),
                _ => STRIPE_OBJECT,
            });
            let mut out = IndexMap::with_capacity(pairs.len());
            for (k, x) in pairs {
                let x = convert(&x, req, None, None)?;
                dict_put(&mut out, Key::dict_key(&k)?, k, x);
            }
            let cell = Arc::new(Mutex::new(out));
            let list = if is_list_class(cls) {
                Some(Arc::new(ListCtx { params: params.map(|p| p.to_vec()).unwrap_or_default(), req: req.clone() }))
            } else {
                None
            };
            register(&cell, Tag { cls, list });
            Ok(V::Dict(cell))
        }
        o => Ok(o.clone()),
    }
}

// ---------------------------------------------------------------- requestor options

/// A requestor's persistent options: those of the global one read at the call (`_GlobalRequestorOptions`),
/// overridden by the call's (`_new_requestor_with_options`)
#[derive(Clone)]
struct Req {
    api_key: V,
    stripe_version: V,
    stripe_account: V,
    stripe_context: V,
    api_base: V,
    max_network_retries: V,
}

impl Req {
    fn global() -> Req {
        let c = CONF.lock();
        Req {
            api_key: c.api_key.clone(),
            stripe_version: c.api_version.clone().unwrap_or_else(|| V::str(default_api_version())),
            stripe_account: V::None,
            stripe_context: V::None,
            // RequestorOptions keeps base_addresses["api"] only when truthy
            api_base: c.api_base.clone(),
            max_network_retries: c.max_network_retries.clone(),
        }
    }

    fn with(&self, o: &Opts) -> Req {
        let mut r = self.clone();
        for (slot, v) in [(&mut r.api_key, &o.api_key), (&mut r.stripe_version, &o.stripe_version),
                          (&mut r.stripe_account, &o.stripe_account), (&mut r.stripe_context, &o.stripe_context)] {
            if !v.is_none() {
                *slot = v.clone();
            }
        }
        r
    }
}

/// `RequestOptions` given to a call (`extract_options_from_dict`)
#[derive(Clone)]
struct Opts {
    api_key: V,
    stripe_version: V,
    stripe_account: V,
    stripe_context: V,
    max_network_retries: V,
    idempotency_key: V,
    content_type: V,
    headers: V,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { api_key: V::None, stripe_version: V::None, stripe_account: V::None, stripe_context: V::None,
               max_network_retries: V::None, idempotency_key: V::None, content_type: V::None, headers: V::None }
    }
}

fn extract_options(params: Vec<(String, V)>) -> (Opts, Vec<(String, V)>) {
    let mut o = Opts::default();
    let mut rest = Vec::with_capacity(params.len());
    for (k, v) in params {
        match k.as_str() {
            "api_key" => o.api_key = v,
            "stripe_version" => o.stripe_version = v,
            "stripe_account" => o.stripe_account = v,
            "stripe_context" => o.stripe_context = v,
            "max_network_retries" => o.max_network_retries = v,
            "idempotency_key" => o.idempotency_key = v,
            "content_type" => o.content_type = v,
            "headers" => o.headers = v,
            _ => rest.push((k, v)),
        }
    }
    (o, rest)
}

/// `merge_options(requestor, request)`: `or` for the persistent keys
fn or(a: &V, b: &V) -> R {
    Ok(if ops::truthy(a)? { a.clone() } else { b.clone() })
}

// ---------------------------------------------------------------- encoding (_encode._api_encode + urlencode)

/// `urllib.parse.quote_plus` (safe ''): str as UTF-8, bytes as they are; anything else is a TypeError
fn quote_plus(v: &V) -> R<String> {
    let b: &[u8] = match v {
        V::Str(s) => s.as_bytes(),
        V::Bytes(b) => b,
        _ => return Err(Exc::type_error("quote_from_bytes() expected bytes")),
    };
    Ok(quote_bytes(b))
}

fn quote_bytes(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        if c.is_ascii_alphanumeric() || b"_.-~".contains(&c) {
            out.push(c as char);
        } else if c == b' ' {
            out.push('+');
        } else {
            out += &format!("%{c:02X}");
        }
    }
    out
}

/// `hasattr(value, "id")` (14.2+) / `hasattr(value, "stripe_id")` (before): the id a value stands for
async fn id_of(cx: &Cx, v: &V) -> R<Option<V>> {
    match v {
        V::Dict(d) => Ok(match tag(d) {
            Some(_) => dget(d, "id"),
            None => None,
        }),
        V::None | V::Bool(_) | V::Int(_) | V::Float(_) | V::Str(_) | V::Bytes(_) | V::List(_) | V::Tuple(_) | V::Set(_)
        | V::DateTime(_) | V::Date(_) | V::Time(_) | V::Delta(_) | V::Decimal(_) | V::Enum(..) => Ok(None),
        _ if !at_least(14, 2, 0) => Ok(None),
        other => match methods::getattr(cx, other, "id").await {
            Ok(x) => Ok(Some(x)),
            Err(e) if e.isinstance(&ATTRIBUTE_ERROR) => Ok(None),
            Err(e) => Err(e),
        },
    }
}

/// `_encode_datetime`: an aware datetime by its UTC instant, a naive one as local time (`time.mktime`)
async fn encode_datetime(cx: &Cx, v: &V) -> R<V> {
    if let V::DateTime(d) = v {
        if d.tz.is_some() {
            let ts = methods::call_method(cx, v, "timestamp", vec![], vec![]).await?;
            let f = ops::num(&ts).unwrap_or(0.0);
            return Ok(V::Int(f.floor() as i64));
        }
        let ts = methods::call_method(cx, &V::DateTime(super::dt::DateTime { wall: d.wall.with_nanosecond_zero(), tz: None, fold: d.fold }), "timestamp", vec![], vec![]).await?;
        return Ok(V::Int(ops::num(&ts).unwrap_or(0.0) as i64));
    }
    Ok(v.clone())
}

trait NoNanos {
    fn with_nanosecond_zero(&self) -> Self;
}

impl NoNanos for chrono::NaiveDateTime {
    fn with_nanosecond_zero(&self) -> Self {
        use chrono::Timelike;
        self.with_nanosecond(0).unwrap_or(*self)
    }
}

fn encode<'a>(cx: &'a Cx, data: Vec<(String, V)>, out: &'a mut Vec<(String, V)>) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<()>> + Send + 'a>> {
    Box::pin(async move {
        for (key, value) in data {
            if value.is_none() {
                continue;
            }
            if let Some(id) = id_of(cx, &value).await? {
                out.push((key, id));
                continue;
            }
            match &value {
                V::List(_) | V::Tuple(_) => {
                    for (i, sv) in ops::iter(&value)?.into_iter().enumerate() {
                        let k = format!("{key}[{i}]");
                        if let V::Dict(_) = &sv {
                            encode(cx, nested(&k, &sv)?, out).await?;
                        } else {
                            out.push((k, sv));
                        }
                    }
                }
                V::Dict(_) => encode(cx, nested(&key, &value)?, out).await?,
                V::DateTime(_) => out.push((key, encode_datetime(cx, &value).await?)),
                V::Bool(b) => out.push((key, V::str(if *b { "true" } else { "false" }))),
                _ => out.push((key, value)),
            }
        }
        Ok(())
    })
}

/// `_encode_nested_dict(key, data)`: `key[subkey]`
fn nested(key: &str, d: &V) -> R<Vec<(String, V)>> {
    let V::Dict(m) = d else { unreachable!() };
    let pairs: Vec<(V, V)> = m.lock().values().cloned().collect();
    pairs.into_iter().map(|(k, v)| Ok((format!("{key}[{}]", ops::str_(&k)?), v))).collect()
}

/// `urlencode(list(_api_encode(params)))` with `[` and `]` put back
async fn encode_params(cx: &Cx, params: &[(String, V)]) -> R<String> {
    let mut pairs = Vec::new();
    encode(cx, params.to_vec(), &mut pairs).await?;
    let mut s = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push('&');
        }
        s += &quote_bytes(k.as_bytes());
        s.push('=');
        s += &match v {
            V::Bytes(b) => quote_bytes(b),
            other => quote_bytes(ops::str_(other)?.as_bytes()),
        };
    }
    Ok(s.replace("%5B", "[").replace("%5D", "]"))
}

// ---------------------------------------------------------------- logging (stripe._util.log_info / log_debug)

static LOG_DEBUG: Loc = Loc { file: "stripe/_util.py", line: 120, func: "log_debug", text: "" };
static LOG_INFO: Loc = Loc { file: "stripe/_util.py", line: 127, func: "log_info", text: "" };

/// `logfmt(dict(message=message, **params))`: sorted keys, a value with whitespace as its repr
fn logfmt(props: &mut Vec<(&str, String)>) -> String {
    props.sort_by(|a, b| a.0.cmp(b.0));
    props
        .iter()
        .map(|(k, v)| {
            let v = if v.chars().any(char::is_whitespace) { ops::str_repr(v) } else { v.clone() };
            format!("{k}={v}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn console_level() -> Option<&'static str> {
    let l = CONF.lock().log.clone();
    for cand in [match &l { V::Str(s) => Some(s.to_string()), _ => None }, std::env::var("STRIPE_LOG").ok()] {
        match cand.as_deref() {
            Some("debug") => return Some("debug"),
            Some("info") => return Some("info"),
            _ => {}
        }
    }
    None
}

async fn log(cx: &Cx, debug: bool, message: &str, mut params: Vec<(&str, String)>) {
    params.push(("message", message.to_string()));
    let msg = logfmt(&mut params);
    match console_level() {
        Some("debug") => eprintln!("{msg}"),
        Some("info") if !debug => eprintln!("{msg}"),
        _ => {}
    }
    let (level, loc) = if debug { (10, &LOG_DEBUG) } else { (20, &LOG_INFO) };
    super::logging::emit(cx, "stripe", level, &msg, vec![], vec![], None, loc).await;
}

fn s(v: &V) -> String {
    ops::str_(v).unwrap_or_default()
}

// ---------------------------------------------------------------- HTTP (RequestsClient + HTTPClient retries)

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            // RequestsClient(timeout=80): connect and read timeouts
            .connect_timeout(std::time::Duration::from_secs(80))
            .read_timeout(std::time::Duration::from_secs(80))
            .redirect(reqwest::redirect::Policy::limited(30))
            // requests sends the header names as written (`Idempotency-Key`)
            .http1_title_case_headers()
            .build()
            .expect("reqwest client")
    })
}

/// thread-local in stripe-python (the last request of the thread): the process's last request here
static LAST_METRICS: Mutex<Option<(String, i64)>> = Mutex::new(None);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

struct Resp {
    code: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// `APIConnectionError(msg, should_retry=...)` of RequestsClient._handle_request_error
fn connection_error(e: &reqwest::Error, url: &str) -> Exc {
    let parsed = url::Url::parse(url).ok();
    let (host, port, https) = match &parsed {
        Some(u) => (u.host_str().unwrap_or("").to_string(), u.port_or_known_default().unwrap_or(0), u.scheme() == "https"),
        None => (String::new(), 0, false),
    };
    let path = parsed.as_ref().map(|u| match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    }).unwrap_or_default();
    let pool = if https { "HTTPSConnectionPool" } else { "HTTPConnectionPool" };
    let conn = if https { "HTTPSConnection" } else { "HTTPConnection" };
    let os = os_error(e);
    let (err, retry) = if e.is_timeout() && !e.is_connect() {
        (format!("ReadTimeout: {pool}(host='{host}', port={port}): Read timed out. (read timeout=80)"), true)
    } else if e.is_connect() || e.is_timeout() || os.is_some() {
        let why = match os {
            Some((n, m)) => format!("[Errno {n}] {m}"),
            None => e.to_string(),
        };
        (format!("ConnectionError: {pool}(host='{host}', port={port}): Max retries exceeded with url: {path} (Caused by \
                  NewConnectionError(\"{conn}(host='{host}', port={port}): Failed to establish a new connection: {why}\"))"), true)
    } else {
        (format!("ConnectionError: {e}"), true)
    };
    let msg = "Unexpected error communicating with Stripe.  If this problem persists,\nlet us know at support@stripe.com.";
    api_connection_error(format!("{msg}\n\n(Network error: {err})"), retry)
}

fn os_error(e: &reqwest::Error) -> Option<(i32, String)> {
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            if let Some(n) = io.raw_os_error() {
                let m = std::io::Error::from_raw_os_error(n).to_string();
                let m = m.rsplit_once(" (os error").map(|(a, _)| a.to_string()).unwrap_or(m);
                return Some((n, m));
            }
        }
        src = s.source();
    }
    None
}

/// requests' errors before any connection: MissingSchema / InvalidSchema (RequestException: not retried)
fn url_error(url: &str) -> Option<Exc> {
    let msg = "Unexpected error communicating with Stripe.  If this problem persists,\nlet us know at support@stripe.com.";
    let err = match url.split_once("://") {
        None => format!("MissingSchema: Invalid URL '{url}': No scheme supplied. Perhaps you meant https://{url}?"),
        Some((sch, _)) if !sch.eq_ignore_ascii_case("http") && !sch.eq_ignore_ascii_case("https") => {
            format!("InvalidSchema: No connection adapters were found for '{url}'")
        }
        _ => return None,
    };
    Some(api_connection_error(format!("{msg}\n\n(Network error: {err})"), false))
}

async fn send_once(method: &str, url: &str, headers: &[(String, String)], body: Option<&str>) -> Result<Resp, Exc> {
    if let Some(e) = url_error(url) {
        return Err(e);
    }
    let m = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).expect("verb");
    // urllib3 percent-encodes the brackets of a query (`expand[0]=` sent as `expand%5B0%5D=`)
    let wire = match url.split_once('?') {
        Some((head, q)) => format!("{head}?{}", q.replace('[', "%5B").replace(']', "%5D")),
        None => url.to_string(),
    };
    let mut rb = http().request(m, &wire);
    rb = rb.header("Accept", "*/*").header("Connection", "keep-alive");
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if let Some(b) = body {
        rb = rb.body(b.to_string());
    }
    let r = rb.send().await.map_err(|e| connection_error(&e, &wire))?;
    let code = r.status().as_u16();
    let hs = r.headers().iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect();
    let body = r.bytes().await.map_err(|e| connection_error(&e, &wire))?.to_vec();
    Ok(Resp { code, headers: hs, body })
}

fn retry_after(r: &Resp) -> Option<i64> {
    r.header("retry-after").and_then(|v| v.trim().replace('_', "").parse::<i64>().ok())
}

/// `HTTPClient._request_with_retries_internal`
async fn send(cx: &Cx, method: &str, url: &str, mut headers: Vec<(String, String)>, body: Option<String>, max_retries: i64) -> R<Resp> {
    let telemetry = ops::truthy(&CONF.lock().enable_telemetry.clone())?;
    if telemetry {
        if let Some((rid, ms)) = LAST_METRICS.lock().clone() {
            let p = V::dict_from(vec![(V::str("last_request_metrics"), V::dict_from(vec![
                (V::str("request_id"), V::str(rid)), (V::str("request_duration_ms"), V::Int(ms))])?)])?;
            headers.push(("X-Stripe-Client-Telemetry".into(), json(&p)?));
        }
    }
    let mut n: i64 = 0;
    loop {
        let start = now_ms();
        let r = send_once(method, url, &headers, body.as_deref()).await;
        let retry = n < max_retries && match &r {
            Err(e) => matches!(e.0.attrs.lock().get("should_retry"), Some(V::Bool(true))),
            Ok(resp) => match resp.header("stripe-should-retry") {
                Some("false") => false,
                Some("true") => true,
                _ => resp.code == 409 || resp.code >= 500,
            },
        };
        if !retry {
            let resp = r?;
            if let Some(rid) = resp.header("request-id") {
                if telemetry {
                    *LAST_METRICS.lock() = Some((rid.to_string(), now_ms() - start));
                }
            }
            return Ok(resp);
        }
        if let Err(e) = &r {
            let um = e.0.attrs.lock().get("user_message").map(s).unwrap_or_default();
            log(cx, false, &format!("Encountered a retryable error {um}"), vec![]).await;
        }
        n += 1;
        let mut sleep = (0.5 * 2f64.powi((n - 1) as i32)).min(5.0);
        sleep *= 0.5 * (1.0 + rand::random::<f64>());
        sleep = sleep.max(0.5);
        let ra = r.as_ref().ok().and_then(retry_after).unwrap_or(0);
        if ra <= 60 {
            sleep = sleep.max(ra as f64);
        }
        log(cx, false, &format!("Initiating retry {n} for request {method} {url} after sleeping {sleep:.2} seconds."), vec![]).await;
        tokio::time::sleep(std::time::Duration::from_secs_f64(sleep)).await;
    }
}

fn json(v: &V) -> R<String> {
    super::pyd::to_json(v, &super::pyd::JsonStyle { ensure_ascii: true, item_sep: ", ", key_sep: ": ", nan_null: false }, false)
}

// ---------------------------------------------------------------- the request (_APIRequestor.request)

/// `platform.uname()` joined (system, node, release, version, machine, processor)
fn uname() -> String {
    // SAFETY: utsname is plain data filled by uname(2)
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) != 0 {
            return "(disabled)".into();
        }
        let f = |a: &[libc::c_char]| std::ffi::CStr::from_ptr(a.as_ptr()).to_string_lossy().into_owned();
        let machine = f(&u.machine);
        let processor = if cfg!(target_os = "macos") {
            if machine == "arm64" { "arm".to_string() } else { "i386".to_string() }
        } else {
            String::new()
        };
        format!("{} {} {} {} {} {}", f(&u.sysname), f(&u.nodename), f(&u.release), f(&u.version), machine, processor)
    }
}

fn platform_string() -> String {
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) != 0 {
            return "(disabled)".into();
        }
        let f = |a: &[libc::c_char]| std::ffi::CStr::from_ptr(a.as_ptr()).to_string_lossy().into_owned();
        let machine = f(&u.machine);
        if cfg!(target_os = "macos") {
            let ver = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
                .ok()
                .and_then(|p| p.split("<key>ProductVersion</key>").nth(1).and_then(|r| r.split("<string>").nth(1)).and_then(|r| r.split("</string>").next()).map(str::to_string))
                .unwrap_or_default();
            let proc_ = if machine == "arm64" { "arm" } else { "i386" };
            format!("macOS-{ver}-{machine}-{proc_}-64bit-Mach-O")
        } else {
            format!("{}-{}-{}-with-glibc", f(&u.sysname), f(&u.release), machine)
        }
    }
}

const AI_AGENTS: [(&str, &str); 10] = [
    ("ANTIGRAVITY_CLI_ALIAS", "antigravity"), ("CLAUDECODE", "claude_code"), ("CLINE_ACTIVE", "cline"),
    ("CODEX_SANDBOX", "codex_cli"), ("CODEX_THREAD_ID", "codex_cli"), ("CODEX_SANDBOX_NETWORK_DISABLED", "codex_cli"),
    ("CODEX_CI", "codex_cli"), ("CURSOR_AGENT", "cursor"), ("GEMINI_CLI", "gemini_cli"), ("OPENCODE", "open_code"),
];

fn ai_agent() -> &'static str {
    if !at_least(14, 4, 1) {
        return "";
    }
    AI_AGENTS.iter().find(|(e, _)| std::env::var(e).map(|v| !v.is_empty()).unwrap_or(false)).map(|(_, a)| *a).unwrap_or("")
}

/// `request_headers(method, "V1", options)`
fn request_headers(method: &str, o: &Merged) -> R<Vec<(String, String)>> {
    let mut ua = format!("Stripe/v1 PythonBindings/{}", version());
    let agent = ai_agent();
    if !agent.is_empty() {
        ua += &format!(" AIAgent/{agent}");
    }
    let (maj, min) = super::python();
    let mut client = vec![
        (V::str("bindings_version"), V::str(version())),
        (V::str("lang"), V::str("python")),
        (V::str("publisher"), V::str("stripe")),
        (V::str("httplib"), V::str("requests")),
        (V::str("lang_version"), V::str(format!("{maj}.{min}.{}", super::python_patch()))),
        (V::str("platform"), V::str(platform_string())),
        (V::str("uname"), V::str(uname())),
    ];
    if !agent.is_empty() {
        client.push((V::str("ai_agent"), V::str(agent)));
    }
    let mut h: Vec<(String, String)> = vec![
        ("X-Stripe-Client-User-Agent".into(), json(&V::dict_from(client)?)?),
        ("User-Agent".into(), ua),
        ("Authorization".into(), format!("Bearer {}", s(&o.api_key))),
    ];
    if ops::truthy(&o.stripe_account)? {
        h.push(("Stripe-Account".into(), s(&o.stripe_account)));
    }
    if ops::truthy(&o.stripe_context)? && !s(&o.stripe_context).is_empty() {
        h.push(("Stripe-Context".into(), s(&o.stripe_context)));
    }
    if ops::truthy(&o.idempotency_key)? {
        h.push(("Idempotency-Key".into(), s(&o.idempotency_key)));
    }
    if method == "post" {
        if !h.iter().any(|(k, _)| k == "Idempotency-Key") {
            h.push(("Idempotency-Key".into(), uuid4()));
        }
        h.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
    }
    if ops::truthy(&o.stripe_version)? {
        h.push(("Stripe-Version".into(), s(&o.stripe_version)));
    }
    Ok(h)
}

fn uuid4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// the options of one request (`merge_options(self._options, options)`)
struct Merged {
    api_key: V,
    stripe_account: V,
    stripe_context: V,
    stripe_version: V,
    max_network_retries: V,
    idempotency_key: V,
    content_type: V,
    headers: V,
}

/// `_APIRequestor.request(method, url, params, options)` on `req`: the converted response
async fn request(cx: &Cx, req: &Req, method: &str, url: &str, params: Vec<(String, V)>, opts: &Opts, cls: Option<u16>) -> R {
    let req = req.with(opts);
    let o = Merged {
        api_key: or(&opts.api_key, &req.api_key)?,
        stripe_account: or(&opts.stripe_account, &req.stripe_account)?,
        stripe_context: or(&opts.stripe_context, &req.stripe_context)?,
        stripe_version: or(&opts.stripe_version, &req.stripe_version)?,
        max_network_retries: if opts.max_network_retries.is_none() { req.max_network_retries.clone() } else { opts.max_network_retries.clone() },
        idempotency_key: opts.idempotency_key.clone(),
        content_type: opts.content_type.clone(),
        headers: opts.headers.clone(),
    };
    if o.api_key.is_none() {
        return Err(stripe_error(&STRIPE_AUTHENTICATION_ERROR, V::str(
            "No API key provided. (HINT: set your API key using \"stripe.api_key = <API-KEY>\"). You can generate API keys \
             from the Stripe web interface.  See https://stripe.com/api for details, or email support@stripe.com if you have \
             any questions."), ErrParts::default()));
    }
    if matches!(&o.content_type, V::Str(c) if &**c == "multipart/form-data") {
        return Err(Exc::runtime("py2axum: stripe multipart requests (content_type=\"multipart/form-data\") are not supported"));
    }
    let base = if ops::truthy(&req.api_base)? { s(&req.api_base) } else { "None".to_string() };
    let mut abs_url = format!("{base}{url}");
    let encoded = encode_params(cx, &params).await?;
    let mut headers = request_headers(method, &o)?;
    let body = if method == "post" {
        Some(encoded.clone())
    } else {
        if !params.is_empty() && !encoded.is_empty() {
            abs_url = with_query(&abs_url, &encoded);
        }
        None
    };
    if let V::Dict(d) = &o.headers {
        let extra: Vec<(V, V)> = d.lock().values().cloned().collect();
        for (k, v) in extra {
            let (k, v) = (s(&k), s(&v));
            match headers.iter_mut().find(|(n, _)| *n == k) {
                Some(slot) => slot.1 = v,
                None => headers.push((k, v)),
            }
        }
    }
    let max_retries = match &o.max_network_retries {
        V::Int(n) => *n,
        V::Bool(b) => *b as i64,
        _ => 0,
    };
    log(cx, false, "Request to Stripe api", vec![("method", method.to_string()), ("url", abs_url.clone())]).await;
    log(cx, true, "Post details", vec![("post_data", encoded.clone()), ("api_version", s(&o.stripe_version))]).await;
    let resp = send(cx, method, &abs_url, headers, body, max_retries).await?;
    log(cx, false, "Stripe API response", vec![("path", abs_url.clone()), ("response_code", resp.code.to_string())]).await;
    // logfmt decodes the body before the level is looked at: a body that is not UTF-8 raises here
    let body_text = match std::str::from_utf8(&resp.body) {
        Ok(t) => t.to_string(),
        Err(_) => {
            methods::call_method(cx, &V::Bytes(Arc::from(&resp.body[..])), "decode", vec![V::str("utf-8")], vec![]).await?;
            String::new()
        }
    };
    log(cx, true, "API response body", vec![("body", body_text)]).await;
    if let Some(rid) = resp.header("request-id") {
        let env = match &CONF.lock().api_key {
            V::Str(k) if k.starts_with("sk_live_") => "live",
            _ => "test",
        };
        log(cx, true, "Dashboard link for request", vec![("link", format!("https://dashboard.stripe.com/{env}/logs/{rid}"))]).await;
    }
    let data = interpret(cx, &resp).await?;
    convert(&data, &req, Some(&params), cls)
}

fn with_query(url: &str, q: &str) -> String {
    let (head, frag) = match url.split_once('#') {
        Some((h, f)) => (h, Some(f)),
        None => (url, None),
    };
    let head = head.split_once('?').map(|(h, _)| h).unwrap_or(head);
    match frag {
        Some(f) => format!("{head}?{q}#{f}"),
        None => format!("{head}?{q}"),
    }
}

/// `_interpret_response`: the JSON body, or the error the status and body make
async fn interpret(cx: &Cx, r: &Resp) -> R {
    let text = match std::str::from_utf8(&r.body) {
        Ok(t) => t.to_string(),
        Err(_) => {
            return Err(stripe_error(&STRIPE_API_ERROR, V::str(format!(
                "Invalid response body from API: {} (HTTP response code was {})", ops::bytes_repr(&r.body), r.code)),
                ErrParts { http_body: V::Bytes(Arc::from(&r.body[..])), http_status: V::Int(r.code as i64), ..Default::default() }));
        }
    };
    let data = match super::pyjson::loads(&text) {
        Ok(d) => d,
        Err(_) => {
            // `APIError(msg, rbody, rcode, rheaders)`: the headers land in json_body
            return Err(stripe_error(&STRIPE_API_ERROR, V::str(format!(
                "Invalid response body from API: {text} (HTTP response code was {})", r.code)),
                ErrParts { http_body: V::str(&text), http_status: V::Int(r.code as i64), json_body: headers_value(r), ..Default::default() }));
        }
    };
    if !(200..300).contains(&r.code) {
        return Err(error_response(cx, &text, r, data).await);
    }
    Ok(data)
}

fn headers_value(r: &Resp) -> V {
    V::dict_from(r.headers.iter().map(|(k, v)| (V::str(k), V::str(v))).collect()).unwrap_or_else(|_| V::empty_dict())
}

/// `handle_error_response`
async fn error_response(cx: &Cx, rbody: &str, r: &Resp, resp: V) -> Exc {
    let parts = |code: V| ErrParts {
        http_body: V::str(rbody),
        http_status: V::Int(r.code as i64),
        json_body: resp.clone(),
        headers: headers_value(r),
        code,
        ..Default::default()
    };
    let error_data = match &resp {
        V::Dict(d) => dget(d, "error"),
        _ => None,
    };
    let Some(error_data) = error_data else {
        // `APIError(msg, rbody, rcode, resp)`: no headers, so no request id
        return stripe_error(&STRIPE_API_ERROR, V::str(format!(
            "Invalid response object from API: {} (HTTP response code was {})", ops::str_repr(rbody), r.code)),
            ErrParts { headers: V::None, ..parts(V::None) });
    };
    if let V::Str(code) = &error_data {
        // OAuth errors: `error` is a string
        let desc = match &resp {
            V::Dict(d) => dget(d, "error_description").unwrap_or_else(|| error_data.clone()),
            _ => error_data.clone(),
        };
        log(cx, false, "Stripe OAuth error received", vec![("error_code", code.to_string()), ("error_description", s(&desc))]).await;
        let class = match &**code {
            "invalid_client" => Some(&STRIPE_OAUTH_INVALID_CLIENT),
            "invalid_grant" => Some(&STRIPE_OAUTH_INVALID_GRANT),
            "invalid_request" => Some(&STRIPE_OAUTH_INVALID_REQUEST),
            "invalid_scope" => Some(&STRIPE_OAUTH_INVALID_SCOPE),
            "unsupported_grant_type" => Some(&STRIPE_OAUTH_UNSUPPORTED_GRANT_TYPE),
            "unsupported_response_type" => Some(&STRIPE_OAUTH_UNSUPPORTED_RESPONSE_TYPE),
            _ => None,
        };
        if let Some(c) = class {
            let mut p = parts(error_data.clone());
            p.oauth = true;
            return stripe_error(c, desc, p);
        }
    }
    let V::Dict(ed) = &error_data else {
        return Exc::attr_error(format!("'{}' object has no attribute 'get'", error_data.type_name()));
    };
    let get = |k: &str| dget(ed, k).unwrap_or(V::None);
    log(cx, false, "Stripe v1 API error received", vec![("error_code", s(&get("code"))), ("error_type", s(&get("type"))),
        ("error_message", s(&get("message"))), ("error_param", s(&get("param")))]).await;
    let message = get("message");
    let is = |t: &str| matches!(get("type"), V::Str(x) if &*x == t);
    if r.code == 429 || (r.code == 400 && matches!(get("code"), V::Str(c) if &*c == "rate_limit")) {
        return stripe_error(&STRIPE_RATE_LIMIT_ERROR, message, parts(V::None));
    }
    match r.code {
        400 | 404 if is("idempotency_error") => stripe_error(&STRIPE_IDEMPOTENCY_ERROR, message, parts(V::None)),
        400 | 404 => stripe_error(&STRIPE_INVALID_REQUEST_ERROR, message, ErrParts { param: Some(get("param")), ..parts(get("code")) }),
        401 => stripe_error(&STRIPE_AUTHENTICATION_ERROR, message, parts(V::None)),
        402 => stripe_error(&STRIPE_CARD_ERROR, message, ErrParts { param: Some(get("param")), ..parts(get("code")) }),
        403 => stripe_error(&STRIPE_PERMISSION_ERROR, message, parts(V::None)),
        _ => stripe_error(&STRIPE_API_ERROR, message, parts(V::None)),
    }
}

// ---------------------------------------------------------------- errors (stripe._error)

struct ErrParts {
    http_body: V,
    http_status: V,
    json_body: V,
    headers: V,
    code: V,
    param: Option<V>,
    oauth: bool,
}

impl Default for ErrParts {
    fn default() -> ErrParts {
        ErrParts { http_body: V::None, http_status: V::None, json_body: V::None, headers: V::None, code: V::None, param: None,
                   oauth: false }
    }
}

fn api_connection_error(msg: String, should_retry: bool) -> Exc {
    let e = stripe_error(&STRIPE_API_CONNECTION_ERROR, V::str(msg), ErrParts::default());
    e.0.attrs.lock().insert("should_retry".into(), V::Bool(should_retry));
    e
}

/// `StripeError.__init__(message, http_body, http_status, json_body, headers, code)` and the subclass's own
fn stripe_error(class: &'static Class, message: V, p: ErrParts) -> Exc {
    let e = Exc::new(class, vec![message.clone()]);
    let body = match &p.http_body {
        V::Bytes(b) if !b.is_empty() => match std::str::from_utf8(b) {
            Ok(t) => V::str(t),
            Err(_) => V::str("<Could not decode body as utf-8. Please report to support@stripe.com>"),
        },
        V::Str(t) if !t.is_empty() => p.http_body.clone(),
        _ => V::None,
    };
    let headers = if ops::truthy(&p.headers).unwrap_or(false) { p.headers.clone() } else { V::empty_dict() };
    let request_id = match &headers {
        V::Dict(d) => dget(d, "request-id").unwrap_or(V::None),
        _ => V::None,
    };
    // `_construct_error_object`: an ErrorObject from json_body["error"] (an OAuthErrorObject from json_body)
    let req = Req::global();
    // ErrorObject / OAuthErrorObject._refresh_from: their known keys first (None), then the response's
    let with_defaults = |values: &V, keys: &[&str], cls: u16| -> V {
        let V::Dict(d) = values else { return V::None };
        let mut pairs: Vec<(V, V)> = keys.iter().map(|k| (V::str(k), V::None)).collect();
        for (k, v) in d.lock().values().cloned() {
            match pairs.iter_mut().find(|(pk, _)| ops::eq_bool(pk, &k)) {
                Some(slot) => slot.1 = v,
                None => pairs.push((k, v)),
            }
        }
        V::dict_from(pairs).and_then(|m| convert(&m, &req, None, Some(cls))).unwrap_or(V::None)
    };
    let error = match (&p.json_body, p.oauth) {
        (V::Dict(_), true) => with_defaults(&p.json_body, &["error", "error_description"], OAUTH_ERROR_OBJECT),
        (V::Dict(d), false) => match dget(d, "error") {
            Some(err @ V::Dict(_)) => with_defaults(&err, &["charge", "code", "decline_code", "doc_url", "message", "param",
                "payment_intent", "payment_method", "setup_intent", "source", "type"], ERROR_OBJECT),
            _ => V::None,
        },
        _ => V::None,
    };
    {
        let mut a = e.0.attrs.lock();
        a.insert("_message".into(), message.clone());
        a.insert("user_message".into(), message);
        a.insert("http_body".into(), body);
        a.insert("http_status".into(), p.http_status);
        a.insert("json_body".into(), p.json_body);
        a.insert("headers".into(), headers);
        a.insert("code".into(), p.code);
        a.insert("request_id".into(), request_id);
        a.insert("error".into(), error);
        if let Some(param) = p.param {
            a.insert("param".into(), param);
        }
    }
    e
}

fn is_stripe(e: &Exc) -> bool {
    let mut stack = vec![e.0.class];
    while let Some(c) = stack.pop() {
        if std::ptr::eq(c, &STRIPE_ERROR) {
            return true;
        }
        stack.extend(c.bases.iter().copied());
    }
    false
}

/// `StripeError.__str__`
pub fn exc_str(e: &Exc) -> Option<String> {
    if !is_stripe(e) {
        return None;
    }
    let a = e.0.attrs.lock();
    let msg = match a.get("_message") {
        Some(m) if ops::truthy(m).unwrap_or(false) => s(m),
        _ => "<empty message>".into(),
    };
    Some(match a.get("request_id") {
        Some(r) if !r.is_none() => format!("Request {}: {msg}", s(r)),
        _ => msg,
    })
}

/// `StripeError.__repr__` / `StripeErrorWithParamCode.__repr__`
pub fn exc_repr(e: &Exc) -> Option<String> {
    if !is_stripe(e) {
        return None;
    }
    let a = e.0.attrs.lock();
    let r = |k: &str| a.get(k).map(|v| ops::repr(v).unwrap_or_default()).unwrap_or_else(|| "None".into());
    let with_param = std::ptr::eq(e.0.class, &STRIPE_CARD_ERROR) || std::ptr::eq(e.0.class, &STRIPE_INVALID_REQUEST_ERROR);
    Some(if with_param {
        format!("{}(message={}, param={}, code={}, http_status={}, request_id={})", e.0.class.name, r("_message"), r("param"),
                r("code"), r("http_status"), r("request_id"))
    } else {
        format!("{}(message={}, http_status={}, request_id={})", e.0.class.name, r("_message"), r("http_status"), r("request_id"))
    })
}

// ---------------------------------------------------------------- resource classmethods

/// `stripe.<Class>.<method>(*ids, **params)`: `arms` maps the number of positional ids to the METHODS row
pub async fn call(cx: &Cx, arms: &[(usize, usize)], mut args: Vec<V>, mut kwargs: Vec<(String, V)>) -> R {
    // ids given by keyword (`retrieve(id="cus_1")`) bind like positional ones
    let names = METHODS[arms.iter().max_by_key(|(n, _)| *n).expect("an arm").1].5;
    for (i, n) in names.iter().enumerate() {
        if let Some(j) = kwargs.iter().position(|(k, _)| k == n) {
            let (cls, _, name, ..) = METHODS[arms[0].1];
            if i < args.len() {
                return Err(Exc::type_error(format!("{cls}.{name}() got multiple values for argument '{n}'")));
            }
            if i > args.len() {
                break;
            }
            args.push(kwargs.remove(j).1);
        }
    }
    let Some(&(_, row)) = arms.iter().find(|(n, _)| *n == args.len()) else {
        let (cls, _, name, ..) = METHODS[arms[0].1];
        let most = arms.iter().map(|(n, _)| *n).max().unwrap_or(0);
        let fewest = arms.iter().map(|(n, _)| *n).min().unwrap_or(0);
        let names = METHODS[arms.iter().max_by_key(|(n, _)| *n).unwrap().1].5;
        return Err(Exc::type_error(if args.len() < fewest {
            let missing = &names[args.len()..fewest];
            format!("{cls}.{name}() missing {} required positional argument{}: {}", missing.len(),
                    if missing.len() > 1 { "s" } else { "" },
                    missing.iter().map(|m| format!("'{m}'")).collect::<Vec<_>>().join(" and "))
        } else {
            format!("{cls}.{name}() takes {} positional argument{} but {} were given", most + 1, if most == 0 { "" } else { "s" }, args.len() + 1)
        }));
    };
    let (cls, object_name, _name, verb, tpl, _names, kind, quoted) = METHODS[row];
    let cls_tag = object_class(object_name);
    let mut url = tpl.to_string();
    if kind == "retrieve" {
        // `cls(id, **params)` then `refresh()`: the id must be a non-empty str (instance_url)
        let mut params = kwargs;
        params.retain(|(k, _)| k != "last_response");
        if let Some(id) = args.first() {
            let id = if ops::truthy(id)? { id.clone() } else { V::None };
            if object_name == "account" {
                url = if id.is_none() { "/v1/account".into() } else { tpl.replace("{0}", &quote_plus(&id)?) };
            } else {
                if !matches!(id, V::Str(_)) {
                    return Err(stripe_error(&STRIPE_INVALID_REQUEST_ERROR, V::str(format!(
                        "Could not determine which URL to request: {cls} instance has invalid ID: {}, <class '{}'>. ID should be of \
                         type `str` (or `unicode`)", ops::repr(&id)?, id.type_name())), ErrParts { param: Some(V::str("id")), ..Default::default() }));
                }
                url = tpl.replace("{0}", &quote_plus(&id)?);
            }
        }
        let (opts, params) = extract_options(params);
        return request(cx, &Req::global(), verb, &url, params, &opts, Some(cls_tag)).await;
    }
    for (i, id) in args.iter().enumerate() {
        let part = if quoted { quote_plus(id)? } else { ops::str_(id)? };
        url = url.replace(&format!("{{{i}}}"), &part);
    }
    if object_name == "account" && args.first().map(|a| !ops::truthy(a).unwrap_or(true)).unwrap_or(false) {
        // Account._build_instance_url: no id is the account of the key
        if let Some(&(_, r0)) = arms.iter().find(|(n, _)| *n == 0) {
            url = METHODS[r0].4.to_string();
        }
    }
    let (opts, params) = extract_options(kwargs);
    let out = request(cx, &Req::global(), verb, &url, params, &opts, None).await?;
    let got = class_of(&out);
    let want = match kind {
        "list" => Some(("ListObject", "list object")),
        "search" => Some(("SearchResultObject", "search result")),
        _ => None,
    };
    if let Some((cname, what)) = want {
        let ok = got.map(|c| class_name(c) == cname).unwrap_or(false);
        if !ok {
            let tn = got.map(class_name).unwrap_or_else(|| out.type_name());
            return Err(Exc::type_error(format!("Expected {what} from API, got {tn}")));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- list objects

/// `_get_filters_for_next_page` / `_previous_page` and the list request on the list's own requestor
async fn page(cx: &Cx, d: &Arc<DictCell>, ctx: &ListCtx, next: bool) -> R {
    let has_more = dget(d, "has_more").ok_or_else(|| Exc::attr_error("has_more"))?;
    if !ops::truthy(&has_more)? {
        // `_empty_list()`: {"data": []} on the global requestor
        let empty = V::dict_from(vec![(V::str("data"), V::list(vec![]))])?;
        return convert_untyped(&empty, &Req::global(), STRIPE_LIST());
    }
    let data = dget(d, "data").ok_or_else(|| Exc::attr_error("data"))?;
    let items = ops::iter(&data)?;
    let item = if next { items.last() } else { items.first() }.cloned().ok_or_else(|| Exc::new(&INDEX_ERROR, vec![V::str("list index out of range")]))?;
    let id = attr(cx, &item, "id").await?;
    if !ops::truthy(&id)? {
        return Err(Exc::value_error("Unexpected: element in .data of list object had no id"));
    }
    let mut params: Vec<(String, V)> = ctx.params.clone();
    let key = if next { "starting_after" } else { "ending_before" };
    match params.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = id,
        None => params.push((key.into(), id)),
    }
    let url = match dget(d, "url") {
        Some(V::Str(u)) => u.to_string(),
        _ => return Err(Exc::value_error("Cannot call .list on a list object without a string \"url\" property")),
    };
    let (opts, params) = extract_options(params);
    request(cx, &ctx.req, "get", &url, params, &opts, None).await
}

#[allow(non_snake_case)]
fn STRIPE_LIST() -> u16 {
    object_class("list")
}

fn convert_untyped(v: &V, req: &Req, cls: u16) -> R {
    convert(v, req, None, Some(cls))
}

/// `auto_paging_iter()`: every item of every page (fetched before the first item is given)
async fn auto_paging(cx: &Cx, d: &Arc<DictCell>, t: &Tag) -> R {
    if is_search_class(t.cls) {
        return Err(Exc::runtime("py2axum: auto_paging_iter() of a stripe search result is not supported"));
    }
    let ctx = t.list.clone().ok_or_else(|| Exc::runtime("py2axum: auto_paging_iter() of a list object not returned by a request"))?;
    let backwards = ctx.params.iter().any(|(k, v)| k == "ending_before" && !v.is_none())
        && !ctx.params.iter().any(|(k, v)| k == "starting_after" && !v.is_none());
    let mut out = Vec::new();
    let mut cur = V::Dict(d.clone());
    loop {
        let V::Dict(cd) = &cur else { break };
        let items = list_items(cd);
        if backwards {
            out.extend(items.into_iter().rev());
        } else {
            out.extend(items);
        }
        let ct = tag(cd).and_then(|t| t.list).unwrap_or_else(|| ctx.clone());
        let next = page(cx, cd, &ct, !backwards).await?;
        let empty = match &next {
            V::Dict(nd) => !ops::truthy(&dget(nd, "data").ok_or_else(|| Exc::attr_error("data"))?)?,
            _ => true,
        };
        if empty {
            break;
        }
        cur = next;
    }
    Ok(V::native(Native::Iter(Mutex::new(out.into()))))
}

fn list_items(d: &Arc<DictCell>) -> Vec<V> {
    match dget(d, "data") {
        Some(v) => ops::iter(&v).unwrap_or_default(),
        None => vec![],
    }
}

/// a list object's `__iter__`, `__len__` (and so its truth): its `data`
pub fn list_data(d: &Arc<DictCell>) -> Option<V> {
    let t = tag(d)?;
    if !is_list_class(t.cls) {
        return None;
    }
    Some(dget(d, "data").unwrap_or_else(|| V::list(vec![])))
}

/// a list object's `__getitem__` of a non-str key
pub fn list_getitem(d: &Arc<DictCell>, k: &V) -> Option<Exc> {
    if matches!(k, V::Str(_)) {
        return None;
    }
    let t = tag(d)?;
    if !is_list_class(t.cls) {
        return None;
    }
    let r = ops::repr(k).unwrap_or_default();
    Some(Exc::new(&KEY_ERROR, vec![V::str(if is_search_class(t.cls) {
        format!("You tried to access the {r} index, but SearchResultObject types only support string keys. (HINT: Search calls \
                 return an object with  a 'data' (which is the data array). You likely want to call .data[{r}])")
    } else {
        format!("You tried to access the {r} index, but ListObject types only support string keys. (HINT: List calls return an \
                 object with a 'data' (which is the data array). You likely want to call .data[{r}])")
    })]))
}

/// `StripeObject.__getitem__` of a missing key: the Invoice hint (14.0.1+)
pub fn missing_key(d: &Arc<DictCell>, k: &V) -> Option<Exc> {
    let t = tag(d)?;
    if at_least(14, 0, 1) && class_name(t.cls) == "Invoice" && (t.cls as usize) < OBJECTS.len() && OBJECTS[t.cls as usize].0 == "invoice"
        && matches!(k, V::Str(s) if &**s == "payment_intent") {
        return Some(Exc::new(&KEY_ERROR, vec![V::str(
            "The 'payment_intent' attribute is no longer available on Invoice objects. See the docs for more details: \
             https://docs.stripe.com/changelog/basil/2025-03-31/add-support-for-multiple-partial-payments-on-invoices#why-is-this-a-breaking-change")]));
    }
    None
}

// ---------------------------------------------------------------- attributes of a StripeObject

const DICT_ATTRS: [&str; 11] = ["clear", "copy", "fromkeys", "get", "items", "keys", "pop", "popitem", "setdefault", "update", "values"];

fn class_attrs(cls: u16) -> &'static [&'static str] {
    CLASS_ATTRS[cls as usize]
}

async fn attr(cx: &Cx, v: &V, name: &str) -> R {
    methods::getattr(cx, v, name).await
}

/// `obj.name` on a StripeObject: a key (`__getattr__`), unless its class has the name. None when `v` is not one,
/// or the name is a dict method (the dict's own)
pub fn getattr(v: &V, name: &str) -> Option<R> {
    let V::Dict(d) = v else { return None };
    let t = tag(d)?;
    if DICT_ATTRS.contains(&name) || name.starts_with("__") {
        return None;
    }
    if class_attrs(t.cls).binary_search(&name).is_ok() {
        return Some(match name {
            "OBJECT_NAME" if (t.cls as usize) < OBJECTS.len() => Ok(V::str(OBJECTS[t.cls as usize].0)),
            "is_empty" if is_list_class(t.cls) => {
                let data = dget(d, "data").ok_or_else(|| Exc::attr_error("data"));
                data.and_then(|x| Ok(V::Bool(!ops::truthy(&x)?)))
            }
            "to_dict" | "to_dict_recursive" | "auto_paging_iter" | "next_page" | "previous_page" => {
                Ok(V::native(Native::MethodOf(v.clone(), static_name(name))))
            }
            _ => Err(unsupported(t.cls, name)),
        });
    }
    if name.starts_with('_') {
        return Some(Err(Exc::attr_error(name)));
    }
    Some(match dget(d, name) {
        Some(x) => Ok(x),
        None => Err(Exc::attr_error(match missing_key(d, &V::str(name)) {
            Some(e) => e.args().first().map(s).unwrap_or_else(|| name.to_string()),
            None => name.to_string(),
        })),
    })
}

fn static_name(name: &str) -> &'static str {
    ["to_dict", "to_dict_recursive", "auto_paging_iter", "next_page", "previous_page"].into_iter().find(|n| *n == name).unwrap_or("to_dict")
}

fn unsupported(cls: u16, name: &str) -> Exc {
    Exc::runtime(format!("py2axum: {}.{name} of a stripe object is not supported (only its fields, the dict methods, to_dict(), \
                          and auto_paging_iter()/next_page()/previous_page() of a list)", class_name(cls)))
}

/// `obj.name(...)` on a StripeObject; None when the dict's own method applies
pub fn call_method<'a>(cx: &'a Cx, v: &'a V, name: &'a str, args: &'a [V], kwargs: &'a [(String, V)])
                      -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'a>>> {
    let V::Dict(d) = v else { return None };
    let t = tag(d)?;
    if DICT_ATTRS.contains(&name) || name.starts_with("__") {
        return None;
    }
    let d = d.clone();
    Some(Box::pin(async move { call_tagged(cx, v, &d, t, name, args, kwargs).await }))
}

async fn call_tagged(cx: &Cx, v: &V, d: &Arc<DictCell>, t: Tag, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let no_args = |what: &str| -> R<()> {
        if !args.is_empty() || !kwargs.is_empty() {
            return Err(Exc::type_error(format!("py2axum: {what}() with arguments on a stripe object is not supported")));
        }
        Ok(())
    };
    match name {
        "to_dict" if class_attrs(t.cls).binary_search(&name).is_ok() => no_args(name).map(|_| {
            V::Dict(Arc::new(Mutex::new(d.lock().clone())))
        }),
        "to_dict_recursive" if class_attrs(t.cls).binary_search(&name).is_ok() => no_args(name).and_then(|_| plain(v)),
        "auto_paging_iter" if is_list_class(t.cls) => match no_args(name) {
            Ok(()) => auto_paging(cx, d, &t).await,
            Err(e) => Err(e),
        },
        "next_page" | "previous_page" if is_list_class(t.cls) && !is_search_class(t.cls) => match no_args(name) {
            Ok(()) => match &t.list {
                Some(ctx) => page(cx, d, ctx, name == "next_page").await,
                None => Err(Exc::runtime(format!("py2axum: {name}() of a list object not returned by a request"))),
            },
            Err(e) => Err(e),
        },
        _ => match getattr(v, name).expect("tagged") {
            Ok(f) => methods::call_value(cx, &f, args.to_vec(), kwargs.to_vec()).await,
            Err(e) => Err(e),
        },
    }
}

/// `_to_dict_recursive()`: plain dicts all the way down
fn plain(v: &V) -> R {
    super::stack_guard()?;
    match v {
        V::Dict(d) => {
            let pairs: Vec<(V, V)> = d.lock().values().cloned().collect();
            let mut out = Vec::with_capacity(pairs.len());
            for (k, x) in pairs {
                let x = match &x {
                    V::List(l) => V::list(l.lock().iter().map(|i| if class_of(i).is_some() { plain(i) } else { Ok(i.clone()) }).collect::<R<Vec<_>>>()?),
                    o if class_of(o).is_some() => plain(o)?,
                    o => o.clone(),
                };
                out.push((k, x));
            }
            V::dict_from(out)
        }
        o => Ok(o.clone()),
    }
}

/// `obj.name = value` on a StripeObject: `self[name] = value`, the options keys changing its requestor
pub fn setattr(v: &V, name: &str, val: &V) -> Option<R<()>> {
    let V::Dict(d) = v else { return None };
    let t = tag(d)?;
    if ["api_key", "stripe_version", "stripe_account", "stripe_context"].contains(&name) || name.starts_with('_') {
        return Some(Err(unsupported(t.cls, name)));
    }
    Some(ops::setitem(v, &V::str(name), val.clone()))
}

/// `StripeObject.__setitem__`: the empty string is refused
pub fn setitem_check(v: &V, k: &V, val: &V) -> R<()> {
    let V::Dict(d) = v else { return Ok(()) };
    if tag(d).is_none() {
        return Ok(());
    }
    if matches!(val, V::Str(x) if x.is_empty()) {
        let k = ops::str_(k)?;
        return Err(Exc::value_error(format!(
            "You cannot set {k} to an empty string on this object. The empty string is treated specially in our requests. If \
             you'd like to delete the property using the save() method on this object, you may set {}.{k}=None. Alternatively, \
             you can pass {k}='' to delete the property when using a resource method such as modify().", str_of(d)?)));
    }
    Ok(())
}

fn str_of(d: &Arc<DictCell>) -> R<String> {
    let v = super::libs::json_dumps(&V::Dict(d.clone()), &[("sort_keys".into(), V::Bool(true)), ("indent".into(), V::Int(2))])?;
    ops::str_(&v)
}

/// `str(obj)`: `json.dumps(obj, sort_keys=True, indent=2)`
pub fn str_hook(v: &V) -> Option<R<String>> {
    let V::Dict(d) = v else { return None };
    tag(d)?;
    Some(str_of(d))
}

/// `repr(obj)`: `<Customer customer id=cus_X at 0x...> JSON: {...}`
pub fn repr_hook(v: &V) -> Option<R<String>> {
    let V::Dict(d) = v else { return None };
    let t = tag(d)?;
    let mut ident = vec![class_name(t.cls).to_string()];
    if let Some(V::Str(o)) = dget(d, "object") {
        ident.push(o.to_string());
    }
    if let Some(V::Str(id)) = dget(d, "id") {
        ident.push(format!("id={id}"));
    }
    Some(str_of(d).map(|j| format!("<{} at {}> JSON: {j}", ident.join(" "), ops::addr(Arc::as_ptr(d) as usize))))
}

// ---------------------------------------------------------------- Webhook / WebhookSignature

fn sve(msg: String, header: &V, payload: &V) -> Exc {
    let e = stripe_error(&STRIPE_SIGNATURE_VERIFICATION_ERROR, V::str(msg), ErrParts { http_body: payload.clone(), ..Default::default() });
    e.0.attrs.lock().insert("sig_header".into(), header.clone());
    e
}

/// `WebhookSignature.verify_header(payload, header, secret, tolerance)`
async fn verify_header(cx: &Cx, payload: &V, header: &V, secret: &V, tolerance: &V) -> R {
    let parsed: R<(V, Vec<V>)> = async {
        let items = ops::iter(&methods::call_method(cx, header, "split", vec![V::str(",")], vec![]).await?)?;
        let mut lists = Vec::new();
        for i in items {
            lists.push(ops::iter(&methods::call_method(cx, &i, "split", vec![V::str("="), V::Int(2)], vec![]).await?)?);
        }
        let first = |l: &Vec<V>| l.first().cloned().ok_or_else(|| Exc::new(&INDEX_ERROR, vec![V::str("list index out of range")]));
        let second = |l: &Vec<V>| l.get(1).cloned().ok_or_else(|| Exc::new(&INDEX_ERROR, vec![V::str("list index out of range")]));
        let mut ts = Vec::new();
        for l in &lists {
            if ops::eq_bool(&first(l)?, &V::str("t")) {
                ts.push(second(l)?);
            }
        }
        let t0 = ts.first().cloned().ok_or_else(|| Exc::new(&INDEX_ERROR, vec![V::str("list index out of range")]))?;
        let timestamp = methods::b_int(&[t0])?;
        let mut sigs = Vec::new();
        for l in &lists {
            if ops::eq_bool(&first(l)?, &V::str("v1")) {
                sigs.push(second(l)?);
            }
        }
        Ok((timestamp, sigs))
    }
    .await;
    let Ok((timestamp, signatures)) = parsed else {
        return Err(sve("Unable to extract timestamp and signatures from header".into(), header, payload));
    };
    if signatures.is_empty() {
        return Err(sve("No signatures found with expected scheme v1".into(), header, payload));
    }
    let ts = ops::percent_format("%d", &timestamp)?;
    let signed = format!("{ts}.{}", ops::str_(payload)?);
    let key = methods::call_method(cx, secret, "encode", vec![V::str("utf-8")], vec![]).await?;
    let msg = methods::call_method(cx, &V::str(&signed), "encode", vec![V::str("utf-8")], vec![]).await?;
    let (V::Bytes(key), V::Bytes(msg)) = (&key, &msg) else { return Err(Exc::type_error("py2axum: encode() did not give bytes")) };
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("any key length");
    mac.update(msg);
    let expected = hex::encode(mac.finalize().into_bytes());
    let mut ok = false;
    for sig in &signatures {
        // hmac.compare_digest(str, str)
        let V::Str(sg) = sig else { return Err(Exc::type_error(format!("unsupported operand types(s) or combination of types: 'str' and '{}'", sig.type_name()))) };
        if !sg.is_ascii() {
            return Err(Exc::type_error("comparing strings with non-ASCII characters is not supported"));
        }
        // in constant time, as hmac.compare_digest (a `==` stopping at the first different byte let a client guess
        // the signature of a forged event from the response times)
        if sg.len() == expected.len() && bool::from(subtle::ConstantTimeEq::ct_eq(sg.as_bytes(), expected.as_bytes())) {
            ok = true;
            break;
        }
    }
    if !ok {
        return Err(sve("No signatures found matching the expected signature for payload".into(), header, payload));
    }
    if ops::truthy(tolerance)? {
        let now = chrono::Utc::now().timestamp_micros() as f64 / 1e6;
        let limit = ops::sub(&V::Float(now), tolerance)?;
        if ops::cmp(&timestamp, &limit)? == std::cmp::Ordering::Less {
            return Err(sve(format!("Timestamp outside the tolerance zone ({ts})"), header, payload));
        }
    }
    Ok(V::Bool(true))
}

fn bind<'a>(f: &str, names: &[&str], args: &'a [V], kwargs: &'a [(String, V)], required: usize) -> R<Vec<Option<V>>> {
    if args.len() > names.len() {
        return Err(Exc::type_error(format!("{f}() takes from {} to {} positional arguments but {} were given", required, names.len(), args.len())));
    }
    let mut out: Vec<Option<V>> = names.iter().enumerate().map(|(i, _)| args.get(i).cloned()).collect();
    for (k, v) in kwargs {
        match names.iter().position(|n| n == k) {
            Some(i) if out[i].is_none() => out[i] = Some(v.clone()),
            Some(_) => return Err(Exc::type_error(format!("{f}() got multiple values for argument '{k}'"))),
            None => return Err(Exc::type_error(format!("{f}() got an unexpected keyword argument '{k}'"))),
        }
    }
    let missing: Vec<String> = names[..required].iter().zip(&out).filter(|(_, v)| v.is_none()).map(|(n, _)| format!("'{n}'")).collect();
    if !missing.is_empty() {
        return Err(Exc::type_error(format!("{f}() missing {} required positional argument{}: {}", missing.len(),
                                           if missing.len() > 1 { "s" } else { "" }, missing.join(" and "))));
    }
    Ok(out)
}

async fn decode_payload(cx: &Cx, payload: V) -> R {
    if let V::Bytes(_) = payload {
        return methods::call_method(cx, &payload, "decode", vec![V::str("utf-8")], vec![]).await;
    }
    Ok(payload)
}

/// `stripe.Webhook.construct_event(payload, sig_header, secret, tolerance=300, api_key=None)`
pub async fn construct_event(cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = bind("Webhook.construct_event", &["payload", "sig_header", "secret", "tolerance", "api_key"], &args, &kwargs, 3)?;
    let payload = decode_payload(cx, a[0].clone().unwrap()).await?;
    let tolerance = a[3].clone().unwrap_or(V::Int(300));
    verify_header(cx, &payload, a[1].as_ref().unwrap(), a[2].as_ref().unwrap(), &tolerance).await?;
    let text = ops::str_(&payload)?;
    let data = super::pyjson::loads(&text)?;
    if !matches!(data, V::Dict(_)) {
        return Err(Exc::attr_error(format!("'{}' object has no attribute 'get'", data.type_name())));
    }
    let mut req = Req::global();
    if let Some(k) = &a[4] {
        if ops::truthy(k)? {
            req.api_key = k.clone();
        }
    }
    convert(&data, &req, None, Some(object_class("event")))
}

/// `stripe.WebhookSignature.verify_header(payload, header, secret, tolerance=None)`
pub async fn verify_header_call(cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = bind("WebhookSignature.verify_header", &["payload", "header", "secret", "tolerance"], &args, &kwargs, 3)?;
    verify_header(cx, a[0].as_ref().unwrap(), a[1].as_ref().unwrap(), a[2].as_ref().unwrap(), &a[3].clone().unwrap_or(V::None)).await
}

/// `stripe.error` under stripe 14.0.0 (back in 14.0.1): the package's `__getattr__` has no `error` entry and
/// raises a bare `AttributeError()` while handling its `KeyError('error')`, itself raised while `tested` (the
/// exception an except clause is matching), or else the exception being handled, is handled
pub fn no_error_module<T>(cx: &Cx, tested: Option<&Exc>) -> R<T> {
    let mut k = Exc::new(&KEY_ERROR, vec![V::str("error")]);
    let handled = tested.cloned().or_else(|| cx.handling.lock().last().cloned());
    if let Some(h) = handled {
        k = super::trace::with_context(k, &h);
    }
    let a = Exc::new(&ATTRIBUTE_ERROR, vec![]);
    if super::python() < (3, 13) {
        // traceback's suggestion among dir(stripe) (private names hidden from 3.13 on)
        a.0.attrs.lock().insert("__tb_suggestion__".into(), V::str("_error"));
    }
    Err(super::trace::with_context(a, &k))
}
