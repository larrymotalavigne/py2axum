//! reportlab/platypus/paraparser.py: ParaParser on top of CPython 3.14's html.parser.HTMLParser
//! (convert_charrefs=False, fed once, never closed: an unterminated construct is dropped, as in reportlab).
//! html.parser changed in CPython's patch releases: `HtmlRev` selects the behaviours (3.14.0's by default).
//! Tags of the port: b strong i em font span br para nobr, unknown tags ignored; the other reportlab tags
//! (u, strike, a, link, img, super, sub, greek, unichar, bullet, seq*, onDraw, index) are refused.

use std::cell::Cell;

use super::colors::Color;
use super::entities::KNOWN_ENTITIES;
use super::fonts::{ps2tt, tt2ps};
use super::paragraph::SWord;
use super::styles::ParagraphStyle;
use super::RlError;

pub const FK_TEXT: u8 = 0;
pub const FK_BREAK: u8 = 3;
/// `_fkind` not set yet.
pub const FK_NONE: u8 = 255;

/// A ParaFrag. Options model attributes that may be absent (`hasattr`).
#[derive(Clone, Debug)]
pub struct Frag {
    /// The family on the parser stack, the PostScript name once the text is attached.
    pub font_name: String,
    pub font_size: f64,
    pub text_color: Option<Color>,
    /// `backColor`: None = absent, Some(None) = set to None.
    pub back_color: Option<Option<Color>>,
    pub bold: u8,
    pub italic: u8,
    pub rise: f64,
    pub nobr: bool,
    pub text: Option<String>,
    /// `words` of a frag produced by a simple-paragraph split (no `text` then).
    pub words: Option<Vec<SWord>>,
    pub line_break: bool,
    pub self_closing: Option<&'static str>,
    pub tag: String,
    pub fkind: Cell<u8>,
}

impl Frag {
    pub fn text(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }
}

/// `_py_sameFrag`.
pub fn same_frag(f: &Frag, g: &Frag) -> bool {
    if f.line_break || g.line_break {
        return false;
    }
    f.font_name == g.font_name
        && f.font_size == g.font_size
        && f.text_color == g.text_color
        && f.rise == g.rise
        && f.back_color.clone().flatten() == g.back_color.clone().flatten()
        && f.nobr == g.nobr
}

/// Python's `ascii(s)` / `repr(s)` of a str.
pub fn py_repr(s: &str, ascii: bool) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut r = String::new();
    r.push(q);
    for c in s.chars() {
        match c {
            '\\' => r.push_str("\\\\"),
            '\n' => r.push_str("\\n"),
            '\r' => r.push_str("\\r"),
            '\t' => r.push_str("\\t"),
            c if c == q => {
                r.push('\\');
                r.push(c)
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => r.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x7f => r.push(c),
            c if !ascii && is_printable(c) => r.push(c),
            c if (c as u32) <= 0xff => r.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) <= 0xffff => r.push_str(&format!("\\u{:04x}", c as u32)),
            c => r.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    r.push(q);
    r
}

/// Approximation of `str.isprintable` for repr (controls and separators other than space are not).
fn is_printable(c: char) -> bool {
    let u = c as u32;
    !(u < 0x20 || (0x7f..0xa0).contains(&u) || u == 0xad || PY_SPACE.contains(&c) || (0x2028..=0x2029).contains(&u))
}

/// Python's `str.isspace()` characters (Unicode 16).
pub const PY_SPACE: [char; 29] = [
    '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}', '\u{20}', '\u{85}', '\u{a0}', '\u{1680}',
    '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}',
    '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
];

pub fn py_isspace(c: char) -> bool {
    PY_SPACE.contains(&c)
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

// ---------------------------------------------------------------- attribute values

#[derive(Clone, Copy, Debug)]
enum Num {
    Abs(f64),
    Rel(f64),
}

/// Python `int(s)` for a str, None on ValueError.
fn py_int(s: &str) -> Option<f64> {
    let t = py_strip(s);
    let (neg, d) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if d.is_empty() || d.starts_with('_') || d.ends_with('_') || d.contains("__") {
        return None;
    }
    let digits: String = d.chars().filter(|&c| c != '_').collect();
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let v: f64 = digits.parse().ok()?;
    Some(if neg { -v } else { v })
}

/// Python `float(s)` for a str, None on ValueError.
fn py_float(s: &str) -> Option<f64> {
    let t = py_strip(s);
    let l = t.to_ascii_lowercase();
    let body = l.trim_start_matches(['+', '-']);
    if l.len() - body.len() > 1 {
        return None;
    }
    let neg = l.starts_with('-');
    match body {
        "inf" | "infinity" => return Some(if neg { f64::NEG_INFINITY } else { f64::INFINITY }),
        "nan" => return Some(f64::NAN),
        _ => {}
    }
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    let ok = body.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | '_' | '+' | '-'));
    if !ok || body.contains("_.") || body.contains("._") || body.contains("_e") || body.contains("e_") {
        return None;
    }
    let digits: String = l.chars().filter(|&c| c != '_').collect();
    digits.parse().ok()
}

