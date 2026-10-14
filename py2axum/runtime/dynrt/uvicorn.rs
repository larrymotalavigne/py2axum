//! What uvicorn (0.54, launched as `uvicorn module:app [options]`) does around the application, so that
//! the binary prints the same lines: its command-line options (the binary takes them, from the command
//! line or `UVICORN_*` variables), `Config.configure_logging()` (LOGGING_CONFIG, `--log-level`,
//! `--no-access-log`, `--use-colors`, `--log-config` in JSON), the server's messages (started, lifespan,
//! running on, shutting down, finished), the access log line of each response, and ProxyHeadersMiddleware
//! (`--proxy-headers`, `--forwarded-allow-ips`): the client address and scheme taken from
//! `X-Forwarded-For` / `X-Forwarded-Proto` when the peer is trusted.
//!
//! One process: `--workers N` is accepted, and the binary serves alone (its output is uvicorn's with one
//! worker).
use std::sync::OnceLock;

use super::logging::{self, emit_sync, Loc, DEBUG, ERROR, INFO, WARNING};
use super::v::*;
use super::Cx;

pub struct Opts {
    pub host: String,
    pub port: u16,
    pub log_level: Option<i64>,
    pub access_log: bool,
    pub use_colors: Option<bool>,
    pub log_config: Option<String>,
    pub proxy_headers: bool,
    pub forwarded_allow_ips: Vec<String>,
    trust: Trust,
    pub timeout_graceful_shutdown: Option<f64>,
}

static OPTS: OnceLock<Opts> = OnceLock::new();

pub fn opts() -> &'static Opts {
    OPTS.get_or_init(|| parse(&[]).unwrap_or_else(|e| usage_error(&e)))
}

/// uvicorn's `--log-level` names (`LOG_LEVELS`)
fn level_of(name: &str) -> Option<i64> {
    Some(match name.to_ascii_lowercase().as_str() {
        "critical" => 50,
        "error" => 40,
        "warning" => 30,
        "info" => 20,
        "debug" => 10,
        "trace" => 5,
        _ => return None,
    })
}

fn usage_error(msg: &str) -> ! {
    eprintln!("Usage: {} [OPTIONS] [APP]\nTry '{} --help' for help.\n\nError: {msg}", prog(), prog());
    std::process::exit(2)
}

fn prog() -> String {
    std::env::args().next().map(|a| a.rsplit('/').next().unwrap_or(&a).to_string()).unwrap_or_else(|| "server".into())
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn flag_env(name: &str) -> Option<bool> {
    env(name).map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "t" | "yes" | "y" | "on"))
}

