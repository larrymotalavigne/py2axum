//! Outgoing HTTP: `httpx.AsyncClient` (0.28) and `aiohttp.ClientSession` (3.14) over reqwest, with
//! their response objects, timeouts, request encodings and exception classes.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::ops;
use super::pyd;
use super::v::*;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Httpx,
    Aiohttp,
    /// a `requests.Response` (what pywebpush returns)
    Requests,
}

/// httpx.Timeout / aiohttp.ClientTimeout (seconds; None = no limit)
#[derive(Clone, Copy, Default)]
pub struct Timeout {
    pub total: Option<f64>,
    pub connect: Option<f64>,
    pub read: Option<f64>,
}

pub struct Client {
    kind: Kind,
    http: reqwest::Client,
    /// the same client without certificate verification (aiohttp's `ssl=False` on a request)
    http_insecure: reqwest::Client,
    headers: Vec<(String, String)>,
    base_url: Option<String>,
    closed: AtomicBool,
}

pub struct Resp {
    kind: Kind,
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    url: String,
    method: String,
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn secs(v: &V) -> R<Option<f64>> {
    match v {
        V::None => Ok(None),
        V::Int(i) => Ok(Some(*i as f64)),
        V::Float(f) => Ok(Some(*f)),
        V::Native(n) => match &**n {
            Native::HttpTimeout(t) => Ok(t.total.or(t.read)),
            _ => Err(Exc::type_error(format!("py2axum: timeout of type {}", v.type_name()))),
        },
        o => Err(Exc::type_error(format!("py2axum: timeout of type {}", o.type_name()))),
    }
}

/// `httpx.Timeout(timeout, *, connect, read, write, pool)` / `aiohttp.ClientTimeout(total, connect, sock_read, sock_connect)`
pub fn timeout(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let mut t = Timeout::default();
    if kind == "httpx" {
        let all = args.first().or_else(|| kw(kwargs, "timeout")).map(secs).transpose()?.flatten();
        t.connect = all;
        t.read = all;
        for (k, v) in kwargs {
            match k.as_str() {
                "timeout" => {}
                "connect" => t.connect = secs(v)?,
                "read" => t.read = secs(v)?,
                "write" | "pool" => {}
                _ => return Err(Exc::type_error(format!("Timeout.__init__() got an unexpected keyword argument '{k}'"))),
            }
        }
    } else {
        t.total = args.first().map(secs).transpose()?.flatten();
        for (k, v) in kwargs {
            match k.as_str() {
                "total" => t.total = secs(v)?,
                "connect" | "sock_connect" => t.connect = secs(v)?.or(t.connect),
                "sock_read" => t.read = secs(v)?,
                "ceil_threshold" => {}
                _ => return Err(Exc::type_error(format!("ClientTimeout.__init__() got an unexpected keyword argument '{k}'"))),
            }
        }
    }
    Ok(V::native(Native::HttpTimeout(t)))
}

fn header_pairs(v: Option<&V>) -> R<Vec<(String, String)>> {
    match v {
        None | Some(V::None) => Ok(vec![]),
        Some(V::Dict(d)) => d.lock().values().map(|(k, v)| Ok((ops::str_(k)?, ops::str_(v)?))).collect(),
        Some(o) => Err(Exc::type_error(format!("py2axum: headers of type {}", o.type_name()))),
    }
}

/// `httpx.AsyncClient(...)` / `aiohttp.ClientSession(...)`
pub fn client(kind: &str, version: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let k = if kind == "httpx" { Kind::Httpx } else { Kind::Aiohttp };
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: pass the client options by keyword"));
    }
    let mut t = match k {
        // httpx: 5 s for each operation; aiohttp: 5 min in all, 30 s to connect
        Kind::Httpx | Kind::Requests => Timeout { total: None, connect: Some(5.0), read: Some(5.0) },
        Kind::Aiohttp => Timeout { total: Some(300.0), connect: Some(30.0), read: None },
    };
    let mut headers: Vec<(String, String)> = match k {
        Kind::Httpx | Kind::Requests => vec![
            ("Accept".into(), "*/*".into()),
            ("Accept-Encoding".into(), "gzip, deflate".into()),
            ("User-Agent".into(), format!("python-httpx/{version}")),
        ],
        Kind::Aiohttp => vec![
            ("Accept".into(), "*/*".into()),
            ("Accept-Encoding".into(), "gzip, deflate".into()),
            ("User-Agent".into(), format!("Python/3 aiohttp/{version}")),
        ],
    };
    let mut base_url = None;
    let mut follow = k == Kind::Aiohttp;
    for (name, v) in kwargs {
        match (k, name.as_str()) {
            // aiohttp: timeout=None is the default timeout
            (Kind::Aiohttp, "timeout") if v.is_none() => {}
            (Kind::Aiohttp, "auth" | "connector") if v.is_none() => {}
            (_, "timeout") => match v {
                V::Native(n) if matches!(&**n, Native::HttpTimeout(_)) => {
                    if let Native::HttpTimeout(x) = &**n {
                        t = *x;
                    }
                }
                other => {
                    let s = secs(other)?;
                    t = Timeout { total: None, connect: s, read: s };
                }
            },
            (_, "headers") => {
                for (hk, hv) in header_pairs(Some(v))? {
                    headers.retain(|(x, _)| !x.eq_ignore_ascii_case(&hk));
                    headers.push((hk, hv));
                }
            }
            (Kind::Httpx, "base_url") => base_url = Some(ops::str_(v)?.trim_end_matches('/').to_string()),
            (Kind::Httpx, "follow_redirects") => follow = ops::truthy(v)?,
            // an injected transport (tests): None is the default one
            (Kind::Httpx, "transport") if v.is_none() => {}
            _ => return Err(Exc::type_error(format!("py2axum: {}({name}=) is not supported", if k == Kind::Httpx { "AsyncClient" } else { "ClientSession" }))),
        }
    }
    let make = || {
        // bodies are decoded here, not by reqwest, so that `Content-Encoding` stays visible like in httpx/aiohttp
        let mut b = reqwest::Client::builder().no_gzip().no_deflate().redirect(if follow {
            reqwest::redirect::Policy::limited(if k == Kind::Httpx { 20 } else { 10 })
        } else {
            reqwest::redirect::Policy::none()
        });
        if let Some(s) = t.total {
            b = b.timeout(Duration::from_secs_f64(s));
        }
        if let Some(s) = t.connect {
            b = b.connect_timeout(Duration::from_secs_f64(s));
        }
        if let Some(s) = t.read {
            b = b.read_timeout(Duration::from_secs_f64(s));
        }
        b
    };
    let http = make().build().map_err(|e| Exc::runtime(format!("py2axum: HTTP client: {e}")))?;
    let http_insecure = make().danger_accept_invalid_certs(true).build().map_err(|e| Exc::runtime(format!("py2axum: HTTP client: {e}")))?;
    Ok(V::native(Native::HttpClient(Arc::new(Client { kind: k, http, http_insecure, headers, base_url, closed: AtomicBool::new(false) }))))
}

