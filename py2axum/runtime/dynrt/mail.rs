//! E-mail: Jinja2 templates (minijinja, with Jinja2/markupsafe output), `email.mime` messages,
//! `email.utils.formataddr/make_msgid`, and `aiosmtplib.send` (lettre).
use std::sync::Arc;

use base64::Engine;
use parking_lot::Mutex;

use super::ops;
use super::v::*;

// ---------------------------------------------------------------- Jinja2

pub struct Jinja {
    env: minijinja::Environment<'static>,
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
    Ok(V::native(Native::Jinja(Arc::new(Jinja { env }))))
}

pub fn jinja_method(j: &Arc<Jinja>, name: &str, args: &[V]) -> R {
    match name {
        "get_template" => {
            let n = ops::str_(args.first().ok_or_else(|| Exc::type_error("get_template() missing 'name'"))?)?;
            j.env.get_template(&n).map_err(|e| Exc::msg(&TEMPLATE_NOT_FOUND, if e.kind() == minijinja::ErrorKind::TemplateNotFound { n.clone() } else { e.to_string() }))?;
            Ok(V::native(Native::JinjaTpl(JinjaTpl { env: j.clone(), name: n })))
        }
        _ => Err(Exc::attr_error(format!("'Environment' object has no attribute '{name}'"))),
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
        V::Date(_) | V::DateTime(_) | V::Time(_) | V::Delta(_) | V::Enum(..) => Value::from(ops::str_(v).unwrap_or_default()),
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
            let tpl = t.env.env.get_template(&t.name).map_err(|e| Exc::runtime(e.to_string()))?;
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
}

pub struct Mime {
    kind: MimeKind,
    headers: Mutex<Vec<(String, String)>>,
    parts: Mutex<Vec<V>>,
}

fn mime(kind: MimeKind, headers: Vec<(String, String)>) -> V {
    V::native(Native::Mime(Arc::new(Mime { kind, headers: Mutex::new(headers), parts: Mutex::new(Vec::new()) })))
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
    match kind {
        "MIMEMultipart" => {
            let sub = argv(args, kwargs, 0, "_subtype").map(ops::str_).transpose()?.unwrap_or_else(|| "mixed".into());
            Ok(mime(MimeKind::Multipart(sub.clone()), vec![("Content-Type".into(), format!("multipart/{sub}")), ("MIME-Version".into(), "1.0".into())]))
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
                    ("Content-Type".into(), format!("application/{sub}")),
                    ("MIME-Version".into(), "1.0".into()),
                    ("Content-Transfer-Encoding".into(), "base64".into()),
                ],
            ))
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
        "as_string" | "as_bytes" => {
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
/// timeout=, validate_certs=)`: envelope from Sender/From and To/Cc/Bcc (Bcc removed), like aiosmtplib.
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
    let allowed = ["hostname", "port", "username", "password", "use_tls", "start_tls", "timeout", "validate_certs"];
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
    let from = get_header(&msg, "Sender").or_else(|| get_header(&msg, "From")).ok_or_else(|| smtp_err("No From header".into()))?;
    let mut rcpts = Vec::new();
    for h in ["To", "Cc", "Bcc"] {
        for (k, v) in msg.headers.lock().iter() {
            if k.eq_ignore_ascii_case(h) {
                rcpts.extend(addresses(v));
            }
        }
    }
    msg.headers.lock().retain(|(k, _)| !k.eq_ignore_ascii_case("Bcc"));
    let from = addresses(&from).into_iter().next().ok_or_else(|| smtp_err("No valid sender".into()))?;
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
