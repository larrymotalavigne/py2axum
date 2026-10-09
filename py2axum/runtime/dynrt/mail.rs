//! E-mail: Jinja2 templates (minijinja, with Jinja2/markupsafe output), `email.mime` messages,
//! `email.utils.formataddr/make_msgid`, and `aiosmtplib.send` (lettre).
use std::sync::Arc;

use base64::Engine;
use parking_lot::Mutex;

use super::ops;
use super::v::*;

// ---------------------------------------------------------------- Jinja2

pub struct Jinja {
    /// written by `env.filters[name] = f` (module level, before any render)
    env: parking_lot::RwLock<minijinja::Environment<'static>>,
    /// the FileSystemLoader's search path (TemplateNotFound's message)
    search: Option<String>,
    /// a Starlette `Jinja2Templates` (its env), not a bare `jinja2.Environment`
    pub templates: bool,
}

pub struct JinjaTpl {
    env: Arc<Jinja>,
    name: String,
}

/// `jinja2.FileSystemLoader(path)` and `select_autoescape([...])` are plain markers until `Environment`.
pub fn fs_loader(path: &V) -> R {
    Ok(V::tuple(vec![V::str("\u{0}loader"), V::str(super::pathio::fspath(path)?)]))
}

pub fn select_autoescape(exts: Option<&V>) -> R {
    let exts = match exts {
        None | Some(V::None) => vec![V::str("html"), V::str("htm"), V::str("xml")],
        Some(v) => ops::iter(v)?,
    };
    Ok(V::tuple(vec![V::str("\u{0}autoescape"), V::list(exts)]))
}

fn marker<'a>(v: &'a V, tag: &str) -> Option<&'a V> {
    match v {
        V::Tuple(t) if t.len() == 2 && matches!(&t[0], V::Str(s) if &**s == tag) => Some(&t[1]),
        _ => None,
    }
}

/// markupsafe.escape
fn markupsafe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out += "&amp;",
            '<' => out += "&lt;",
            '>' => out += "&gt;",
            '"' => out += "&#34;",
            '\'' => out += "&#39;",
            c => out.push(c),
        }
    }
    out
}

/// `jinja2.Environment(loader=FileSystemLoader(dir), autoescape=select_autoescape([...]) | bool)`
pub fn environment(kwargs: &[(String, V)]) -> R {
    Ok(V::native(Native::Jinja(Arc::new(Jinja { search: search_path(kwargs)?, env: parking_lot::RwLock::new(build_env(kwargs)?), templates: false }))))
}

fn build_env(kwargs: &[(String, V)]) -> R<minijinja::Environment<'static>> {
    let mut env = minijinja::Environment::new();
    env.set_keep_trailing_newline(false);
    for (k, v) in kwargs {
        match k.as_str() {
            "loader" => {
                let dir = marker(v, "\u{0}loader").ok_or_else(|| Exc::type_error("py2axum: Environment(loader=) must be FileSystemLoader(path)"))?;
                env.set_loader(minijinja::path_loader(ops::str_(dir)?));
            }
            "autoescape" => {
                let exts: Vec<String> = match (v, marker(v, "\u{0}autoescape")) {
                    (_, Some(list)) => ops::iter(list)?.iter().map(|e| ops::str_(e).map(|s| s.trim_start_matches('.').to_ascii_lowercase())).collect::<R<_>>()?,
                    (V::Bool(true), None) => vec!["*".into()],
                    (V::Bool(false), None) => vec![],
                    _ => return Err(Exc::type_error("py2axum: Environment(autoescape=) must be a bool or select_autoescape([...])")),
                };
                env.set_auto_escape_callback(move |name: &str| {
                    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
                    if exts.iter().any(|e| e == "*" || *e == ext) {
                        minijinja::AutoEscape::Html
                    } else {
                        minijinja::AutoEscape::None
                    }
                });
            }
            other => return Err(Exc::type_error(format!("py2axum: jinja2.Environment({other}=) is not supported"))),
        }
    }
    // Jinja2's escaping (markupsafe) and value formatting
    env.set_formatter(|out, state, value| {
        let s = if value.is_undefined() { String::new() } else { value.to_string() };
        if state.auto_escape() != minijinja::AutoEscape::None && !value.is_safe() {
            out.write_str(&markupsafe(&s))?;
        } else {
            out.write_str(&s)?;
        }
        Ok(())
    });
    Ok(env)
}

