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
    // call_next's response is BaseHTTPMiddleware's _StreamingResponse; a route handler's, the route's class
    let cls = parts.extensions.get::<RespClass>().map(|c| c.0).unwrap_or("starlette.middleware.base._StreamingResponse");
    let mut headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().iter().map(|&b| b as char).collect::<String>()))
        .collect();
    // Starlette's init_headers puts content-length (a known body size) before content-type; axum adds it later
    let st = parts.status.as_u16();
    if !headers.iter().any(|(k, _)| k == "content-length") && !(st < 200 || st == 204 || st == 304) {
        if let Some(n) = axum::body::HttpBody::size_hint(&body).exact() {
            let at = headers.iter().position(|(k, _)| k == "content-type").unwrap_or(headers.len());
            headers.insert(at, ("content-length".into(), n.to_string()));
        }
    }
    V::native(Native::RespObj(Arc::new(RespObj {
        status: Mutex::new(parts.status.as_u16() as i64),
        headers: Mutex::new(headers),
        body: RespBody::Raw(Mutex::new(Some(body))),
        media: None,
        cls,
    })))
}

pub struct RespObj {
    pub status: Mutex<i64>,
    /// user headers then the ones Starlette adds (content-length, content-type...), in order
    pub headers: Mutex<Vec<(String, String)>>,
    pub body: RespBody,
    pub media: Option<String>,
    /// its class, dotted (the default repr: `<starlette.responses.JSONResponse object at 0x...>`)
    pub cls: &'static str,
}

/// The class of the Response an axum response was made from (`resp::from_response` restores it)
#[derive(Clone, Copy)]
pub struct RespClass(pub &'static str);

/// The dotted class of a Response kind the runtime builds
pub fn class_path(kind: &str) -> &'static str {
    match kind {
        "JSONResponse" => "starlette.responses.JSONResponse",
        "PlainTextResponse" => "starlette.responses.PlainTextResponse",
        "HTMLResponse" => "starlette.responses.HTMLResponse",
        "RedirectResponse" => "starlette.responses.RedirectResponse",
        "FileResponse" => "starlette.responses.FileResponse",
        "StreamingResponse" => "starlette.responses.StreamingResponse",
        "ORJSONResponse" => "fastapi.responses.ORJSONResponse",
        _ => "starlette.responses.Response",
    }
}

/// `repr(response.headers)`: Starlette's `Headers.__repr__`
pub fn headers_repr(items: &[(String, String)]) -> R<String> {
    let mut keys: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() == items.len() {
        let d = V::dict_from(items.iter().map(|(k, v)| (V::str(k), V::str(v))).collect())?;
        return Ok(format!("MutableHeaders({})", ops::repr(&d)?));
    }
    let raw = V::list(items.iter().map(|(k, v)| V::tuple(vec![
        V::Bytes(k.bytes().collect::<Vec<u8>>().into()), V::Bytes(v.chars().map(|c| c as u8).collect::<Vec<u8>>().into())])).collect());
    Ok(format!("MutableHeaders(raw={})", ops::repr(&raw)?))
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

/// a response class's own `media_type`
pub fn default_media(kind: &str) -> Option<&'static str> {
    match kind {
        "JSONResponse" | "ORJSONResponse" => Some("application/json"),
        "PlainTextResponse" => Some("text/plain"),
        "HTMLResponse" => Some("text/html"),
        _ => None,
    }
}