/// `yarl.URL(url)`: yarl's parts (default port of the scheme, `host` None without a host)
pub fn yarl_url(args: &[V], kwargs: &[(String, V)]) -> R {
    if !kwargs.is_empty() || args.len() != 1 {
        return Err(Exc::type_error("py2axum: yarl.URL(str) only"));
    }
    Ok(V::native(Native::YarlUrl(ops::str_(&args[0])?)))
}

pub fn yarl_attr(u: &str, name: &str) -> R {
    let p = reqwest::Url::parse(u).ok();
    let opt = |x: Option<String>| x.map(V::str).unwrap_or(V::None);
    Ok(match name {
        "host" | "raw_host" => opt(p.as_ref().and_then(|p| p.host_str().map(|h| h.trim_matches(|c| c == '[' || c == ']').to_string()))),
        "scheme" => V::str(p.as_ref().map(|p| p.scheme()).unwrap_or("")),
        "port" => p.as_ref().and_then(|p| p.port_or_known_default()).map(|x| V::Int(x as i64)).unwrap_or(V::None),
        "explicit_port" => p.as_ref().and_then(|p| p.port()).map(|x| V::Int(x as i64)).unwrap_or(V::None),
        "path" | "raw_path" => V::str(p.as_ref().map(|p| p.path()).unwrap_or(u)),
        "query_string" | "raw_query_string" => V::str(p.as_ref().and_then(|p| p.query()).unwrap_or("")),
        "fragment" => V::str(p.as_ref().and_then(|p| p.fragment()).unwrap_or("")),
        "user" => opt(p.as_ref().map(|p| p.username().to_string()).filter(|s| !s.is_empty())),
        "password" => opt(p.as_ref().and_then(|p| p.password().map(str::to_string))),
        _ => return Err(Exc::attr_error(format!("'URL' object has no attribute '{name}'"))),
    })
}