/// `Jinja2Templates(directory=...)`: `Environment(loader=FileSystemLoader(directory), autoescape=select_autoescape())`
pub fn templates(args: &[V], kwargs: &[(String, V)]) -> R {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "directory") {
        return Err(Exc::type_error(format!("py2axum: Jinja2Templates({k}=) is not supported")));
    }
    let dir = args.first().or_else(|| kwargs.first().map(|(_, v)| v)).filter(|v| !v.is_none())
        .ok_or_else(|| Exc::type_error("py2axum: Jinja2Templates() needs directory="))?;
    let kw = [("loader".into(), fs_loader(dir)?), ("autoescape".into(), select_autoescape(None)?)];
    Ok(V::native(Native::Jinja(Arc::new(Jinja { search: search_path(&kw)?, env: parking_lot::RwLock::new(build_env(&kw)?), templates: true }))))
}

fn search_path(kwargs: &[(String, V)]) -> R<Option<String>> {
    match kwargs.iter().find(|(k, _)| k == "loader").and_then(|(_, v)| marker(v, "\u{0}loader")) {
        Some(d) => Ok(Some(ops::str_(d)?)),
        None => Ok(None),
    }
}

/// `jinja2.TemplateNotFound` as FileSystemLoader raises it
fn not_found(j: &Jinja, name: &str, e: minijinja::Error) -> Exc {
    if e.kind() != minijinja::ErrorKind::TemplateNotFound {
        return Exc::msg(&TEMPLATE_NOT_FOUND, e.to_string());
    }
    match &j.search {
        Some(p) => Exc::msg(&TEMPLATE_NOT_FOUND, format!("{} not found in search path: {}", ops::str_repr(name), ops::str_repr(p))),
        None => Exc::msg(&TEMPLATE_NOT_FOUND, name.to_string()),
    }
}

fn render_template(j: &Jinja, name: &str, ctx: Vec<(String, minijinja::Value)>) -> R<String> {
    let env = j.env.read();
    let tpl = env.get_template(name).map_err(|e| not_found(j, name, e))?;
    tpl.render(minijinja::Value::from_iter(ctx)).map_err(|e| Exc::runtime(format!("jinja2: {e}")))
}

/// `templates.TemplateResponse(request, name, context=None, status_code=200, headers=None, media_type=None)`
fn template_response(j: &Jinja, args: &[V], kwargs: &[(String, V)]) -> R {
    const NAMES: [&str; 6] = ["request", "name", "context", "status_code", "headers", "media_type"];
    let mut slots: Vec<Option<V>> = vec![None; NAMES.len()];
    if args.len() > NAMES.len() {
        return Err(Exc::type_error("TemplateResponse() takes at most 6 positional arguments"));
    }
    for (i, a) in args.iter().enumerate() {
        slots[i] = Some(a.clone());
    }
    for (k, v) in kwargs {
        let i = NAMES.iter().position(|n| n == k).ok_or_else(|| Exc::type_error(format!("py2axum: TemplateResponse({k}=) is not supported")))?;
        if slots[i].replace(v.clone()).is_some() {
            return Err(Exc::type_error(format!("TemplateResponse() got multiple values for argument '{k}'")));
        }
    }
    let request = slots[0].clone().ok_or_else(|| Exc::type_error("TemplateResponse() missing 1 required positional argument: 'request'"))?;
    let name = ops::str_(slots[1].as_ref().ok_or_else(|| Exc::type_error("TemplateResponse() missing 1 required positional argument: 'name'"))?)?;
    let mut ctx: Vec<(String, minijinja::Value)> = Vec::new();
    match &slots[2] {
        None | Some(V::None) => {}
        Some(V::Dict(d)) => {
            for (k, v) in d.lock().values() {
                ctx.push((ops::str_(k)?, to_jinja(v)));
            }
        }
        Some(o) => return Err(Exc::type_error(format!("py2axum: TemplateResponse(context=) must be a dict, not {}", o.type_name()))),
    }
    if !ctx.iter().any(|(k, _)| k == "request") {
        ctx.push(("request".into(), to_jinja(&request)));
    }
    let content = render_template(j, &name, ctx)?;
    let rest: Vec<V> = slots[3..].iter().map(|v| v.clone().unwrap_or(V::None)).collect();
    super::resp::new("HTMLResponse", &[vec![V::str(content)], rest].concat(), &[])
}