pub fn with_charset(media: &str) -> String {
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
    let status = |i: usize, d: i64| -> R<i64> {
        match arg(args, kwargs, i, "status_code") {
            None => Ok(d),
            Some(V::Int(s)) => Ok(*s),
            Some(o) => Err(Exc::type_error(format!("status_code must be an int, not {}", o.type_name()))),
        }
    };
    let mut headers = user_headers(arg(args, kwargs, 2, "headers"))?;
    let obj = match kind {
        "RedirectResponse" => {
            let url = ops::str_(arg(args, kwargs, 0, "url").ok_or_else(|| Exc::type_error("RedirectResponse() missing 'url'"))?)?;
            headers.push(("content-length".into(), "0".into()));
            headers.push(("location".into(), quote(&url, ":/%#?=@[]!$&'()*+,;")));
            RespObj { status: Mutex::new(status(1, 307)?), headers: Mutex::new(headers), body: RespBody::Bytes(vec![]), media: None, cls: class_path(kind) }
        }
        "FileResponse" => {
            let path = super::pathio::fspath(arg(args, kwargs, 0, "path").ok_or_else(|| Exc::type_error("FileResponse() missing 'path'"))?)?;
            let filename = arg(args, kwargs, 5, "filename").map(ops::str_).transpose()?;
            let disposition = arg(args, kwargs, 8, "content_disposition_type").map(ops::str_).transpose()?.unwrap_or_else(|| "attachment".into());
            let media = match arg(args, kwargs, 3, "media_type") {
                Some(m) => ops::str_(m)?,
                // an unknown type: application/octet-stream from Starlette 1.1, text/plain before
                None => guess_type(filename.as_deref().unwrap_or(&path))?
                    .unwrap_or_else(|| if super::web::starlette() >= (1, 1) { "application/octet-stream" } else { "text/plain" }.to_string()),
            };
            RespObj { status: Mutex::new(status(1, 200)?), headers: Mutex::new(headers), body: RespBody::File { path, filename, disposition }, media: Some(media), cls: class_path(kind) }
        }
        _ => {
            let (default_media, json) = (default_media(kind), matches!(kind, "JSONResponse" | "ORJSONResponse"));
            let content = arg(args, kwargs, 0, "content").cloned();
            let body: Vec<u8> = match (&content, json) {
                (None, false) => vec![],
                // FastAPI's ORJSONResponse.render: orjson.dumps(content, option=OPT_NON_STR_KEYS | OPT_SERIALIZE_NUMPY)
                (c, true) if kind == "ORJSONResponse" => super::orj::dumps_bytes(c.as_ref().unwrap_or(&V::None),
                    super::orj::OPT_NON_STR_KEYS | super::orj::OPT_SERIALIZE_NUMPY)?,
                // Starlette: json.dumps(content, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
                (c, true) => super::pyd::to_json(c.as_ref().unwrap_or(&V::None), &super::pyd::JsonStyle { ensure_ascii: false, item_sep: ",", key_sep: ":", nan_null: false }, false)?.into_bytes(),
                (Some(V::Bytes(b)), _) => b.to_vec(),
                (Some(V::Str(s)), _) => s.as_bytes().to_vec(),
                // Starlette's `render`: `content.encode(self.charset)`
                (Some(o), _) => return Err(Exc::attr_error(format!("'{}' object has no attribute 'encode'", o.type_name()))),
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
            RespObj { status: Mutex::new(st), headers: Mutex::new(headers), body: RespBody::Bytes(body), media, cls: class_path(kind) }
        }
    };
    Ok(V::native(Native::RespObj(Arc::new(obj))))
}

/// `mimetypes.knownfiles`: read at the first guess, in this order, over the built-in table (`mimetypes.init`)
const KNOWN_FILES: [&str; 9] = ["/etc/mime.types", "/etc/httpd/mime.types", "/etc/httpd/conf/mime.types", "/etc/apache/mime.types",
    "/etc/apache2/mime.types", "/usr/local/etc/httpd/conf/mime.types", "/usr/local/lib/netscape/mime.types",
    "/usr/local/etc/httpd/conf/mime.types", "/usr/local/etc/mime.types"];

/// `mimetypes`' database after `init()`: `types_map` and `types_map_inv`, [0] strict, [1] the common types
struct MimeDb {
    types: [std::collections::HashMap<String, String>; 2],
    inv: [indexmap::IndexMap<String, Vec<String>>; 2],
}

impl MimeDb {
    /// `MimeTypes.add_type`
    fn add(&mut self, ty: &str, ext: &str, strict: bool) {
        let i = usize::from(!strict);
        self.types[i].insert(ext.to_string(), ty.to_string());
        let exts = self.inv[i].entry(ty.to_string()).or_default();
        if !exts.iter().any(|e| e == ext) {
            exts.push(ext.to_string());
        }
    }
}

static MIME_DB: std::sync::OnceLock<MimeDb> = std::sync::OnceLock::new();

/// `mimetypes.init()`: CPython's built-in tables of the project's Python (mimetab.rs; they changed in 3.13, 3.14
/// and 3.15) in their order, then each existing known file (`type ext ext...` lines, `#` comments) through
/// `add_type`, like `MimeTypes.readfp`: a later entry replaces an earlier one, `types_map_inv` keeps both
fn mime_db() -> &'static MimeDb {
    MIME_DB.get_or_init(|| {
        let mut db = MimeDb { types: Default::default(), inv: Default::default() };
        for (ext, ty) in super::mimetab::tables(super::python()).0 {
            db.add(ty, ext, true);
        }
        for (ext, ty) in super::mimetab::common(super::python()) {
            db.add(ty, ext, false);
        }
        for f in KNOWN_FILES {
            if !std::path::Path::new(f).is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(f) else { continue };
            for line in text.split_inclusive('\n') {
                // str.split(): Unicode whitespace and the ASCII separators \x1c-\x1f
                let words: Vec<&str> = line
                    .split(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
                    .filter(|w| !w.is_empty())
                    .take_while(|w| !w.starts_with('#'))
                    .collect();
                if let [ty, suffixes @ ..] = &words[..] {
                    for suf in suffixes {
                        db.add(ty, &format!(".{suf}"), true);
                    }
                }
            }
        }
        db
    })
}

/// `mimetypes.guess_type(path)[0]` (Starlette's FileResponse)
pub fn guess_type(name: &str) -> R<Option<String>> {
    Ok(guess(name, true, false)?.0)
}

/// `MimeTypes.guess_type(url, strict)` (`file`: `guess_file_type(path, strict=)` of 3.13+): a URL with a scheme
/// of two characters or more uses its path (`data:` its media type), anything else is a file path
fn guess(url: &str, strict: bool, file: bool) -> R<(Option<String>, Option<String>)> {
    let path = if file {
        url.to_string()
    } else {
        let p = super::stdlib::url_split(url, true)?;
        if p.scheme.len() <= 1 {
            url.to_string()
        } else if p.scheme == "data" {
            let u = &p.path;
            let Some(comma) = u.find(',') else { return Ok((None, None)) };
            let ty = match u[..comma].find(';') {
                Some(semi) => &u[..semi],
                None => &u[..comma],
            };
            let ty = if ty.contains('=') || !ty.contains('/') { "text/plain" } else { ty };
            return Ok((Some(ty.to_string()), None));
        } else {
            p.path
        }
    };
    let (_, suffixes, encodings) = super::mimetab::tables(super::python());
    let find = |t: &'static [(&'static str, &'static str)], k: &str| t.binary_search_by(|(x, _)| (*x).cmp(k)).ok().map(|i| t[i].1);
    let (mut base, mut ext) = splitext(&path);
    while let Some(to) = find(suffixes, &py_lower(&ext)) {
        (base, ext) = splitext(&format!("{base}{to}"));
    }
    // encodings_map is case sensitive
    let encoding = find(encodings, &ext).map(str::to_string);
    if encoding.is_some() {
        (_, ext) = splitext(&base);
    }
    let ext = py_lower(&ext);
    let db = mime_db();
    let ty = db.types[0].get(&ext).or_else(|| if strict { None } else { db.types[1].get(&ext) });
    Ok((ty.cloned(), encoding))
}

/// `str.lower()` (full Unicode case mapping, like CPython)
fn py_lower(s: &str) -> String {
    s.to_lowercase()
}

fn strict_arg(name: &str, args: &[V], kwargs: &[(String, V)], max: usize, kw_only: bool) -> R<bool> {
    let fname = format!("{name}()");
    if args.len() > max {
        let takes = if max == 1 { "1 positional argument".to_string() } else { format!("from 1 to {max} positional arguments") };
        return Err(Exc::type_error(format!("{fname} takes {takes} but {} were given", args.len())));
    }
    let mut strict = if kw_only { None } else { args.get(1).cloned() };
    for (k, v) in kwargs {
        if k != "strict" {
            return Err(Exc::type_error(format!("{fname} got an unexpected keyword argument '{k}'")));
        }
        if strict.is_some() {
            return Err(Exc::type_error(format!("{fname} got multiple values for argument 'strict'")));
        }
        strict = Some(v.clone());
    }
    strict.map(|v| ops::truthy(&v)).unwrap_or(Ok(true))
}

/// `mimetypes.guess_type / guess_file_type / guess_extension / guess_all_extensions` (the module's functions,
/// on the database `init()` builds)
pub fn mimetypes(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let first = |what: &str| {
        args.first().cloned().ok_or_else(|| Exc::type_error(format!("{name}() missing 1 required positional argument: '{what}'")))
    };
    let opt = |v: Option<String>| v.map(V::str).unwrap_or(V::None);
    match name {
        "guess_type" | "guess_file_type" => {
            let file = name == "guess_file_type";
            let strict = strict_arg(name, args, kwargs, if file { 1 } else { 2 }, file)?;
            let v = first(if file { "path" } else { "url" })?;
            let (ty, enc) = match &v {
                // a bytes URL with a scheme: nothing matches its bytes (CPython compares them to str keys);
                // a bytes path: os.fsdecode from 3.13, never matched before
                V::Bytes(b) => {
                    // urlparse's _coerce_args decodes bytes as ASCII (strict) first
                    if let Some(p) = b.iter().position(|c| *c >= 0x80).filter(|_| !file) {
                        return Err(Exc::msg(
                            &UNICODE_DECODE_ERROR,
                            format!("'ascii' codec can't decode byte 0x{:02x} in position {p}: ordinal not in range(128)", b[p]),
                        ));
                    }
                    let text = String::from_utf8_lossy(b).into_owned();
                    let is_url = !file && super::stdlib::url_split(&text, true)?.scheme.len() > 1;
                    if is_url || super::python() < (3, 13) {
                        (None, None)
                    } else {
                        match std::str::from_utf8(b) {
                            Ok(s) => guess(s, strict, true)?,
                            Err(_) => return Err(Exc::type_error("py2axum: mimetypes: a non-UTF-8 bytes path is not supported")),
                        }
                    }
                }
                // a path: os.path.splitext (3.13+) and posixpath.splitext (before) are the same function here
                other => guess(&super::pathio::fspath(other)?, strict, file)?,
            };
            Ok(V::tuple(vec![opt(ty), opt(enc)]))
        }
        "guess_extension" | "guess_all_extensions" => {
            let strict = strict_arg(name, args, kwargs, 2, false)?;
            let ty = first("type")?;
            let V::Str(t) = &ty else {
                return Err(Exc::attr_error(format!("'{}' object has no attribute 'lower'", ty.type_name())));
            };
            let t = py_lower(t);
            let db = mime_db();
            let mut exts: Vec<String> = db.inv[0].get(&t).cloned().unwrap_or_default();
            if !strict {
                for e in db.inv[1].get(&t).into_iter().flatten() {
                    if !exts.contains(e) {
                        exts.push(e.clone());
                    }
                }
            }
            if name == "guess_extension" {
                Ok(opt(exts.into_iter().next()))
            } else {
                Ok(V::list(exts.into_iter().map(V::str).collect()))
            }
        }
        _ => Err(Exc::attr_error(format!("module 'mimetypes' has no attribute '{name}'"))),
    }
}

/// `posixpath.splitext`: the extension starts at the last dot of the last component, after its leading dots
fn splitext(p: &str) -> (String, String) {
    let name_start = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let name = &p[name_start..];
    let lead = name.len() - name.trim_start_matches('.').len();
    match name[lead..].rfind('.') {
        Some(i) => {
            let at = name_start + lead + i;
            (p[..at].to_string(), p[at..].to_string())
        }
        None => (p.to_string(), String::new()),
    }
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
        "status_code" => Ok(V::Int(*r.status.lock())),
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
            *r.status.lock() = *i;
            Ok(())
        }
        _ => Err(Exc::type_error("status_code must be an int")),
    }
}