/// `httpx.URL(url)`: the parts read from it
pub fn url(args: &[V]) -> R {
    let s = ops::str_(args.first().ok_or_else(|| Exc::type_error("URL() missing 'url'"))?)?;
    Ok(V::native(Native::HttpUrl(s)))
}

pub fn url_attr(u: &str, name: &str) -> R {
    let parsed = reqwest::Url::parse(u).ok();
    Ok(match name {
        "host" => V::str(parsed.as_ref().and_then(|p| p.host_str()).unwrap_or("")),
        "scheme" => V::str(parsed.as_ref().map(|p| p.scheme()).unwrap_or("")),
        "path" => V::str(parsed.as_ref().map(|p| p.path()).unwrap_or("")),
        "port" => parsed.as_ref().and_then(|p| p.port()).map(|p| V::Int(p as i64)).unwrap_or(V::None),
        _ => return Err(Exc::attr_error(format!("'URL' object has no attribute '{name}'"))),
    })
}

fn exc(class: &'static Class, msg: String) -> Exc {
    Exc::msg(class, msg)
}

fn request_error(k: Kind, e: &reqwest::Error, url: &str) -> Exc {
    let host = reqwest::Url::parse(url).ok();
    match k {
        Kind::Httpx | Kind::Requests => {
            if e.is_timeout() && e.is_connect() {
                exc(&HTTPX_CONNECT_TIMEOUT, "timed out".into())
            } else if e.is_timeout() {
                exc(&HTTPX_READ_TIMEOUT, "timed out".into())
            } else if e.is_connect() {
                exc(&HTTPX_CONNECT_ERROR, "All connection attempts failed".into())
            } else if e.is_redirect() {
                exc(&HTTPX_TOO_MANY_REDIRECTS, "Exceeded maximum allowed redirects.".into())
            } else {
                exc(&HTTPX_NETWORK_ERROR, e.to_string())
            }
        }
        Kind::Aiohttp => {
            if e.is_timeout() && e.is_connect() {
                exc(&AIO_CONNECTION_TIMEOUT, format!("Connection timeout to host {url}"))
            } else if e.is_timeout() {
                // the `total` timer: asyncio.TimeoutError, not a ClientError
                Exc::new(&TIMEOUT_ERROR, vec![])
            } else if e.is_connect() {
                let (h, p) = host.map(|u| (u.host_str().unwrap_or("").to_string(), u.port_or_known_default().unwrap_or(0))).unwrap_or_default();
                exc(&AIO_CONNECTOR_ERROR, format!("Cannot connect to host {h}:{p} ssl:default [Connect call failed]"))
            } else if e.is_redirect() {
                exc(&AIO_TOO_MANY_REDIRECTS, format!("0, message='', url='{url}'"))
            } else {
                exc(&AIO_CLIENT_ERROR, e.to_string())
            }
        }
    }
}

fn query_pairs(v: &V, k: Kind) -> R<Vec<(String, String)>> {
    let items: Vec<(V, V)> = match v {
        V::None => return Ok(vec![]),
        // an already encoded query string (`params=urlencode(...)`)
        V::Str(s) => return Ok(form_urlencoded::parse(s.as_bytes()).into_owned().collect()),
        V::Dict(d) => d.lock().values().cloned().collect(),
        V::List(l) => l.lock().iter().map(|t| match t {
            V::Tuple(p) if p.len() == 2 => Ok((p[0].clone(), p[1].clone())),
            _ => Err(Exc::type_error("py2axum: params must be a dict or a list of pairs")),
        }).collect::<R<_>>()?,
        o => return Err(Exc::type_error(format!("py2axum: params of type {}", o.type_name()))),
    };
    let mut out = vec![];
    for (key, val) in items {
        let key = ops::str_(&key)?;
        let vals = match &val {
            V::List(l) if k == Kind::Httpx => l.lock().clone(),
            _ => vec![val.clone()],
        };
        for x in vals {
            let s = match (&x, k) {
                (V::Bool(b), Kind::Httpx) => (if *b { "true" } else { "false" }).to_string(),
                (V::None, Kind::Httpx) => String::new(),
                (V::Bool(_), Kind::Aiohttp) => return Err(Exc::type_error("Invalid variable type: value should be str, int or float, got True of type <class 'bool'>")),
                _ => ops::str_(&x)?,
            };
            out.push((key.clone(), s));
        }
    }
    Ok(out)
}