pub fn jinja_method(j: &Arc<Jinja>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "TemplateResponse" if j.templates => template_response(j, args, kwargs),
        "get_template" => {
            let n = ops::str_(args.first().ok_or_else(|| Exc::type_error("get_template() missing 'name'"))?)?;
            j.env.read().get_template(&n).map_err(|e| not_found(j, &n, e))?;
            Ok(V::native(Native::JinjaTpl(JinjaTpl { env: j.clone(), name: n })))
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if j.templates { "Jinja2Templates" } else { "Environment" }))),
    }
}

/// A runtime value seen from a template (attributes read synchronously).
struct Obj(V);

impl std::fmt::Debug for Obj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.type_name())
    }
}

impl minijinja::value::Object for Obj {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let name = key.as_str()?;
        let v = match &self.0 {
            V::Inst(i) => i.field(name)?,
            V::Obj(o) => o.get_attr_sync(name).ok()?,
            _ => return None,
        };
        Some(to_jinja(&v))
    }
    fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&ops::str_(&self.0).unwrap_or_default())
    }
    /// dates: `strftime`/`isoformat`... (synchronous methods only)
    fn call_method(self: &Arc<Self>, _: &minijinja::State, method: &str, args: &[minijinja::Value]) -> Result<minijinja::Value, minijinja::Error> {
        let err = |m: String| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, m);
        if !matches!(self.0, V::Date(_) | V::DateTime(_)) {
            return Err(minijinja::Error::from(minijinja::ErrorKind::UnknownMethod));
        }
        let args: Vec<V> = args.iter().map(from_jinja).collect::<Result<_, _>>().map_err(err)?;
        super::methods::value_method_sync(&self.0, method, &args).map(|v| to_jinja(&v)).map_err(|e| err(e.message()))
    }
}

/// `env.filters[name] = f`: a filter calling the translated function, synchronously like Jinja2 (a
/// function that awaits cannot run in the middle of a render).
pub fn add_filter(j: &Arc<Jinja>, name: &V, f: V) -> R<()> {
    let name = ops::str_(name)?;
    let fname = name.clone();
    j.env.write().add_filter(name, move |value: minijinja::Value, rest: minijinja::value::Rest<minijinja::Value>| {
        let err = |m: String| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, m);
        let mut args = vec![from_jinja(&value).map_err(err)?];
        for a in rest.iter() {
            args.push(from_jinja(a).map_err(err)?);
        }
        let cx = super::root_cx();
        match futures_util::FutureExt::now_or_never(super::methods::call_value(&cx, &f, args, vec![])) {
            Some(Ok(v)) => Ok(to_jinja(&v)),
            Some(Err(e)) => Err(err(e.message())),
            None => Err(err(format!("py2axum: the Jinja2 filter {fname} awaited during a render"))),
        }
    });
    Ok(())
}

fn from_jinja(v: &minijinja::Value) -> Result<V, String> {
    use minijinja::value::ValueKind;
    if let Some(o) = v.downcast_object_ref::<Obj>() {
        return Ok(o.0.clone());
    }
    Ok(match v.kind() {
        ValueKind::Undefined | ValueKind::None => V::None,
        ValueKind::Bool => V::Bool(v.is_true()),
        ValueKind::Number => match i64::try_from(v.clone()) {
            Ok(i) => V::Int(i),
            Err(_) => V::Float(f64::try_from(v.clone()).map_err(|e| e.to_string())?),
        },
        ValueKind::String => V::str(v.as_str().unwrap_or_default()),
        _ => return Err("py2axum: only str, number, bool and None arguments are passed to a method from a template".into()),
    })
}

fn to_jinja(v: &V) -> minijinja::Value {
    use minijinja::Value;
    match v {
        V::None => Value::from(()),
        V::Bool(b) => Value::from(*b),
        V::Int(i) => Value::from(*i),
        V::Float(f) => Value::from(*f),
        V::Str(s) => Value::from(s.to_string()),
        V::List(l) => Value::from(l.lock().iter().map(to_jinja).collect::<Vec<_>>()),
        V::Tuple(t) => Value::from(t.iter().map(to_jinja).collect::<Vec<_>>()),
        V::Dict(d) => Value::from_iter(d.lock().values().map(|(k, x)| (ops::str_(k).unwrap_or_default(), to_jinja(x)))),
        // dates, enums, decimals...: rendered as str() in Python
        V::Time(_) | V::Delta(_) | V::Enum(..) => Value::from(ops::str_(v).unwrap_or_default()),
        // dates keep their methods (`{{ d.strftime('%Y') }}`), rendered as str()
        other => Value::from_object(Obj(other.clone())),
    }
}

