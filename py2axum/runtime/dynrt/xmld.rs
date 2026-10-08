//! `xmltodict.parse` (xmltodict 1.0, default options, `process_namespaces=` / `namespaces=`) on a small
//! XML parser of its own: elements, attributes, character and predefined entity references, CDATA,
//! comments and processing instructions (ignored). A DOCTYPE is refused. Malformed input raises
//! `xml.parsers.expat.ExpatError` with expat's message for the usual cases.

use indexmap::IndexMap;

use super::v::*;

struct Opts {
    namespaces: bool,
    /// `namespaces={uri: short}`: None/"" drops the namespace
    short: IndexMap<String, Option<String>>,
}

struct Frame {
    item: Option<Vec<(String, V)>>,
    data: Vec<String>,
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

fn expat(msg: &str, s: &[u8], at: usize) -> Exc {
    let at = at.min(s.len());
    let line = s[..at].iter().filter(|b| **b == b'\n').count() + 1;
    let col = at - s[..at].iter().rposition(|b| *b == b'\n').map(|p| p + 1).unwrap_or(0);
    Exc::msg(&EXPAT_ERROR, format!("{msg}: line {line}, column {col}"))
}

fn name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b':' || c >= 0x80
}

fn name_char(c: u8) -> bool {
    name_start(c) || c.is_ascii_digit() || c == b'-' || c == b'.'
}