/// `client.get(url, ...)`, `client.post(...)`, `session.post(...)`... (the request is sent when called)
pub async fn send(c: &Client, method: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    if c.closed.load(Ordering::Relaxed) {
        return Err(match c.kind {
            Kind::Httpx | Kind::Requests => Exc::runtime("Cannot send a request, as the client has been closed."),
            Kind::Aiohttp => Exc::runtime("Session is closed"),
        });
    }
    let raw = ops::str_(args.first().or_else(|| kw(kwargs, "url")).ok_or_else(|| Exc::type_error("missing 'url'"))?)?;
    let url = match &c.base_url {
        Some(b) if !raw.contains("://") => format!("{b}/{}", raw.trim_start_matches('/')),
        _ => raw.clone(),
    };
    let mut parsed = reqwest::Url::parse(&url).map_err(|_| match c.kind {
        Kind::Httpx | Kind::Requests => {
            if url.contains("://") { exc(&HTTPX_INVALID_URL, format!("Invalid URL {url:?}")) } else { exc(&HTTPX_UNSUPPORTED_PROTOCOL, "Request URL is missing an 'http://' or 'https://' protocol.".into()) }
        }
        Kind::Aiohttp => exc(&AIO_INVALID_URL, url.clone()),
    })?;
    let mut headers = c.headers.clone();
    let mut body: Option<(Vec<u8>, &str)> = None;
    let mut per_request_timeout = None;
    let mut follow = None;
    let mut insecure = false;
    for (k, v) in kwargs {
        match k.as_str() {
            "url" => {}
            "params" => {
                let pairs = query_pairs(v, c.kind)?;
                if !pairs.is_empty() {
                    parsed.query_pairs_mut().extend_pairs(pairs);
                }
            }
            "headers" => {
                for (hk, hv) in header_pairs(Some(v))? {
                    headers.retain(|(x, _)| !x.eq_ignore_ascii_case(&hk));
                    headers.push((hk, hv));
                }
            }
            "json" if !v.is_none() => {
                let style = match c.kind {
                    Kind::Httpx | Kind::Requests => pyd::JsonStyle { ensure_ascii: false, item_sep: ",", key_sep: ":" },
                    Kind::Aiohttp => pyd::JsonStyle { ensure_ascii: true, item_sep: ", ", key_sep: ": " },
                };
                body = Some((pyd::to_json(v, &style, false)?.into_bytes(), "application/json"));
            }
            "data" | "content" if !v.is_none() => {
                body = Some(match v {
                    V::Dict(_) if k == "data" => {
                        let pairs = query_pairs(v, Kind::Httpx)?;
                        let enc: String = form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish();
                        (enc.into_bytes(), "application/x-www-form-urlencoded")
                    }
                    V::Str(s) => (s.as_bytes().to_vec(), if c.kind == Kind::Aiohttp { "text/plain; charset=utf-8" } else { "" }),
                    V::Bytes(b) => (b.to_vec(), if c.kind == Kind::Aiohttp { "application/octet-stream" } else { "" }),
                    o => return Err(Exc::type_error(format!("py2axum: {k}= of type {}", o.type_name()))),
                });
            }
            "json" | "data" | "content" => {}
            "timeout" => per_request_timeout = Some(secs(v)?),
            // aiohttp: ssl=False skips certificate verification; True/None verify
            "ssl" if c.kind == Kind::Aiohttp => insecure = matches!(v, V::Bool(false)),
            "proxy" if v.is_none() => {}
            "follow_redirects" | "allow_redirects" => follow = Some(ops::truthy(v)?),
            _ => return Err(Exc::type_error(format!("py2axum: {method}({k}=) is not supported"))),
        }
    }
    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Exc::value_error(format!("invalid method {method}")))?;
    let final_url = parsed.to_string();
    let mut rb = if insecure { c.http_insecure.request(m, parsed) } else { c.http.request(m, parsed) };
    for (k, v) in &headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if let Some((b, ct)) = body {
        if !ct.is_empty() && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            rb = rb.header("content-type", ct);
        }
        rb = rb.body(b);
    }
    if let Some(Some(s)) = per_request_timeout {
        rb = rb.timeout(Duration::from_secs_f64(s));
    }
    if follow == Some(false) || (follow == Some(true) && c.kind == Kind::Httpx) {
        // per-request redirect policies are not available in reqwest: refused rather than ignored
        return Err(Exc::type_error("py2axum: a per-request redirect option is not supported (set it on the client)"));
    }
    let resp = rb.send().await.map_err(|e| request_error(c.kind, &e, &final_url))?;
    let status = resp.status().as_u16();
    let url_out = resp.url().to_string();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().iter().map(|&b| b as char).collect()))
        .collect();
    let body = resp.bytes().await.map_err(|e| request_error(c.kind, &e, &final_url))?.to_vec();
    let body = decode_body(&headers, body).map_err(|e| match c.kind {
        Kind::Aiohttp => exc(&AIO_CLIENT_ERROR, format!("400, message='Can not decode content-encoding: {e}'")),
        _ => exc(&HTTPX_DECODING_ERROR, e),
    })?;
    let reason = super::status_phrase(status).unwrap_or("").to_string();
    Ok(V::native(Native::HttpResp(Arc::new(Resp { kind: c.kind, status, reason, headers, body, url: url_out, method: method.to_string() }))))
}