/// uvicorn (httptools) has a status line for 100..599 only: any other status is a KeyError raised by the
/// server's `send` and the connection is dropped without an answer. The KeyError goes up through the
/// middlewares (their `finally` blocks run), then the server drops the connection (`is_drop`).
pub fn status_drop(status: i64) -> Exc {
    let e = Exc::new(&KEY_ERROR, vec![V::Int(status)]);
    e.0.attrs.lock().insert("__py2axum_drop__".into(), V::Bool(true));
    e
}

pub fn is_drop(e: &Exc) -> bool {
    e.0.attrs.lock().contains_key("__py2axum_drop__")
}

/// The returned response object, sent as Starlette would (a status as the application gave it: uvicorn has
/// a status line for 100..599 only).
pub fn into_response(r: &RespObj) -> R<Response> {
    let mut out = into_response_inner(r)?;
    out.extensions_mut().insert(RespClass(r.cls));
    Ok(out)
}

fn into_response_inner(r: &RespObj) -> R<Response> {
    let status = *r.status.lock();
    if !(100..=599).contains(&status) {
        return Err(status_drop(status));
    }
    let status = status as u16;
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
            // Starlette's order: the endpoint's headers, then content-type, accept-ranges and content-disposition
            // (FileResponse.__init__) and the stat headers (set_stat_headers), each only when the endpoint did not
            // set it (`setdefault`): an endpoint's own Content-Disposition replaces the one `filename=` makes
            let mut h = std::mem::take(&mut headers);
            fn setdefault(h: &mut Vec<(String, String)>, k: &str, v: impl FnOnce() -> String) {
                if !h.iter().any(|(x, _)| x == k) {
                    h.push((k.to_string(), v()));
                }
            }
            setdefault(&mut h, "content-type", || with_charset(r.media.as_deref().unwrap_or("text/plain")));
            setdefault(&mut h, "accept-ranges", || "bytes".to_string());
            if let Some(f) = filename {
                let q = quote(f, "");
                setdefault(&mut h, "content-disposition", || if &q != f { format!("{disposition}; filename*=utf-8''{q}") } else { format!("{disposition}; filename=\"{f}\"") });
            }
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).unwrap_or_default();
            let st_mtime = mtime.as_secs() as f64 + mtime.subsec_nanos() as f64 * 1e-9;
            setdefault(&mut h, "content-length", || data.len().to_string());
            let lm = chrono::DateTime::from_timestamp(mtime.as_secs() as i64, 0).unwrap_or_default();
            setdefault(&mut h, "last-modified", || lm.format("%a, %d %b %Y %H:%M:%S GMT").to_string());
            setdefault(&mut h, "etag", || {
                use md5::Digest;
                let mut m = md5::Md5::new();
                m.update(format!("{}-{}", ops::float_repr(st_mtime), data.len()).as_bytes());
                format!("\"{:x}\"", m.finalize())
            });
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
            super::uvicorn::exception(&e);
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