impl<'a> P<'a> {
    fn err(&self, msg: &str, at: usize) -> Exc {
        expat(msg, self.s, at)
    }
    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }
    fn starts(&self, p: &str) -> bool {
        self.s[self.i..].starts_with(p.as_bytes())
    }
    fn ws(&mut self) {
        while !self.eof() && matches!(self.s[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }
    fn name(&mut self) -> R<String> {
        let start = self.i;
        if self.eof() {
            return Err(self.err("no element found", self.i));
        }
        if !name_start(self.s[self.i]) {
            return Err(self.err("not well-formed (invalid token)", self.i));
        }
        while !self.eof() && name_char(self.s[self.i]) {
            self.i += 1;
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }
    /// skips up to and past `end`
    fn skip_to(&mut self, end: &str) -> R<()> {
        match self.s[self.i..].windows(end.len()).position(|w| w == end.as_bytes()) {
            Some(p) => {
                self.i += p + end.len();
                Ok(())
            }
            None => Err(self.err("unclosed token", self.i)),
        }
    }
    /// `&...;` at self.i: the replacement text
    fn entity(&mut self) -> R<String> {
        let at = self.i;
        let end = self.s[at..].iter().position(|b| *b == b';').ok_or_else(|| self.err("not well-formed (invalid token)", at))?;
        let body = String::from_utf8_lossy(&self.s[at + 1..at + end]).into_owned();
        self.i = at + end + 1;
        let ch = |n: Option<u32>| n.and_then(char::from_u32).map(String::from).ok_or_else(|| expat("reference to invalid character number", self.s, at));
        Ok(match body.as_str() {
            "lt" => "<".into(),
            "gt" => ">".into(),
            "amp" => "&".into(),
            "quot" => "\"".into(),
            "apos" => "'".into(),
            b if b.starts_with("#x") => ch(u32::from_str_radix(&b[2..], 16).ok())?,
            b if b.starts_with('#') => ch(b[1..].parse().ok())?,
            b if !b.is_empty() && b.bytes().all(name_char) => return Err(self.err("undefined entity", at)),
            _ => return Err(self.err("not well-formed (invalid token)", at)),
        })
    }
    fn attr_value(&mut self) -> R<String> {
        let q = self.s[self.i];
        if q != b'"' && q != b'\'' {
            return Err(self.err("not well-formed (invalid token)", self.i));
        }
        self.i += 1;
        let mut out = String::new();
        let mut run = self.i;
        loop {
            if self.eof() {
                return Err(self.err("unclosed token", self.i));
            }
            match self.s[self.i] {
                c if c == q => break,
                b'<' => return Err(self.err("not well-formed (invalid token)", self.i)),
                b'&' => {
                    out += &String::from_utf8_lossy(&self.s[run..self.i]);
                    out += &self.entity()?;
                    run = self.i;
                    continue;
                }
                _ => self.i += 1,
            }
        }
        out += &String::from_utf8_lossy(&self.s[run..self.i]);
        self.i += 1;
        // attribute-value normalization: each whitespace character is a space
        Ok(out.replace("\r\n", " ").replace(['\t', '\n', '\r'], " "))
    }
}

fn build_name(o: &Opts, ns: &[IndexMap<String, String>], qname: &str, is_attr: bool, p: &P, at: usize) -> R<String> {
    if !o.namespaces {
        return Ok(qname.to_string());
    }
    let (prefix, local) = match qname.split_once(':') {
        Some((a, b)) => (a, b),
        None => ("", qname),
    };
    if prefix == "xml" {
        return Ok(format!("http://www.w3.org/XML/1998/namespace:{local}"));
    }
    // an unprefixed attribute has no namespace
    if prefix.is_empty() && is_attr {
        return Ok(qname.to_string());
    }
    let uri = ns.iter().rev().find_map(|m| m.get(prefix));
    let uri = match uri {
        Some(u) if !u.is_empty() => u.clone(),
        _ if prefix.is_empty() => return Ok(local.to_string()),
        _ => return Err(p.err("unbound prefix", at)),
    };
    Ok(match o.short.get(&uri) {
        Some(None) => local.to_string(),
        Some(Some(s)) if s.is_empty() => local.to_string(),
        Some(Some(s)) => format!("{s}:{local}"),
        None => format!("{uri}:{local}"),
    })
}

fn push(item: Option<Vec<(String, V)>>, key: String, data: V) -> Vec<(String, V)> {
    let mut item = item.unwrap_or_default();
    match item.iter_mut().find(|(k, _)| *k == key) {
        Some((_, v)) => match v {
            V::List(l) => l.lock().push(data),
            other => *other = V::list(vec![other.clone(), data]),
        },
        None => item.push((key, data)),
    }
    item
}

fn to_dict(item: Vec<(String, V)>) -> R {
    V::dict_from(item.into_iter().map(|(k, v)| (V::str(k), v)).collect())
}

fn parse_doc(s: &[u8], o: &Opts) -> R {
    let mut p = P { s, i: 0 };
    let mut stack: Vec<Frame> = Vec::new();
    let mut cur = Frame { item: None, data: Vec::new() };
    // open elements: (qname, the position of its name, namespace declarations)
    let mut open: Vec<(String, usize)> = Vec::new();
    let mut nsdecl: Vec<IndexMap<String, String>> = Vec::new();
    let mut root_done = false;
    // a UTF-8 byte order mark
    if p.starts("\u{feff}") {
        p.i += 3;
    }
    loop {
        if p.eof() {
            if !open.is_empty() || !root_done {
                return Err(p.err("no element found", p.i));
            }
            break;
        }
        if p.s[p.i] == b'<' {
            let lt = p.i;
            if p.starts("<?") {
                if lt != 0 && p.starts("<?xml") && p.s.get(lt + 5).is_some_and(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n' | b'?')) {
                    return Err(p.err("XML or text declaration not at start of entity", lt));
                }
                p.skip_to("?>")?;
                continue;
            }
            if p.starts("<!--") {
                p.skip_to("-->")?;
                continue;
            }
            if p.starts("<![CDATA[") {
                if open.is_empty() {
                    return Err(p.err(if root_done { "junk after document element" } else { "syntax error" }, lt));
                }
                p.i += 9;
                let st = p.i;
                p.skip_to("]]>")?;
                cur.data.push(String::from_utf8_lossy(&p.s[st..p.i - 3]).into_owned());
                continue;
            }
            if p.starts("<!DOCTYPE") {
                return Err(Exc::type_error("py2axum: xmltodict.parse() of a document with a DOCTYPE is not supported"));
            }
            if p.starts("</") {
                p.i += 2;
                let at = p.i;
                let name = p.name()?;
                p.ws();
                if p.eof() {
                    return Err(p.err("no element found", p.i));
                }
                if p.s[p.i] != b'>' {
                    return Err(p.err("not well-formed (invalid token)", p.i));
                }
                p.i += 1;
                match open.last() {
                    Some((n, _)) if *n == name => {}
                    Some(_) => return Err(p.err("mismatched tag", at)),
                    None => return Err(p.err("junk after document element", lt)),
                }
                let full = build_name(o, &nsdecl, &name, false, &p, at)?;
                open.pop();
                nsdecl.pop();
                end_element(&mut stack, &mut cur, full);
                if open.is_empty() {
                    root_done = true;
                }
                continue;
            }
            if root_done {
                return Err(p.err("junk after document element", lt));
            }
            p.i += 1;
            let at = p.i;
            let name = p.name()?;
            let mut attrs: Vec<(String, String, usize)> = Vec::new();
            let mut decls: IndexMap<String, String> = IndexMap::new();
            let self_close;
            loop {
                let before = p.i;
                p.ws();
                if p.eof() {
                    return Err(p.err("no element found", p.i));
                }
                match p.s[p.i] {
                    b'>' => {
                        p.i += 1;
                        self_close = false;
                        break;
                    }
                    b'/' => {
                        p.i += 1;
                        if p.eof() {
                            return Err(p.err("no element found", p.i));
                        }
                        if p.s[p.i] != b'>' {
                            return Err(p.err("not well-formed (invalid token)", p.i));
                        }
                        p.i += 1;
                        self_close = true;
                        break;
                    }
                    _ if p.i == before => return Err(p.err("not well-formed (invalid token)", p.i)),
                    _ => {
                        let aat = p.i;
                        let an = p.name()?;
                        p.ws();
                        if p.eof() || p.s[p.i] != b'=' {
                            return Err(p.err("not well-formed (invalid token)", p.i));
                        }
                        p.i += 1;
                        p.ws();
                        if p.eof() {
                            return Err(p.err("no element found", p.i));
                        }
                        let av = p.attr_value()?;
                        if attrs.iter().any(|(n, _, _)| *n == an) {
                            return Err(p.err("duplicate attribute", aat));
                        }
                        if o.namespaces && (an == "xmlns" || an.starts_with("xmlns:")) {
                            decls.insert(an.strip_prefix("xmlns:").unwrap_or("").to_string(), av);
                            continue;
                        }
                        attrs.push((an, av, aat));
                    }
                }
            }
            nsdecl.push(decls.clone());
            // expat reports an unbound prefix at the start tag
            let full = build_name(o, &nsdecl, &name, false, &p, lt)?;
            let mut item: Vec<(String, V)> = Vec::new();
            for (an, av, _) in &attrs {
                item.push((format!("@{}", build_name(o, &nsdecl, an, true, &p, lt)?), V::str(av)));
            }
            if !decls.is_empty() {
                let d = to_dict(decls.into_iter().map(|(k, v)| (k, V::str(v))).collect())?;
                item.push(("@xmlns".into(), d));
            }
            stack.push(std::mem::replace(&mut cur, Frame { item: if item.is_empty() { None } else { Some(item) }, data: Vec::new() }));
            if self_close {
                nsdecl.pop();
                end_element(&mut stack, &mut cur, full);
                root_done = true;
                if !open.is_empty() {
                    root_done = false;
                }
            } else {
                open.push((name, at));
            }
            continue;
        }
        // character data
        let st = p.i;
        let mut text = String::new();
        let mut run = st;
        while !p.eof() && p.s[p.i] != b'<' {
            if p.s[p.i] == b'&' {
                text += &String::from_utf8_lossy(&p.s[run..p.i]);
                if open.is_empty() {
                    return Err(p.err(if root_done { "junk after document element" } else { "syntax error" }, p.i));
                }
                text += &p.entity()?;
                run = p.i;
                continue;
            }
            p.i += 1;
        }
        text += &String::from_utf8_lossy(&p.s[run..p.i]);
        if open.is_empty() {
            if let Some(off) = text.bytes().position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n')) {
                return Err(p.err(if root_done { "junk after document element" } else { "syntax error" }, st + off));
            }
            continue;
        }
        cur.data.push(text.replace("\r\n", "\n").replace('\r', "\n"));
    }
    match cur.item {
        Some(item) => to_dict(item),
        None => Ok(V::None),
    }
}

fn end_element(stack: &mut Vec<Frame>, cur: &mut Frame, name: String) {
    let joined = cur.data.concat();
    let data = Some(joined.trim_matches(|c: char| c.is_whitespace()).to_string()).filter(|d| !d.is_empty());
    let item = cur.item.take();
    let parent = stack.pop().expect("element frame");
    *cur = parent;
    let value = match (item, data) {
        (Some(it), d) => {
            let it = match d {
                Some(d) => push(Some(it), "#text".into(), V::str(d)),
                None => it,
            };
            to_dict(it).unwrap_or(V::None)
        }
        (None, d) => d.map(V::str).unwrap_or(V::None),
    };
    cur.item = Some(push(cur.item.take(), name, value));
}

/// `xmltodict.parse(xml_input, process_namespaces=False, namespaces=None)`
pub fn parse(args: &[V], kwargs: &[(String, V)]) -> R {
    let input = args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "xml_input").map(|(_, v)| v)).ok_or_else(|| Exc::type_error("parse() missing 1 required positional argument: 'xml_input'"))?;
    let mut o = Opts { namespaces: false, short: IndexMap::new() };
    for (k, v) in kwargs {
        match k.as_str() {
            "xml_input" | "dict_constructor" => {}
            "process_namespaces" => o.namespaces = super::ops::truthy(v)?,
            "namespaces" => {
                if let V::Dict(d) = v {
                    for (_, (uri, short)) in d.lock().iter() {
                        let short = if short.is_none() { None } else { Some(super::ops::str_(short)?) };
                        o.short.insert(super::ops::str_(uri)?, short);
                    }
                } else if !v.is_none() {
                    return Err(Exc::type_error("py2axum: xmltodict.parse(namespaces=) takes a dict"));
                }
            }
            other => return Err(Exc::type_error(format!("py2axum: xmltodict.parse({other}=) is not supported"))),
        }
    }
    let bytes: Vec<u8> = match input {
        V::Str(s) => s.as_bytes().to_vec(),
        V::Bytes(b) => b.to_vec(),
        other => return Err(Exc::type_error(format!("py2axum: xmltodict.parse() of a {} is not supported", other.type_name()))),
    };
    if let V::Bytes(_) = input {
        if std::str::from_utf8(&bytes).is_err() {
            return Err(Exc::type_error("py2axum: xmltodict.parse() of bytes that are not UTF-8 is not supported"));
        }
    }
    parse_doc(&bytes, &o)
}
