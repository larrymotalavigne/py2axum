//! Starlette response objects returned by an endpoint (Response, JSONResponse, PlainTextResponse,
//! HTMLResponse, RedirectResponse, FileResponse), cookies as `http.cookies` writes them, background tasks.
use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use parking_lot::Mutex;

use super::ops;
use super::v::*;
use super::Cx;

pub enum RespBody {
    Bytes(Vec<u8>),
    File { path: String, filename: Option<String>, disposition: String },
    /// the response of `call_next` (a middleware): sent once, as is
    Raw(Mutex<Option<Body>>),
}

/// `await call_next(request)`: the inner response, its headers editable.
pub fn from_response(r: Response) -> V {
    let (parts, body) = r.into_parts();
    let headers = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().iter().map(|&b| b as char).collect::<String>()))
        .collect();
    V::native(Native::RespObj(Arc::new(RespObj {
        status: Mutex::new(parts.status.as_u16()),
        headers: Mutex::new(headers),
        body: RespBody::Raw(Mutex::new(Some(body))),
        media: None,
    })))
}

pub struct RespObj {
    pub status: Mutex<u16>,
    /// user headers then the ones Starlette adds (content-length, content-type...), in order
    pub headers: Mutex<Vec<(String, String)>>,
    pub body: RespBody,
    pub media: Option<String>,
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn arg<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kw(kwargs, name)).filter(|v| !matches!(v, V::None))
}

fn user_headers(h: Option<&V>) -> R<Vec<(String, String)>> {
    match h {
        None => Ok(vec![]),
        Some(V::Dict(d)) => d.lock().values().map(|(k, v)| Ok((ops::str_(k)?.to_ascii_lowercase(), ops::str_(v)?))).collect(),
        Some(o) => Err(Exc::type_error(format!("headers must be a mapping, not {}", o.type_name()))),
    }
}

fn with_charset(media: &str) -> String {
    if media.starts_with("text/") && !media.to_ascii_lowercase().contains("charset=") {
        format!("{media}; charset=utf-8")
    } else {
        media.to_string()
    }
}

/// `urllib.parse.quote(s, safe=...)`
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "_.-~".contains(c) || safe.contains(c) {
            out.push(c);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

/// `Response(...)`, `JSONResponse(...)`... (`kind` = the class name)
pub fn new(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let allowed: &[&str] = match kind {
        "RedirectResponse" => &["url", "status_code", "headers"],
        "FileResponse" => &["path", "status_code", "headers", "media_type", "filename", "content_disposition_type"],
        _ => &["content", "status_code", "headers", "media_type"],
    };
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("py2axum: {kind}({k}=) is not supported")));
    }
    let status = |i: usize, d: u16| -> R<u16> {
        match arg(args, kwargs, i, "status_code") {
            None => Ok(d),
            Some(V::Int(s)) => Ok(status_u16(*s)),
            Some(o) => Err(Exc::type_error(format!("status_code must be an int, not {}", o.type_name()))),
        }
    };
    let mut headers = user_headers(arg(args, kwargs, 2, "headers"))?;
    let obj = match kind {
        "RedirectResponse" => {
            let url = ops::str_(arg(args, kwargs, 0, "url").ok_or_else(|| Exc::type_error("RedirectResponse() missing 'url'"))?)?;
            headers.push(("content-length".into(), "0".into()));
            headers.push(("location".into(), quote(&url, ":/%#?=@[]!$&'()*+,;")));
            RespObj { status: Mutex::new(status(1, 307)?), headers: Mutex::new(headers), body: RespBody::Bytes(vec![]), media: None }
        }
        "FileResponse" => {
            let path = super::pathio::fspath(arg(args, kwargs, 0, "path").ok_or_else(|| Exc::type_error("FileResponse() missing 'path'"))?)?;
            let filename = arg(args, kwargs, 4, "filename").map(ops::str_).transpose()?;
            let disposition = arg(args, kwargs, 5, "content_disposition_type").map(ops::str_).transpose()?.unwrap_or_else(|| "attachment".into());
            let media = match arg(args, kwargs, 3, "media_type") {
                Some(m) => ops::str_(m)?,
                None => guess_type(filename.as_deref().unwrap_or(&path)).unwrap_or("text/plain").to_string(),
            };
            RespObj { status: Mutex::new(status(1, 200)?), headers: Mutex::new(headers), body: RespBody::File { path, filename, disposition }, media: Some(media) }
        }
        _ => {
            let (default_media, json) = match kind {
                "JSONResponse" => (Some("application/json"), true),
                "PlainTextResponse" => (Some("text/plain"), false),
                "HTMLResponse" => (Some("text/html"), false),
                _ => (None, false),
            };
            let content = arg(args, kwargs, 0, "content").cloned();
            let body: Vec<u8> = match (&content, json) {
                (None, false) => vec![],
                // Starlette: json.dumps(content, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
                (c, true) => super::pyd::to_json(c.as_ref().unwrap_or(&V::None), &super::pyd::JsonStyle { ensure_ascii: false, item_sep: ",", key_sep: ":", nan_null: false }, false)?.into_bytes(),
                (Some(V::Bytes(b)), _) => b.to_vec(),
                (Some(V::Str(s)), _) => s.as_bytes().to_vec(),
                (Some(o), _) => return Err(Exc::type_error(format!("py2axum: {kind} content must be str or bytes, not {}", o.type_name()))),
            };
            let media = arg(args, kwargs, 3, "media_type").map(ops::str_).transpose()?.or(default_media.map(str::to_string));
            let st = status(1, 200)?;
            if !(st < 200 || st == 204 || st == 304) && !headers.iter().any(|(k, _)| k == "content-length") {
                headers.push(("content-length".into(), body.len().to_string()));
            }
            if let Some(m) = &media {
                if !headers.iter().any(|(k, _)| k == "content-type") {
                    headers.push(("content-type".into(), with_charset(m)));
                }
            }
            RespObj { status: Mutex::new(st), headers: Mutex::new(headers), body: RespBody::Bytes(body), media }
        }
    };
    Ok(V::native(Native::RespObj(Arc::new(obj))))
}