/// The options: command line, then `UVICORN_*` variables (click's auto_envvar_prefix), then the binary's
/// own variables (HOST, PORT, PY2AXUM_LOG_LEVEL, PY2AXUM_SHUTDOWN_TIMEOUT), then the defaults: uvicorn's
/// (127.0.0.1:8000) when the binary is given command-line options, else its historical 0.0.0.0:8080.
fn parse(extra: &[String]) -> Result<Opts, String> {
    let argv: Vec<String> = std::env::args().skip(1).chain(extra.iter().cloned()).collect();
    let cli = !argv.is_empty();
    let mut o = Opts {
        host: env("UVICORN_HOST").or_else(|| env("HOST")).unwrap_or_else(|| if cli { "127.0.0.1" } else { "0.0.0.0" }.into()),
        port: 0,
        log_level: None,
        access_log: flag_env("UVICORN_ACCESS_LOG").unwrap_or(true),
        use_colors: flag_env("UVICORN_USE_COLORS"),
        log_config: env("UVICORN_LOG_CONFIG"),
        proxy_headers: flag_env("UVICORN_PROXY_HEADERS").unwrap_or(true),
        forwarded_allow_ips: vec![],
        trust: Trust::default(),
        timeout_graceful_shutdown: None,
    };
    let mut port = env("UVICORN_PORT").or_else(|| env("PORT"));
    let mut level = env("UVICORN_LOG_LEVEL").or_else(|| env("PY2AXUM_LOG_LEVEL"));
    let mut fwd = env("UVICORN_FORWARDED_ALLOW_IPS");
    let mut tgs = env("UVICORN_TIMEOUT_GRACEFUL_SHUTDOWN").or_else(|| env("PY2AXUM_SHUTDOWN_TIMEOUT"));
    let mut it = argv.into_iter().peekable();
    let mut app_seen = false;
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = |n: &str| -> Result<String, String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => it.next().ok_or_else(|| format!("Option '{n}' requires an argument.")),
            }
        };
        match name.as_str() {
            "--host" => o.host = value("--host")?,
            "--port" => port = Some(value("--port")?),
            "--log-level" => level = Some(value("--log-level")?),
            "--log-config" => o.log_config = Some(value("--log-config")?),
            "--access-log" => o.access_log = true,
            "--no-access-log" => o.access_log = false,
            "--use-colors" => o.use_colors = Some(true),
            "--no-use-colors" => o.use_colors = Some(false),
            "--proxy-headers" => o.proxy_headers = true,
            "--no-proxy-headers" => o.proxy_headers = false,
            "--forwarded-allow-ips" => fwd = Some(value("--forwarded-allow-ips")?),
            "--timeout-graceful-shutdown" => tgs = Some(value("--timeout-graceful-shutdown")?),
            // accepted for a command line copied from uvicorn's: one process serves, the app is compiled in
            "--workers" | "--timeout-keep-alive" | "--limit-concurrency" | "--backlog" | "--app-dir" => {
                value(&name)?;
            }
            "--factory" | "--no-server-header" | "--no-date-header" => {}
            s if s.starts_with('-') => return Err(format!("No such option: {s}")),
            _ if !app_seen => app_seen = true,
            s => return Err(format!("Got unexpected extra argument ({s})")),
        }
    }
    o.port = match port {
        None if cli => 8000,
        None => 8080,
        Some(p) => p.parse().map_err(|_| format!("Invalid value for '--port': '{p}' is not a valid integer."))?,
    };
    if let Some(l) = level {
        o.log_level = Some(level_of(&l).ok_or_else(|| {
            format!("Invalid value for '--log-level': '{l}' is not one of 'critical', 'error', 'warning', 'info', 'debug', 'trace'.")
        })?);
    }
    o.forwarded_allow_ips = fwd
        .or_else(|| env("FORWARDED_ALLOW_IPS"))
        .unwrap_or_else(|| "127.0.0.1,::1".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    o.trust = Trust::parse(&o.forwarded_allow_ips);
    o.timeout_graceful_shutdown = match tgs {
        None => None,
        Some(t) => Some(t.parse().map_err(|_| format!("Invalid value for '--timeout-graceful-shutdown': '{t}' is not a valid integer."))?),
    };
    Ok(o)
}

/// the panic that drops a connection whose status has no status line (uvicorn logged it already)
pub const DROP_PANIC: &str = "py2axum: status code outside 100..599: the connection is dropped";

/// panics the server uses to drop a connection are not reported (the rest are, as by default)
fn quiet_drop_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.payload().downcast_ref::<String>().map(|s| s.as_str()).or_else(|| info.payload().downcast_ref::<&str>().copied());
        if msg != Some(DROP_PANIC) {
            default(info);
        }
    }));
}

/// `Config.configure_logging()`: TRACE registered, uvicorn's LOGGING_CONFIG (or `--log-config`), then
/// the level and access-log options. Runs before the application is imported, as in uvicorn.
pub fn configure_logging() {
    let o = opts();
    quiet_drop_panics();
    logging::add_level_name(5, "TRACE");
    let r = match &o.log_config {
        None => logging::dict_config(&default_config(o.use_colors)),
        Some(path) if path.ends_with(".json") => match std::fs::read_to_string(path) {
            Ok(text) => super::pyjson::loads(&text).and_then(|cfg| logging::dict_config(&cfg)),
            Err(e) => Err(super::pathio::os_err(e, path)),
        },
        Some(path) => Err(Exc::value_error(format!("py2axum: --log-config {path}: only a JSON dictConfig file is supported"))),
    };
    if let Err(e) = r {
        eprintln!("{}", super::trace::format_exc(&e).trim_end());
        std::process::exit(1);
    }
    if let Some(l) = o.log_level {
        for n in ["uvicorn.error", "uvicorn.access", "uvicorn.asgi"] {
            logging::get_logger(n).set_level(l);
        }
    }
    if !o.access_log {
        let a = logging::logger_value(logging::get_logger("uvicorn.access"));
        let _ = super::methods::setattr(&a, "handlers", V::list(vec![]));
        let _ = super::methods::setattr(&a, "propagate", V::Bool(false));
    }
}