// ---------------------------------------------------------------- patched request methods

/// `aiohttp.ClientSession._request` / `httpx.AsyncClient.request` replaced by the project (a wrapper
/// timing the calls...): the clients call it instead of sending directly
static HOOKS: parking_lot::RwLock<[Option<V>; 2]> = parking_lot::RwLock::new([None, None]);

fn hook_slot(kind: &str) -> usize {
    if kind == "aiohttp" {
        0
    } else {
        1
    }
}

/// `aiohttp.ClientSession._request = f` / `httpx.AsyncClient.request = f`
pub fn set_hook(kind: &str, f: V) -> R {
    HOOKS.write()[hook_slot(kind)] = Some(f);
    Ok(V::None)
}

/// reading `aiohttp.ClientSession._request` / `httpx.AsyncClient.request`: the replacement if any, else
/// the library's method (an `async def` taking the client, the method and the URL)
pub fn request_fn(kind: &'static str) -> V {
    if let Some(f) = HOOKS.read()[hook_slot(kind)].clone() {
        return f;
    }
    let call: super::KwFn = Arc::new(move |_cx: &super::Cx, args: Vec<V>, kwargs: Vec<(String, V)>| {
        Box::pin(async move {
            if args.len() < 3 {
                return Err(Exc::type_error(format!("py2axum: {kind} request method called with {} positional arguments", args.len())));
            }
            let mut args = args.into_iter();
            let (client, method, url) = (args.next().unwrap(), args.next().unwrap(), args.next().unwrap());
            let V::Native(n) = &client else { return Err(Exc::type_error("py2axum: the request method needs its client")) };
            let Native::HttpClient(c) = &**n else { return Err(Exc::type_error("py2axum: the request method needs its client")) };
            let method = ops::str_(&method)?.to_ascii_uppercase();
            // the defaults aiohttp's get()/head()/post() pass along are the client's own behaviour here
            let kwargs: Vec<(String, V)> = kwargs
                .into_iter()
                .filter(|(k, v)| !(k == "data" && v.is_none()) && !(k == "allow_redirects" && matches!(v, V::Bool(b) if *b == (method != "HEAD"))))
                .collect();
            let mut rest = vec![url];
            rest.extend(args);
            send(c, &method, &rest, &kwargs).await
        })
    });
    let (name, qual, module) = if kind == "aiohttp" { ("_request", "ClientSession._request", "aiohttp.client") } else { ("request", "AsyncClient.request", "httpx._client") };
    let attrs = vec![
        ("__name__".to_string(), V::str(name)),
        ("__qualname__".to_string(), V::str(qual)),
        ("__module__".to_string(), V::str(module)),
        ("__doc__".to_string(), V::None),
    ];
    V::native(Native::PyFn(PyFn { call, is_async: true, attrs: parking_lot::Mutex::new(attrs) }))
}