/// `mimetypes.guess_type` for the usual extensions
pub fn guess_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit('/').next()?.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ics" => "text/calendar",
        "mp4" => "video/mp4",
        _ => return None,
    })
}

// ---------------------------------------------------------------- cookies

const LEGAL: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!#$%&'*+-.^_`|~:";

/// `http.cookies._quote`
fn cookie_quote(v: &str) -> String {
    if !v.is_empty() && v.chars().all(|c| LEGAL.contains(c)) {
        return v.to_string();
    }
    let unescaped = format!("{LEGAL} ()/<=>?@[]{{}}");
    let mut out = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            c if (c as u32) < 256 && !unescaped.contains(c) => out += &format!("\\{:03o}", c as u32),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `http.cookies._getdate(future)`
fn cookie_date(future: i64) -> String {
    let t = chrono::Utc::now() + chrono::Duration::seconds(future);
    t.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Starlette's `set_cookie` (attributes sorted by name, like `Morsel.OutputString`).
pub fn set_cookie(headers: &Mutex<Vec<(String, String)>>, args: &[V], kwargs: &[(String, V)], delete: bool) -> R {
    let names = ["key", "value", "max_age", "expires", "path", "domain", "secure", "httponly", "samesite"];
    for (k, _) in kwargs {
        if !names.contains(&k.as_str()) {
            return Err(Exc::type_error(format!("py2axum: set_cookie({k}=) is not supported")));
        }
    }
    let get = |name: &str| -> Option<V> {
        let i = names.iter().position(|n| *n == name).unwrap();
        // delete_cookie(key, path, domain, secure, httponly, samesite)
        let pos = if delete { ["key", "path", "domain", "secure", "httponly", "samesite"].iter().position(|n| *n == name) } else { Some(i) };
        pos.and_then(|p| args.get(p)).or_else(|| kw(kwargs, name)).cloned()
    };
    let key = ops::str_(&get("key").ok_or_else(|| Exc::type_error("set_cookie() missing 'key'"))?)?;
    let value = if delete { String::new() } else { get("value").map(|v| ops::str_(&v)).transpose()?.unwrap_or_default() };
    let mut attrs: Vec<(&str, String)> = Vec::new();
    let max_age = if delete { Some(V::Int(0)) } else { get("max_age").filter(|v| !v.is_none()) };
    let expires = if delete { Some(V::Int(0)) } else { get("expires").filter(|v| !v.is_none()) };
    if let Some(m) = max_age {
        attrs.push(("max-age", format!("Max-Age={}", ops::str_(&m)?)));
    }
    if let Some(e) = expires {
        let s = match e {
            V::Int(i) => cookie_date(i),
            V::DateTime(d) => d.utc().format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            o => ops::str_(&o)?,
        };
        attrs.push(("expires", format!("expires={s}")));
    }
    let path = get("path").map(|v| if v.is_none() { Ok(None) } else { ops::str_(&v).map(Some) }).transpose()?.unwrap_or(Some("/".into()));
    if let Some(p) = path {
        attrs.push(("path", format!("Path={p}")));
    }
    if let Some(d) = get("domain").filter(|v| !v.is_none()) {
        attrs.push(("domain", format!("Domain={}", ops::str_(&d)?)));
    }
    if get("secure").map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false) {
        attrs.push(("secure", "Secure".into()));
    }
    if get("httponly").map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false) {
        attrs.push(("httponly", "HttpOnly".into()));
    }
    let samesite = match get("samesite") {
        None => Some("lax".to_string()),
        Some(V::None) => None,
        Some(v) => Some(ops::str_(&v)?),
    };
    if let Some(s) = samesite {
        if !matches!(s.to_ascii_lowercase().as_str(), "strict" | "lax" | "none") {
            return Err(Exc::msg(&ASSERTION_ERROR, "samesite must be either 'strict', 'lax' or 'none'"));
        }
        attrs.push(("samesite", format!("SameSite={s}")));
    }
    attrs.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = format!("{key}={}", cookie_quote(&value));
    for (_, a) in attrs {
        out += "; ";
        out += &a;
    }
    headers.lock().push(("set-cookie".into(), out));
    Ok(V::None)
}