/// uvicorn.config.LOGGING_CONFIG
fn default_config(use_colors: Option<bool>) -> V {
    let d = |items: Vec<(&str, V)>| V::dict_from(items.into_iter().map(|(k, v)| (V::str(k), v)).collect()).expect("dict");
    let colors = match use_colors {
        Some(b) => V::Bool(b),
        None => V::None,
    };
    d(vec![
        ("version", V::Int(1)),
        ("disable_existing_loggers", V::Bool(false)),
        (
            "formatters",
            d(vec![
                ("default", d(vec![("()", V::str("uvicorn.logging.DefaultFormatter")), ("fmt", V::str("%(levelprefix)s %(message)s")), ("use_colors", colors.clone())])),
                (
                    "access",
                    d(vec![
                        ("()", V::str("uvicorn.logging.AccessFormatter")),
                        ("fmt", V::str("%(levelprefix)s %(client_addr)s - \"%(request_line)s\" %(status_code)s")),
                        ("use_colors", colors),
                    ]),
                ),
            ]),
        ),
        (
            "handlers",
            d(vec![
                ("default", d(vec![("formatter", V::str("default")), ("class", V::str("logging.StreamHandler")), ("stream", V::str("ext://sys.stderr"))])),
                ("access", d(vec![("formatter", V::str("access")), ("class", V::str("logging.StreamHandler")), ("stream", V::str("ext://sys.stdout"))])),
            ]),
        ),
        (
            "loggers",
            d(vec![
                ("uvicorn", d(vec![("handlers", V::list(vec![V::str("default")])), ("level", V::str("INFO")), ("propagate", V::Bool(false))])),
                ("uvicorn.error", d(vec![("level", V::str("INFO"))])),
                ("uvicorn.access", d(vec![("handlers", V::list(vec![V::str("access")])), ("level", V::str("INFO")), ("propagate", V::Bool(false))])),
            ]),
        ),
    ])
}

// ---------------------------------------------------------------- the server's messages

static SERVER: Loc = Loc { file: "uvicorn/server.py", line: 103, func: "_serve", text: "" };
static LIFESPAN: Loc = Loc { file: "uvicorn/lifespan/on.py", line: 48, func: "startup", text: "" };
static HTTP: Loc = Loc { file: "uvicorn/protocols/http/httptools_impl.py", line: 482, func: "send", text: "" };

fn style_cyan_d(msg: &str) -> String {
    msg.replace("%d", "\x1b[36m%d\x1b[0m")
}

/// "Started server process [pid]" (after the import, before the lifespan)
pub fn started() {
    let m = "Started server process [%d]";
    let pid = V::Int(std::process::id() as i64);
    emit_sync("uvicorn.error", INFO, m, vec![pid], vec![("color_message", V::str(style_cyan_d(m)))], None, &SERVER);
}

pub fn lifespan_info(msg: &str) {
    emit_sync("uvicorn.error", INFO, msg, vec![], vec![], None, &LIFESPAN);
}

/// a lifespan that raised: Starlette sends the traceback as the failure message, uvicorn logs it, then
/// "Application startup failed. Exiting." (or shutdown)
pub fn lifespan_failed(e: &Exc, phase: &str) {
    let tb = super::trace::format_exc(e);
    emit_sync("uvicorn.error", ERROR, &tb, vec![], vec![], None, &LIFESPAN);
    emit_sync("uvicorn.error", ERROR, &format!("Application {phase} failed. Exiting."), vec![], vec![], None, &LIFESPAN);
}