/// `_num(s)` (units i/in/cm/mm/pt/pica, relative values with a sign).
fn py_num(s: &str) -> Result<Num, RlError> {
    let chars: Vec<char> = s.chars().collect();
    let mut body: String = s.to_string();
    let mut unit = 1.0;
    let lead = chars.iter().take_while(|&&c| py_isspace(c)).count();
    'outer: for split in (lead..=chars.len()).rev() {
        let rest: String = chars[split..].iter().collect();
        for (u, v) in [("i", 72.0), ("in", 72.0), ("cm", 72.0 / 2.54), ("mm", 72.0 / 2.54 * 0.1), ("pt", 1.0), ("pica", 12.0)] {
            if let Some(tail) = rest.strip_prefix(u) {
                if tail.chars().all(py_isspace) {
                    body = chars[lead..split].iter().collect();
                    unit = v;
                    break 'outer;
                }
            }
        }
    }
    let Some(c0) = body.chars().next() else {
        return Err(RlError::Index("string index out of range".into()));
    };
    let v = py_int(&body).or_else(|| py_float(&body)).ok_or_else(|| {
        RlError::Value(format!("could not convert string to float: {}", py_repr(&body, false)))
    })?;
    if c0 == '+' || c0 == '-' {
        Ok(Num::Rel(v * unit))
    } else {
        Ok(Num::Abs(v * unit))
    }
}

/// `toColor(s)` for markup attribute values.
pub fn to_color(arg: &str) -> Result<Color, RlError> {
    let t = arg.trim_matches(py_isspace);
    let low = t.to_lowercase();
    for (pfx, alpha) in [("rgba", true), ("rgb", false)] {
        if let Some(rest) = low.strip_prefix(pfx) {
            let rest = rest.trim_start_matches(py_isspace);
            if let Some(inner) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
                if !inner.contains(')') {
                    return css_rgb(arg, inner, alpha);
                }
            }
        }
    }
    if ["pcmyk", "cmyk", "hsl"].iter().any(|p| low.starts_with(p)) || t.starts_with(['(', '[']) || arg.contains("Blacker") || arg.contains("Whiter") {
        return Err(RlError::Unsupported(format!("color {arg:?}")));
    }
    if let Some(c) = Color::named(&arg.to_lowercase()) {
        return Ok(c);
    }
    let bad = || RlError::Value(format!("Invalid color value {}", py_repr(arg, false)));
    let (digits, radix) = if let Some(v) = arg.strip_prefix('#') {
        (v, 16)
    } else if arg.len() >= 2 && arg[..2].eq_ignore_ascii_case("0x") {
        (&arg[2..], 16)
    } else {
        (arg, 10)
    };
    let d = py_strip(digits);
    let (neg, d) = match d.strip_prefix('-') {
        Some(x) => (true, x),
        None => (false, d.strip_prefix('+').unwrap_or(d)),
    };
    let d: String = d.chars().filter(|&c| c != '_').collect();
    let v = i128::from_str_radix(&d, radix).map_err(|_| bad())?;
    let v = if neg { -v } else { v };
    Ok(Color::rgb(((v >> 16) & 0xFF) as f64 / 255.0, ((v >> 8) & 0xFF) as f64 / 255.0, (v & 0xFF) as f64 / 255.0))
}