pub fn tpl_method(t: &JinjaTpl, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "render" => {
            let mut ctx: Vec<(String, minijinja::Value)> = Vec::new();
            if let Some(V::Dict(d)) = args.first() {
                for (k, v) in d.lock().values() {
                    ctx.push((ops::str_(k)?, to_jinja(v)));
                }
            }
            for (k, v) in kwargs {
                ctx.push((k.clone(), to_jinja(v)));
            }
            let env = t.env.env.read();
            let tpl = env.get_template(&t.name).map_err(|e| Exc::runtime(e.to_string()))?;
            let ctx = minijinja::Value::from_iter(ctx);
            tpl.render(ctx).map(V::str).map_err(|e| Exc::runtime(format!("jinja2: {e}")))
        }
        _ => Err(Exc::attr_error(format!("'Template' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- email.mime

pub enum MimeKind {
    Multipart(String),
    Text { sub: String, charset: String, body: String },
    App { sub: String, data: Vec<u8> },
    /// `MIMEBase(maintype, subtype)`: its payload is set by `set_payload` (and `encoders.encode_base64`)
    Base,
}

pub struct Mime {
    kind: MimeKind,
    headers: Mutex<Vec<(String, String)>>,
    parts: Mutex<Vec<V>>,
    payload: Mutex<Option<V>>,
}

fn mime(kind: MimeKind, headers: Vec<(String, String)>) -> V {
    V::native(Native::Mime(Arc::new(Mime { kind, headers: Mutex::new(headers), parts: Mutex::new(Vec::new()), payload: Mutex::new(None) })))
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn argv<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kw(kwargs, name)).filter(|v| !matches!(v, V::None))
}

/// `MIMEMultipart(_subtype="mixed")`, `MIMEText(_text, _subtype="plain", _charset=None)`,
/// `MIMEApplication(_data, _subtype="octet-stream")`
pub fn mime_new(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    // `**_params` of the Content-Type (MIMEBase.add_header): every keyword not a named parameter
    let named: &[&str] = match kind {
        "MIMEMultipart" => &["_subtype"],
        "MIMEText" => &["_text", "_subtype", "_charset"],
        "MIMEApplication" => &["_data", "_subtype"],
        _ => &["_maintype", "_subtype"],
    };
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| matches!(k.as_str(), "policy" | "boundary" | "_subparts" | "_encoder") || (kind == "MIMEText" && !named.contains(&k.as_str()))) {
        return Err(Exc::type_error(format!("py2axum: {kind}({k}=) is not supported")));
    }
    let params = |mut ctype: String| -> R<String> {
        for (k, v) in kwargs.iter().filter(|(k, _)| !named.contains(&k.as_str())) {
            ctype += &format!("; {}", format_param(&k.replace('_', "-"), &ops::str_(v)?));
        }
        Ok(ctype)
    };
    match kind {
        "MIMEMultipart" => {
            let sub = argv(args, kwargs, 0, "_subtype").map(ops::str_).transpose()?.unwrap_or_else(|| "mixed".into());
            Ok(mime(MimeKind::Multipart(sub.clone()), vec![("Content-Type".into(), params(format!("multipart/{sub}"))?), ("MIME-Version".into(), "1.0".into())]))
        }
        "MIMEText" => {
            let body = ops::str_(argv(args, kwargs, 0, "_text").ok_or_else(|| Exc::type_error("MIMEText() missing '_text'"))?)?;
            let sub = argv(args, kwargs, 1, "_subtype").map(ops::str_).transpose()?.unwrap_or_else(|| "plain".into());
            let charset = match argv(args, kwargs, 2, "_charset") {
                Some(c) => ops::str_(c)?.to_ascii_lowercase(),
                None if body.is_ascii() => "us-ascii".into(),
                None => "utf-8".into(),
            };
            let cte = if charset == "us-ascii" { "7bit" } else { "base64" };
            Ok(mime(
                MimeKind::Text { sub: sub.clone(), charset: charset.clone(), body },
                vec![
                    ("Content-Type".into(), format!("text/{sub}; charset=\"{charset}\"")),
                    ("MIME-Version".into(), "1.0".into()),
                    ("Content-Transfer-Encoding".into(), cte.into()),
                ],
            ))
        }
        "MIMEApplication" => {
            let data = match argv(args, kwargs, 0, "_data") {
                Some(V::Bytes(b)) => b.to_vec(),
                Some(V::Str(s)) => s.as_bytes().to_vec(),
                _ => return Err(Exc::type_error("MIMEApplication() needs bytes")),
            };
            let sub = argv(args, kwargs, 1, "_subtype").map(ops::str_).transpose()?.unwrap_or_else(|| "octet-stream".into());
            Ok(mime(
                MimeKind::App { sub: sub.clone(), data },
                vec![
                    ("Content-Type".into(), params(format!("application/{sub}"))?),
                    ("MIME-Version".into(), "1.0".into()),
                    ("Content-Transfer-Encoding".into(), "base64".into()),
                ],
            ))
        }
        "MIMEBase" => {
            let main = ops::str_(argv(args, kwargs, 0, "_maintype").ok_or_else(|| Exc::type_error("MIMEBase.__init__() missing 2 required positional arguments: '_maintype' and '_subtype'"))?)?;
            let sub = ops::str_(argv(args, kwargs, 1, "_subtype").ok_or_else(|| Exc::type_error("MIMEBase.__init__() missing 1 required positional argument: '_subtype'"))?)?;
            Ok(mime(MimeKind::Base, vec![("Content-Type".into(), params(format!("{main}/{sub}"))?), ("MIME-Version".into(), "1.0".into())]))
        }
        _ => Err(Exc::type_error(format!("py2axum: email.mime {kind} is not supported"))),
    }
}

/// RFC 2047 encoded-word(s) for a non-ASCII header value (utf-8, base64), as compat32 writes them.
fn encode_header(v: &str) -> String {
    if v.is_ascii() {
        return v.to_string();
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in v.chars() {
        // 45 bytes of text -> 60 base64 chars: the word stays under 76 columns
        if chunk.len() + c.len_utf8() > 45 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(chunk);
    }
    // email.charset: utf-8 headers use the SHORTEST of base64 and Q (Q on a tie)
    words
        .iter()
        .map(|w| {
            let b = base64::engine::general_purpose::STANDARD.encode(w.as_bytes());
            let mut q = String::new();
            for byte in w.bytes() {
                match byte {
                    b' ' => q.push('_'),
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'!' | b'*' | b'+' | b'/' => q.push(byte as char),
                    _ => q += &format!("={byte:02X}"),
                }
            }
            if b.len() < q.len() { format!("=?utf-8?b?{b}?=") } else { format!("=?utf-8?q?{q}?=") }
        })
        .collect::<Vec<_>>()
        .join("\r\n ")
}

/// header params: `name="value"`, or RFC 2231 for non-ASCII values
fn format_param(k: &str, v: &str) -> String {
    if v.is_ascii() {
        format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("{k}*=utf-8''{}", super::resp::quote(v, ""))
    }
}

pub fn mime_method(m: &Arc<Mime>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "attach" => {
            if !matches!(m.kind, MimeKind::Multipart(_)) {
                return Err(Exc::type_error("Attach is not valid on a message with a non-multipart payload"));
            }
            m.parts.lock().push(args.first().cloned().ok_or_else(|| Exc::type_error("attach() missing 'payload'"))?);
            Ok(V::None)
        }
        "add_header" => {
            let hname = ops::str_(args.first().ok_or_else(|| Exc::type_error("add_header() missing '_name'"))?)?;
            let mut value = args.get(1).map(ops::str_).transpose()?.unwrap_or_default();
            for (k, v) in kwargs {
                value += &format!("; {}", format_param(&k.replace('_', "-"), &ops::str_(v)?));
            }
            m.headers.lock().push((hname, value));
            Ok(V::None)
        }
        "set_payload" if matches!(m.kind, MimeKind::Base) => {
            if args.len() > 1 || !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: set_payload(payload) only (no charset)"));
            }
            *m.payload.lock() = Some(args.first().cloned().ok_or_else(|| Exc::type_error("set_payload() missing 1 required positional argument: 'payload'"))?);
            Ok(V::None)
        }
        "as_string" | "as_bytes" => {
            // unixfrom=, maxheaderlen=, policy=: refused, never ignored
            if !args.is_empty() || !kwargs.is_empty() {
                return Err(Exc::type_error(format!("py2axum: Message.{name}() with arguments is not supported")));
            }
            let s = render(m);
            Ok(if name == "as_bytes" { V::Bytes(Arc::from(s.into_bytes())) } else { V::str(s) })
        }
        "get" => {
            let k = ops::str_(args.first().ok_or_else(|| Exc::type_error("get() missing 'name'"))?)?;
            Ok(get_header(m, &k).map(V::str).or_else(|| args.get(1).cloned()).unwrap_or(V::None))
        }
        _ => Err(Exc::attr_error(format!("'Message' object has no attribute '{name}'"))),
    }
}

fn get_header(m: &Mime, k: &str) -> Option<String> {
    m.headers.lock().iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone())
}