/// "Uvicorn running on http://host:port (Press CTRL+C to quit)"
pub fn running(port: u16) {
    let o = opts();
    let fmt = if o.host.contains(':') { "%s://[%s]:%d" } else { "%s://%s:%d" };
    let m = format!("Uvicorn running on {fmt} (Press CTRL+C to quit)");
    let cm = format!("Uvicorn running on \x1b[1m{fmt}\x1b[0m (Press CTRL+C to quit)");
    emit_sync(
        "uvicorn.error",
        INFO,
        &m,
        vec![V::str("http"), V::str(&o.host), V::Int(port as i64)],
        vec![("color_message", V::str(cm))],
        None,
        &SERVER,
    );
}

pub fn info(msg: &str) {
    emit_sync("uvicorn.error", INFO, msg, vec![], vec![], None, &SERVER);
}

pub fn finished() {
    let m = "Finished server process [%d]";
    let pid = V::Int(std::process::id() as i64);
    emit_sync("uvicorn.error", INFO, m, vec![pid], vec![("color_message", V::str(style_cyan_d(m)))], None, &SERVER);
}

/// "Exception in ASGI application" with the traceback (uvicorn's `run_asgi`)
pub async fn asgi_exception(cx: &Cx, e: &Exc) {
    logging::emit(cx, "uvicorn.error", ERROR, "Exception in ASGI application\n", vec![], vec![], Some(e.clone()), &HTTP).await;
}

/// an error of the server outside any request ("ASGI callable returned without starting response."...)
pub fn error(msg: &str) {
    emit_sync("uvicorn.error", ERROR, msg, vec![], vec![], None, &HTTP);
}

pub fn warning(msg: &str) {
    emit_sync("uvicorn.error", WARNING, msg, vec![], vec![], None, &HTTP);
}

pub fn debug(msg: &str) {
    emit_sync("uvicorn.error", DEBUG, msg, vec![], vec![], None, &HTTP);
}

static ACCESS: std::sync::LazyLock<std::sync::Arc<logging::Logger>> = std::sync::LazyLock::new(|| logging::get_logger("uvicorn.access"));

/// is the access log on (the cost of every request when it is off: `--no-access-log`, a level above INFO)
#[inline]
pub fn access_enabled() -> bool {
    ACCESS.enabled_for(INFO) && ACCESS.has_handlers()
}

/// the access log line, when the response starts: `'%s - "%s %s HTTP/%s" %d'`
pub async fn access(cx: &Cx, status: u16) {
    access_status(cx, status as i64).await
}

/// the access line of any status the application set (one outside 100..599 included)
pub async fn access_status(cx: &Cx, status: i64) {
    if !access_enabled() {
        return;
    }
    Box::pin(access_line(cx, status)).await
}

async fn access_line(cx: &Cx, status: i64) {
    let r = &cx.req;
    let client = match &r.client {
        Some((h, p)) => format!("{h}:{p}"),
        None => String::new(),
    };
    let mut path = quote_path(&r.path);
    if !r.raw_query.is_empty() {
        path = format!("{path}?{}", r.raw_query);
    }
    let args = vec![V::str(client), V::str(&r.method), V::str(path), V::str(r.http_version), V::Int(status)];
    logging::emit(cx, "uvicorn.access", INFO, "%s - \"%s %s HTTP/%s\" %d", args, vec![], None, &HTTP).await;
}