/// `cssParse` for rgb()/rgba().
fn css_rgb(s: &str, inner: &str, has_alpha: bool) -> Result<Color, RlError> {
    let n: Vec<&str> = inner.split(',').collect();
    if n.len() != 3 + has_alpha as usize {
        return Err(RlError::Value(format!("css color {} has wrong number of components", py_repr(s, false))));
    }
    let a = if has_alpha {
        let v = py_float(n[3]).ok_or_else(|| RlError::Value(format!("bad alpha argument value {} in css color {}", py_repr(n[3], false), py_repr(s, false))))?;
        v.max(0.0).min(1.0)
    } else {
        1.0
    };
    let pc = n[0].contains('%');
    let mut rgb = [0.0; 3];
    for (k, v) in n[..3].iter().enumerate() {
        rgb[k] = if pc {
            let v = py_strip(v);
            let c = py_float(&v[..v.len().saturating_sub(1)])
                .ok_or_else(|| RlError::Value(format!("bad percentage argument value {} in css color {}", py_repr(v, false), py_repr(s, false))))?;
            let c = c.max(0.0).min(100.0) / 100.0;
            ((c * 255.0 + 0.5) as i64) as f64 / 255.0
        } else {
            let mut c = py_float(v).ok_or_else(|| RlError::Value(format!("bad argument value {} in css color {}", py_repr(py_strip(v), false), py_repr(s, false))))?;
            if 0.0 < c && c <= 1.0 {
                c *= 255.0
            }
            (c.max(0.0).min(255.0) as i64) as f64 / 255.0
        };
    }
    Ok(Color::rgba(rgb[0], rgb[1], rgb[2], a))
}

// ---------------------------------------------------------------- HTMLParser (3.14.0)

fn ws5(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\x0c' | ' ')
}

fn quote_or_ws(c: char) -> bool {
    matches!(c, '\'' | '"' | '\t' | '\n' | '\r' | '\x0c' | ' ' | '/')
}

/// `[\t\n\r\f ]*=[\t\n\r\f ]*(?:'[^']*'|"[^"]*"|(?!['"])[^>\t\n\r\f ]*)` at r: (value start, end).
fn match_value(raw: &[char], r: usize) -> Option<(usize, usize)> {
    let n = raw.len();
    let mut a = r;
    while a < n && ws5(raw[a]) {
        a += 1;
    }
    if a >= n || raw[a] != '=' {
        return None;
    }
    let b0 = a + 1;
    let mut pos = b0;
    while pos < n && ws5(raw[pos]) {
        pos += 1;
    }
    loop {
        if pos < n && (raw[pos] == '\'' || raw[pos] == '"') {
            let q = raw[pos];
            if let Some(e) = raw[pos + 1..].iter().position(|&c| c == q) {
                return Some((pos, pos + 1 + e + 1));
            }
        } else {
            let mut e = pos;
            while e < n && raw[e] != '>' && !ws5(raw[e]) {
                e += 1;
            }
            return Some((pos, e));
        }
        if pos == b0 {
            return None;
        }
        pos -= 1;
    }
}

/// `locatetagend.match(rawdata, p).end()`.
fn locate_tag_end(raw: &[char], p: usize) -> usize {
    let n = raw.len();
    let mut q = p + 1;
    while q < n && !matches!(raw[q], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        q += 1;
    }
    while q < n && (ws5(raw[q]) || raw[q] == '/') {
        q += 1;
    }
    loop {
        if q >= n || q == 0 || !quote_or_ws(raw[q - 1]) || matches!(raw[q], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
            break;
        }
        let mut r = q + 1;
        while r < n && !matches!(raw[r], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '=' | '>') {
            r += 1;
        }
        if let Some((_, e)) = match_value(raw, r) {
            r = e;
        }
        while r < n && (ws5(raw[r]) || raw[r] == '/') {
            r += 1;
        }
        q = r;
    }
    if q < n && raw[q] == '>' {
        q += 1;
    }
    q
}

/// `tagfind_tolerant.match(rawdata, p)`: (name, end).
fn tag_find(raw: &[char], p: usize) -> (String, usize) {
    let n = raw.len();
    let mut q = p + 1;
    while q < n && !matches!(raw[q], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        q += 1;
    }
    let name: String = raw[p..q].iter().collect();
    while q < n && (ws5(raw[q]) || (raw[q] == '/' && !(q + 1 < n && raw[q + 1] == '>'))) {
        q += 1;
    }
    (name, q)
}

struct AttrMatch {
    name: String,
    value: Option<String>,
    end: usize,
}

/// `attrfind_tolerant.match(rawdata, k)`.
fn attr_find(raw: &[char], k: usize) -> Option<AttrMatch> {
    let n = raw.len();
    if k >= n || k == 0 || !quote_or_ws(raw[k - 1]) || matches!(raw[k], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        return None;
    }
    let mut r = k + 1;
    while r < n && !matches!(raw[r], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '=' | '>') {
        r += 1;
    }
    let name: String = raw[k..r].iter().collect();
    let mut value = None;
    if let Some((vs, e)) = match_value(raw, r) {
        value = Some(raw[vs..e].iter().collect::<String>());
        r = e;
    }
    while r < n && (ws5(raw[r]) || (raw[r] == '/' && !(r + 1 < n && raw[r + 1] == '>'))) {
        r += 1;
    }
    Some(AttrMatch { name, value, end: r })
}