/// `msg["Subject"] = ...` appends (email.message.Message semantics)
pub fn mime_setitem(m: &Mime, k: &V, v: &V) -> R<()> {
    m.headers.lock().push((ops::str_(k)?, ops::str_(v)?));
    Ok(())
}

pub fn mime_getitem(m: &Mime, k: &V) -> R {
    Ok(get_header(m, &ops::str_(k)?).map(V::str).unwrap_or(V::None))
}

fn b64_lines(data: &[u8]) -> String {
    let enc = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::new();
    for chunk in enc.as_bytes().chunks(76) {
        out += std::str::from_utf8(chunk).unwrap();
        out += "\n";
    }
    out
}

fn render(m: &Arc<Mime>) -> String {
    render_with(m, false)
}

/// compat32 folding of a header line longer than 78 characters, at its "; " parameter separators
fn fold(line: String) -> String {
    if line.len() <= 78 || !line.contains("; ") {
        return line;
    }
    let mut out = String::new();
    let mut cur = String::new();
    for (i, piece) in line.split("; ").enumerate() {
        let add = if i == 0 { piece.to_string() } else { format!("; {piece}") };
        if !cur.is_empty() && cur.len() + add.len() > 78 {
            out += &cur;
            out += ";\n";
            cur = format!(" {}", add.trim_start_matches("; "));
        } else {
            cur += &add;
        }
    }
    out + &cur
}