/// a client method through the project's replacement of the request method: aiohttp's `get(url)` calls
/// `self._request("GET", url, allow_redirects=True, **kwargs)`, `post` passes `data=None`...; httpx's
/// `get(url)` calls `self.request("GET", url, **kwargs)` (only the keywords given)
async fn hooked(cx: &super::Cx, hook: &V, recv: &V, c: &Client, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let (method, mut rest) = if name == "request" {
        let mut it = args.into_iter();
        (it.next().ok_or_else(|| Exc::type_error("request() missing 'method'"))?, it.collect::<Vec<_>>())
    } else {
        (V::str(name.to_ascii_uppercase()), args)
    };
    if rest.is_empty() {
        return Err(Exc::type_error(format!("{name}() missing 1 required positional argument: 'url'")));
    }
    let url = rest.remove(0);
    let mut kw: Vec<(String, V)> = Vec::new();
    if c.kind == Kind::Aiohttp {
        let take = |kwargs: &mut Vec<(String, V)>, k: &str| kwargs.iter().position(|(x, _)| x == k).map(|i| kwargs.remove(i).1);
        let mut kwargs = kwargs.clone();
        match name {
            "get" | "options" | "head" => {
                let v = take(&mut kwargs, "allow_redirects").unwrap_or(V::Bool(name != "head"));
                kw.push(("allow_redirects".into(), v));
            }
            "post" | "put" | "patch" => {
                let v = take(&mut kwargs, "data").unwrap_or(V::None);
                kw.push(("data".into(), v));
            }
            _ => {}
        }
        kw.extend(kwargs);
    } else {
        kw = kwargs;
    }
    let mut a = vec![recv.clone(), method, url];
    a.extend(rest);
    let r = super::methods::call_value(cx, hook, a, kw).await?;
    super::aio::await_value(r).await
}

pub async fn client_method(cx: &super::Cx, recv: &V, c: &Client, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if matches!(name, "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "request") {
        let hook = HOOKS.read()[if c.kind == Kind::Aiohttp { 0 } else { 1 }].clone();
        if let Some(h) = hook.filter(|_| c.kind != Kind::Requests) {
            return hooked(cx, &h, recv, c, name, args, kwargs).await;
        }
    }
    let (args, kwargs) = (&args[..], &kwargs[..]);
    match name {
        "get" | "post" | "put" | "patch" | "delete" | "head" | "options" => send(c, &name.to_ascii_uppercase(), args, kwargs).await,
        "request" => {
            let m = ops::str_(args.first().ok_or_else(|| Exc::type_error("request() missing 'method'"))?)?.to_ascii_uppercase();
            send(c, &m, &args[1..], kwargs).await
        }
        "aclose" | "close" => {
            c.closed.store(true, Ordering::Relaxed);
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if c.kind == Kind::Httpx { "AsyncClient" } else { "ClientSession" }))),
    }
}

