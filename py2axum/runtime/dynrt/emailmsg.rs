//! `email.message.EmailMessage` under the modern policies (`email.policy.default`, `SMTP`, `SMTPUTF8`, `HTTP`):
//! headers set and read (unstructured and address headers, parsed and folded by `emailhdr`), `set_content(str)`
//! (the text handler of `email.contentmanager`: its choice of Content-Transfer-Encoding), `as_bytes()`,
//! `as_string()`, `str()` and `bytes()` (the BytesGenerator and Generator of `email.generator`). The headers
//! with a structured value of their own (Date, Message-ID, Content-*, MIME-Version...) and every other method
//! are refused.
use std::sync::Arc;

use base64::Engine;
use parking_lot::Mutex;

use super::emailhdr::{self as h, Policy, Tok, C};
use super::ops;
use super::v::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pol {
    Default,
    Smtp,
    SmtpUtf8,
    Http,
}

impl Pol {
    fn policy(self, utf8: bool) -> Policy {
        match self {
            Pol::Default => Policy { linesep: "\n", max_line_length: Some(78), utf8 },
            Pol::Smtp => Policy { linesep: "\r\n", max_line_length: Some(78), utf8 },
            Pol::SmtpUtf8 => Policy { linesep: "\r\n", max_line_length: Some(78), utf8: true },
            Pol::Http => Policy { linesep: "\r\n", max_line_length: None, utf8 },
        }
    }
    pub fn from_marker(v: &V) -> R<Pol> {
        if let V::Native(n) = v {
            if let Native::ExtType(t) = &**n {
                match *t {
                    "email.policy.default" => return Ok(Pol::Default),
                    "email.policy.SMTP" => return Ok(Pol::Smtp),
                    "email.policy.SMTPUTF8" => return Ok(Pol::SmtpUtf8),
                    "email.policy.HTTP" => return Ok(Pol::Http),
                    _ => {}
                }
            }
        }
        Err(Exc::type_error("py2axum: EmailMessage(policy=) supports email.policy.default, SMTP, SMTPUTF8 and HTTP only"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Unstructured,
    Address,
    /// written by set_content (Content-Type, Content-Transfer-Encoding, MIME-Version): its value as is
    Literal,
}

#[derive(Clone)]
struct Hdr {
    name: String,
    tree: Tok,
    /// `str(header)`
    decoded: String,
}

#[derive(Clone)]
enum Payload {
    Text(String),
    /// `8bit` non-ASCII text (a str with surrogate escapes in CPython)
    Raw(Vec<u8>),
}

#[derive(Clone, Default)]
struct State {
    headers: Vec<Hdr>,
    payload: Option<Payload>,
}

pub struct EmailMsg {
    pol: Pol,
    st: Mutex<State>,
}

/// `EmailMessage(policy=None)`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: EmailMessage(policy) by keyword only"));
    }
    let mut pol = Pol::Default;
    for (k, v) in kwargs {
        match k.as_str() {
            "policy" if !v.is_none() => pol = Pol::from_marker(v)?,
            "policy" => {}
            _ => return Err(Exc::type_error(format!("MIMEPart.__init__() got an unexpected keyword argument '{k}'"))),
        }
    }
    Ok(V::native(Native::EmailMsg(Arc::new(EmailMsg { pol, st: Mutex::new(State::default()) }))))
}

const ADDRESS: &[&str] = &["to", "from", "cc", "bcc", "reply-to", "sender", "resent-to", "resent-cc", "resent-bcc", "resent-from", "resent-sender"];
const UNIQUE: &[&str] = &["subject", "to", "from", "cc", "bcc", "reply-to", "sender", "date", "orig-date", "mime-version", "content-type",
    "content-disposition", "content-transfer-encoding", "message-id"];
/// headers parsed by a class of their own in `email.headerregistry`
const REFUSED: &[&str] = &["date", "resent-date", "orig-date", "mime-version", "content-type", "content-disposition",
    "content-transfer-encoding", "message-id", "in-reply-to", "references"];

fn max_count(name: &str) -> Option<usize> {
    let l = name.to_lowercase();
    if UNIQUE.contains(&l.as_str()) || l == "in-reply-to" && super::python() >= (3, 13) || l == "references" && super::python() >= (3, 13) {
        Some(1)
    } else {
        None
    }
}

/// `str.splitlines()` boundaries
fn line_breaks(s: &str) -> usize {
    let mut n = 0;
    let mut it = s.chars().peekable();
    let mut cur = false;
    while let Some(c) = it.next() {
        cur = true;
        let brk = match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                true
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => true,
            _ => false,
        };
        if brk {
            n += 1;
            cur = false;
        }
    }
    n + usize::from(cur)
}