fn find_from(raw: &[char], from: usize, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    if from > raw.len() {
        return None;
    }
    (from..=raw.len().saturating_sub(p.len())).find(|&i| raw[i..].starts_with(&p))
}

fn starts(raw: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    raw.len() >= i + p.len() && raw[i..i + p.len()] == p[..]
}

// ---------------------------------------------------------------- html.parser revisions

/// The html.parser behaviours that changed after 3.14.0 and that ParaParser can observe (fed once, never
/// closed, convert_charrefs=False). All false: 3.14.0 (and 3.13.8-3.13.9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HtmlRev {
    /// xmp, iframe, noembed, noframes are RAWTEXT and plaintext swallows the rest (3.12.13, 3.13.10, 3.14.1).
    pub rawtext_more: bool,
    /// `&#` not followed by a digit (or x + hex digit) is data when 2 characters follow, whatever comes
    /// after; before, only when a `;` follows somewhere (3.13.10, 3.14.1).
    pub charref_data: bool,
    /// `<!-->` and `<!--->` close the comment before a later `-->` is looked for (3.13.15, 3.14.7).
    pub abrupt_comment_first: bool,
}

static HTML_REV: std::sync::OnceLock<HtmlRev> = std::sync::OnceLock::new();

/// The html.parser of the Python the document is made on (set once, before the first paragraph).
pub fn set_html_rev(r: HtmlRev) {
    let _ = HTML_REV.set(r);
}

fn rev() -> HtmlRev {
    HTML_REV.get().copied().unwrap_or_default()
}

// ---------------------------------------------------------------- ParaParser

struct Parser<'a> {
    style: &'a ParagraphStyle,
    stack: Vec<Frag>,
    frags: Vec<Frag>,
    cdata_elem: Option<String>,
    escapable: bool,
}

const SPAN_KEYS: &str =
    "['backColor', 'backcolor', 'bgcolor', 'color', 'face', 'fg', 'fontName', 'fontSize', 'fontname', 'fontsize', 'name', 'size', 'style', 'textColor', 'textcolor']";

fn syntax(msg: String) -> RlError {
    RlError::Value(format!("paraparser: syntax error: {msg}"))
}

fn list_index() -> RlError {
    RlError::Index("list index out of range".into())
}

/// One attribute applied by `_push`.
enum Attr {
    FontName(Option<String>),
    FontSize(Num),
    TextColor(Color),
    BackColor(Color),
    Ignored,
}

impl<'a> Parser<'a> {
    fn top(&self) -> Result<&Frag, RlError> {
        self.stack.last().ok_or_else(list_index)
    }

    fn push(&mut self, tag: &str, f: impl FnOnce(&mut Frag)) -> Result<(), RlError> {
        let mut frag = self.top()?.clone();
        frag.tag = tag.to_string();
        f(&mut frag);
        self.stack.push(frag);
        Ok(())
    }

    fn pop(&mut self, tag: &str) -> Result<(), RlError> {
        let frag = self.stack.pop().ok_or_else(|| RlError::Index("pop from empty list".into()))?;
        if frag.tag == tag {
            return Ok(());
        }
        Err(RlError::Value(format!("Parse error: saw </{tag}> instead of expected </{}>", frag.tag)))
    }

    /// `getAttributes(attr, _spanAttrMap)` (font/span) or with an empty map (nobr).
    fn span_attrs(&self, attrs: &[(String, Option<String>)], empty_map: bool) -> Result<Vec<Attr>, RlError> {
        let mut out = Vec::new();
        for (k, v) in attrs {
            let a = match (empty_map, k.as_str()) {
                (false, "size" | "fontsize") => match v {
                    Some(s) => Attr::FontSize(py_num(s)?),
                    None => return Err(RlError::Type("expected string or bytes-like object, got 'NoneType'".into())),
                },
                (false, "face" | "name" | "fontname") => Attr::FontName(v.clone()),
                (false, "fg" | "color" | "textcolor" | "backcolor" | "bgcolor") => {
                    let c = match v {
                        Some(s) => to_color(s)?,
                        None => return Err(RlError::Value("Invalid color value None".into())),
                    };
                    if k == "backcolor" || k == "bgcolor" {
                        Attr::BackColor(c)
                    } else {
                        Attr::TextColor(c)
                    }
                }
                (false, "style") => Attr::Ignored,
                _ => {
                    let keys = if empty_map { "[]" } else { SPAN_KEYS };
                    return Err(syntax(format!("invalid attribute name {k} attrMap={keys}")));
                }
            };
            out.push(a);
        }
        Ok(out)
    }