/// `urllib.parse.quote(path)` (safe "/") of the decoded path, as uvicorn writes it
fn quote_path(raw: &str) -> String {
    let decoded = super::web::unquote(raw);
    let mut out = String::with_capacity(decoded.len());
    for b in decoded.as_bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~/".contains(b) {
            out.push(*b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

// ---------------------------------------------------------------- ProxyHeadersMiddleware

fn parse_host_port(v: &str) -> (String, u16) {
    if let Some(rest) = v.strip_prefix('[') {
        let Some(end) = rest.find(']') else { return (v.to_string(), 0) };
        let host = &rest[..end];
        let rem = &rest[end + 1..];
        if rem.is_empty() {
            return (host.to_string(), 0);
        }
        let Some(p) = rem.strip_prefix(':') else { return (v.to_string(), 0) };
        return (host.to_string(), p.parse().unwrap_or(0));
    }
    if v.matches(':').count() == 1 {
        let (h, p) = v.rsplit_once(':').unwrap();
        return match p.parse() {
            Ok(p) => (h.to_string(), p),
            Err(_) => (v.to_string(), 0),
        };
    }
    (v.to_string(), 0)
}

/// uvicorn's `_TrustedHosts`: "*", addresses, networks, other literals
#[derive(Default)]
struct Trust {
    all: bool,
    ips: Vec<std::net::IpAddr>,
    nets: Vec<(std::net::IpAddr, u8)>,
    literals: Vec<String>,
}

impl Trust {
    fn parse(hosts: &[String]) -> Trust {
        let mut t = Trust { all: hosts.iter().any(|h| h == "*"), ..Default::default() };
        for h in hosts {
            if let Some((net, len)) = h.split_once('/') {
                match (net.parse(), len.parse::<u8>()) {
                    (Ok(n), Ok(l)) => t.nets.push((n, l)),
                    _ => t.literals.push(h.clone()),
                }
            } else {
                match h.parse() {
                    Ok(ip) => t.ips.push(ip),
                    Err(_) => t.literals.push(h.clone()),
                }
            }
        }
        t
    }

    fn contains(&self, host: &str) -> bool {
        if self.all {
            return true;
        }
        if host.is_empty() {
            return false;
        }
        match host.parse::<std::net::IpAddr>() {
            Ok(ip) => self.ips.contains(&ip) || self.nets.iter().any(|(n, l)| in_net(ip, *n, *l)),
            Err(_) => self.literals.iter().any(|l| l == host),
        }
    }
}

fn trusted(host: &str) -> bool {
    opts().trust.contains(host)
}

fn in_net(ip: std::net::IpAddr, net: std::net::IpAddr, len: u8) -> bool {
    match (ip, net) {
        (std::net::IpAddr::V4(a), std::net::IpAddr::V4(b)) if len <= 32 => {
            let m = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            u32::from(a) & m == u32::from(b) & m
        }
        (std::net::IpAddr::V6(a), std::net::IpAddr::V6(b)) if len <= 128 => {
            let m = if len == 0 { 0 } else { u128::MAX << (128 - len) };
            u128::from(a) & m == u128::from(b) & m
        }
        _ => false,
    }
}

/// `--proxy-headers` (on by default): a trusted peer's X-Forwarded-Proto and X-Forwarded-For give the
/// request's scheme and client
pub fn proxy_headers(headers: &axum::http::HeaderMap, client: &mut Option<(String, u16)>, scheme: &mut String) {
    if !headers.contains_key("x-forwarded-for") && !headers.contains_key("x-forwarded-proto") {
        return;
    }
    let o = opts();
    if !o.proxy_headers {
        return;
    }
    let host = client.as_ref().map(|(h, _)| h.as_str()).unwrap_or("");
    if !trusted(host) {
        return;
    }
    if let Some(p) = headers.get("x-forwarded-proto") {
        let p: String = p.as_bytes().iter().map(|b| *b as char).collect::<String>().trim().to_string();
        if ["http", "https", "ws", "wss"].contains(&p.as_str()) {
            *scheme = p;
        }
    }
    let fwd: Vec<String> = headers.get_all("x-forwarded-for").iter().map(|v| v.as_bytes().iter().map(|b| *b as char).collect()).collect();
    if fwd.is_empty() {
        return;
    }
    let joined = fwd.join(", ");
    let hosts: Vec<&str> = joined.split(',').map(|s| s.trim()).collect();
    let (h, p) = if o.trust.all {
        parse_host_port(hosts[0])
    } else {
        hosts
            .iter()
            .rev()
            .map(|hp| parse_host_port(hp))
            .find(|(h, _)| !trusted(h))
            .unwrap_or_else(|| parse_host_port(hosts[0]))
    };
    if !h.is_empty() {
        *client = Some((h, p));
    }
}

/// an unhandled exception of a request outside the async path: "Exception in ASGI application"
pub fn exception(e: &Exc) {
    emit_sync("uvicorn.error", ERROR, "Exception in ASGI application\n", vec![], vec![], Some(e.clone()), &HTTP);
}

/// a request's exception that left the application, logged by `run_asgi` in the request's task: the record
/// goes to the scope outside SentryAsgiMiddleware's (the import's), but DedupeIntegration remembers per task,
/// so an exception Sentry already captured for the request is not sent twice
pub fn request_exception(cx: &Cx, e: &Exc) {
    super::sentry::in_request_task(cx, || exception(e));
}

/// binds the listening socket (after the lifespan startup, as uvicorn) and says where the server runs;
/// a failure is uvicorn's: the OSError logged, exit status 1
pub async fn bind() -> tokio::net::TcpListener {
    let o = opts();
    let host = o.host.trim_start_matches('[').trim_end_matches(']');
    match tokio::net::TcpListener::bind((host, o.port)).await {
        Ok(l) => {
            let port = l.local_addr().map(|a| a.port()).unwrap_or(o.port);
            running(port);
            l
        }
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(0);
            let msg = e.to_string();
            let msg = msg.split(" (os error").next().unwrap_or(&msg).to_lowercase();
            error(&format!("[Errno {errno}] error while attempting to bind on address ('{host}', {}): {msg}", o.port));
            std::process::exit(1);
        }
    }
}

/// `http.HTTPStatus(code).phrase` of the project's Python (3.13 renamed 413, 414, 416, 422); "" for a
/// code it does not know (uvicorn's AccessFormatter catches the ValueError)
pub fn http_phrase(code: i64) -> &'static str {
    if super::python() < (3, 13) {
        match code {
            413 => return "Request Entity Too Large",
            414 => return "Request-URI Too Long",
            416 => return "Requested Range Not Satisfiable",
            422 => return "Unprocessable Entity",
            _ => {}
        }
    }
    match code {
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
        _ => "",
    }
}