enum Val {
    Str(String),
    None,
    /// an address header set to a list (or a non-str value): `', '.join(str(Group(None, [item])))`, parsed again
    Joined(String),
}

fn header_value(name: &str, value: &V) -> R<Val> {
    let l = name.to_lowercase();
    let address = ADDRESS.contains(&l.as_str());
    Ok(match value {
        V::Str(s) => Val::Str(s.to_string()),
        V::None if address => Val::Joined("None".into()),
        V::None => Val::None,
        other if address => {
            let items = match other {
                V::List(_) | V::Tuple(_) => ops::iter(other)?,
                x => vec![x.clone()],
            };
            let mut parts = Vec::new();
            for it in items {
                match it {
                    V::Str(s) => parts.push(s.to_string()),
                    V::None => parts.push("None".into()),
                    _ => return Err(Exc::type_error("py2axum: an address header set to a value other than str (or a list of str) is not supported")),
                }
            }
            Val::Joined(parts.join(", "))
        }
        other => return Err(Exc::type_error(format!("py2axum: header {name} set to a {} is not supported (str only)", other.type_name()))),
    })
}

/// `policy.header_store_parse(name, value)`: the header factory's parse
fn store_parse(name: &str, value: &V) -> R<Hdr> {
    let v = h::ver();
    if v.check_name {
        let ok_name = {
            let n = name.strip_suffix('\n').unwrap_or(name);
            !n.is_empty() && n.bytes().all(|b| (0x21..=0x39).contains(&b) || (0x3b..=0x7e).contains(&b))
        };
        if !ok_name {
            return Err(Exc::value_error(format!("Header field name contains invalid characters: {}", ops::str_repr(name))));
        }
    }
    let l = name.to_lowercase();
    if REFUSED.contains(&l.as_str()) && !(super::python() < (3, 13) && (l == "in-reply-to" || l == "references")) {
        return Err(Exc::type_error(format!("py2axum: EmailMessage header {name} is not supported (unstructured and address headers only)")));
    }
    let val = header_value(name, value)?;
    if let Val::Str(t) = &val {
        if line_breaks(t) > 1 {
            return Err(Exc::value_error("Header values may not contain linefeed or carriage return characters"));
        }
    }
    if !ADDRESS.contains(&l.as_str()) {
        let tree = h::get_unstructured(match &val {
            Val::Str(t) => t,
            _ => "",
        })?;
        let decoded = tree.str_();
        return Ok(Hdr { name: name.to_string(), tree, decoded });
    }
    match val {
        Val::Str(t) => {
            let tree = h::get_address_list(&t)?;
            let decoded = address_decoded(&tree)?;
            tree.all_defects()?;
            Ok(Hdr { name: name.to_string(), tree, decoded })
        }
        Val::Joined(t) => {
            let tree = h::get_address_list(&t)?;
            Ok(Hdr { name: name.to_string(), tree, decoded: t })
        }
        Val::None => unreachable!(),
    }
}

/// `', '.join(str(Group(addr.display_name, [Address(mb.display_name or '', mb.local_part or '', mb.domain or '')])))`
fn address_decoded(tree: &Tok) -> R<String> {
    let mut groups = Vec::new();
    for addr in tree.addresses() {
        let gname = addr.display_name()?;
        let mut addrs = Vec::new();
        for mb in addr.all_mailboxes() {
            let (dn, lp, dom) = if mb.c == C::InvalidMailbox {
                (String::new(), String::new(), String::new())
            } else if mb.c == C::Group {
                // before 3.13.14/3.14.5, junk after a group turned the group itself into an "invalid-mailbox"
                mb.display_name()?;
                return Err(Exc::attr_error("'Group' object has no attribute 'local_part'"));
            } else {
                (mb.display_name()?.unwrap_or_default(), mb.local_part()?.unwrap_or_default(), mb.domain()?.unwrap_or_default())
            };
            addrs.push(address_str(&dn, &lp, &dom));
        }
        groups.push(match (&gname, addrs.len()) {
            (None, 1) => addrs.remove(0),
            _ => {
                let disp = match &gname {
                    Some(d) if h::has_special(d) => h::quote_string(d),
                    Some(d) => d.clone(),
                    None => "None".into(),
                };
                let list = addrs.join(", ");
                format!("{disp}:{}{list};", if list.is_empty() { "" } else { " " })
            }
        });
    }
    Ok(groups.join(", "))
}