    fn start_para(&mut self, attrs: &[(String, Option<String>)]) -> Result<(), RlError> {
        if !attrs.is_empty() {
            return Err(RlError::Unsupported("<para> attributes".into()));
        }
        let style = self.style;
        let (fam, b, i) = ps2tt(&style.font_name).map_err(|e| match e {
            e @ RlError::Value(_) => e.annotate(&format!("error with style name={}", style.name)),
            e => e,
        })?;
        self.stack = vec![Frag {
            font_name: fam.to_string(),
            font_size: style.font_size,
            text_color: style.text_color.clone(),
            back_color: None,
            bold: b,
            italic: i,
            rise: 0.0,
            nobr: false,
            text: None,
            words: None,
            line_break: false,
            self_closing: None,
            tag: "para".into(),
            fkind: Cell::new(FK_NONE),
        }];
        Ok(())
    }

    fn handle_starttag(&mut self, tag: &str, attrs: Vec<(String, Option<String>)>) -> Result<(), RlError> {
        // list of pairs -> dict (a repeated name keeps its first position, last value)
        let mut d: Vec<(String, Option<String>)> = Vec::new();
        for (k, v) in attrs {
            match d.iter_mut().find(|(x, _)| *x == k) {
                Some(e) => e.1 = v,
                None => d.push((k, v)),
            }
        }
        match tag {
            "b" | "strong" => self.push(tag, |f| f.bold = 1),
            "i" | "em" => self.push(tag, |f| f.italic = 1),
            "font" | "span" => {
                if tag == "span" && d.iter().any(|(k, _)| k == "style") {
                    let a = self.span_attrs(&d, false)?;
                    drop(a);
                    return Err(RlError::Value("findSpanStyle not implemented in this parser".into()));
                }
                let a = self.span_attrs(&d, false)?;
                let mut fname: Option<(&'static str, u8, u8)> = None;
                for x in &a {
                    if let Attr::FontName(n) = x {
                        let n = n.as_deref().ok_or_else(|| RlError::Attribute("'NoneType' object has no attribute 'lower'".into()))?;
                        fname = Some(ps2tt(n)?);
                    }
                }
                self.push(tag, |f| {
                    for x in a {
                        match x {
                            Attr::FontSize(Num::Abs(v)) => f.font_size = v,
                            Attr::FontSize(Num::Rel(v)) => f.font_size += v,
                            Attr::TextColor(c) => f.text_color = Some(c),
                            Attr::BackColor(c) => f.back_color = Some(Some(c)),
                            Attr::FontName(_) | Attr::Ignored => {}
                        }
                    }
                    if let Some((fam, b, i)) = fname {
                        f.font_name = fam.to_string();
                        f.bold = b;
                        f.italic = i;
                    }
                })
            }
            "br" => self.push("br", |f| {
                f.self_closing = Some("br");
                f.line_break = true;
                f.text = Some(String::new());
            }),
            "nobr" => {
                self.span_attrs(&d, true)?;
                self.push("nobr", |f| f.nobr = true)
            }
            "para" => self.start_para(&d),
            "u" | "strike" | "link" | "a" | "img" | "super" | "sup" | "sub" | "greek" | "unichar" | "bullet" | "seq"
            | "seqdefault" | "seqreset" | "seqchain" | "seqformat" | "ondraw" | "index" => {
                Err(RlError::Unsupported(format!("paragraph markup <{tag}>")))
            }
            _ => Ok(()),
        }
    }

    fn handle_endtag(&mut self, tag: &str) -> Result<(), RlError> {
        match tag {
            "b" | "strong" | "i" | "em" | "font" | "span" | "nobr" | "para" | "u" | "strike" | "sub" | "greek" | "unichar" => {
                self.pop(tag)
            }
            "super" | "sup" => self.pop("super"),
            "br" => {
                let top = self.top()?;
                match top.self_closing {
                    None => return Err(RlError::Attribute("'ParaFrag' object has no attribute '_selfClosingTag'".into())),
                    Some(t) if t != "br" || !top.line_break => return Err(RlError::Value("Parser failure in <br/>".into())),
                    _ => {}
                }
                self.stack.last_mut().unwrap().self_closing = None;
                self.handle_data("")?;
                self.pop("br")
            }
            "link" | "a" | "img" | "bullet" => Err(RlError::Unsupported(format!("paragraph markup </{tag}>"))),
            _ => Ok(()),
        }
    }

    fn handle_data(&mut self, data: &str) -> Result<(), RlError> {
        let mut frag = self.top()?.clone();
        if let Some(t) = frag.self_closing {
            if !data.is_empty() {
                return Err(syntax(format!("No content allowed in {t} tag")));
            }
            return Ok(());
        }
        frag.font_name = tt2ps(&frag.font_name, frag.bold, frag.italic)?.to_string();
        frag.text = Some(data.to_string());
        self.frags.push(frag);
        Ok(())
    }

    fn handle_charref(&mut self, name: &str) -> Result<(), RlError> {
        let n: Option<u128> = if let Some(h) = name.strip_prefix('x') {
            Some(u128::from_str_radix(h, 16).unwrap_or(u128::MAX))
        } else if name.chars().all(|c| c.is_ascii_digit()) {
            Some(name.parse::<u128>().unwrap_or(u128::MAX))
        } else {
            None
        };
        let Some(n) = n else {
            return Err(RlError::Attribute("unknown_charref".into()));
        };
        if n >= 0x110000 {
            return Err(RlError::Value("chr() arg not in range(0x110000)".into()));
        }
        let c = char::from_u32(n as u32).ok_or_else(|| RlError::Unsupported("surrogate character reference".into()))?;
        self.handle_data(&c.to_string())
    }

    fn handle_entityref(&mut self, name: &str) -> Result<(), RlError> {
        match KNOWN_ENTITIES.binary_search_by(|(k, _)| k.as_bytes().cmp(name.as_bytes())) {
            Ok(i) => self.handle_data(KNOWN_ENTITIES[i].1),
            Err(_) => self.handle_data(&format!("&{name};")),
        }
    }

    // ---- HTMLParser internals

    fn parse_starttag(&mut self, raw: &[char], i: usize) -> Result<isize, RlError> {
        let endpos = locate_tag_end(raw, i + 1);
        if raw[endpos - 1] != '>' {
            return Ok(-1);
        }
        let (name, mut k) = tag_find(raw, i + 1);
        let tag = name.to_lowercase();
        let mut attrs = Vec::new();
        while k < endpos {
            let Some(m) = attr_find(raw, k) else { break };
            let mut v = m.value;
            if let Some(s) = &v {
                let c: Vec<char> = s.chars().collect();
                if c.len() >= 2 && (c[0] == '\'' || c[0] == '"') && c[c.len() - 1] == c[0] {
                    v = Some(c[1..c.len() - 1].iter().collect());
                }
            }
            if let Some(s) = &v {
                if s.contains('&') {
                    return Err(RlError::Unsupported("character reference in a markup attribute".into()));
                }
            }
            attrs.push((m.name.to_lowercase(), v));
            k = m.end;
        }
        let end: String = raw[k..endpos].iter().collect();
        let end = py_strip(&end);
        if end != ">" && end != "/>" {
            let d: String = raw[i..endpos].iter().collect();
            self.handle_data(&d)?;
            return Ok(endpos as isize);
        }
        if end.ends_with("/>") {
            self.handle_starttag(&tag, attrs)?;
            self.handle_endtag(&tag)?;
        } else {
            self.handle_starttag(&tag, attrs)?;
            let rawtext: &[&str] = if rev().rawtext_more {
                &["script", "style", "xmp", "iframe", "noembed", "noframes", "plaintext"]
            } else {
                &["script", "style"]
            };
            if rawtext.contains(&tag.as_str()) {
                self.cdata_elem = Some(tag);
                self.escapable = false;
            } else if tag == "textarea" || tag == "title" {
                self.cdata_elem = Some(tag);
                self.escapable = true;
            }
        }
        Ok(endpos as isize)
    }

    fn parse_bogus_comment(raw: &[char], i: usize) -> isize {
        match find_from(raw, i + 2, ">") {
            Some(p) => p as isize + 1,
            None => -1,
        }
    }

    fn parse_endtag(&mut self, raw: &[char], i: usize) -> Result<isize, RlError> {
        if find_from(raw, i + 2, ">").is_none() {
            return Ok(-1);
        }
        if !(i + 2 < raw.len() && raw[i + 2].is_ascii_alphabetic()) {
            if raw.get(i + 2) == Some(&'>') {
                return Ok(i as isize + 3);
            }
            return Ok(Parser::parse_bogus_comment(raw, i));
        }
        let j = locate_tag_end(raw, i + 2);
        if raw[j - 1] != '>' {
            return Ok(-1);
        }
        let (name, _) = tag_find(raw, i + 2);
        self.handle_endtag(&name.to_lowercase())?;
        self.cdata_elem = None;
        self.escapable = true;
        Ok(j as isize)
    }

    fn parse_comment(raw: &[char], i: usize) -> isize {
        let n = raw.len();
        if rev().abrupt_comment_first {
            // commentabruptclose.match(rawdata, i+4) first: `-?>`
            if raw.get(i + 4) == Some(&'>') {
                return i as isize + 5;
            }
            if starts(raw, i + 4, "->") {
                return i as isize + 6;
            }
        }
        let mut p = i + 4;
        while p + 2 < n + 1 {
            if starts(raw, p, "-->") {
                return p as isize + 3;
            }
            if starts(raw, p, "--!>") {
                return p as isize + 4;
            }
            p += 1;
        }
        if raw.get(i + 4) == Some(&'>') {
            return i as isize + 5;
        }
        if starts(raw, i + 4, "->") {
            return i as isize + 6;
        }
        -1
    }

    fn parse_html_declaration(raw: &[char], i: usize) -> isize {
        if starts(raw, i, "<![CDATA[") {
            return match find_from(raw, i + 9, "]]>") {
                Some(j) => j as isize + 3,
                None => -1,
            };
        }
        let head: String = raw[i..raw.len().min(i + 9)].iter().collect();
        if head.to_lowercase() == "<!doctype" {
            return match find_from(raw, i + 9, ">") {
                Some(j) => j as isize + 1,
                None => -1,
            };
        }
        Parser::parse_bogus_comment(raw, i)
    }

    /// `interesting.search(rawdata, i)`.
    fn interesting(&self, raw: &[char], i: usize) -> Option<usize> {
        match &self.cdata_elem {
            None => (i..raw.len()).find(|&j| raw[j] == '&' || raw[j] == '<'),
            // re.compile(r'\z'): the rest is data
            Some(el) if el == "plaintext" => Some(raw.len()),
            Some(el) => {
                let el: Vec<char> = el.chars().collect();
                (i..raw.len()).find(|&j| {
                    if self.escapable && raw[j] == '&' {
                        return true;
                    }
                    raw[j] == '<'
                        && raw.get(j + 1) == Some(&'/')
                        && raw.len() > j + 2 + el.len()
                        && raw[j + 2..j + 2 + el.len()].iter().zip(&el).all(|(a, b)| a.to_ascii_lowercase() == *b)
                        && matches!(raw[j + 2 + el.len()], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>')
                })
            }
        }
    }

    /// `goahead(0)`.
    fn goahead(&mut self, raw: &[char]) -> Result<(), RlError> {
        let n = raw.len();
        let mut i = 0usize;
        while i < n {
            let j = match self.interesting(raw, i) {
                Some(j) => j,
                None => {
                    if self.cdata_elem.is_some() {
                        break;
                    }
                    n
                }
            };
            if i < j {
                let d: String = raw[i..j].iter().collect();
                self.handle_data(&d)?;
            }
            i = j;
            if i == n {
                break;
            }
            if raw[i] == '<' {
                let k = if i + 1 < n && raw[i + 1].is_ascii_alphabetic() {
                    self.parse_starttag(raw, i)?
                } else if starts(raw, i, "</") {
                    self.parse_endtag(raw, i)?
                } else if starts(raw, i, "<!--") {
                    Parser::parse_comment(raw, i)
                } else if starts(raw, i, "<?") {
                    match find_from(raw, i + 2, ">") {
                        Some(j) => j as isize + 1,
                        None => -1,
                    }
                } else if starts(raw, i, "<!") {
                    Parser::parse_html_declaration(raw, i)
                } else if i + 1 < n {
                    self.handle_data("<")?;
                    i as isize + 1
                } else {
                    break;
                };
                if k < 0 {
                    break;
                }
                i = k as usize;
            } else if starts(raw, i, "&#") {
                // charref: &#(?:[0-9]+|[xX][0-9a-fA-F]+)[^0-9a-fA-F]
                let p = i + 2;
                let mut m = None;
                if p < n && raw[p].is_ascii_digit() {
                    let mut q = p;
                    while q < n && raw[q].is_ascii_digit() {
                        q += 1;
                    }
                    if q < n && !raw[q].is_ascii_hexdigit() {
                        m = Some(q + 1);
                    }
                } else if p < n && (raw[p] == 'x' || raw[p] == 'X') {
                    let mut q = p + 1;
                    while q < n && raw[q].is_ascii_hexdigit() {
                        q += 1;
                    }
                    if q > p + 1 && q < n {
                        m = Some(q + 1);
                    }
                }
                if let Some(mut k) = m {
                    let name: String = raw[i + 2..k - 1].iter().collect();
                    self.handle_charref(&name)?;
                    if raw[k - 1] != ';' {
                        k -= 1;
                    }
                    i = k;
                    continue;
                }
                if rev().charref_data {
                    // incomplete_charref = '&#(?:[0-9]|[xX][0-9a-fA-F])'
                    let p = i + 2;
                    let incomplete = p < n
                        && (raw[p].is_ascii_digit() || ((raw[p] == 'x' || raw[p] == 'X') && p + 1 < n && raw[p + 1].is_ascii_hexdigit()));
                    if !incomplete && i + 3 < n {
                        self.handle_data("&#")?;
                        i += 2;
                        continue;
                    }
                    break;
                }
                if raw[i..].contains(&';') {
                    self.handle_data("&#")?;
                }
                break;
            } else {
                // entityref: &([a-zA-Z][-.a-zA-Z0-9]*)[^a-zA-Z0-9]
                let p = i + 1;
                let mut m = None;
                if p < n && raw[p].is_ascii_alphabetic() {
                    let mut q = p + 1;
                    while q < n && (raw[q].is_ascii_alphanumeric() || raw[q] == '-' || raw[q] == '.') {
                        q += 1;
                    }
                    if q < n {
                        m = Some(q);
                    } else {
                        m = (p + 1..q).rev().find(|&x| raw[x] == '-' || raw[x] == '.');
                    }
                }
                if let Some(q) = m {
                    let name: String = raw[p..q].iter().collect();
                    self.handle_entityref(&name)?;
                    let mut k = q + 1;
                    if raw[k - 1] != ';' {
                        k -= 1;
                    }
                    i = k;
                    continue;
                }
                if p < n && (raw[p].is_ascii_alphabetic() || raw[p] == '#') {
                    break; // incomplete
                } else if i + 1 < n {
                    self.handle_data("&")?;
                    i += 1;
                } else {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// `cleanBlockQuotedText(text)`.
pub fn clean_block_quoted_text(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let l = super::paragraph::strip(line);
        let words: Vec<String> = super::paragraph::split(l).into_iter().filter(|w| !w.is_empty()).collect();
        let j = words.join(" ");
        if !j.is_empty() {
            out.push(j);
        }
    }
    out.join(" ")
}

/// `ParaParser().parse(text, style)` on cleaned text: the frags (bullet frags are not supported).
pub fn parse(text: &str, style: &ParagraphStyle) -> Result<Vec<Frag>, RlError> {
    let tc: Vec<char> = text.chars().collect();
    let is_para = tc.len() >= 6 && tc[0] == '<' && {
        let mut k = 1;
        while k < tc.len() && py_isspace(tc[k]) {
            k += 1;
        }
        starts(&tc, k, "para") && {
            let r = k + 4;
            r < tc.len() && (py_isspace(tc[r]) || tc[r] == '>' || starts(&tc, r, "/>"))
        }
    };
    let full = if is_para { text.to_string() } else { format!("<para>{text}</para>") };
    let raw: Vec<char> = full.chars().collect();
    let mut p = Parser { style, stack: Vec::new(), frags: Vec::new(), cdata_elem: None, escapable: true };
    if let Err(e) = p.goahead(&raw) {
        let head = format!("\nparagraph text {} caused exception", py_repr(&full, true));
        return Err(e.annotate(&head));
    }
    if !p.stack.is_empty() {
        let tags: Vec<&str> = p.stack.iter().rev().map(|f| f.tag.as_str()).collect();
        return Err(syntax(format!("parse ended with {} unclosed tags\n {}", p.stack.len(), tags.join("\n "))));
    }
    Ok(p.frags)
}