fn render_with(m: &Arc<Mime>, folded: bool) -> String {
    let boundary = match &m.kind {
        MimeKind::Multipart(_) => Some(format!("==============={:019}==", rand_u64() % 10_000_000_000_000_000_000)),
        _ => None,
    };
    let mut out = String::new();
    for (k, v) in m.headers.lock().iter() {
        let v = match (&boundary, k.eq_ignore_ascii_case("content-type")) {
            (Some(b), true) => format!("{v}; boundary=\"{b}\""),
            _ => encode_header(v),
        };
        let line = format!("{k}: {v}");
        out += &if folded { fold(line) } else { line };
        out += "\n";
    }
    out += "\n";
    match &m.kind {
        MimeKind::Multipart(_) => {
            // email.generator: "--b\n" part, then "\n--b\n" part..., then "\n--b--\n"
            let b = boundary.unwrap();
            let parts: Vec<String> = m
                .parts
                .lock()
                .iter()
                .filter_map(|p| match p {
                    V::Native(n) => match &**n {
                        Native::Mime(sub) => Some(render_with(sub, folded)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                out += &format!("\n--{b}\n\n--{b}--\n");
            } else {
                for (i, p) in parts.iter().enumerate() {
                    out += &format!("{}--{b}\n{p}", if i == 0 { "" } else { "\n" });
                }
                out += &format!("\n--{b}--\n");
            }
        }
        MimeKind::Text { charset, body, .. } if charset == "us-ascii" => out += body,
        MimeKind::Text { body, .. } => out += &b64_lines(body.as_bytes()),
        MimeKind::App { data, .. } => out += &b64_lines(data),
        MimeKind::Base => match &*m.payload.lock() {
            Some(V::Str(s)) => out += s,
            Some(V::Bytes(b)) => out += &String::from_utf8_lossy(b),
            _ => {}
        },
    }
    out
}

fn rand_u64() -> u64 {
    use rand::RngCore;
    rand::thread_rng().next_u64()
}

/// `email.utils.formataddr((name, addr))`
pub fn formataddr(pair: &V) -> R {
    let items = ops::iter(pair)?;
    let (name, addr) = (items.first().cloned().unwrap_or(V::None), ops::str_(items.get(1).unwrap_or(&V::None))?);
    let name = if name.is_none() { String::new() } else { ops::str_(&name)? };
    if name.is_empty() {
        return Ok(V::str(addr));
    }
    if !name.is_ascii() {
        return Ok(V::str(format!("{} <{addr}>", encode_header(&name))));
    }
    let specials = "[]\\()<>@,:;\".";
    if name.chars().any(|c| specials.contains(c)) {
        let q = name.replace('\\', "\\\\").replace('"', "\\\"");
        return Ok(V::str(format!("\"{q}\" <{addr}>")));
    }
    Ok(V::str(format!("{name} <{addr}>")))
}

/// `email.utils.formatdate(timeval=None, localtime=False, usegmt=False)`: RFC 2822 date, seconds truncated,
/// `-0000` for UTC unless `usegmt` (then `GMT`), the machine's offset with `localtime`
pub fn formatdate(args: &[V], kwargs: &[(String, V)]) -> R {
    use chrono::{Offset, TimeZone};
    let mut p = [args.first().cloned(), args.get(1).cloned(), args.get(2).cloned()];
    if args.len() > 3 {
        return Err(Exc::type_error(format!("formatdate() takes from 0 to 3 positional arguments but {} were given", args.len())));
    }
    for (k, v) in kwargs {
        let i = match k.as_str() {
            "timeval" => 0,
            "localtime" => 1,
            "usegmt" => 2,
            _ => return Err(Exc::type_error(format!("formatdate() got an unexpected keyword argument '{k}'"))),
        };
        p[i] = Some(v.clone());
    }
    let flag = |v: &Option<V>| -> R<bool> { v.as_ref().map(ops::truthy).transpose().map(|b| b.unwrap_or(false)) };
    let (local, gmt) = (flag(&p[1])?, flag(&p[2])?);
    let micros: i64 = match &p[0] {
        None | Some(V::None) => chrono::Utc::now().timestamp_micros(),
        Some(V::Int(i)) => i.checked_mul(1_000_000).ok_or_else(|| Exc::new(&OVERFLOW_ERROR, vec![V::str("timestamp out of range for platform time_t")]))?,
        // datetime.fromtimestamp rounds to the microsecond, half to even
        Some(V::Float(f)) => (f * 1e6).round_ties_even() as i64,
        Some(o) => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    };
    let utc = chrono::DateTime::from_timestamp_micros(micros).ok_or_else(|| Exc::value_error("year is out of range"))?;
    let (wall, zone) = if local {
        let off = chrono::Local.offset_from_utc_datetime(&utc.naive_utc()).fix();
        let secs = off.local_minus_utc();
        let (sign, a) = if secs < 0 { ('-', -secs) } else { ('+', secs) };
        (utc.naive_utc() + chrono::TimeDelta::seconds(secs as i64), format!("{sign}{:02}{:02}", a / 3600, a % 3600 / 60))
    } else {
        (utc.naive_utc(), if gmt { "GMT".to_string() } else { "-0000".to_string() })
    };
    Ok(V::str(format!("{} {zone}", wall.format("%a, %d %b %Y %H:%M:%S"))))
}

/// `email.utils.make_msgid(domain=...)`
pub fn make_msgid(domain: Option<&V>) -> R {
    let d = match domain {
        Some(v) if !v.is_none() => ops::str_(v)?,
        _ => hostname(),
    };
    let t = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() / 100;
    Ok(V::str(format!("<{t}.{}.{}@{d}>", std::process::id(), rand_u64() % 1_000_000_000_000_000_000)))
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into())
}

// ---------------------------------------------------------------- aiosmtplib.send

fn addresses(field: &str) -> Vec<String> {
    field
        .split(',')
        .filter_map(|a| {
            let a = a.trim();
            let addr = match (a.rfind('<'), a.rfind('>')) {
                (Some(i), Some(j)) if j > i => &a[i + 1..j],
                _ => a,
            };
            if addr.contains('@') { Some(addr.to_string()) } else { None }
        })
        .collect()
}

/// `await aiosmtplib.send(message, hostname=, port=, username=, password=, use_tls=, start_tls=,
/// timeout=, validate_certs=, sender=, recipients=)`: envelope from `sender=`/`recipients=` when given, else
/// from Sender/From and To/Cc/Bcc; Bcc removed either way, like aiosmtplib.
pub async fn smtp_send(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::transport::smtp::client::{Tls, TlsParameters};
    use lettre::AsyncTransport;
    let msg = match args.first() {
        Some(V::Native(n)) => match &**n {
            Native::Mime(m) => m.clone(),
            _ => return Err(Exc::type_error("py2axum: aiosmtplib.send() needs an email.mime message")),
        },
        _ => return Err(Exc::type_error("py2axum: aiosmtplib.send() needs an email.mime message")),
    };
    let allowed = ["hostname", "port", "username", "password", "use_tls", "start_tls", "timeout", "validate_certs", "sender", "recipients"];
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("py2axum: aiosmtplib.send({k}=) is not supported")));
    }
    let get = |k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).filter(|v| !v.is_none());
    let host = get("hostname").map(|v| ops::str_(&v)).transpose()?.unwrap_or_else(|| "localhost".into());
    let use_tls = get("use_tls").map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false);
    let port = match get("port") {
        Some(V::Int(p)) => p as u16,
        _ => if use_tls { 465 } else { 25 },
    };
    let validate = get("validate_certs").map(|v| ops::truthy(&v)).transpose()?.unwrap_or(true);
    let start_tls = get("start_tls").map(|v| ops::truthy(&v)).transpose()?;
    let timeout = match get("timeout") {
        Some(V::Int(t)) => Some(std::time::Duration::from_secs(t as u64)),
        Some(V::Float(t)) => Some(std::time::Duration::from_secs_f64(t)),
        _ => Some(std::time::Duration::from_secs(60)),
    };
    let smtp_err = |e: String| Exc::msg(&SMTP_EXCEPTION, e);
    let tls_params = TlsParameters::builder(host.clone()).dangerous_accept_invalid_certs(!validate).build().map_err(|e| smtp_err(e.to_string()))?;
    let tls = if use_tls {
        Tls::Wrapper(tls_params)
    } else {
        match start_tls {
            Some(true) => Tls::Required(tls_params),
            Some(false) => Tls::None,
            None => Tls::Opportunistic(tls_params),
        }
    };
    let mut builder = lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::builder_dangerous(&host).port(port).tls(tls).timeout(timeout);
    if let (Some(u), Some(p)) = (get("username"), get("password")) {
        builder = builder.credentials(Credentials::new(ops::str_(&u)?, ops::str_(&p)?));
    }
    let transport = builder.build();
    let from = match get("sender") {
        Some(s) => ops::str_(&s)?,
        None => {
            let h = get_header(&msg, "Sender").or_else(|| get_header(&msg, "From")).ok_or_else(|| smtp_err("No From header".into()))?;
            addresses(&h).into_iter().next().ok_or_else(|| smtp_err("No valid sender".into()))?
        }
    };
    let mut rcpts = Vec::new();
    match get("recipients") {
        Some(V::Str(r)) => rcpts.push(r.to_string()),
        Some(r) => {
            for x in ops::iter(&r)? {
                rcpts.push(ops::str_(&x)?);
            }
        }
        None => {
            for h in ["To", "Cc", "Bcc"] {
                for (k, v) in msg.headers.lock().iter() {
                    if k.eq_ignore_ascii_case(h) {
                        rcpts.extend(addresses(v));
                    }
                }
            }
        }
    }
    msg.headers.lock().retain(|(k, _)| !k.eq_ignore_ascii_case("Bcc"));
    let parse = |a: &str| a.parse::<lettre::Address>().map_err(|e| smtp_err(e.to_string()));
    let envelope = lettre::address::Envelope::new(Some(parse(&from)?), rcpts.iter().map(|a| parse(a)).collect::<R<Vec<_>>>()?)
        .map_err(|e| smtp_err(e.to_string()))?;
    // aiosmtplib flattens with the SMTP policy: headers folded at 78 columns, CRLF
    let raw = render_with(&msg, true).replace('\n', "\r\n");
    // lettre ends the DATA with CRLF "." CRLF itself
    let raw = raw.strip_suffix("\r\n").unwrap_or(&raw).to_string();
    let resp = transport.send_raw(&envelope, raw.as_bytes()).await.map_err(|e| smtp_err(e.to_string()))?;
    // aiosmtplib returns ({recipient: response}, message): enough for callers that ignore it
    Ok(V::tuple(vec![V::empty_dict(), V::str(resp.message().collect::<Vec<_>>().join("\n"))]))
}

/// `email.encoders.encode_base64(msg)`: the payload base64-encoded (76-column lines), and its header
pub fn encode_base64(msg: &V) -> R {
    let m = match msg {
        V::Native(n) => match &**n {
            Native::Mime(m) => m.clone(),
            _ => return Err(Exc::type_error("py2axum: encode_base64() of a non-message")),
        },
        _ => return Err(Exc::type_error("py2axum: encode_base64() of a non-message")),
    };
    let data = match &*m.payload.lock() {
        Some(V::Bytes(b)) => b.to_vec(),
        // CPython: get_payload(decode=True) is None, base64.encodebytes(None) raises
        None => return Err(Exc::type_error("expected bytes-like object, not NoneType")),
        Some(_) => return Err(Exc::type_error("py2axum: encode_base64() of a str payload is not supported")),
    };
    *m.payload.lock() = Some(V::str(b64_lines(&data)));
    m.headers.lock().push(("Content-Transfer-Encoding".into(), "base64".into()));
    Ok(V::None)
}