/// `str(headerregistry.Address(display_name, username, domain))`
fn address_str(disp: &str, user: &str, domain: &str) -> String {
    let lp = if user.chars().any(h::dot_atom_end) { h::quote_string(user) } else { user.to_string() };
    let spec = if !domain.is_empty() {
        format!("{lp}@{domain}")
    } else if lp.is_empty() {
        "<>".into()
    } else {
        lp
    };
    let disp = if h::has_special(disp) { h::quote_string(disp) } else { disp.to_string() };
    if !disp.is_empty() {
        format!("{disp} <{}>", if spec == "<>" { "" } else { &spec })
    } else {
        spec
    }
}

fn literal(name: &str, value: &str) -> Hdr {
    Hdr { name: name.into(), tree: Tok { s: value.into(), ..h::tl(C::Literal, vec![]) }, decoded: value.into() }
}

impl EmailMsg {
    fn setitem(&self, name: &str, value: &V) -> R<()> {
        let mut st = self.st.lock();
        if let Some(maxc) = max_count(name) {
            let l = name.to_lowercase();
            let found = st.headers.iter().filter(|x| x.name.to_lowercase() == l).count();
            if found >= maxc {
                return Err(Exc::value_error(format!("There may be at most {maxc} {name} headers in a message")));
            }
        }
        drop(st);
        let hdr = store_parse(name, value)?;
        st = self.st.lock();
        st.headers.push(hdr);
        Ok(())
    }

    fn get(&self, name: &str) -> Option<String> {
        let l = name.to_lowercase();
        self.st.lock().headers.iter().find(|x| x.name.to_lowercase() == l).map(|x| x.decoded.clone())
    }

    fn contains(&self, name: &str) -> bool {
        let l = name.to_lowercase();
        self.st.lock().headers.iter().any(|x| x.name.to_lowercase() == l)
    }