pub fn method(r: &RespObj, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "set_cookie" => set_cookie(&r.headers, args, kwargs, false),
        "delete_cookie" => set_cookie(&r.headers, args, kwargs, true),
        _ => Err(Exc::attr_error(format!("'Response' object has no attribute '{name}'"))),
    }
}

pub fn attr(r: &Arc<RespObj>, name: &str) -> R {
    match name {
        "status_code" => Ok(V::Int(*r.status.lock() as i64)),
        "headers" => Ok(V::native(Native::RespHeaders(r.clone()))),
        "body" => match &r.body {
            RespBody::Bytes(b) => Ok(V::Bytes(Arc::from(&b[..]))),
            RespBody::File { .. } => Err(Exc::attr_error("'FileResponse' object has no attribute 'body'")),
            RespBody::Raw(_) => Err(Exc::attr_error("'_StreamingResponse' object has no attribute 'body'")),
        },
        "media_type" => Ok(r.media.as_deref().map(V::str).unwrap_or(V::None)),
        _ => Err(Exc::attr_error(format!("'Response' object has no attribute '{name}'"))),
    }
}

pub fn set_status(r: &RespObj, v: &V) -> R<()> {
    match v {
        V::Int(i) => {
            *r.status.lock() = status_u16(*i);
            Ok(())
        }
        _ => Err(Exc::type_error("status_code must be an int")),
    }
}

/// uvicorn (httptools) has a status line for 100..599 only: any other status is a KeyError raised by the
/// server's `send` and the connection is dropped without an answer. The KeyError goes up through the
/// middlewares (their `finally` blocks run), then the server drops the connection (`is_drop`).
pub fn status_drop(status: u16) -> Exc {
    let e = Exc::new(&KEY_ERROR, vec![V::Int(status as i64)]);
    e.0.attrs.lock().insert("__py2axum_drop__".into(), V::Bool(true));
    e
}

pub fn is_drop(e: &Exc) -> bool {
    e.0.attrs.lock().contains_key("__py2axum_drop__")
}

/// The returned response object, sent as Starlette would.
/// A status kept as given while it fits (0 stands for any other value: never a valid status, never a
/// truncated one that would look valid).
fn status_u16(s: i64) -> u16 {
    u16::try_from(s).ok().filter(|s| *s <= 999).unwrap_or(0)
}