// ---------------------------------------------------------------- WebSockets (uvicorn's `websockets` implementation)

static WS: Loc = Loc { file: "uvicorn/protocols/websockets/websockets_impl.py", line: 280, func: "asgi_send", text: "" };

fn ws_args(req: &super::web::ReqCell) -> Vec<V> {
    let client = match &req.client {
        Some((h, p)) => format!("{h}:{p}"),
        None => String::new(),
    };
    let mut path = quote_path(&req.path);
    if !req.raw_query.is_empty() {
        path = format!("{path}?{}", req.raw_query);
    }
    vec![V::str(client), V::str(path)]
}

/// `client - "WebSocket /path" [accepted]`, `... 403` (closed before accepting), `... <status>` (a denial response)
pub fn ws_handshake(req: &super::web::ReqCell, status: Option<i64>) {
    let (fmt, args) = match status {
        None => ("%s - \"WebSocket %s\" [accepted]", ws_args(req)),
        Some(403) => ("%s - \"WebSocket %s\" 403", ws_args(req)),
        Some(s) => {
            let mut a = ws_args(req);
            a.push(V::Int(s));
            ("%s - \"WebSocket %s\" %d", a)
        }
    };
    emit_sync("uvicorn.error", INFO, fmt, args, vec![], None, &WS);
}

/// what the websockets library logs once the handshake's response is written
pub fn ws_response(status: u16) {
    if status == 101 {
        emit_sync("uvicorn.error", INFO, "connection open", vec![], vec![], None, &WS);
    } else {
        let args = vec![V::Int(status as i64), V::str(http_phrase(status as i64))];
        emit_sync("uvicorn.error", INFO, "connection rejected (%d %s)", args, vec![], None, &WS);
    }
}