    /// `set_content(str, subtype='plain', charset='utf-8', cte=None)` (contentmanager.set_text_content)
    fn set_content(&self, args: &[V], kwargs: &[(String, V)]) -> R<()> {
        let names = ["string", "subtype", "charset", "cte"];
        let mut vals: [Option<V>; 4] = Default::default();
        if args.len() > 4 {
            return Err(Exc::type_error("py2axum: set_content() takes the text, subtype, charset and cte only"));
        }
        for (i, a) in args.iter().enumerate() {
            vals[i] = Some(a.clone());
        }
        for (k, v) in kwargs {
            match names.iter().position(|n| n == k) {
                Some(i) if i > 0 && vals[i].is_none() => vals[i] = Some(v.clone()),
                _ => {
                    return Err(Exc::type_error(format!(
                        "py2axum: set_content({k}=) is not supported (subtype, charset and cte of a str only)"
                    )))
                }
            }
        }
        let text = match &vals[0] {
            Some(V::Str(s)) => s.to_string(),
            Some(other) => return Err(Exc::type_error(format!("py2axum: set_content() of a {} is not supported (str only)", other.type_name()))),
            None => return Err(Exc::type_error("set_content() missing 1 required positional argument: 'obj'")),
        };
        let subtype = match &vals[1] {
            None => "plain".to_string(),
            Some(V::Str(s)) if !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`{|}~".contains(&b)) => s.to_string(),
            Some(_) => return Err(Exc::type_error("py2axum: set_content(subtype=) must be a MIME token of at most 40 characters")),
        };
        let charset = match &vals[2] {
            None => "utf-8",
            Some(V::Str(s)) => match &**s {
                "utf-8" | "utf8" => "utf-8",
                "us-ascii" | "ascii" => "us-ascii",
                _ => return Err(Exc::type_error("py2axum: set_content(charset=) supports utf-8 and us-ascii only")),
            },
            Some(_) => return Err(Exc::type_error("py2axum: set_content(charset=) must be a str")),
        };
        let cte = match &vals[3] {
            None | Some(V::None) => None,
            Some(V::Str(s)) => Some(s.to_string()),
            Some(_) => return Err(Exc::type_error("py2axum: set_content(cte=) must be a str")),
        };
        // ContentManager.set_content: clear_content, then set_text_content
        {
            let mut st = self.st.lock();
            st.headers.retain(|x| !x.name.to_lowercase().starts_with("content-"));
            st.payload = None;
            st.headers.push(literal("Content-Type", &format!("text/{subtype}")));
        }
        let (cte, payload) = encode_text(&text, charset, cte.as_deref(), self.pol)?;
        let mut st = self.st.lock();
        st.payload = Some(payload);
        if let Some(ct) = st.headers.iter_mut().find(|x| x.name.eq_ignore_ascii_case("content-type")) {
            *ct = literal(&ct.name.clone(), &format!("text/{subtype}; charset=\"{charset}\""));
        }
        st.headers.push(literal("Content-Transfer-Encoding", &cte));
        if !st.headers.iter().any(|x| x.name.eq_ignore_ascii_case("mime-version")) {
            st.headers.push(literal("MIME-Version", "1.0"));
        }
        Ok(())
    }

    /// The flattened message: `bytes` (BytesGenerator) or text (Generator, utf8 for `str()`)
    fn generate(&self, binary: bool, pol: Pol, utf8: bool) -> R<Vec<u8>> {
        let v = h::ver();
        let st = self.st.lock().clone();
        let policy = pol.policy(utf8 || pol == Pol::SmtpUtf8);
        let mut headers = st.headers.clone();
        let mut body: Vec<u8> = Vec::new();
        match &st.payload {
            None => {}
            Some(Payload::Text(t)) => write_lines(&mut body, t.as_bytes(), policy.linesep),
            Some(Payload::Raw(b)) if binary => write_lines(&mut body, b, policy.linesep),
            Some(Payload::Raw(b)) => {
                // Generator._handle_text: the surrogate-escaped text is re-encoded with its charset's body
                // encoding (base64 for utf-8), and the Content-Transfer-Encoding header rewritten in place
                let enc = b64_body(b, 57);
                write_lines(&mut body, enc.as_bytes(), policy.linesep);
                if let Some(c) = headers.iter_mut().find(|x| x.name.eq_ignore_ascii_case("content-transfer-encoding")) {
                    *c = literal(&c.name.clone(), "base64");
                }
            }
        }
        let mut out: Vec<u8> = Vec::new();
        for hd in &headers {
            let folded = h::fold_header(&hd.name, &hd.tree, &policy)?;
            let ls = policy.linesep;
            // BytesGenerator: fold_binary encodes (ascii, or utf-8 under utf8=True) before the check, whose
            // message shows the bytes
            if binary && !policy.utf8 && !folded.is_ascii() {
                let (pos, c) = folded.chars().enumerate().find(|(_, c)| !c.is_ascii()).unwrap();
                return Err(Exc::msg(&UNICODE_ENCODE_ERROR, format!(
                    "'ascii' codec can't encode character {} in position {pos}: ordinal not in range(128)",
                    ops::str_repr(&c.to_string()))));
            }
            if !binary || v.verify_bytes {
                let shown = |f: &str| if binary { ops::bytes_repr(f.as_bytes()) } else { ops::str_repr(f) };
                if !folded.ends_with(ls) {
                    let l = if binary { ops::bytes_repr(ls.as_bytes()) } else { ops::str_repr(ls) };
                    return Err(Exc::msg(&HEADER_WRITE_ERROR, format!("folded header does not end with {l}: {}", shown(&folded))));
                }
                if newline_without_fwsp(&folded[..folded.len() - ls.len()]) {
                    return Err(Exc::msg(&HEADER_WRITE_ERROR, format!("folded header contains newline: {}", shown(&folded))));
                }
            }
            out.extend_from_slice(folded.as_bytes());
        }
        out.extend_from_slice(policy.linesep.as_bytes());
        out.extend(body);
        Ok(out)
    }

    pub fn as_bytes(&self, pol: Option<Pol>) -> R {
        let bytes = self.generate(true, pol.unwrap_or(self.pol), false)?;
        Ok(V::Bytes(Arc::from(bytes)))
    }

    pub fn as_string(&self, pol: Option<Pol>, utf8: bool) -> R {
        let bytes = self.generate(false, pol.unwrap_or(self.pol), utf8)?;
        Ok(V::str(String::from_utf8(bytes).map_err(|_| Exc::runtime("py2axum: non-UTF-8 message text"))?))
    }

    /// `str(msg)`: `as_string(policy=self.policy.clone(utf8=True))`
    pub fn str_(&self) -> R<String> {
        match self.as_string(None, true)? {
            V::Str(s) => Ok(s.to_string()),
            _ => unreachable!(),
        }
    }
}

/// generator.NEWLINE_WITHOUT_FWSP: `\r\n[^ \t]|\r[^ \n\t]|\n[^ \t]`
fn newline_without_fwsp(s: &str) -> bool {
    let b = s.as_bytes();
    for i in 0..b.len() {
        match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                if b.get(i + 2).is_some_and(|c| *c != b' ' && *c != b'\t') {
                    return true;
                }
            }
            b'\r' => {
                if b.get(i + 1).is_some_and(|c| *c != b' ' && *c != b'\n' && *c != b'\t') {
                    return true;
                }
            }
            b'\n' => {
                if b.get(i + 1).is_some_and(|c| *c != b' ' && *c != b'\t') {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Generator._write_lines: lines split on `\r\n|\r|\n`, written with the policy's line separator
fn write_lines(out: &mut Vec<u8>, data: &[u8], linesep: &str) {
    if data.is_empty() {
        return;
    }
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        let eol = match data[i] {
            b'\r' if data.get(i + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => 0,
        };
        if eol > 0 {
            out.extend_from_slice(&data[start..i]);
            out.extend_from_slice(linesep.as_bytes());
            i += eol;
            start = i;
        } else {
            i += 1;
        }
    }
    out.extend_from_slice(&data[start..]);
}

/// base64mime.body_encode / contentmanager._encode_base64: lines of `maxlen // 4 * 3` bytes, each ending with `\n`
fn b64_body(data: &[u8], line_bytes: usize) -> String {
    let per = line_bytes.max(1);
    let mut out = String::new();
    for chunk in data.chunks(per) {
        out += &base64::engine::general_purpose::STANDARD.encode(chunk);
        out.push('\n');
    }
    out
}

/// bytes.splitlines(): `\r\n`, `\r`, `\n`
fn byte_lines(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let eol = match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => 0,
        };
        if eol > 0 {
            out.push(&b[start..i]);
            i += eol;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < b.len() {
        out.push(&b[start..]);
    }
    out
}

/// contentmanager._encode_text: (cte, payload)
fn encode_text(string: &str, charset: &str, cte: Option<&str>, pol: Pol) -> R<(String, Payload)> {
    let policy = pol.policy(false);
    let v = (super::python(), h::ver());
    let http_fixed = match v.0 {
        (3, 12) => false,
        (3, 13) => super::python_at_least(3, 13, 10),
        (3, 14) => super::python_at_least(3, 14, 1),
        _ => true,
    };
    let maxlen = match policy.max_line_length {
        Some(m) => m,
        None if http_fixed => i64::MAX as usize,
        None => return Err(Exc::type_error("'<=' not supported between instances of 'int' and 'NoneType'")),
    };
    let encoded: Vec<u8> = if charset == "utf-8" {
        string.as_bytes().to_vec()
    } else {
        match super::methods::str_encode(string, &[V::str("ascii")], &[])? {
            V::Bytes(b) => b.to_vec(),
            _ => unreachable!(),
        }
    };
    let lines = byte_lines(&encoded);
    let ls = policy.linesep.as_bytes();
    let embedded = |ls_: &[u8], n: usize| -> Vec<u8> {
        let mut out = Vec::new();
        for (i, l) in lines.iter().take(n).enumerate() {
            if i > 0 {
                out.extend_from_slice(ls_);
            }
            out.extend_from_slice(l);
        }
        out.extend_from_slice(ls_);
        out
    };
    let normal = || embedded(b"\n", usize::MAX);
    let mut cte = cte.map(|s| s.to_string());
    if cte.is_none() {
        if lines.iter().map(|l| l.len()).max().unwrap_or(0) <= maxlen {
            let nb = normal();
            if nb.is_ascii() {
                return Ok(("7bit".into(), Payload::Text(String::from_utf8(nb).unwrap())));
            }
            return Ok(("8bit".into(), Payload::Raw(nb)));
        }
        let sniff = embedded(ls, 10);
        let sniff_qp = qp_body_encode(&sniff, maxlen)?;
        let sniff_b64 = (sniff.len() + 2) / 3 * 4 + 1;
        if sniff_qp.len() > sniff_b64 {
            cte = Some("base64".into());
        } else {
            if lines.len() <= 10 {
                return Ok(("quoted-printable".into(), Payload::Text(sniff_qp)));
            }
            cte = Some("quoted-printable".into());
        }
    }
    let cte = cte.unwrap();
    let data = match cte.as_str() {
        "7bit" => {
            let nb = normal();
            if let Some(pos) = nb.iter().position(|b| !b.is_ascii()) {
                return Err(Exc::msg(&UNICODE_DECODE_ERROR, format!(
                    "'ascii' codec can't decode byte 0x{:02x} in position {pos}: ordinal not in range(128)", nb[pos])));
            }
            Payload::Text(String::from_utf8(nb).unwrap())
        }
        "8bit" => {
            let nb = normal();
            if nb.is_ascii() {
                Payload::Text(String::from_utf8(nb).unwrap())
            } else {
                Payload::Raw(nb)
            }
        }
        "quoted-printable" => Payload::Text(qp_body_encode(&normal(), maxlen)?),
        "base64" => Payload::Text(b64_body(&embedded(ls, usize::MAX), maxlen / 4 * 3)),
        other => return Err(Exc::value_error(format!("Unknown content transfer encoding {other}"))),
    };
    Ok((cte, data))
}

/// quoprimime.body_encode of latin-1 text (`body` as its bytes)
fn qp_body_encode(body: &[u8], maxlinelen: usize) -> R<String> {
    if maxlinelen < 4 {
        return Err(Exc::value_error("maxlinelen must be at least 4"));
    }
    if body.is_empty() {
        return Ok(String::new());
    }
    let safe = |c: u8| c == b'\t' || (b' '..=b'~').contains(&c) && c != b'=';
    let mut t = String::new();
    for &c in body {
        if safe(c) || c == b'\r' || c == b'\n' {
            t.push(c as char);
        } else {
            t += &format!("={c:02X}");
        }
    }
    let quote = |c: u8| format!("={c:02X}");
    let maxlen = maxlinelen as i64;
    let soft_break = "=\n";
    let mut enc: Vec<String> = Vec::new();
    for line in byte_lines(t.as_bytes()) {
        let line = std::str::from_utf8(line).unwrap();
        let b = line.as_bytes();
        let n = b.len() as i64;
        let mut start: i64 = 0;
        let laststart = n - 1 - maxlen;
        while start <= laststart {
            let stop = start + maxlen - 1;
            if b[(stop - 2) as usize] == b'=' {
                enc.push(line[start as usize..(stop - 1) as usize].to_string());
                start = stop - 2;
            } else if b[(stop - 1) as usize] == b'=' {
                enc.push(line[start as usize..stop as usize].to_string());
                start = stop - 1;
            } else {
                enc.push(format!("{}=", &line[start as usize..stop as usize]));
                start = stop;
            }
        }
        if n > 0 && (b[(n - 1) as usize] == b' ' || b[(n - 1) as usize] == b'\t') {
            let room = start - laststart;
            let last = b[(n - 1) as usize];
            let q = if room >= 3 {
                quote(last)
            } else if room == 2 {
                format!("{}{soft_break}", last as char)
            } else {
                format!("{soft_break}{}", quote(last))
            };
            enc.push(format!("{}{q}", &line[start as usize..(n - 1) as usize]));
        } else {
            enc.push(line[start as usize..].to_string());
        }
    }
    if matches!(t.as_bytes().last(), Some(b'\r' | b'\n')) {
        enc.push(String::new());
    }
    Ok(enc.join("\n"))
}

fn key(k: &V) -> R<String> {
    match k {
        V::Str(s) => Ok(s.to_string()),
        other => Err(Exc::attr_error(format!("'{}' object has no attribute 'lower'", other.type_name()))),
    }
}

fn policy_kw(kwargs: &[(String, V)], allowed: &[&str], what: &str) -> R<Option<Pol>> {
    let mut pol = None;
    for (k, v) in kwargs {
        if !allowed.contains(&k.as_str()) {
            return Err(Exc::type_error(format!("py2axum: EmailMessage.{what}({k}=) is not supported")));
        }
        match k.as_str() {
            "policy" if !v.is_none() => pol = Some(Pol::from_marker(v)?),
            "unixfrom" if ops::truthy(v)? => return Err(Exc::type_error(format!("py2axum: EmailMessage.{what}(unixfrom=True) is not supported"))),
            "maxheaderlen" if !v.is_none() => return Err(Exc::type_error(format!("py2axum: EmailMessage.{what}(maxheaderlen=) is not supported"))),
            _ => {}
        }
    }
    Ok(pol)
}

pub fn method(m: &Arc<EmailMsg>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "set_content" => m.set_content(args, kwargs).map(|_| V::None),
        "as_bytes" | "as_string" => {
            if !args.is_empty() {
                return Err(Exc::type_error(format!("py2axum: EmailMessage.{name}() takes keyword arguments only")));
            }
            let allowed: &[&str] = if name == "as_bytes" { &["unixfrom", "policy"] } else { &["unixfrom", "maxheaderlen", "policy"] };
            let pol = policy_kw(kwargs, allowed, name)?;
            if name == "as_bytes" {
                m.as_bytes(pol)
            } else {
                m.as_string(pol, false)
            }
        }
        "get" => {
            if !kwargs.is_empty() || args.is_empty() || args.len() > 2 {
                return Err(Exc::type_error("py2axum: EmailMessage.get(name, failobj=None) by position only"));
            }
            Ok(m.get(&key(&args[0])?).map(V::str).unwrap_or_else(|| args.get(1).cloned().unwrap_or(V::None)))
        }
        "get_all" => {
            if !kwargs.is_empty() || args.is_empty() || args.len() > 2 {
                return Err(Exc::type_error("py2axum: EmailMessage.get_all(name, failobj=None) by position only"));
            }
            let l = key(&args[0])?.to_lowercase();
            let vals: Vec<V> = m.st.lock().headers.iter().filter(|x| x.name.to_lowercase() == l).map(|x| V::str(&x.decoded)).collect();
            Ok(if vals.is_empty() { args.get(1).cloned().unwrap_or(V::None) } else { V::list(vals) })
        }
        "keys" if args.is_empty() && kwargs.is_empty() => Ok(V::list(m.st.lock().headers.iter().map(|x| V::str(&x.name)).collect())),
        "values" if args.is_empty() && kwargs.is_empty() => Ok(V::list(m.st.lock().headers.iter().map(|x| V::str(&x.decoded)).collect())),
        "items" if args.is_empty() && kwargs.is_empty() => Ok(V::list(
            m.st.lock().headers.iter().map(|x| V::tuple(vec![V::str(&x.name), V::str(&x.decoded)])).collect(),
        )),
        _ => Err(Exc::type_error(format!("py2axum: EmailMessage.{name}() is not supported"))),
    }
}

pub fn getitem(m: &EmailMsg, k: &V) -> R {
    Ok(m.get(&key(k)?).map(V::str).unwrap_or(V::None))
}

pub fn setitem(m: &EmailMsg, k: &V, v: &V) -> R<()> {
    m.setitem(&key(k)?, v)
}

pub fn delitem(m: &EmailMsg, k: &V) -> R<()> {
    let l = key(k)?.to_lowercase();
    m.st.lock().headers.retain(|x| x.name.to_lowercase() != l);
    Ok(())
}

pub fn contains(m: &EmailMsg, k: &V) -> R<bool> {
    Ok(m.contains(&key(k)?))
}

pub fn len(m: &EmailMsg) -> usize {
    m.st.lock().headers.len()
}

pub fn keys(m: &EmailMsg) -> Vec<V> {
    m.st.lock().headers.iter().map(|x| V::str(&x.name)).collect()
}