pub fn into_response(r: &RespObj) -> R<Response> {
    let status = *r.status.lock();
    if !(100..=599).contains(&status) {
        return Err(status_drop(status));
    }
    let mut headers = r.headers.lock().clone();
    let body = match &r.body {
        RespBody::Raw(b) => {
            let body = b.lock().take().ok_or_else(|| Exc::runtime("py2axum: response already sent"))?;
            let mut out = Response::builder().status(status);
            for (k, v) in &headers {
                out = out.header(k.as_str(), axum::http::HeaderValue::from_bytes(&v.chars().map(|c| c as u8).collect::<Vec<u8>>()).map_err(|e| Exc::value_error(e.to_string()))?);
            }
            return Ok(out.body(body).unwrap_or_else(super::web::bad_response));
        }
        RespBody::Bytes(b) => b.clone(),
        RespBody::File { path, filename, disposition } => {
            let meta = std::fs::metadata(path).map_err(|_| Exc::runtime(format!("File at path {path} does not exist.")))?;
            if !meta.is_file() {
                return Err(Exc::runtime(format!("File at path {path} is not a file.")));
            }
            // a disk read: off the loop worker (Starlette reads in a thread)
            let data = super::thread::blocking(|| std::fs::read(path)).map_err(|e| Exc::runtime(format!("{e}")))?;
            let mut h = vec![("content-type".to_string(), with_charset(r.media.as_deref().unwrap_or("text/plain"))), ("accept-ranges".to_string(), "bytes".to_string())];
            if let Some(f) = filename {
                let q = quote(f, "");
                h.push(("content-disposition".into(), if &q != f { format!("{disposition}; filename*=utf-8''{q}") } else { format!("{disposition}; filename=\"{f}\"") }));
            }
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).unwrap_or_default();
            let st_mtime = mtime.as_secs() as f64 + mtime.subsec_nanos() as f64 * 1e-9;
            h.push(("content-length".into(), data.len().to_string()));
            let lm = chrono::DateTime::from_timestamp(mtime.as_secs() as i64, 0).unwrap_or_default();
            h.push(("last-modified".into(), lm.format("%a, %d %b %Y %H:%M:%S GMT").to_string()));
            let etag = {
                use md5::Digest;
                let mut h = md5::Md5::new();
                h.update(format!("{}-{}", ops::float_repr(st_mtime), data.len()).as_bytes());
                format!("{:x}", h.finalize())
            };
            h.push(("etag".into(), format!("\"{etag}\"")));
            h.extend(headers.drain(..));
            headers = h;
            data
        }
    };
    let mut b = Response::builder().status(status);
    for (k, v) in &headers {
        b = b.header(k.as_str(), v.as_str());
    }
    Ok(b.body(Body::from(body)).unwrap_or_else(super::web::bad_response))
}

// ---------------------------------------------------------------- background tasks

pub struct Tasks(pub Mutex<Vec<(V, Vec<V>, Vec<(String, V)>)>>);

pub fn tasks_method(t: &Tasks, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "add_task" => {
            let f = args.first().cloned().ok_or_else(|| Exc::type_error("add_task() missing 'func'"))?;
            t.0.lock().push((f, args[1..].to_vec(), kwargs.to_vec()));
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'BackgroundTasks' object has no attribute '{name}'"))),
    }
}

/// The request's BackgroundTasks (one per request, shared by the endpoint and its dependencies).
pub fn background(cx: &Cx) -> V {
    cx.background.get_or_init(|| V::native(Native::Tasks(Tasks(Mutex::new(Vec::new()))))).clone()
}

/// After the response: the tasks in order; a failure is logged (the client has its response).
pub async fn run_background(cx: Cx) {
    let Some(V::Native(n)) = cx.background.get().cloned() else { return };
    let Native::Tasks(t) = &*n else { return };
    let tasks = std::mem::take(&mut *t.0.lock());
    for (f, args, kwargs) in tasks {
        if let Err(e) = super::methods::call_value(&cx, &f, args, kwargs).await {
            eprintln!("ERROR:py2axum:Exception in ASGI application: {:?}", e);
            break;
        }
    }
}