pub fn client_attr(c: &Client, name: &str) -> R {
    match name {
        "is_closed" | "closed" => Ok(V::Bool(c.closed.load(Ordering::Relaxed))),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if c.kind == Kind::Httpx { "AsyncClient" } else { "ClientSession" }))),
    }
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn charset(&self) -> Option<String> {
        let ct = self.header("content-type")?;
        ct.split(';').skip(1).find_map(|p| {
            let (k, v) = p.split_once('=')?;
            (k.trim().eq_ignore_ascii_case("charset")).then(|| v.trim().trim_matches('"').to_ascii_lowercase())
        })
    }
    fn text(&self, strict: bool) -> R<String> {
        match self.charset().as_deref() {
            None | Some("utf-8") | Some("utf8") => {
                if strict {
                    String::from_utf8(self.body.clone()).map_err(|_| Exc::msg(&UNICODE_DECODE_ERROR, "'utf-8' codec can't decode the response"))
                } else {
                    Ok(String::from_utf8_lossy(&self.body).into_owned())
                }
            }
            Some("iso-8859-1") | Some("latin-1") | Some("latin1") => Ok(self.body.iter().map(|&b| b as char).collect()),
            Some("ascii") | Some("us-ascii") if self.body.is_ascii() => Ok(String::from_utf8_lossy(&self.body).into_owned()),
            Some(cs) => Err(Exc::value_error(format!("py2axum: response charset {cs} is not supported"))),
        }
    }
    fn headers_v(&self) -> R {
        V::dict_from(self.headers.iter().map(|(k, v)| (V::str(k), V::str(v))).collect())
    }
}

pub fn resp_attr(r: &Resp, name: &str) -> R {
    Ok(match (r.kind, name) {
        (Kind::Httpx, "status_code") | (Kind::Aiohttp, "status") => V::Int(r.status as i64),
        (Kind::Httpx, "reason_phrase") | (Kind::Aiohttp, "reason") => V::str(&r.reason),
        (Kind::Httpx, "text") => V::str(r.text(false)?),
        (Kind::Httpx, "content") => V::Bytes(Arc::from(&r.body[..])),
        (Kind::Httpx, "is_success") => V::Bool((200..300).contains(&r.status)),
        (Kind::Httpx, "is_error") => V::Bool(r.status >= 400),
        (Kind::Aiohttp, "ok") => V::Bool(r.status < 400),
        (_, "url") => V::str(&r.url),
        (_, "headers") => r.headers_v()?,
        (Kind::Aiohttp, "content_type") => V::str(r.header("content-type").unwrap_or("application/octet-stream").split(';').next().unwrap_or("").trim()),
        (Kind::Requests, "status_code") => V::Int(r.status as i64),
        (Kind::Requests, "reason") => V::str(&r.reason),
        (Kind::Requests, "text") => V::str(r.text(false)?),
        (Kind::Requests, "content") => V::Bytes(Arc::from(&r.body[..])),
        (Kind::Requests, "ok") => V::Bool(r.status < 400),
        _ => return Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if r.kind == Kind::Aiohttp { "ClientResponse" } else { "Response" }))),
    })
}

/// A response read in full, as `requests` gives it.
/// the body decoded per `Content-Encoding` (gzip; deflate as zlib, else raw like httpx), headers untouched
fn decode_body(headers: &[(String, String)], body: Vec<u8>) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let enc = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-encoding")).map(|(_, v)| v.trim().to_ascii_lowercase());
    let mut out = Vec::new();
    match enc.as_deref() {
        Some("gzip") | Some("x-gzip") => flate2::read::MultiGzDecoder::new(&body[..]).read_to_end(&mut out).map_err(|e| e.to_string())?,
        Some("deflate") => match flate2::read::ZlibDecoder::new(&body[..]).read_to_end(&mut out) {
            Ok(n) => n,
            Err(_) => {
                out.clear();
                flate2::read::DeflateDecoder::new(&body[..]).read_to_end(&mut out).map_err(|e| e.to_string())?
            }
        },
        _ => return Ok(body),
    };
    Ok(out)
}

pub async fn from_reqwest(resp: reqwest::Response) -> R {
    let status = resp.status().as_u16();
    let url = resp.url().to_string();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().iter().map(|&b| b as char).collect()))
        .collect();
    let body = resp.bytes().await.map_err(|e| Exc::msg(&REQUESTS_CONNECTION_ERROR, e.to_string()))?.to_vec();
    let reason = super::status_phrase(status).unwrap_or("").to_string();
    Ok(V::native(Native::HttpResp(Arc::new(Resp { kind: Kind::Requests, status, reason, headers, body, url, method: "POST".into() }))))
}

pub fn status_of(v: &V) -> u16 {
    match v {
        V::Native(n) => match &**n {
            Native::HttpResp(r) => r.status,
            _ => 0,
        },
        _ => 0,
    }
}