// ---------------------------------------------------------------- MutableHeaders

fn hkey(k: &V) -> R<String> {
    match k {
        V::Str(s) => Ok(s.to_lowercase()),
        o => Err(Exc::attr_error(format!("'{}' object has no attribute 'lower'", o.type_name()))),
    }
}

fn hval(v: &V) -> R<String> {
    match v {
        V::Str(s) => Ok(s.to_string()),
        o => Err(Exc::attr_error(format!("'{}' object has no attribute 'encode'", o.type_name()))),
    }
}

/// `headers[key]`: the first value
pub fn headers_getitem(r: &Mutex<Vec<(String, String)>>, k: &V) -> R {
    let key = hkey(k)?;
    r.lock().iter().find(|(x, _)| *x == key).map(|(_, v)| V::str(v)).ok_or_else(|| Exc::new(&KEY_ERROR, vec![k.clone()]))
}

/// `headers[key] = value`: replaces the first value, drops the others (Starlette's MutableHeaders)
pub fn headers_setitem(r: &Mutex<Vec<(String, String)>>, k: &V, v: &V) -> R<()> {
    let key = hkey(k)?;
    let val = hval(v)?;
    let mut h = r.lock();
    match h.iter().position(|(x, _)| *x == key) {
        Some(i) => {
            h[i].1 = val;
            let mut j = 0;
            h.retain(|(x, _)| {
                j += 1;
                j - 1 == i || *x != key
            });
        }
        None => h.push((key, val)),
    }
    Ok(())
}

pub fn headers_delitem(r: &Mutex<Vec<(String, String)>>, k: &V) -> R<()> {
    let key = hkey(k)?;
    let mut h = r.lock();
    let n = h.len();
    h.retain(|(x, _)| *x != key);
    if h.len() == n {
        return Err(Exc::new(&KEY_ERROR, vec![k.clone()]));
    }
    Ok(())
}

pub fn headers_contains(r: &Mutex<Vec<(String, String)>>, k: &V) -> R<bool> {
    let key = match k {
        V::Str(s) => s.to_lowercase(),
        _ => return Ok(false),
    };
    Ok(r.lock().iter().any(|(x, _)| *x == key))
}

pub fn headers_method(r: &Mutex<Vec<(String, String)>>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let a = |i: usize| args.get(i).cloned().ok_or_else(|| Exc::type_error(format!("{name}() missing argument")));
    match name {
        "get" => match headers_getitem(r, &a(0)?) {
            Ok(v) => Ok(v),
            Err(e) if e.isinstance(&KEY_ERROR) => Ok(args.get(1).cloned().or_else(|| kw(kwargs, "default").cloned()).unwrap_or(V::None)),
            Err(e) => Err(e),
        },
        "getlist" => {
            let key = hkey(&a(0)?)?;
            Ok(V::list(r.lock().iter().filter(|(x, _)| *x == key).map(|(_, v)| V::str(v)).collect()))
        }
        "keys" => Ok(V::list(r.lock().iter().map(|(k, _)| V::str(k)).collect())),
        "values" => Ok(V::list(r.lock().iter().map(|(_, v)| V::str(v)).collect())),
        "items" => Ok(V::list(r.lock().iter().map(|(k, v)| V::tuple(vec![V::str(k), V::str(v)])).collect())),
        "update" => {
            let other = a(0)?;
            let items: Vec<(V, V)> = match &other {
                V::Dict(d) => d.lock().values().cloned().collect(),
                o => return Err(Exc::type_error(format!("py2axum: MutableHeaders.update({}) is not supported", o.type_name()))),
            };
            for (k, v) in items {
                headers_setitem(r, &k, &v)?;
            }
            Ok(V::None)
        }
        "setdefault" => {
            let (k, v) = (a(0)?, a(1)?);
            match headers_getitem(r, &k) {
                Ok(x) => Ok(x),
                Err(_) => {
                    r.lock().push((hkey(&k)?, hval(&v)?));
                    Ok(v)
                }
            }
        }
        "append" => {
            let (k, v) = (a(0)?, a(1)?);
            r.lock().push((hkey(&k)?, hval(&v)?));
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'MutableHeaders' object has no attribute '{name}'"))),
    }
}