/// `response.text` of a response value (WebPushException's message)
pub fn text_of(v: &V) -> String {
    match v {
        V::Native(n) => match &**n {
            Native::HttpResp(r) => r.text(false).unwrap_or_default(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn loads(text: &str) -> R {
    pyd::loads(text)
}

pub fn resp_method(r: &Arc<Resp>, recv: &V, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let _ = args;
    match (r.kind, name) {
        (Kind::Httpx | Kind::Requests, "json") => loads(&r.text(false)?),
        (Kind::Httpx, "raise_for_status") => {
            if (200..300).contains(&r.status) {
                return Ok(recv.clone());
            }
            let error_type = match r.status / 100 {
                1 => "Informational response",
                3 => "Redirect response",
                4 => "Client error",
                5 => "Server error",
                _ => "Invalid status code",
            };
            let mut msg = format!("{error_type} '{} {}' for url '{}'\n", r.status, r.reason, r.url);
            if (300..400).contains(&r.status) {
                if let Some(loc) = r.header("location") {
                    msg += &format!("Redirect location: '{loc}'\n");
                }
            }
            msg += &format!("For more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{}", r.status);
            let e = exc(&HTTPX_STATUS_ERROR, msg);
            e.0.attrs.lock().insert("response".into(), recv.clone());
            Err(e)
        }
        (Kind::Aiohttp, "text") => Ok(V::str(r.text(true)?)),
        (Kind::Aiohttp, "read") => Ok(V::Bytes(Arc::from(&r.body[..]))),
        (Kind::Aiohttp, "json") => {
            if let Some(V::Str(ct)) = kw(kwargs, "content_type") {
                if !ct.is_empty() && !r.header("content-type").unwrap_or("").to_lowercase().contains(&ct.to_lowercase()) {
                    return Err(content_type_error(r));
                }
            } else if kw(kwargs, "content_type").is_none() && !r.header("content-type").unwrap_or("").to_lowercase().contains("json") {
                return Err(content_type_error(r));
            }
            let text = r.text(true)?;
            if text.trim().is_empty() {
                return Ok(V::None);
            }
            loads(text.trim())
        }
        (Kind::Aiohttp, "raise_for_status") => {
            if r.status < 400 {
                return Ok(V::None);
            }
            Err(response_error(&AIO_RESPONSE_ERROR, r, &r.reason))
        }
        (Kind::Aiohttp, "release" | "close") | (Kind::Httpx, "aclose" | "close") => Ok(V::None),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if r.kind == Kind::Aiohttp { "ClientResponse" } else { "Response" }))),
    }
}

fn response_error(class: &'static Class, r: &Resp, message: &str) -> Exc {
    let e = exc(class, format!("{}, message={}, url={}", r.status, ops::repr(&V::str(message)).unwrap_or_default(), ops::repr(&V::str(&r.url)).unwrap_or_default()));
    let mut a = e.0.attrs.lock();
    a.insert("status".into(), V::Int(r.status as i64));
    a.insert("message".into(), V::str(message));
    drop(a);
    e
}

fn content_type_error(r: &Resp) -> Exc {
    let ct = r.header("content-type").unwrap_or("").to_lowercase();
    response_error(&AIO_CONTENT_TYPE_ERROR, r, &format!("Attempt to decode JSON with unexpected mimetype: {ct}"))
}

/// `async with x as y`: what `__aenter__` gives
pub fn aenter(v: &V) -> R {
    match v {
        V::Native(n) if matches!(&**n, Native::HttpClient(_) | Native::HttpResp(_)) => Ok(v.clone()),
        V::Session(_) => Ok(v.clone()),
        other => Err(Exc::type_error(format!("'{}' object does not support the asynchronous context manager protocol", other.type_name()))),
    }
}

/// `__aexit__`: the client is closed, an engine connection rolled back (the exception, if any, propagates)
pub async fn aexit(v: &V) -> R {
    if let V::Session(s) = v {
        s.rollback().await?;
    }
    if let V::Native(n) = v {
        if let Native::HttpClient(c) = &**n {
            c.closed.store(true, Ordering::Relaxed);
        }
    }
    Ok(V::None)
}

/// `resp.request.method` & co are not modelled; the method is kept for error messages
pub fn method_of(r: &Resp) -> &str {
    &r.method
}
