//! PyYAML 6.0 (`import yaml`): `safe_load` / `load(Loader=SafeLoader)` as a port of its pure-Python loader
//! (reader.py, scanner.py, parser.py, composer.py, resolver.py, SafeConstructor of constructor.py, error.py):
//! the same values, the same exceptions with the same messages, marks and snippets. The dumping side
//! (representer, serializer, emitter) is in `yamlemit`.
//!
//! What cannot follow CPython: an integer beyond 64 bits (OverflowError, like `json.loads`), and the nesting
//! depth at which the recursive composer raises RecursionError (CPython's depends on the caller's stack: a
//! fixed depth here). Both are documented in supported.md.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use indexmap::IndexMap;
use num_bigint::BigInt;

use super::ops;
use super::v::*;

// ---------------------------------------------------------------- characters

/// '\0 \t\r\n\x85  '
const BZT: &str = "\0 \t\r\n\u{85}\u{2028}\u{2029}";
/// '\0 \r\n\x85  '
const BZ: &str = "\0 \r\n\u{85}\u{2028}\u{2029}";
/// '\0\r\n\x85  '
const ZBREAK: &str = "\0\r\n\u{85}\u{2028}\u{2029}";
/// '\r\n\x85  '
const BREAK: &str = "\r\n\u{85}\u{2028}\u{2029}";

fn is_in(c: char, set: &str) -> bool {
    set.contains(c)
}

fn alnum(c: char) -> bool {
    c.is_ascii_digit() || c.is_ascii_uppercase() || c.is_ascii_lowercase() || c == '-' || c == '_'
}

/// `str.isprintable()` (the translating Python's Unicode database: dyn.str_classes)
pub fn printable(c: char) -> bool {
    super::methods::printable(c)
}

/// `repr(str)` as CPython writes it (non-printable characters escaped)
pub fn py_repr(s: &str) -> String {
    ops::str_repr(s)
}

fn char_repr(c: char) -> String {
    py_repr(&c.to_string())
}

/// Python's `str.isspace()` (what `int()`, `float()` and `str.strip()` skip)
fn py_space(c: char) -> bool {
    matches!(c, '\t'..='\r' | '\x1c'..='\x1f' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// a Unicode decimal digit's value (`int()` and `float()` accept every script's digits)
fn decimal(c: char) -> Option<u32> {
    if c.is_ascii_digit() {
        return Some(c as u32 - '0' as u32);
    }
    let n = c as u32;
    super::unidata::DECIMAL_RUNS.iter().find(|(a, len)| n >= *a && n < a + len).map(|(a, _)| n - a)
}

/// `_PyUnicode_TransformDecimalAndSpaceToASCII` then the surrounding whitespace removed; None when a character is
/// neither ASCII nor a digit/space (then no number can parse)
fn ascii_number(s: &str) -> Option<String> {
    let t: String = s.chars().map(|c| if py_space(c) { ' ' } else if let Some(d) = decimal(c) { char::from_digit(d, 10).unwrap() } else { c }).collect();
    if !t.is_ascii() {
        return None;
    }
    Some(t.trim_matches(|c: char| c == ' ' || ('\t'..='\r').contains(&c) || ('\x1c'..='\x1f').contains(&c)).to_string())
}

/// `int(s, base)` for a str (PyLong_FromString): sign, prefix of its base, single underscores between digits
fn py_int(s: &str, base: u32) -> R<BigInt> {
    let bad = || {
        let r: String = py_repr(s).chars().take(200).collect();
        Exc::value_error(format!("invalid literal for int() with base {base}: {r}"))
    };
    let t = ascii_number(s).ok_or_else(bad)?;
    let (neg, mut body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, &t[..]),
    };
    let prefix = match base {
        16 => Some("0x"),
        8 => Some("0o"),
        2 => Some("0b"),
        _ => None,
    };
    let mut after_prefix = false;
    if let Some(p) = prefix {
        if body.len() >= 2 && body[..2].eq_ignore_ascii_case(p) {
            body = &body[2..];
            after_prefix = true;
        }
    }
    // underscores: single, between digits (or right after the base prefix)
    let b = body.as_bytes();
    if b.is_empty() || b[b.len() - 1] == b'_' || (b[0] == b'_' && !after_prefix) || body.contains("__") {
        return Err(bad());
    }
    let digits: String = body.chars().filter(|c| *c != '_').collect();
    if digits.is_empty() || !digits.chars().all(|c| c.to_digit(base).is_some()) {
        return Err(bad());
    }
    if base == 10 && digits.len() > 4300 {
        return Err(Exc::value_error(format!(
            "Exceeds the limit (4300 digits) for integer string conversion: value has {} digits; use sys.set_int_max_str_digits() to increase the limit",
            digits.len()
        )));
    }
    let v = BigInt::parse_bytes(digits.as_bytes(), base).ok_or_else(bad)?;
    Ok(if neg { -v } else { v })
}

/// `float(s)` for a str: CPython's grammar (Rust's parser reads the same one), digits of any script
fn py_float(s: &str) -> R<f64> {
    let bad = || Exc::value_error(format!("could not convert string to float: {}", py_repr(s)));
    let t = ascii_number(s).ok_or_else(bad)?;
    let (sign, body) = match t.as_bytes().first() {
        Some(b'-') => (-1.0, &t[1..]),
        Some(b'+') => (1.0, &t[1..]),
        _ => (1.0, &t[..]),
    };
    let low = body.to_ascii_lowercase();
    match low.as_str() {
        "inf" | "infinity" => return Ok(sign * f64::INFINITY),
        "nan" => return Ok(f64::NAN.copysign(sign)),
        _ => {}
    }
    let ok = {
        let b = low.as_bytes();
        let mut i = 0;
        let int = b.iter().take_while(|c| c.is_ascii_digit()).count();
        i += int;
        let mut frac = 0;
        if i < b.len() && b[i] == b'.' {
            i += 1;
            frac = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            i += frac;
        }
        let mut ok = int + frac > 0;
        if ok && i < b.len() && b[i] == b'e' {
            i += 1;
            if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                i += 1;
            }
            let e = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            ok = e > 0;
            i += e;
        }
        ok && i == b.len()
    };
    if !ok {
        return Err(bad());
    }
    low.parse::<f64>().map(|x| sign * x).map_err(|_| bad())
}

fn big_to_v(v: &BigInt, text: &str) -> R {
    num_traits::ToPrimitive::to_i64(v).map(V::Int).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, format!("py2axum: the YAML integer {text} does not fit in 64 bits")))
}

// ---------------------------------------------------------------- marks and errors

/// `yaml.error.Mark`: a position in the stream; `buffer` is the whole text for a str or bytes stream (None for a
/// file: no snippet)
pub struct Mark {
    pub name: V,
    pub index: usize,
    pub line: usize,
    pub column: usize,
    pub buffer: Option<Arc<[char]>>,
    pub pointer: usize,
}

impl Mark {
    /// `get_snippet(indent, max_length)`
    pub fn snippet(&self, indent: i64, max_length: f64) -> Option<String> {
        let buf = self.buffer.as_ref()?;
        let half = max_length / 2.0 - 1.0;
        let stop = |c: char| is_in(c, "\0\r\n\u{85}\u{2028}\u{2029}");
        let mut head = "";
        let mut start = self.pointer as i64;
        while start > 0 && !stop(buf[start as usize - 1]) {
            start -= 1;
            if (self.pointer as i64 - start) as f64 > half {
                head = " ... ";
                start += 5;
                break;
            }
        }
        let mut tail = "";
        let mut end = self.pointer as i64;
        while (end as usize) < buf.len() && !stop(buf[end as usize]) {
            end += 1;
            if (end - self.pointer as i64) as f64 > half {
                tail = " ... ";
                end -= 5;
                break;
            }
        }
        let n = buf.len() as i64;
        let (a, b) = (start.clamp(0, n), end.clamp(0, n));
        let snippet: String = if a < b { buf[a as usize..b as usize].iter().collect() } else { String::new() };
        let pad = |k: i64| " ".repeat(k.max(0) as usize);
        Some(format!("{}{head}{snippet}{tail}\n{}^", pad(indent), pad(indent + self.pointer as i64 - start + head.len() as i64)))
    }

    /// `str(mark)`
    pub fn text(&self) -> String {
        let name = ops::str_(&self.name).unwrap_or_default();
        let mut s = format!("  in \"{name}\", line {}, column {}", self.line + 1, self.column + 1);
        if let Some(sn) = self.snippet(4, 75.0) {
            s.push_str(":\n");
            s.push_str(&sn);
        }
        s
    }
}

fn mark_v(m: &Mark) -> V {
    V::native(Native::YamlMark(Arc::new(Mark { name: m.name.clone(), index: m.index, line: m.line, column: m.column, buffer: m.buffer.clone(), pointer: m.pointer })))
}

impl Clone for Mark {
    fn clone(&self) -> Self {
        Mark { name: self.name.clone(), index: self.index, line: self.line, column: self.column, buffer: self.buffer.clone(), pointer: self.pointer }
    }
}

/// A MarkedYAMLError subclass raised as PyYAML does: `Class(context, context_mark, problem, problem_mark)`
fn marked(class: &'static Class, context: Option<String>, cmark: Option<&Mark>, problem: String, pmark: Option<&Mark>) -> Exc {
    Exc::new(class, vec![context.map(V::str).unwrap_or(V::None), cmark.map(mark_v).unwrap_or(V::None), V::str(problem), pmark.map(mark_v).unwrap_or(V::None)])
}

fn scan_err(context: Option<String>, cmark: Option<&Mark>, problem: String, pmark: &Mark) -> Exc {
    marked(&YAML_SCANNER_ERROR, context, cmark, problem, Some(pmark))
}

fn reader_err(name: &V, position: usize, character: i64, encoding: &str, reason: &str) -> Exc {
    Exc::new(&YAML_READER_ERROR, vec![name.clone(), V::Int(position as i64), V::Int(character), V::str(encoding), V::str(reason)])
}

const MARKED_FIELDS: &[&str] = &["context", "context_mark", "problem", "problem_mark", "note"];
const READER_FIELDS: &[&str] = &["name", "position", "character", "encoding", "reason"];

/// An attribute PyYAML's `__init__` sets on its exceptions (from the arguments they were raised with)
pub fn exc_attr(e: &Exc, name: &str) -> Option<V> {
    let fields: &[&str] = if e.isinstance(&YAML_MARKED_ERROR) {
        MARKED_FIELDS
    } else if e.isinstance(&YAML_READER_ERROR) {
        READER_FIELDS
    } else {
        return None;
    };
    let i = fields.iter().position(|f| *f == name)?;
    if let Some(v) = e.0.attrs.lock().get(name) {
        return Some(v.clone());
    }
    Some(e.args().get(i).cloned().unwrap_or(V::None))
}

fn field(e: &Exc, name: &str) -> V {
    exc_attr(e, name).unwrap_or(V::None)
}

fn mark_of(v: &V) -> Option<Arc<Mark>> {
    match v {
        V::Native(n) => match &**n {
            Native::YamlMark(m) => Some(m.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn text_of(v: &V) -> String {
    match mark_of(v) {
        Some(m) => m.text(),
        None => ops::str_(v).unwrap_or_default(),
    }
}

/// `str(exc)` of PyYAML's exceptions: MarkedYAMLError.__str__ and ReaderError.__str__
pub fn exc_str(e: &Exc) -> Option<String> {
    if e.isinstance(&YAML_MARKED_ERROR) {
        let (context, cmark, problem, pmark, note) = (field(e, "context"), field(e, "context_mark"), field(e, "problem"), field(e, "problem_mark"), field(e, "note"));
        let mut lines = vec![];
        if !context.is_none() {
            lines.push(ops::str_(&context).unwrap_or_default());
        }
        if !cmark.is_none() {
            let same = match (mark_of(&cmark), mark_of(&pmark)) {
                (Some(a), Some(b)) => ops::eq_bool(&a.name, &b.name) && a.line == b.line && a.column == b.column,
                _ => false,
            };
            if problem.is_none() || pmark.is_none() || !same {
                lines.push(text_of(&cmark));
            }
        }
        if !problem.is_none() {
            lines.push(ops::str_(&problem).unwrap_or_default());
        }
        if !pmark.is_none() {
            lines.push(text_of(&pmark));
        }
        if !note.is_none() {
            lines.push(ops::str_(&note).unwrap_or_default());
        }
        return Some(lines.join("\n"));
    }
    if e.isinstance(&YAML_READER_ERROR) && e.args().len() == 5 {
        let a = e.args();
        let name = ops::str_(&a[0]).unwrap_or_default();
        let pos = ops::str_(&a[1]).unwrap_or_default();
        let ch = match &a[2] {
            V::Int(c) => *c,
            _ => 0,
        };
        let (enc, reason) = (ops::str_(&a[3]).unwrap_or_default(), ops::str_(&a[4]).unwrap_or_default());
        // a decoding error keeps the undecodable byte as an int (bytes indexing): the same message as a character
        let _ = enc;
        return Some(format!("unacceptable character #x{ch:04x}: {reason}\n  in \"{name}\", position {pos}"));
    }
    None
}

/// `mark.<name>`
pub fn mark_attr(m: &Mark, name: &str) -> R {
    Ok(match name {
        "name" => m.name.clone(),
        "index" => V::Int(m.index as i64),
        "line" => V::Int(m.line as i64),
        "column" => V::Int(m.column as i64),
        "buffer" => match &m.buffer {
            Some(b) => V::str(b.iter().collect::<String>()),
            None => V::None,
        },
        "pointer" => match &m.buffer {
            Some(_) => V::Int(m.pointer as i64),
            None => V::None,
        },
        _ => return Err(Exc::attr_error(format!("'Mark' object has no attribute '{name}'"))),
    })
}

/// `mark.get_snippet(indent=4, max_length=75)`
pub fn mark_method(m: &Mark, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "get_snippet" => {}
        _ => return Err(Exc::attr_error(format!("'Mark' object has no attribute '{name}'"))),
    }
    let get = |i: usize, k: &str| args.get(i).or_else(|| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v));
    let indent = match get(0, "indent") {
        None => 4,
        Some(V::Int(i)) => *i,
        Some(V::Bool(b)) => *b as i64,
        Some(_) => return Err(Exc::type_error("py2axum: Mark.get_snippet(indent=) takes an int")),
    };
    let max = match get(1, "max_length") {
        None => 75.0,
        Some(V::Int(i)) => *i as f64,
        Some(V::Float(f)) => *f,
        Some(_) => return Err(Exc::type_error("py2axum: Mark.get_snippet(max_length=) takes a number")),
    };
    Ok(m.snippet(indent, max).map(V::str).unwrap_or(V::None))
}

// ---------------------------------------------------------------- reader

#[derive(Clone, Copy)]
enum Codec {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Codec::Utf8 => "utf-8",
            Codec::Utf16Le => "utf-16-le",
            Codec::Utf16Be => "utf-16-be",
        }
    }
}

/// `codecs.utf_8_decode` / `utf_16_*_decode(data, 'strict', final)`: (text, bytes consumed), or the error's
/// (start, reason)
fn decode(codec: Codec, b: &[u8], fin: bool) -> Result<(Vec<char>, usize), (usize, &'static str)> {
    match codec {
        Codec::Utf8 => match std::str::from_utf8(b) {
            Ok(s) => Ok((s.chars().collect(), b.len())),
            Err(e) => {
                let p = e.valid_up_to();
                match e.error_len() {
                    None if !fin => Ok((std::str::from_utf8(&b[..p]).unwrap().chars().collect(), p)),
                    None => Err((p, "unexpected end of data")),
                    Some(_) => Err((p, if matches!(b[p], 0xc2..=0xf4) { "invalid continuation byte" } else { "invalid start byte" })),
                }
            }
        },
        Codec::Utf16Le | Codec::Utf16Be => {
            let le = matches!(codec, Codec::Utf16Le);
            let unit = |i: usize| if le { u16::from_le_bytes([b[i], b[i + 1]]) } else { u16::from_be_bytes([b[i], b[i + 1]]) };
            let mut out = vec![];
            let mut q = 0;
            loop {
                if q + 2 > b.len() {
                    // an odd byte left
                    if q == b.len() || !fin {
                        return Ok((out, q));
                    }
                    return Err((q, "truncated data"));
                }
                let ch = unit(q);
                q += 2;
                if !(0xD800..=0xDFFF).contains(&ch) {
                    out.push(char::from_u32(ch as u32).unwrap());
                    continue;
                }
                if ch >= 0xDC00 {
                    return Err((q - 2, "illegal encoding"));
                }
                if q + 2 > b.len() {
                    if !fin {
                        return Ok((out, q - 2));
                    }
                    return Err((q - 2, "unexpected end of data"));
                }
                let ch2 = unit(q);
                q += 2;
                if !(0xDC00..=0xDFFF).contains(&ch2) {
                    return Err((q - 4, "illegal UTF-16 surrogate"));
                }
                out.push(char::from_u32(0x10000 + (((ch as u32) - 0xD800) << 10) + ((ch2 as u32) - 0xDC00)).unwrap());
            }
        }
    }
}

enum Raw {
    None,
    Text(Vec<char>),
    Bytes(Vec<u8>),
}

impl Raw {
    fn len(&self) -> usize {
        match self {
            Raw::None => 0,
            Raw::Text(t) => t.len(),
            Raw::Bytes(b) => b.len(),
        }
    }
}

/// A file-like stream, read as PyYAML reads it: `stream.read(4096)` until it returns nothing
enum Chunks {
    None,
    Text(Vec<char>, usize),
    Bytes(Vec<u8>, usize),
}

struct Reader {
    name: V,
    stream: bool,
    stream_pointer: usize,
    eof: bool,
    buffer: Vec<char>,
    pointer: usize,
    raw: Raw,
    codec: Option<Codec>,
    index: usize,
    line: usize,
    column: usize,
    src: Chunks,
    /// the whole buffer of a str/bytes stream, shared by the marks
    frozen: Option<Arc<[char]>>,
}

fn allowed(c: char) -> bool {
    matches!(c, '\x09' | '\x0A' | '\x0D' | '\x20'..='\x7E' | '\u{85}' | '\u{A0}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

impl Reader {
    fn blank(name: V) -> Reader {
        Reader {
            name,
            stream: false,
            stream_pointer: 0,
            eof: true,
            buffer: vec![],
            pointer: 0,
            raw: Raw::None,
            codec: None,
            index: 0,
            line: 0,
            column: 0,
            src: Chunks::None,
            frozen: None,
        }
    }

    fn from_str(s: &str) -> R<Reader> {
        let mut r = Reader::blank(V::str("<unicode string>"));
        let chars: Vec<char> = s.chars().collect();
        r.check_printable(&chars)?;
        r.buffer = chars;
        r.buffer.push('\0');
        r.frozen = Some(Arc::from(r.buffer.clone()));
        Ok(r)
    }

    fn from_bytes(b: &[u8]) -> R<Reader> {
        let mut r = Reader::blank(V::str("<byte string>"));
        r.raw = Raw::Bytes(b.to_vec());
        r.determine_encoding()?;
        r.frozen = Some(Arc::from(r.buffer.clone()));
        Ok(r)
    }

    fn from_stream(name: V, src: Chunks) -> R<Reader> {
        let mut r = Reader::blank(name);
        r.stream = true;
        r.eof = false;
        r.src = src;
        r.determine_encoding()?;
        Ok(r)
    }

    fn peek(&mut self, index: usize) -> R<char> {
        if let Some(c) = self.buffer.get(self.pointer + index) {
            return Ok(*c);
        }
        self.update(index + 1)?;
        Ok(self.buffer.get(self.pointer + index).copied().unwrap_or('\0'))
    }

    fn prefix(&mut self, length: usize) -> R<String> {
        if self.pointer + length >= self.buffer.len() {
            self.update(length)?;
        }
        let end = (self.pointer + length).min(self.buffer.len());
        Ok(self.buffer[self.pointer.min(end)..end].iter().collect())
    }

    fn forward(&mut self, mut length: usize) -> R<()> {
        if self.pointer + length + 1 >= self.buffer.len() {
            self.update(length + 1)?;
        }
        while length > 0 {
            let ch = self.buffer.get(self.pointer).copied().unwrap_or('\0');
            self.pointer += 1;
            self.index += 1;
            if is_in(ch, "\n\u{85}\u{2028}\u{2029}") || (ch == '\r' && self.buffer.get(self.pointer).copied().unwrap_or('\0') != '\n') {
                self.line += 1;
                self.column = 0;
            } else if ch != '\u{FEFF}' {
                self.column += 1;
            }
            length -= 1;
        }
        Ok(())
    }

    fn mark(&self) -> Mark {
        Mark {
            name: self.name.clone(),
            index: self.index,
            line: self.line,
            column: self.column,
            buffer: if self.stream { None } else { self.frozen.clone() },
            pointer: self.pointer,
        }
    }

    fn determine_encoding(&mut self) -> R<()> {
        while !self.eof && (matches!(self.raw, Raw::None) || self.raw.len() < 2) {
            self.update_raw();
        }
        if let Raw::Bytes(b) = &self.raw {
            self.codec = Some(if b.starts_with(&[0xFF, 0xFE]) {
                Codec::Utf16Le
            } else if b.starts_with(&[0xFE, 0xFF]) {
                Codec::Utf16Be
            } else {
                Codec::Utf8
            });
        }
        self.update(1)
    }

    fn check_printable(&self, data: &[char]) -> R<()> {
        if let Some(i) = data.iter().position(|c| !allowed(*c)) {
            let position = self.index + (self.buffer.len() - self.pointer) + i;
            return Err(reader_err(&self.name, position, data[i] as i64, "unicode", "special characters are not allowed"));
        }
        Ok(())
    }

    fn update(&mut self, length: usize) -> R<()> {
        if matches!(self.raw, Raw::None) {
            return Ok(());
        }
        self.buffer.drain(..self.pointer);
        self.pointer = 0;
        while self.buffer.len() < length {
            if !self.eof {
                self.update_raw();
            }
            let (data, converted) = match (&self.raw, self.codec) {
                (Raw::Bytes(b), Some(codec)) => match decode(codec, b, self.eof) {
                    Ok(x) => x,
                    Err((start, reason)) => {
                        let position = if self.stream { self.stream_pointer - b.len() + start } else { start };
                        return Err(reader_err(&self.name, position, b[start] as i64, codec.name(), reason));
                    }
                },
                (Raw::Text(t), _) => (t.clone(), t.len()),
                _ => (vec![], 0),
            };
            self.check_printable(&data)?;
            self.buffer.extend(data);
            match &mut self.raw {
                Raw::Bytes(b) => {
                    b.drain(..converted);
                }
                Raw::Text(t) => {
                    t.drain(..converted);
                }
                Raw::None => {}
            }
            if self.eof {
                self.buffer.push('\0');
                self.raw = Raw::None;
                break;
            }
        }
        Ok(())
    }

    fn update_raw(&mut self) {
        const SIZE: usize = 4096;
        let n = match &mut self.src {
            Chunks::Text(t, pos) => {
                let end = (*pos + SIZE).min(t.len());
                let data = t[*pos..end].to_vec();
                *pos = end;
                let n = data.len();
                match &mut self.raw {
                    Raw::Text(r) => r.extend(data),
                    _ => self.raw = Raw::Text(data),
                }
                n
            }
            Chunks::Bytes(b, pos) => {
                let end = (*pos + SIZE).min(b.len());
                let data = b[*pos..end].to_vec();
                *pos = end;
                let n = data.len();
                match &mut self.raw {
                    Raw::Bytes(r) => r.extend(data),
                    _ => self.raw = Raw::Bytes(data),
                }
                n
            }
            Chunks::None => 0,
        };
        self.stream_pointer += n;
        if n == 0 {
            self.eof = true;
        }
    }
}

// ---------------------------------------------------------------- scanner

#[derive(Clone)]
enum Tok {
    StreamStart,
    StreamEnd,
    /// name, value: YAML (major is 1), TAG (handle, prefix), or none
    Directive(String, DirVal),
    DocStart,
    DocEnd,
    BlockSeqStart,
    BlockMapStart,
    BlockEnd,
    FlowSeqStart,
    FlowMapStart,
    FlowSeqEnd,
    FlowMapEnd,
    Key,
    Value,
    BlockEntry,
    FlowEntry,
    Alias(String),
    Anchor(String),
    /// (handle, suffix)
    Tag(Option<String>, String),
    /// value, plain, style
    Scalar(String, bool, Option<char>),
}

#[derive(Clone)]
enum DirVal {
    None,
    Yaml(bool),
    Tag(String, String),
}

impl Tok {
    fn id(&self) -> &'static str {
        match self {
            Tok::StreamStart => "<stream start>",
            Tok::StreamEnd => "<stream end>",
            Tok::Directive(..) => "<directive>",
            Tok::DocStart => "<document start>",
            Tok::DocEnd => "<document end>",
            Tok::BlockSeqStart => "<block sequence start>",
            Tok::BlockMapStart => "<block mapping start>",
            Tok::BlockEnd => "<block end>",
            Tok::FlowSeqStart => "[",
            Tok::FlowMapStart => "{",
            Tok::FlowSeqEnd => "]",
            Tok::FlowMapEnd => "}",
            Tok::Key => "?",
            Tok::Value => ":",
            Tok::BlockEntry => "-",
            Tok::FlowEntry => ",",
            Tok::Alias(_) => "<alias>",
            Tok::Anchor(_) => "<anchor>",
            Tok::Tag(..) => "<tag>",
            Tok::Scalar(..) => "<scalar>",
        }
    }
}

#[derive(Clone)]
struct Token {
    t: Tok,
    start: Mark,
    end: Mark,
}

struct SimpleKey {
    token_number: usize,
    required: bool,
    index: usize,
    line: usize,
    column: usize,
    mark: Mark,
}

struct Scanner {
    r: Reader,
    done: bool,
    flow_level: i64,
    tokens: VecDeque<Token>,
    tokens_taken: usize,
    indent: i64,
    indents: Vec<i64>,
    allow_simple_key: bool,
    possible_simple_keys: IndexMap<i64, SimpleKey>,
}

macro_rules! tok {
    ($t:expr, $s:expr, $e:expr) => {
        Token { t: $t, start: $s, end: $e }
    };
}

impl Scanner {
    fn new(r: Reader) -> Scanner {
        let mut s = Scanner {
            r,
            done: false,
            flow_level: 0,
            tokens: VecDeque::new(),
            tokens_taken: 0,
            indent: -1,
            indents: vec![],
            allow_simple_key: true,
            possible_simple_keys: IndexMap::new(),
        };
        let m = s.r.mark();
        s.tokens.push_back(tok!(Tok::StreamStart, m.clone(), m));
        s
    }

    fn fill(&mut self) -> R<()> {
        while self.need_more_tokens()? {
            self.fetch_more_tokens()?;
        }
        Ok(())
    }

    fn check(&mut self, f: impl Fn(&Tok) -> bool) -> R<bool> {
        self.fill()?;
        Ok(self.tokens.front().map(|t| f(&t.t)).unwrap_or(false))
    }

    fn peek_token(&mut self) -> R<Token> {
        self.fill()?;
        self.tokens.front().cloned().ok_or_else(|| Exc::runtime("py2axum: YAML scanner exhausted"))
    }

    fn get_token(&mut self) -> R<Token> {
        self.fill()?;
        let t = self.tokens.pop_front().ok_or_else(|| Exc::runtime("py2axum: YAML scanner exhausted"))?;
        self.tokens_taken += 1;
        Ok(t)
    }

    fn need_more_tokens(&mut self) -> R<bool> {
        if self.done {
            return Ok(false);
        }
        if self.tokens.is_empty() {
            return Ok(true);
        }
        self.stale_possible_simple_keys()?;
        Ok(self.next_possible_simple_key() == Some(self.tokens_taken))
    }

    fn fetch_more_tokens(&mut self) -> R<()> {
        self.scan_to_next_token()?;
        self.stale_possible_simple_keys()?;
        self.unwind_indent(self.r.column as i64);
        let ch = self.r.peek(0)?;
        if ch == '\0' {
            return self.fetch_stream_end();
        }
        if ch == '%' && self.r.column == 0 {
            return self.fetch_directive();
        }
        if ch == '-' && self.check_document_indicator("---")? {
            return self.fetch_document_indicator(Tok::DocStart);
        }
        if ch == '.' && self.check_document_indicator("...")? {
            return self.fetch_document_indicator(Tok::DocEnd);
        }
        match ch {
            '[' => return self.fetch_flow_collection_start(Tok::FlowSeqStart),
            '{' => return self.fetch_flow_collection_start(Tok::FlowMapStart),
            ']' => return self.fetch_flow_collection_end(Tok::FlowSeqEnd),
            '}' => return self.fetch_flow_collection_end(Tok::FlowMapEnd),
            ',' => return self.fetch_flow_entry(),
            _ => {}
        }
        if ch == '-' && is_in(self.r.peek(1)?, BZT) {
            return self.fetch_block_entry();
        }
        if ch == '?' && (self.flow_level != 0 || is_in(self.r.peek(1)?, BZT)) {
            return self.fetch_key();
        }
        if ch == ':' && (self.flow_level != 0 || is_in(self.r.peek(1)?, BZT)) {
            return self.fetch_value();
        }
        match ch {
            '*' => return self.fetch_anchor(true),
            '&' => return self.fetch_anchor(false),
            '!' => return self.fetch_tag(),
            '|' if self.flow_level == 0 => return self.fetch_block_scalar(false),
            '>' if self.flow_level == 0 => return self.fetch_block_scalar(true),
            '\'' => return self.fetch_flow_scalar(false),
            '"' => return self.fetch_flow_scalar(true),
            _ => {}
        }
        if self.check_plain()? {
            return self.fetch_plain();
        }
        Err(scan_err(Some("while scanning for the next token".into()), None, format!("found character {} that cannot start any token", char_repr(ch)), &self.r.mark()))
    }

    fn next_possible_simple_key(&self) -> Option<usize> {
        let mut min: Option<usize> = None;
        for k in self.possible_simple_keys.values() {
            if min.is_none() || k.token_number < min.unwrap() {
                min = Some(k.token_number);
            }
        }
        min
    }

    fn stale_possible_simple_keys(&mut self) -> R<()> {
        let levels: Vec<i64> = self.possible_simple_keys.keys().copied().collect();
        for level in levels {
            let key = &self.possible_simple_keys[&level];
            if key.line != self.r.line || self.r.index - key.index > 1024 {
                if key.required {
                    return Err(scan_err(Some("while scanning a simple key".into()), Some(&key.mark), "could not find expected ':'".into(), &self.r.mark()));
                }
                self.possible_simple_keys.shift_remove(&level);
            }
        }
        Ok(())
    }

    fn save_possible_simple_key(&mut self) -> R<()> {
        let required = self.flow_level == 0 && self.indent == self.r.column as i64;
        if self.allow_simple_key {
            self.remove_possible_simple_key()?;
            let token_number = self.tokens_taken + self.tokens.len();
            let key = SimpleKey { token_number, required, index: self.r.index, line: self.r.line, column: self.r.column, mark: self.r.mark() };
            self.possible_simple_keys.insert(self.flow_level, key);
        }
        Ok(())
    }

    fn remove_possible_simple_key(&mut self) -> R<()> {
        if let Some(key) = self.possible_simple_keys.get(&self.flow_level) {
            if key.required {
                return Err(scan_err(Some("while scanning a simple key".into()), Some(&key.mark), "could not find expected ':'".into(), &self.r.mark()));
            }
            self.possible_simple_keys.shift_remove(&self.flow_level);
        }
        Ok(())
    }

    fn unwind_indent(&mut self, column: i64) {
        if self.flow_level != 0 {
            return;
        }
        while self.indent > column {
            let m = self.r.mark();
            self.indent = self.indents.pop().unwrap_or(-1);
            self.tokens.push_back(tok!(Tok::BlockEnd, m.clone(), m));
        }
    }

    fn add_indent(&mut self, column: i64) -> bool {
        if self.indent < column {
            self.indents.push(self.indent);
            self.indent = column;
            return true;
        }
        false
    }

    fn fetch_stream_end(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        self.possible_simple_keys.clear();
        let m = self.r.mark();
        self.tokens.push_back(tok!(Tok::StreamEnd, m.clone(), m));
        self.done = true;
        Ok(())
    }

    fn fetch_directive(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_directive()?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn check_document_indicator(&mut self, ind: &str) -> R<bool> {
        Ok(self.r.column == 0 && self.r.prefix(3)? == ind && is_in(self.r.peek(3)?, BZT))
    }

    fn fetch_document_indicator(&mut self, t: Tok) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        let s = self.r.mark();
        self.r.forward(3)?;
        let e = self.r.mark();
        self.tokens.push_back(tok!(t, s, e));
        Ok(())
    }

    fn fetch_flow_collection_start(&mut self, t: Tok) -> R<()> {
        self.save_possible_simple_key()?;
        self.flow_level += 1;
        self.allow_simple_key = true;
        let s = self.r.mark();
        self.r.forward(1)?;
        let e = self.r.mark();
        self.tokens.push_back(tok!(t, s, e));
        Ok(())
    }

    fn fetch_flow_collection_end(&mut self, t: Tok) -> R<()> {
        self.remove_possible_simple_key()?;
        // an unbalanced ']' / '}' takes the level below 0, as in Python (then the parser fails)
        self.flow_level -= 1;
        self.allow_simple_key = false;
        let s = self.r.mark();
        self.r.forward(1)?;
        let e = self.r.mark();
        self.tokens.push_back(tok!(t, s, e));
        Ok(())
    }
}

impl Scanner {
    fn fetch_flow_entry(&mut self) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let s = self.r.mark();
        self.r.forward(1)?;
        let e = self.r.mark();
        self.tokens.push_back(tok!(Tok::FlowEntry, s, e));
        Ok(())
    }

    fn fetch_block_entry(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(scan_err(None, None, "sequence entries are not allowed here".into(), &self.r.mark()));
            }
            if self.add_indent(self.r.column as i64) {
                let m = self.r.mark();
                self.tokens.push_back(tok!(Tok::BlockSeqStart, m.clone(), m));
            }
        }
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let s = self.r.mark();
        self.r.forward(1)?;
        let e = self.r.mark();
        self.tokens.push_back(tok!(Tok::BlockEntry, s, e));
        Ok(())
    }

    fn fetch_key(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(scan_err(None, None, "mapping keys are not allowed here".into(), &self.r.mark()));
            }
            if self.add_indent(self.r.column as i64) {
                let m = self.r.mark();
                self.tokens.push_back(tok!(Tok::BlockMapStart, m.clone(), m));
            }
        }
        self.allow_simple_key = self.flow_level == 0;
        self.remove_possible_simple_key()?;
        let s = self.r.mark();
        self.r.forward(1)?;
        let e = self.r.mark();
        self.tokens.push_back(tok!(Tok::Key, s, e));
        Ok(())
    }

    fn fetch_value(&mut self) -> R<()> {
        if let Some(key) = self.possible_simple_keys.shift_remove(&self.flow_level) {
            let at = key.token_number - self.tokens_taken;
            self.tokens.insert(at, tok!(Tok::Key, key.mark.clone(), key.mark.clone()));
            if self.flow_level == 0 && self.add_indent(key.column as i64) {
                self.tokens.insert(at, tok!(Tok::BlockMapStart, key.mark.clone(), key.mark.clone()));
            }
            self.allow_simple_key = false;
        } else {
            if self.flow_level == 0 && !self.allow_simple_key {
                return Err(scan_err(None, None, "mapping values are not allowed here".into(), &self.r.mark()));
            }
            if self.flow_level == 0 && self.add_indent(self.r.column as i64) {
                let m = self.r.mark();
                self.tokens.push_back(tok!(Tok::BlockMapStart, m.clone(), m));
            }
            self.allow_simple_key = self.flow_level == 0;
            self.remove_possible_simple_key()?;
        }
        let s = self.r.mark();
        self.r.forward(1)?;
        let e = self.r.mark();
        self.tokens.push_back(tok!(Tok::Value, s, e));
        Ok(())
    }

    fn fetch_anchor(&mut self, alias: bool) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_anchor(alias)?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_tag(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_tag()?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_block_scalar(&mut self, folded: bool) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let t = self.scan_block_scalar(folded)?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, double: bool) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_flow_scalar(double)?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_plain(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_plain()?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn check_plain(&mut self) -> R<bool> {
        let ch = self.r.peek(0)?;
        if !is_in(ch, "\0 \t\r\n\u{85}\u{2028}\u{2029}-?:,[]{}#&*!|>'\"%@`") {
            return Ok(true);
        }
        Ok(!is_in(self.r.peek(1)?, BZT) && (ch == '-' || (self.flow_level == 0 && is_in(ch, "?:"))))
    }

    fn scan_to_next_token(&mut self) -> R<()> {
        if self.r.index == 0 && self.r.peek(0)? == '\u{FEFF}' {
            self.r.forward(1)?;
        }
        loop {
            while self.r.peek(0)? == ' ' {
                self.r.forward(1)?;
            }
            if self.r.peek(0)? == '#' {
                while !is_in(self.r.peek(0)?, ZBREAK) {
                    self.r.forward(1)?;
                }
            }
            if !self.scan_line_break()?.is_empty() {
                if self.flow_level == 0 {
                    self.allow_simple_key = true;
                }
            } else {
                return Ok(());
            }
        }
    }

    fn directive_err(&self, start: &Mark, problem: String) -> Exc {
        scan_err(Some("while scanning a directive".into()), Some(start), problem, &self.r.mark())
    }

    fn scan_directive(&mut self) -> R<Token> {
        let start = self.r.mark();
        self.r.forward(1)?;
        let name = self.scan_directive_name(&start)?;
        let (value, end) = if name == "YAML" {
            let v = self.scan_yaml_directive_value(&start)?;
            (v, self.r.mark())
        } else if name == "TAG" {
            let v = self.scan_tag_directive_value(&start)?;
            (v, self.r.mark())
        } else {
            let end = self.r.mark();
            while !is_in(self.r.peek(0)?, ZBREAK) {
                self.r.forward(1)?;
            }
            (DirVal::None, end)
        };
        self.scan_directive_ignored_line(&start)?;
        Ok(tok!(Tok::Directive(name, value), start, end))
    }

    fn scan_directive_name(&mut self, start: &Mark) -> R<String> {
        let mut length = 0;
        let mut ch = self.r.peek(length)?;
        while alnum(ch) {
            length += 1;
            ch = self.r.peek(length)?;
        }
        if length == 0 {
            return Err(self.directive_err(start, format!("expected alphabetic or numeric character, but found {}", char_repr(ch))));
        }
        let value = self.r.prefix(length)?;
        self.r.forward(length)?;
        let ch = self.r.peek(0)?;
        if !is_in(ch, BZ) {
            return Err(self.directive_err(start, format!("expected alphabetic or numeric character, but found {}", char_repr(ch))));
        }
        Ok(value)
    }

    fn scan_yaml_directive_value(&mut self, start: &Mark) -> R<DirVal> {
        while self.r.peek(0)? == ' ' {
            self.r.forward(1)?;
        }
        let major = self.scan_yaml_directive_number(start)?;
        if self.r.peek(0)? != '.' {
            let c = self.r.peek(0)?;
            return Err(self.directive_err(start, format!("expected a digit or '.', but found {}", char_repr(c))));
        }
        self.r.forward(1)?;
        self.scan_yaml_directive_number(start)?;
        if !is_in(self.r.peek(0)?, BZ) {
            let c = self.r.peek(0)?;
            return Err(self.directive_err(start, format!("expected a digit or ' ', but found {}", char_repr(c))));
        }
        Ok(DirVal::Yaml(major.trim_start_matches('0') == "1"))
    }

    fn scan_yaml_directive_number(&mut self, start: &Mark) -> R<String> {
        let ch = self.r.peek(0)?;
        if !ch.is_ascii_digit() {
            return Err(self.directive_err(start, format!("expected a digit, but found {}", char_repr(ch))));
        }
        let mut length = 0;
        while self.r.peek(length)?.is_ascii_digit() {
            length += 1;
        }
        let value = self.r.prefix(length)?;
        self.r.forward(length)?;
        Ok(value)
    }

    fn scan_tag_directive_value(&mut self, start: &Mark) -> R<DirVal> {
        while self.r.peek(0)? == ' ' {
            self.r.forward(1)?;
        }
        let handle = self.scan_tag_handle("directive", start)?;
        let ch = self.r.peek(0)?;
        if ch != ' ' {
            return Err(self.directive_err(start, format!("expected ' ', but found {}", char_repr(ch))));
        }
        while self.r.peek(0)? == ' ' {
            self.r.forward(1)?;
        }
        let prefix = self.scan_tag_uri("directive", start)?;
        let ch = self.r.peek(0)?;
        if !is_in(ch, BZ) {
            return Err(self.directive_err(start, format!("expected ' ', but found {}", char_repr(ch))));
        }
        Ok(DirVal::Tag(handle, prefix))
    }

    fn scan_directive_ignored_line(&mut self, start: &Mark) -> R<()> {
        while self.r.peek(0)? == ' ' {
            self.r.forward(1)?;
        }
        if self.r.peek(0)? == '#' {
            while !is_in(self.r.peek(0)?, ZBREAK) {
                self.r.forward(1)?;
            }
        }
        let ch = self.r.peek(0)?;
        if !is_in(ch, ZBREAK) {
            return Err(self.directive_err(start, format!("expected a comment or a line break, but found {}", char_repr(ch))));
        }
        self.scan_line_break()?;
        Ok(())
    }

    fn scan_anchor(&mut self, alias: bool) -> R<Token> {
        let start = self.r.mark();
        let name = if alias { "alias" } else { "anchor" };
        self.r.forward(1)?;
        let mut length = 0;
        let mut ch = self.r.peek(length)?;
        while alnum(ch) {
            length += 1;
            ch = self.r.peek(length)?;
        }
        if length == 0 {
            return Err(scan_err(Some(format!("while scanning an {name}")), Some(&start), format!("expected alphabetic or numeric character, but found {}", char_repr(ch)), &self.r.mark()));
        }
        let value = self.r.prefix(length)?;
        self.r.forward(length)?;
        let ch = self.r.peek(0)?;
        if !is_in(ch, "\0 \t\r\n\u{85}\u{2028}\u{2029}?:,]}%@`") {
            return Err(scan_err(Some(format!("while scanning an {name}")), Some(&start), format!("expected alphabetic or numeric character, but found {}", char_repr(ch)), &self.r.mark()));
        }
        let end = self.r.mark();
        Ok(tok!(if alias { Tok::Alias(value) } else { Tok::Anchor(value) }, start, end))
    }

    fn scan_tag(&mut self) -> R<Token> {
        let start = self.r.mark();
        let mut ch = self.r.peek(1)?;
        let (handle, suffix) = if ch == '<' {
            self.r.forward(2)?;
            let suffix = self.scan_tag_uri("tag", &start)?;
            if self.r.peek(0)? != '>' {
                let c = self.r.peek(0)?;
                return Err(scan_err(Some("while parsing a tag".into()), Some(&start), format!("expected '>', but found {}", char_repr(c)), &self.r.mark()));
            }
            self.r.forward(1)?;
            (None, suffix)
        } else if is_in(ch, BZT) {
            self.r.forward(1)?;
            (None, "!".to_string())
        } else {
            let mut length = 1;
            let mut use_handle = false;
            while !is_in(ch, BZ) {
                if ch == '!' {
                    use_handle = true;
                    break;
                }
                length += 1;
                ch = self.r.peek(length)?;
            }
            let handle = if use_handle {
                self.scan_tag_handle("tag", &start)?
            } else {
                self.r.forward(1)?;
                "!".to_string()
            };
            let suffix = self.scan_tag_uri("tag", &start)?;
            (Some(handle), suffix)
        };
        let ch = self.r.peek(0)?;
        if !is_in(ch, BZ) {
            return Err(scan_err(Some("while scanning a tag".into()), Some(&start), format!("expected ' ', but found {}", char_repr(ch)), &self.r.mark()));
        }
        let end = self.r.mark();
        Ok(tok!(Tok::Tag(handle, suffix), start, end))
    }

    fn block_err(&self, start: &Mark, problem: String) -> Exc {
        scan_err(Some("while scanning a block scalar".into()), Some(start), problem, &self.r.mark())
    }

    fn scan_block_scalar(&mut self, folded: bool) -> R<Token> {
        let mut chunks = String::new();
        let start = self.r.mark();
        self.r.forward(1)?;
        let (chomping, increment) = self.scan_block_scalar_indicators(&start)?;
        self.scan_block_scalar_ignored_line(&start)?;
        let min_indent = (self.indent + 1).max(1);
        let (mut breaks, indent, mut end) = match increment {
            None => {
                let (breaks, max_indent, end) = self.scan_block_scalar_indentation()?;
                (breaks, min_indent.max(max_indent), end)
            }
            Some(inc) => {
                let indent = min_indent + inc - 1;
                let (breaks, end) = self.scan_block_scalar_breaks(indent)?;
                (breaks, indent, end)
            }
        };
        let mut line_break = String::new();
        while self.r.column as i64 == indent && self.r.peek(0)? != '\0' {
            chunks.extend(breaks.iter().map(|s| s.as_str()));
            let leading_non_space = !is_in(self.r.peek(0)?, " \t");
            let mut length = 0;
            while !is_in(self.r.peek(length)?, ZBREAK) {
                length += 1;
            }
            chunks.push_str(&self.r.prefix(length)?);
            self.r.forward(length)?;
            line_break = self.scan_line_break()?;
            let (b, e) = self.scan_block_scalar_breaks(indent)?;
            breaks = b;
            end = e;
            if self.r.column as i64 == indent && self.r.peek(0)? != '\0' {
                if folded && line_break == "\n" && leading_non_space && !is_in(self.r.peek(0)?, " \t") {
                    if breaks.is_empty() {
                        chunks.push(' ');
                    }
                } else {
                    chunks.push_str(&line_break);
                }
            } else {
                break;
            }
        }
        if chomping != Some(false) {
            chunks.push_str(&line_break);
        }
        if chomping == Some(true) {
            chunks.extend(breaks.iter().map(|s| s.as_str()));
        }
        Ok(tok!(Tok::Scalar(chunks, false, Some(if folded { '>' } else { '|' })), start, end))
    }

    fn scan_block_scalar_indicators(&mut self, start: &Mark) -> R<(Option<bool>, Option<i64>)> {
        let mut chomping = None;
        let mut increment = None;
        let mut ch = self.r.peek(0)?;
        if is_in(ch, "+-") {
            chomping = Some(ch == '+');
            self.r.forward(1)?;
            ch = self.r.peek(0)?;
            if ch.is_ascii_digit() {
                let inc = ch as i64 - '0' as i64;
                if inc == 0 {
                    return Err(self.block_err(start, "expected indentation indicator in the range 1-9, but found 0".into()));
                }
                increment = Some(inc);
                self.r.forward(1)?;
            }
        } else if ch.is_ascii_digit() {
            let inc = ch as i64 - '0' as i64;
            if inc == 0 {
                return Err(self.block_err(start, "expected indentation indicator in the range 1-9, but found 0".into()));
            }
            increment = Some(inc);
            self.r.forward(1)?;
            ch = self.r.peek(0)?;
            if is_in(ch, "+-") {
                chomping = Some(ch == '+');
                self.r.forward(1)?;
            }
        }
        let ch = self.r.peek(0)?;
        if !is_in(ch, BZ) {
            return Err(self.block_err(start, format!("expected chomping or indentation indicators, but found {}", char_repr(ch))));
        }
        Ok((chomping, increment))
    }

    fn scan_block_scalar_ignored_line(&mut self, start: &Mark) -> R<()> {
        while self.r.peek(0)? == ' ' {
            self.r.forward(1)?;
        }
        if self.r.peek(0)? == '#' {
            while !is_in(self.r.peek(0)?, ZBREAK) {
                self.r.forward(1)?;
            }
        }
        let ch = self.r.peek(0)?;
        if !is_in(ch, ZBREAK) {
            return Err(self.block_err(start, format!("expected a comment or a line break, but found {}", char_repr(ch))));
        }
        self.scan_line_break()?;
        Ok(())
    }

    fn scan_block_scalar_indentation(&mut self) -> R<(Vec<String>, i64, Mark)> {
        let mut chunks = vec![];
        let mut max_indent = 0;
        let mut end = self.r.mark();
        while is_in(self.r.peek(0)?, " \r\n\u{85}\u{2028}\u{2029}") {
            if self.r.peek(0)? != ' ' {
                chunks.push(self.scan_line_break()?);
                end = self.r.mark();
            } else {
                self.r.forward(1)?;
                if self.r.column as i64 > max_indent {
                    max_indent = self.r.column as i64;
                }
            }
        }
        Ok((chunks, max_indent, end))
    }

    fn scan_block_scalar_breaks(&mut self, indent: i64) -> R<(Vec<String>, Mark)> {
        let mut chunks = vec![];
        let mut end = self.r.mark();
        while (self.r.column as i64) < indent && self.r.peek(0)? == ' ' {
            self.r.forward(1)?;
        }
        while is_in(self.r.peek(0)?, BREAK) {
            chunks.push(self.scan_line_break()?);
            end = self.r.mark();
            while (self.r.column as i64) < indent && self.r.peek(0)? == ' ' {
                self.r.forward(1)?;
            }
        }
        Ok((chunks, end))
    }

    fn scan_flow_scalar(&mut self, double: bool) -> R<Token> {
        let mut chunks = String::new();
        let start = self.r.mark();
        let quote = self.r.peek(0)?;
        self.r.forward(1)?;
        self.scan_flow_scalar_non_spaces(double, &start, &mut chunks)?;
        while self.r.peek(0)? != quote {
            self.scan_flow_scalar_spaces(double, &start, &mut chunks)?;
            self.scan_flow_scalar_non_spaces(double, &start, &mut chunks)?;
        }
        self.r.forward(1)?;
        let end = self.r.mark();
        Ok(tok!(Tok::Scalar(chunks, false, Some(if double { '"' } else { '\'' })), start, end))
    }

    fn scan_flow_scalar_non_spaces(&mut self, double: bool, start: &Mark, chunks: &mut String) -> R<()> {
        let dq = || Some("while scanning a double-quoted scalar".to_string());
        loop {
            let mut length = 0;
            while !is_in(self.r.peek(length)?, "'\"\\\0 \t\r\n\u{85}\u{2028}\u{2029}") {
                length += 1;
            }
            if length > 0 {
                chunks.push_str(&self.r.prefix(length)?);
                self.r.forward(length)?;
            }
            let ch = self.r.peek(0)?;
            if !double && ch == '\'' && self.r.peek(1)? == '\'' {
                chunks.push('\'');
                self.r.forward(2)?;
            } else if (double && ch == '\'') || (!double && is_in(ch, "\"\\")) {
                chunks.push(ch);
                self.r.forward(1)?;
            } else if double && ch == '\\' {
                self.r.forward(1)?;
                let ch = self.r.peek(0)?;
                let rep = match ch {
                    '0' => Some('\0'),
                    'a' => Some('\x07'),
                    'b' => Some('\x08'),
                    't' | '\t' => Some('\x09'),
                    'n' => Some('\x0A'),
                    'v' => Some('\x0B'),
                    'f' => Some('\x0C'),
                    'r' => Some('\x0D'),
                    'e' => Some('\x1B'),
                    ' ' => Some(' '),
                    '"' => Some('"'),
                    '\\' => Some('\\'),
                    '/' => Some('/'),
                    'N' => Some('\u{85}'),
                    '_' => Some('\u{A0}'),
                    'L' => Some('\u{2028}'),
                    'P' => Some('\u{2029}'),
                    _ => None,
                };
                let code_len = match ch {
                    'x' => Some(2),
                    'u' => Some(4),
                    'U' => Some(8),
                    _ => None,
                };
                if let Some(r) = rep {
                    chunks.push(r);
                    self.r.forward(1)?;
                } else if let Some(length) = code_len {
                    self.r.forward(1)?;
                    for k in 0..length {
                        let c = self.r.peek(k)?;
                        if !c.is_ascii_hexdigit() {
                            return Err(scan_err(dq(), Some(start), format!("expected escape sequence of {length} hexadecimal numbers, but found {}", char_repr(c)), &self.r.mark()));
                        }
                    }
                    let code = u32::from_str_radix(&self.r.prefix(length)?, 16).unwrap_or(u32::MAX);
                    match char::from_u32(code) {
                        Some(c) => chunks.push(c),
                        // chr() of a surrogate is a str CPython keeps; a Rust string cannot (U+FFFD, documented)
                        None if (0xD800..=0xDFFF).contains(&code) => chunks.push('\u{FFFD}'),
                        None => return Err(Exc::value_error("chr() arg not in range(0x110000)")),
                    }
                    self.r.forward(length)?;
                } else if is_in(ch, BREAK) {
                    self.scan_line_break()?;
                    self.scan_flow_scalar_breaks(start, chunks)?;
                } else {
                    return Err(scan_err(dq(), Some(start), format!("found unknown escape character {}", char_repr(ch)), &self.r.mark()));
                }
            } else {
                return Ok(());
            }
        }
    }

    fn scan_flow_scalar_spaces(&mut self, _double: bool, start: &Mark, chunks: &mut String) -> R<()> {
        let mut length = 0;
        while is_in(self.r.peek(length)?, " \t") {
            length += 1;
        }
        let whitespaces = self.r.prefix(length)?;
        self.r.forward(length)?;
        let ch = self.r.peek(0)?;
        if ch == '\0' {
            return Err(scan_err(Some("while scanning a quoted scalar".into()), Some(start), "found unexpected end of stream".into(), &self.r.mark()));
        } else if is_in(ch, BREAK) {
            let line_break = self.scan_line_break()?;
            let mut breaks = String::new();
            let n = self.scan_flow_scalar_breaks(start, &mut breaks)?;
            if line_break != "\n" {
                chunks.push_str(&line_break);
            } else if n == 0 {
                chunks.push(' ');
            }
            chunks.push_str(&breaks);
        } else {
            chunks.push_str(&whitespaces);
        }
        Ok(())
    }

    /// the line breaks of a quoted scalar (their number returned)
    fn scan_flow_scalar_breaks(&mut self, start: &Mark, chunks: &mut String) -> R<usize> {
        let mut n = 0;
        loop {
            let prefix = self.r.prefix(3)?;
            if (prefix == "---" || prefix == "...") && is_in(self.r.peek(3)?, BZT) {
                return Err(scan_err(Some("while scanning a quoted scalar".into()), Some(start), "found unexpected document separator".into(), &self.r.mark()));
            }
            while is_in(self.r.peek(0)?, " \t") {
                self.r.forward(1)?;
            }
            if is_in(self.r.peek(0)?, BREAK) {
                chunks.push_str(&self.scan_line_break()?);
                n += 1;
            } else {
                return Ok(n);
            }
        }
    }

    fn scan_plain(&mut self) -> R<Token> {
        let mut chunks = String::new();
        let start = self.r.mark();
        let mut end = start.clone();
        let indent = self.indent + 1;
        // Python's list of space chunks: None (a document separator ended the scalar) or their text
        let mut spaces: Option<String> = Some(String::new());
        loop {
            let mut length = 0;
            if self.r.peek(0)? == '#' {
                break;
            }
            loop {
                let ch = self.r.peek(length)?;
                if is_in(ch, BZT) {
                    break;
                }
                if ch == ':' {
                    let n = self.r.peek(length + 1)?;
                    if is_in(n, BZT) || (self.flow_level != 0 && is_in(n, ",[]{}")) {
                        break;
                    }
                }
                if self.flow_level != 0 && is_in(ch, ",?[]{}") {
                    break;
                }
                length += 1;
            }
            if length == 0 {
                break;
            }
            self.allow_simple_key = false;
            chunks.push_str(spaces.as_deref().unwrap_or(""));
            chunks.push_str(&self.r.prefix(length)?);
            self.r.forward(length)?;
            end = self.r.mark();
            spaces = self.scan_plain_spaces()?;
            if spaces.as_deref().unwrap_or("").is_empty() || self.r.peek(0)? == '#' || (self.flow_level == 0 && (self.r.column as i64) < indent) {
                break;
            }
        }
        Ok(tok!(Tok::Scalar(chunks, true, None), start, end))
    }

    /// None when a document separator ends the scalar (Python returns None)
    fn scan_plain_spaces(&mut self) -> R<Option<String>> {
        let mut chunks = String::new();
        let mut length = 0;
        while self.r.peek(length)? == ' ' {
            length += 1;
        }
        let whitespaces = self.r.prefix(length)?;
        self.r.forward(length)?;
        let ch = self.r.peek(0)?;
        if is_in(ch, BREAK) {
            let line_break = self.scan_line_break()?;
            self.allow_simple_key = true;
            let prefix = self.r.prefix(3)?;
            if (prefix == "---" || prefix == "...") && is_in(self.r.peek(3)?, BZT) {
                return Ok(None);
            }
            let mut breaks = String::new();
            let mut nbreaks = 0;
            while is_in(self.r.peek(0)?, " \r\n\u{85}\u{2028}\u{2029}") {
                if self.r.peek(0)? == ' ' {
                    self.r.forward(1)?;
                } else {
                    breaks.push_str(&self.scan_line_break()?);
                    nbreaks += 1;
                    let prefix = self.r.prefix(3)?;
                    if (prefix == "---" || prefix == "...") && is_in(self.r.peek(3)?, BZT) {
                        return Ok(None);
                    }
                }
            }
            if line_break != "\n" {
                chunks.push_str(&line_break);
            } else if nbreaks == 0 {
                chunks.push(' ');
            }
            chunks.push_str(&breaks);
        } else if !whitespaces.is_empty() {
            chunks.push_str(&whitespaces);
        }
        Ok(Some(chunks))
    }

    fn scan_tag_handle(&mut self, name: &str, start: &Mark) -> R<String> {
        let ch = self.r.peek(0)?;
        if ch != '!' {
            return Err(scan_err(Some(format!("while scanning a {name}")), Some(start), format!("expected '!', but found {}", char_repr(ch)), &self.r.mark()));
        }
        let mut length = 1;
        let mut ch = self.r.peek(length)?;
        if ch != ' ' {
            while alnum(ch) {
                length += 1;
                ch = self.r.peek(length)?;
            }
            if ch != '!' {
                self.r.forward(length)?;
                return Err(scan_err(Some(format!("while scanning a {name}")), Some(start), format!("expected '!', but found {}", char_repr(ch)), &self.r.mark()));
            }
            length += 1;
        }
        let value = self.r.prefix(length)?;
        self.r.forward(length)?;
        Ok(value)
    }

    fn scan_tag_uri(&mut self, name: &str, start: &Mark) -> R<String> {
        let mut chunks = String::new();
        let mut nchunks = 0;
        let mut length = 0;
        let mut ch = self.r.peek(length)?;
        while ch.is_ascii_alphanumeric() || is_in(ch, "-;/?:@&=+$,_.!~*'()[]%") {
            if ch == '%' {
                chunks.push_str(&self.r.prefix(length)?);
                self.r.forward(length)?;
                length = 0;
                chunks.push_str(&self.scan_uri_escapes(name, start)?);
                nchunks += 2;
            } else {
                length += 1;
            }
            ch = self.r.peek(length)?;
        }
        if length > 0 {
            chunks.push_str(&self.r.prefix(length)?);
            self.r.forward(length)?;
            nchunks += 1;
        }
        if nchunks == 0 {
            return Err(scan_err(Some(format!("while parsing a {name}")), Some(start), format!("expected URI, but found {}", char_repr(ch)), &self.r.mark()));
        }
        Ok(chunks)
    }

    fn scan_uri_escapes(&mut self, name: &str, start: &Mark) -> R<String> {
        let mut codes = vec![];
        let mark = self.r.mark();
        while self.r.peek(0)? == '%' {
            self.r.forward(1)?;
            for k in 0..2 {
                let c = self.r.peek(k)?;
                if !c.is_ascii_hexdigit() {
                    return Err(scan_err(Some(format!("while scanning a {name}")), Some(start), format!("expected URI escape sequence of 2 hexadecimal numbers, but found {}", char_repr(c)), &self.r.mark()));
                }
            }
            codes.push(u8::from_str_radix(&self.r.prefix(2)?, 16).unwrap_or(0));
            self.r.forward(2)?;
        }
        match std::str::from_utf8(&codes) {
            Ok(s) => Ok(s.to_string()),
            Err(e) => {
                let p = e.valid_up_to();
                let msg = match e.error_len() {
                    None if codes.len() - p > 1 => format!("can't decode bytes in position {}-{}: unexpected end of data", p, codes.len() - 1),
                    None => format!("can't decode byte 0x{:02x} in position {p}: unexpected end of data", codes[p]),
                    Some(_) => format!("can't decode byte 0x{:02x} in position {p}: invalid {} byte", codes[p], if matches!(codes[p], 0xc2..=0xf4) { "continuation" } else { "start" }),
                };
                Err(marked(&YAML_SCANNER_ERROR, Some(format!("while scanning a {name}")), Some(start), format!("'utf-8' codec {msg}"), Some(&mark)))
            }
        }
    }

    fn scan_line_break(&mut self) -> R<String> {
        let ch = self.r.peek(0)?;
        if is_in(ch, "\r\n\u{85}") {
            if self.r.prefix(2)? == "\r\n" {
                self.r.forward(2)?;
            } else {
                self.r.forward(1)?;
            }
            return Ok("\n".into());
        } else if is_in(ch, "\u{2028}\u{2029}") {
            self.r.forward(1)?;
            return Ok(ch.to_string());
        }
        Ok(String::new())
    }
}

// ---------------------------------------------------------------- parser

#[derive(Clone)]
enum Ev {
    StreamStart,
    StreamEnd,
    DocStart,
    DocEnd,
    Alias(String),
    Scalar { anchor: Option<String>, tag: Option<String>, implicit: (bool, bool), value: String, style: Option<char> },
    SeqStart { anchor: Option<String>, tag: Option<String>, implicit: bool },
    SeqEnd,
    MapStart { anchor: Option<String>, tag: Option<String>, implicit: bool },
    MapEnd,
}

#[derive(Clone)]
struct Event {
    e: Ev,
    start: Mark,
    end: Mark,
}

#[derive(Clone, Copy)]
enum PS {
    StreamStart,
    ImplicitDocumentStart,
    DocumentStart,
    DocumentEnd,
    DocumentContent,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
}

fn parse_err(context: Option<String>, cmark: Option<&Mark>, problem: String, pmark: &Mark) -> Exc {
    marked(&YAML_PARSER_ERROR, context, cmark, problem, Some(pmark))
}

struct Parser {
    s: Scanner,
    current: Option<Event>,
    tag_handles: IndexMap<String, String>,
    states: Vec<PS>,
    marks: Vec<Mark>,
    state: Option<PS>,
}

fn default_tags() -> IndexMap<String, String> {
    IndexMap::from([("!".to_string(), "!".to_string()), ("!!".to_string(), "tag:yaml.org,2002:".to_string())])
}

macro_rules! ev {
    ($e:expr, $s:expr, $n:expr) => {
        Event { e: $e, start: $s, end: $n }
    };
}

impl Parser {
    fn new(s: Scanner) -> Parser {
        Parser { s, current: None, tag_handles: IndexMap::new(), states: vec![], marks: vec![], state: Some(PS::StreamStart) }
    }

    fn fill(&mut self) -> R<()> {
        if self.current.is_none() {
            if let Some(st) = self.state {
                self.current = Some(self.run(st)?);
            }
        }
        Ok(())
    }

    fn check_event(&mut self, f: impl Fn(&Ev) -> bool) -> R<bool> {
        self.fill()?;
        Ok(self.current.as_ref().map(|e| f(&e.e)).unwrap_or(false))
    }

    fn peek_event(&mut self) -> R<Event> {
        self.fill()?;
        self.current.clone().ok_or_else(|| Exc::runtime("py2axum: YAML parser exhausted"))
    }

    fn get_event(&mut self) -> R<Event> {
        self.fill()?;
        self.current.take().ok_or_else(|| Exc::runtime("py2axum: YAML parser exhausted"))
    }

    fn pop_state(&mut self) -> PS {
        self.states.pop().unwrap_or(PS::DocumentEnd)
    }

    fn run(&mut self, st: PS) -> R<Event> {
        match st {
            PS::StreamStart => {
                let t = self.s.get_token()?;
                self.state = Some(PS::ImplicitDocumentStart);
                Ok(ev!(Ev::StreamStart, t.start, t.end))
            }
            PS::ImplicitDocumentStart => {
                if !self.s.check(|t| matches!(t, Tok::Directive(..) | Tok::DocStart | Tok::StreamEnd))? {
                    self.tag_handles = default_tags();
                    let t = self.s.peek_token()?;
                    self.states.push(PS::DocumentEnd);
                    self.state = Some(PS::BlockNode);
                    Ok(ev!(Ev::DocStart, t.start.clone(), t.start))
                } else {
                    self.parse_document_start()
                }
            }
            PS::DocumentStart => self.parse_document_start(),
            PS::DocumentEnd => {
                let t = self.s.peek_token()?;
                let start = t.start.clone();
                let mut end = t.start;
                if self.s.check(|t| matches!(t, Tok::DocEnd))? {
                    end = self.s.get_token()?.end;
                }
                self.state = Some(PS::DocumentStart);
                Ok(ev!(Ev::DocEnd, start, end))
            }
            PS::DocumentContent => {
                if self.s.check(|t| matches!(t, Tok::Directive(..) | Tok::DocStart | Tok::DocEnd | Tok::StreamEnd))? {
                    let m = self.s.peek_token()?.start;
                    self.state = Some(self.pop_state());
                    Ok(empty_scalar(m))
                } else {
                    self.parse_node(true, false)
                }
            }
            PS::BlockNode => self.parse_node(true, false),
            PS::BlockSequenceFirstEntry => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_block_sequence_entry()
            }
            PS::BlockSequenceEntry => self.parse_block_sequence_entry(),
            PS::IndentlessSequenceEntry => {
                if self.s.check(|t| matches!(t, Tok::BlockEntry))? {
                    let t = self.s.get_token()?;
                    if !self.s.check(|t| matches!(t, Tok::BlockEntry | Tok::Key | Tok::Value | Tok::BlockEnd))? {
                        self.states.push(PS::IndentlessSequenceEntry);
                        return self.parse_node(true, false);
                    }
                    self.state = Some(PS::IndentlessSequenceEntry);
                    return Ok(empty_scalar(t.end));
                }
                let t = self.s.peek_token()?;
                self.state = Some(self.pop_state());
                Ok(ev!(Ev::SeqEnd, t.start.clone(), t.start))
            }
            PS::BlockMappingFirstKey => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_block_mapping_key()
            }
            PS::BlockMappingKey => self.parse_block_mapping_key(),
            PS::BlockMappingValue => {
                if self.s.check(|t| matches!(t, Tok::Value))? {
                    let t = self.s.get_token()?;
                    if !self.s.check(|t| matches!(t, Tok::Key | Tok::Value | Tok::BlockEnd))? {
                        self.states.push(PS::BlockMappingKey);
                        return self.parse_node(true, true);
                    }
                    self.state = Some(PS::BlockMappingKey);
                    return Ok(empty_scalar(t.end));
                }
                self.state = Some(PS::BlockMappingKey);
                let t = self.s.peek_token()?;
                Ok(empty_scalar(t.start))
            }
            PS::FlowSequenceFirstEntry => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_flow_sequence_entry(true)
            }
            PS::FlowSequenceEntry => self.parse_flow_sequence_entry(false),
            PS::FlowSequenceEntryMappingKey => {
                let t = self.s.get_token()?;
                if !self.s.check(|t| matches!(t, Tok::Value | Tok::FlowEntry | Tok::FlowSeqEnd))? {
                    self.states.push(PS::FlowSequenceEntryMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = Some(PS::FlowSequenceEntryMappingValue);
                Ok(empty_scalar(t.end))
            }
            PS::FlowSequenceEntryMappingValue => {
                if self.s.check(|t| matches!(t, Tok::Value))? {
                    let t = self.s.get_token()?;
                    if !self.s.check(|t| matches!(t, Tok::FlowEntry | Tok::FlowSeqEnd))? {
                        self.states.push(PS::FlowSequenceEntryMappingEnd);
                        return self.parse_node(false, false);
                    }
                    self.state = Some(PS::FlowSequenceEntryMappingEnd);
                    return Ok(empty_scalar(t.end));
                }
                self.state = Some(PS::FlowSequenceEntryMappingEnd);
                let t = self.s.peek_token()?;
                Ok(empty_scalar(t.start))
            }
            PS::FlowSequenceEntryMappingEnd => {
                self.state = Some(PS::FlowSequenceEntry);
                let t = self.s.peek_token()?;
                Ok(ev!(Ev::MapEnd, t.start.clone(), t.start))
            }
            PS::FlowMappingFirstKey => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_flow_mapping_key(true)
            }
            PS::FlowMappingKey => self.parse_flow_mapping_key(false),
            PS::FlowMappingValue => {
                if self.s.check(|t| matches!(t, Tok::Value))? {
                    let t = self.s.get_token()?;
                    if !self.s.check(|t| matches!(t, Tok::FlowEntry | Tok::FlowMapEnd))? {
                        self.states.push(PS::FlowMappingKey);
                        return self.parse_node(false, false);
                    }
                    self.state = Some(PS::FlowMappingKey);
                    return Ok(empty_scalar(t.end));
                }
                self.state = Some(PS::FlowMappingKey);
                let t = self.s.peek_token()?;
                Ok(empty_scalar(t.start))
            }
            PS::FlowMappingEmptyValue => {
                self.state = Some(PS::FlowMappingKey);
                let t = self.s.peek_token()?;
                Ok(empty_scalar(t.start))
            }
        }
    }

    fn parse_document_start(&mut self) -> R<Event> {
        while self.s.check(|t| matches!(t, Tok::DocEnd))? {
            self.s.get_token()?;
        }
        if !self.s.check(|t| matches!(t, Tok::StreamEnd))? {
            let start = self.s.peek_token()?.start;
            self.process_directives()?;
            if !self.s.check(|t| matches!(t, Tok::DocStart))? {
                let t = self.s.peek_token()?;
                return Err(parse_err(None, None, format!("expected '<document start>', but found {}", py_repr(t.t.id())), &t.start));
            }
            let t = self.s.get_token()?;
            self.states.push(PS::DocumentEnd);
            self.state = Some(PS::DocumentContent);
            Ok(ev!(Ev::DocStart, start, t.end))
        } else {
            let t = self.s.get_token()?;
            self.state = None;
            Ok(ev!(Ev::StreamEnd, t.start, t.end))
        }
    }

    fn process_directives(&mut self) -> R<()> {
        let mut version = false;
        self.tag_handles = IndexMap::new();
        while self.s.check(|t| matches!(t, Tok::Directive(..)))? {
            let t = self.s.get_token()?;
            match &t.t {
                Tok::Directive(name, DirVal::Yaml(one)) if name == "YAML" => {
                    if version {
                        return Err(parse_err(None, None, "found duplicate YAML directive".into(), &t.start));
                    }
                    if !one {
                        return Err(parse_err(None, None, "found incompatible YAML document (version 1.* is required)".into(), &t.start));
                    }
                    version = true;
                }
                Tok::Directive(name, DirVal::Tag(handle, prefix)) if name == "TAG" => {
                    if self.tag_handles.contains_key(handle) {
                        return Err(parse_err(None, None, format!("duplicate tag handle {}", py_repr(handle)), &t.start));
                    }
                    self.tag_handles.insert(handle.clone(), prefix.clone());
                }
                _ => {}
            }
        }
        for (k, v) in default_tags() {
            self.tag_handles.entry(k).or_insert(v);
        }
        Ok(())
    }

    fn parse_node(&mut self, block: bool, indentless_sequence: bool) -> R<Event> {
        if self.s.check(|t| matches!(t, Tok::Alias(_)))? {
            let t = self.s.get_token()?;
            self.state = Some(self.pop_state());
            let Tok::Alias(name) = t.t else { unreachable!() };
            return Ok(ev!(Ev::Alias(name), t.start, t.end));
        }
        let mut anchor = None;
        let mut tag: Option<(Option<String>, String)> = None;
        let (mut start, mut end, mut tag_mark): (Option<Mark>, Option<Mark>, Option<Mark>) = (None, None, None);
        if self.s.check(|t| matches!(t, Tok::Anchor(_)))? {
            let t = self.s.get_token()?;
            start = Some(t.start.clone());
            end = Some(t.end.clone());
            if let Tok::Anchor(a) = t.t {
                anchor = Some(a);
            }
            if self.s.check(|t| matches!(t, Tok::Tag(..)))? {
                let t = self.s.get_token()?;
                tag_mark = Some(t.start.clone());
                end = Some(t.end.clone());
                if let Tok::Tag(h, s) = t.t {
                    tag = Some((h, s));
                }
            }
        } else if self.s.check(|t| matches!(t, Tok::Tag(..)))? {
            let t = self.s.get_token()?;
            start = Some(t.start.clone());
            tag_mark = Some(t.start.clone());
            end = Some(t.end.clone());
            if let Tok::Tag(h, s) = t.t {
                tag = Some((h, s));
            }
            if self.s.check(|t| matches!(t, Tok::Anchor(_)))? {
                let t = self.s.get_token()?;
                end = Some(t.end.clone());
                if let Tok::Anchor(a) = t.t {
                    anchor = Some(a);
                }
            }
        }
        let tag: Option<String> = match tag {
            None => None,
            Some((Some(handle), suffix)) => match self.tag_handles.get(&handle) {
                Some(p) => Some(format!("{p}{suffix}")),
                None => {
                    return Err(parse_err(Some("while parsing a node".into()), start.as_ref(), format!("found undefined tag handle {}", py_repr(&handle)), tag_mark.as_ref().unwrap()));
                }
            },
            Some((None, suffix)) => Some(suffix),
        };
        if start.is_none() {
            let m = self.s.peek_token()?.start;
            start = Some(m.clone());
            end = Some(m);
        }
        let start = start.unwrap();
        let mut end = end.unwrap();
        let implicit = tag.is_none() || tag.as_deref() == Some("!");
        if indentless_sequence && self.s.check(|t| matches!(t, Tok::BlockEntry))? {
            end = self.s.peek_token()?.end;
            self.state = Some(PS::IndentlessSequenceEntry);
            return Ok(ev!(Ev::SeqStart { anchor, tag, implicit }, start, end));
        }
        if self.s.check(|t| matches!(t, Tok::Scalar(..)))? {
            let t = self.s.get_token()?;
            let Tok::Scalar(value, plain, style) = t.t else { unreachable!() };
            let imp = if (plain && tag.is_none()) || tag.as_deref() == Some("!") {
                (true, false)
            } else if tag.is_none() {
                (false, true)
            } else {
                (false, false)
            };
            self.state = Some(self.pop_state());
            return Ok(ev!(Ev::Scalar { anchor, tag, implicit: imp, value, style }, start, t.end));
        }
        if self.s.check(|t| matches!(t, Tok::FlowSeqStart))? {
            end = self.s.peek_token()?.end;
            self.state = Some(PS::FlowSequenceFirstEntry);
            return Ok(ev!(Ev::SeqStart { anchor, tag, implicit }, start, end));
        }
        if self.s.check(|t| matches!(t, Tok::FlowMapStart))? {
            end = self.s.peek_token()?.end;
            self.state = Some(PS::FlowMappingFirstKey);
            return Ok(ev!(Ev::MapStart { anchor, tag, implicit }, start, end));
        }
        if block && self.s.check(|t| matches!(t, Tok::BlockSeqStart))? {
            end = self.s.peek_token()?.start;
            self.state = Some(PS::BlockSequenceFirstEntry);
            return Ok(ev!(Ev::SeqStart { anchor, tag, implicit }, start, end));
        }
        if block && self.s.check(|t| matches!(t, Tok::BlockMapStart))? {
            end = self.s.peek_token()?.start;
            self.state = Some(PS::BlockMappingFirstKey);
            return Ok(ev!(Ev::MapStart { anchor, tag, implicit }, start, end));
        }
        if anchor.is_some() || tag.is_some() {
            self.state = Some(self.pop_state());
            return Ok(ev!(Ev::Scalar { anchor, tag, implicit: (implicit, false), value: String::new(), style: None }, start, end));
        }
        let t = self.s.peek_token()?;
        Err(parse_err(Some(format!("while parsing a {} node", if block { "block" } else { "flow" })), Some(&start), format!("expected the node content, but found {}", py_repr(t.t.id())), &t.start))
    }

    fn parse_block_sequence_entry(&mut self) -> R<Event> {
        if self.s.check(|t| matches!(t, Tok::BlockEntry))? {
            let t = self.s.get_token()?;
            if !self.s.check(|t| matches!(t, Tok::BlockEntry | Tok::BlockEnd))? {
                self.states.push(PS::BlockSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = Some(PS::BlockSequenceEntry);
            return Ok(empty_scalar(t.end));
        }
        if !self.s.check(|t| matches!(t, Tok::BlockEnd))? {
            let t = self.s.peek_token()?;
            return Err(parse_err(Some("while parsing a block collection".into()), self.marks.last(), format!("expected <block end>, but found {}", py_repr(t.t.id())), &t.start));
        }
        let t = self.s.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(ev!(Ev::SeqEnd, t.start, t.end))
    }

    fn parse_block_mapping_key(&mut self) -> R<Event> {
        if self.s.check(|t| matches!(t, Tok::Key))? {
            let t = self.s.get_token()?;
            if !self.s.check(|t| matches!(t, Tok::Key | Tok::Value | Tok::BlockEnd))? {
                self.states.push(PS::BlockMappingValue);
                return self.parse_node(true, true);
            }
            self.state = Some(PS::BlockMappingValue);
            return Ok(empty_scalar(t.end));
        }
        if !self.s.check(|t| matches!(t, Tok::BlockEnd))? {
            let t = self.s.peek_token()?;
            return Err(parse_err(Some("while parsing a block mapping".into()), self.marks.last(), format!("expected <block end>, but found {}", py_repr(t.t.id())), &t.start));
        }
        let t = self.s.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(ev!(Ev::MapEnd, t.start, t.end))
    }

    fn parse_flow_sequence_entry(&mut self, first: bool) -> R<Event> {
        if !self.s.check(|t| matches!(t, Tok::FlowSeqEnd))? {
            if !first {
                if self.s.check(|t| matches!(t, Tok::FlowEntry))? {
                    self.s.get_token()?;
                } else {
                    let t = self.s.peek_token()?;
                    return Err(parse_err(Some("while parsing a flow sequence".into()), self.marks.last(), format!("expected ',' or ']', but got {}", py_repr(t.t.id())), &t.start));
                }
            }
            if self.s.check(|t| matches!(t, Tok::Key))? {
                let t = self.s.peek_token()?;
                self.state = Some(PS::FlowSequenceEntryMappingKey);
                return Ok(ev!(Ev::MapStart { anchor: None, tag: None, implicit: true }, t.start, t.end));
            } else if !self.s.check(|t| matches!(t, Tok::FlowSeqEnd))? {
                self.states.push(PS::FlowSequenceEntry);
                return self.parse_node(false, false);
            }
        }
        let t = self.s.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(ev!(Ev::SeqEnd, t.start, t.end))
    }

    fn parse_flow_mapping_key(&mut self, first: bool) -> R<Event> {
        if !self.s.check(|t| matches!(t, Tok::FlowMapEnd))? {
            if !first {
                if self.s.check(|t| matches!(t, Tok::FlowEntry))? {
                    self.s.get_token()?;
                } else {
                    let t = self.s.peek_token()?;
                    return Err(parse_err(Some("while parsing a flow mapping".into()), self.marks.last(), format!("expected ',' or '}}', but got {}", py_repr(t.t.id())), &t.start));
                }
            }
            if self.s.check(|t| matches!(t, Tok::Key))? {
                let t = self.s.get_token()?;
                if !self.s.check(|t| matches!(t, Tok::Value | Tok::FlowEntry | Tok::FlowMapEnd))? {
                    self.states.push(PS::FlowMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = Some(PS::FlowMappingValue);
                return Ok(empty_scalar(t.end));
            } else if !self.s.check(|t| matches!(t, Tok::FlowMapEnd))? {
                self.states.push(PS::FlowMappingEmptyValue);
                return self.parse_node(false, false);
            }
        }
        let t = self.s.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(ev!(Ev::MapEnd, t.start, t.end))
    }
}

fn empty_scalar(m: Mark) -> Event {
    ev!(Ev::Scalar { anchor: None, tag: None, implicit: (true, false), value: String::new(), style: None }, m.clone(), m)
}

// ---------------------------------------------------------------- composer and resolver

pub(super) const TAG_STR: &str = "tag:yaml.org,2002:str";
const TAG_SEQ: &str = "tag:yaml.org,2002:seq";
const TAG_MAP: &str = "tag:yaml.org,2002:map";
const TAG_VALUE: &str = "tag:yaml.org,2002:value";
const TAG_MERGE: &str = "tag:yaml.org,2002:merge";

fn re(p: &str) -> regex::Regex {
    // Python's `$` (no MULTILINE) also matches before a final newline
    regex::Regex::new(&format!(r"\A(?:{p})\n?\z")).unwrap()
}

static RE_BOOL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| re("yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF"));
static RE_FLOAT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    re(r"[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)")
});
static RE_INT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| re(r"[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+"));
static RE_MERGE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| re("<<"));
static RE_NULL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| re("~|null|Null|NULL|"));
static RE_TIMESTAMP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    re(r"[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?")
});
static RE_VALUE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| re("="));
static RE_YAML: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| re(r"!|&|\*"));

/// `Resolver.resolve(ScalarNode, value, (True, _))`: the implicit resolvers of the first character, in the order
/// PyYAML registers them; None when none matches
pub(super) fn implicit_tag(value: &str) -> Option<&'static str> {
    let first = value.chars().next();
    let table: [(&str, &str, &std::sync::LazyLock<regex::Regex>); 8] = [
        ("tag:yaml.org,2002:bool", "yYnNtTfFoO", &RE_BOOL),
        ("tag:yaml.org,2002:float", "-+0123456789.", &RE_FLOAT),
        ("tag:yaml.org,2002:int", "-+0123456789", &RE_INT),
        (TAG_MERGE, "<", &RE_MERGE),
        ("tag:yaml.org,2002:null", "~nN", &RE_NULL),
        ("tag:yaml.org,2002:timestamp", "0123456789", &RE_TIMESTAMP),
        (TAG_VALUE, "=", &RE_VALUE),
        ("tag:yaml.org,2002:yaml", "!&*", &RE_YAML),
    ];
    for (tag, firsts, rx) in table {
        let applies = match first {
            None => tag == "tag:yaml.org,2002:null",
            Some(c) => firsts.contains(c),
        };
        if applies && rx.is_match(value) {
            return Some(tag);
        }
    }
    None
}

#[derive(Clone)]
enum NK {
    Scalar(String),
    Seq(Vec<usize>),
    Map(Vec<(usize, usize)>),
}

struct Node {
    tag: String,
    kind: NK,
    start: Mark,
}

impl Node {
    fn id(&self) -> &'static str {
        match self.kind {
            NK::Scalar(_) => "scalar",
            NK::Seq(_) => "sequence",
            NK::Map(_) => "mapping",
        }
    }
}

/// Nesting depth past which the recursive composer raises RecursionError. CPython's limit depends on the
/// caller's stack: measured from an async endpoint under uvicorn (PyYAML 6.0.3, Python 3.12), a collection
/// nested 471 deep loads and one 472 deep does not; a scalar costs one more frame.
pub const MAX_DEPTH: usize = 471;

fn recursion() -> Exc {
    Exc::msg(&RECURSION_ERROR, "maximum recursion depth exceeded")
}

struct Loader {
    p: Parser,
    nodes: Vec<Node>,
    anchors: HashMap<String, usize>,
}

impl Loader {
    fn get_single_node(&mut self) -> R<Option<usize>> {
        self.p.get_event()?;
        let mut document = None;
        if !self.p.check_event(|e| matches!(e, Ev::StreamEnd))? {
            document = Some(self.compose_document()?);
        }
        if !self.p.check_event(|e| matches!(e, Ev::StreamEnd))? {
            let event = self.p.get_event()?;
            let start = &self.nodes[document.unwrap()].start;
            return Err(marked(&YAML_COMPOSER_ERROR, Some("expected a single document in the stream".into()), Some(start), "but found another document".into(), Some(&event.start)));
        }
        self.p.get_event()?;
        Ok(document)
    }

    fn compose_document(&mut self) -> R<usize> {
        self.p.get_event()?;
        let node = self.compose_node(0)?;
        self.p.get_event()?;
        self.anchors.clear();
        Ok(node)
    }

    fn compose_node(&mut self, depth: usize) -> R<usize> {
        if depth > MAX_DEPTH {
            return Err(recursion());
        }
        super::stack_guard()?;
        if depth == MAX_DEPTH && self.p.check_event(|e| matches!(e, Ev::Scalar { .. }))? {
            return Err(recursion());
        }
        if self.p.check_event(|e| matches!(e, Ev::Alias(_)))? {
            let event = self.p.get_event()?;
            let Ev::Alias(anchor) = &event.e else { unreachable!() };
            return match self.anchors.get(anchor) {
                Some(n) => Ok(*n),
                None => Err(marked(&YAML_COMPOSER_ERROR, None, None, format!("found undefined alias {}", py_repr(anchor)), Some(&event.start))),
            };
        }
        let event = self.p.peek_event()?;
        let anchor = match &event.e {
            Ev::Scalar { anchor, .. } | Ev::SeqStart { anchor, .. } | Ev::MapStart { anchor, .. } => anchor.clone(),
            _ => None,
        };
        if let Some(a) = &anchor {
            if let Some(first) = self.anchors.get(a) {
                return Err(marked(&YAML_COMPOSER_ERROR, Some(format!("found duplicate anchor {}; first occurrence", py_repr(a))), Some(&self.nodes[*first].start), "second occurrence".into(), Some(&event.start)));
            }
        }
        match event.e {
            Ev::Scalar { .. } => {
                let event = self.p.get_event()?;
                let Ev::Scalar { tag, implicit, value, .. } = event.e else { unreachable!() };
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => if implicit.0 { implicit_tag(&value) } else { None }.unwrap_or(TAG_STR).to_string(),
                };
                let id = self.push(Node { tag, kind: NK::Scalar(value), start: event.start });
                if let Some(a) = anchor {
                    self.anchors.insert(a, id);
                }
                Ok(id)
            }
            Ev::SeqStart { .. } => {
                let start = self.p.get_event()?;
                let Ev::SeqStart { tag, .. } = start.e else { unreachable!() };
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => TAG_SEQ.to_string(),
                };
                let id = self.push(Node { tag, kind: NK::Seq(vec![]), start: start.start });
                if let Some(a) = anchor {
                    self.anchors.insert(a, id);
                }
                while !self.p.check_event(|e| matches!(e, Ev::SeqEnd))? {
                    let child = self.compose_node(depth + 1)?;
                    if let NK::Seq(v) = &mut self.nodes[id].kind {
                        v.push(child);
                    }
                }
                self.p.get_event()?;
                Ok(id)
            }
            Ev::MapStart { .. } => {
                let start = self.p.get_event()?;
                let Ev::MapStart { tag, .. } = start.e else { unreachable!() };
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => TAG_MAP.to_string(),
                };
                let id = self.push(Node { tag, kind: NK::Map(vec![]), start: start.start });
                if let Some(a) = anchor {
                    self.anchors.insert(a, id);
                }
                while !self.p.check_event(|e| matches!(e, Ev::MapEnd))? {
                    let k = self.compose_node(depth + 1)?;
                    let v = self.compose_node(depth + 1)?;
                    if let NK::Map(m) = &mut self.nodes[id].kind {
                        m.push((k, v));
                    }
                }
                self.p.get_event()?;
                Ok(id)
            }
            _ => Err(Exc::runtime("py2axum: unexpected YAML event")),
        }
    }

    fn push(&mut self, n: Node) -> usize {
        self.nodes.push(n);
        self.nodes.len() - 1
    }
}

// ---------------------------------------------------------------- SafeConstructor

fn ctor_err(context: Option<&str>, cmark: Option<&Mark>, problem: String, pmark: &Mark) -> Exc {
    marked(&YAML_CONSTRUCTOR_ERROR, context.map(String::from), cmark, problem, Some(pmark))
}

#[derive(Clone, Copy, PartialEq)]
enum GenKind {
    Seq,
    Map,
    Set,
    Omap,
    Pairs,
}

/// a constructor of PyYAML written as a generator: the container is made (and seen by aliases) first, filled
/// later, after the document's other containers were made
struct Gen {
    kind: GenKind,
    node: usize,
    data: V,
}

struct Ctor<'a> {
    nodes: &'a mut Vec<Node>,
    constructed: HashMap<usize, V>,
    recursive: HashSet<usize>,
    gens: Vec<Gen>,
}

/// `d[key] = value`: the first equal key object stays
fn dict_set(d: &V, k: V, v: V) -> R<()> {
    let V::Dict(m) = d else { return Ok(()) };
    let key = Key::dict_key(&k)?;
    let mut m = m.lock();
    match m.get_mut(&key) {
        Some(e) => e.1 = v,
        None => {
            m.insert(key, (k, v));
        }
    }
    Ok(())
}

fn unhashable(v: &V) -> bool {
    matches!(v, V::List(_) | V::Dict(_) | V::Set(_))
}

impl Ctor<'_> {
    fn construct_document(&mut self, root: usize) -> R {
        let data = self.construct_object(root, 0)?;
        while !self.gens.is_empty() {
            let gens = std::mem::take(&mut self.gens);
            for g in gens {
                self.fill(g)?;
            }
        }
        Ok(data)
    }

    fn construct_object(&mut self, node: usize, depth: usize) -> R {
        if let Some(v) = self.constructed.get(&node) {
            return Ok(v.clone());
        }
        if depth > MAX_DEPTH {
            return Err(recursion());
        }
        super::stack_guard()?;
        if self.recursive.contains(&node) {
            return Err(ctor_err(None, None, "found unconstructable recursive node".into(), &self.nodes[node].start));
        }
        self.recursive.insert(node);
        let tag = self.nodes[node].tag.clone();
        let gen = |kind, data: V| (Some(kind), data);
        let (kind, data) = match tag.strip_prefix("tag:yaml.org,2002:").unwrap_or("") {
            "null" => {
                self.construct_scalar(node, depth)?;
                (None, V::None)
            }
            "bool" => {
                let v = self.construct_scalar(node, depth)?.to_lowercase();
                (None, match v.as_str() {
                    "yes" | "true" | "on" => V::Bool(true),
                    "no" | "false" | "off" => V::Bool(false),
                    _ => return Err(Exc::new(&KEY_ERROR, vec![V::str(v)])),
                })
            }
            "int" => (None, self.construct_int(node, depth)?),
            "float" => (None, self.construct_float(node, depth)?),
            "binary" => (None, self.construct_binary(node, depth)?),
            "timestamp" => (None, self.construct_timestamp(node, depth)?),
            "str" => (None, V::str(self.construct_scalar(node, depth)?)),
            "omap" => gen(GenKind::Omap, V::list(vec![])),
            "pairs" => gen(GenKind::Pairs, V::list(vec![])),
            "set" => gen(GenKind::Set, V::Set(Default::default())),
            "seq" => gen(GenKind::Seq, V::list(vec![])),
            "map" => gen(GenKind::Map, V::empty_dict()),
            _ => return Err(ctor_err(None, None, format!("could not determine a constructor for the tag {}", py_repr(&tag)), &self.nodes[node].start)),
        };
        if let Some(kind) = kind {
            self.gens.push(Gen { kind, node, data: data.clone() });
        }
        self.constructed.insert(node, data.clone());
        self.recursive.remove(&node);
        Ok(data)
    }

    /// the rest of a generator constructor
    fn fill(&mut self, g: Gen) -> R<()> {
        let node = g.node;
        match g.kind {
            GenKind::Seq => {
                let items = self.construct_sequence(node)?;
                if let V::List(l) = &g.data {
                    l.lock().extend(items);
                }
            }
            GenKind::Map => {
                let value = self.construct_mapping(node, 0)?;
                if let V::Dict(m) = &value {
                    let items: Vec<(V, V)> = m.lock().values().cloned().collect();
                    for (k, v) in items {
                        dict_set(&g.data, k, v)?;
                    }
                }
            }
            GenKind::Set => {
                let value = self.construct_mapping(node, 0)?;
                if let (V::Dict(m), V::Set(s)) = (&value, &g.data) {
                    let keys: Vec<V> = m.lock().values().map(|(k, _)| k.clone()).collect();
                    let mut s = s.lock();
                    for k in keys {
                        let key = Key::set_elem(&k)?;
                        s.entry(key).or_insert(k);
                    }
                }
            }
            GenKind::Omap | GenKind::Pairs => {
                let what = if g.kind == GenKind::Omap { "while constructing an ordered map" } else { "while constructing pairs" };
                let start = self.nodes[node].start.clone();
                let NK::Seq(items) = self.nodes[node].kind.clone() else {
                    return Err(ctor_err(Some(what), Some(&start), format!("expected a sequence, but found {}", self.nodes[node].id()), &start));
                };
                for sub in items {
                    let NK::Map(pairs) = self.nodes[sub].kind.clone() else {
                        return Err(ctor_err(Some(what), Some(&start), format!("expected a mapping of length 1, but found {}", self.nodes[sub].id()), &self.nodes[sub].start.clone()));
                    };
                    if pairs.len() != 1 {
                        return Err(ctor_err(Some(what), Some(&start), format!("expected a single mapping item, but found {} items", pairs.len()), &self.nodes[sub].start.clone()));
                    }
                    let k = self.construct_object(pairs[0].0, 0)?;
                    let v = self.construct_object(pairs[0].1, 0)?;
                    if let V::List(l) = &g.data {
                        l.lock().push(V::tuple(vec![k, v]));
                    }
                }
            }
        }
        Ok(())
    }

    /// SafeConstructor.construct_scalar: a mapping's `=` key stands for its value
    fn construct_scalar(&mut self, node: usize, depth: usize) -> R<String> {
        if depth > MAX_DEPTH {
            return Err(recursion());
        }
        if let NK::Map(pairs) = &self.nodes[node].kind {
            for (k, v) in pairs.clone() {
                if self.nodes[k].tag == TAG_VALUE {
                    return self.construct_scalar(v, depth + 1);
                }
            }
        }
        match &self.nodes[node].kind {
            NK::Scalar(s) => Ok(s.clone()),
            _ => Err(ctor_err(None, None, format!("expected a scalar node, but found {}", self.nodes[node].id()), &self.nodes[node].start)),
        }
    }

    fn construct_sequence(&mut self, node: usize) -> R<Vec<V>> {
        let NK::Seq(items) = self.nodes[node].kind.clone() else {
            return Err(ctor_err(None, None, format!("expected a sequence node, but found {}", self.nodes[node].id()), &self.nodes[node].start));
        };
        items.into_iter().map(|c| self.construct_object(c, 0)).collect()
    }

    fn flatten_mapping(&mut self, node: usize, depth: usize) -> R<()> {
        if depth > MAX_DEPTH {
            return Err(recursion());
        }
        super::stack_guard()?;
        let mut merge: Vec<(usize, usize)> = vec![];
        let mut index = 0;
        loop {
            let NK::Map(pairs) = &self.nodes[node].kind else { return Ok(()) };
            if index >= pairs.len() {
                break;
            }
            let (k, v) = pairs[index];
            if self.nodes[k].tag == TAG_MERGE {
                if let NK::Map(p) = &mut self.nodes[node].kind {
                    p.remove(index);
                }
                match self.nodes[v].kind.clone() {
                    NK::Map(_) => {
                        self.flatten_mapping(v, depth + 1)?;
                        if let NK::Map(p) = &self.nodes[v].kind {
                            merge.extend(p.iter().copied());
                        }
                    }
                    NK::Seq(subs) => {
                        let mut submerge = vec![];
                        for sub in subs {
                            if !matches!(self.nodes[sub].kind, NK::Map(_)) {
                                return Err(ctor_err(Some("while constructing a mapping"), Some(&self.nodes[node].start.clone()), format!("expected a mapping for merging, but found {}", self.nodes[sub].id()), &self.nodes[sub].start.clone()));
                            }
                            self.flatten_mapping(sub, depth + 1)?;
                            if let NK::Map(p) = &self.nodes[sub].kind {
                                submerge.push(p.clone());
                            }
                        }
                        submerge.reverse();
                        for p in submerge {
                            merge.extend(p);
                        }
                    }
                    NK::Scalar(_) => {
                        return Err(ctor_err(Some("while constructing a mapping"), Some(&self.nodes[node].start.clone()), format!("expected a mapping or list of mappings for merging, but found {}", self.nodes[v].id()), &self.nodes[v].start.clone()));
                    }
                }
            } else if self.nodes[k].tag == TAG_VALUE {
                self.nodes[k].tag = TAG_STR.to_string();
                index += 1;
            } else {
                index += 1;
            }
        }
        if !merge.is_empty() {
            if let NK::Map(p) = &mut self.nodes[node].kind {
                merge.extend(p.iter().copied());
                *p = merge;
            }
        }
        Ok(())
    }

    fn construct_mapping(&mut self, node: usize, depth: usize) -> R {
        if matches!(self.nodes[node].kind, NK::Map(_)) {
            self.flatten_mapping(node, depth)?;
        }
        let NK::Map(pairs) = self.nodes[node].kind.clone() else {
            return Err(ctor_err(None, None, format!("expected a mapping node, but found {}", self.nodes[node].id()), &self.nodes[node].start));
        };
        let mapping = V::empty_dict();
        for (k, v) in pairs {
            let key = self.construct_object(k, 0)?;
            if unhashable(&key) {
                return Err(ctor_err(Some("while constructing a mapping"), Some(&self.nodes[node].start.clone()), "found unhashable key".into(), &self.nodes[k].start.clone()));
            }
            let value = self.construct_object(v, 0)?;
            dict_set(&mapping, key, value)?;
        }
        Ok(mapping)
    }

    fn construct_int(&mut self, node: usize, depth: usize) -> R {
        let raw = self.construct_scalar(node, depth)?;
        let mut value: String = raw.replace('_', "");
        let index_err = || Exc::msg(&INDEX_ERROR, "string index out of range");
        let first = value.chars().next().ok_or_else(index_err)?;
        let neg = first == '-';
        if first == '+' || first == '-' {
            value.remove(0);
        }
        let v: BigInt = if value == "0" {
            BigInt::from(0)
        } else if let Some(d) = value.strip_prefix("0b") {
            py_int(d, 2)?
        } else if let Some(d) = value.strip_prefix("0x") {
            py_int(d, 16)?
        } else if value.chars().next().ok_or_else(index_err)? == '0' {
            py_int(&value, 8)?
        } else if value.contains(':') {
            let mut digits = value.split(':').map(|p| py_int(p, 10)).collect::<R<Vec<_>>>()?;
            digits.reverse();
            let mut base = BigInt::from(1);
            let mut total = BigInt::from(0);
            for d in digits {
                total += d * &base;
                base *= 60;
            }
            total
        } else {
            py_int(&value, 10)?
        };
        big_to_v(&if neg { -v } else { v }, &raw)
    }

    fn construct_float(&mut self, node: usize, depth: usize) -> R {
        let raw = self.construct_scalar(node, depth)?;
        let mut value: String = raw.replace('_', "").to_lowercase();
        let first = value.chars().next().ok_or_else(|| Exc::msg(&INDEX_ERROR, "string index out of range"))?;
        let sign = if first == '-' { -1.0 } else { 1.0 };
        if first == '+' || first == '-' {
            value.remove(0);
        }
        Ok(V::Float(if value == ".inf" {
            sign * f64::INFINITY
        } else if value == ".nan" {
            f64::NAN
        } else if value.contains(':') {
            let mut digits = value.split(':').map(py_float).collect::<R<Vec<_>>>()?;
            digits.reverse();
            let mut base = BigInt::from(1);
            let mut total = 0.0;
            for d in digits {
                let b = num_traits::ToPrimitive::to_f64(&base).unwrap_or(f64::INFINITY);
                if b.is_infinite() {
                    return Err(Exc::msg(&OVERFLOW_ERROR, "int too large to convert to float"));
                }
                total += d * b;
                base *= 60;
            }
            sign * total
        } else {
            sign * py_float(&value)?
        }))
    }

    fn construct_binary(&mut self, node: usize, depth: usize) -> R {
        let s = self.construct_scalar(node, depth)?;
        let start = &self.nodes[node].start;
        let chars: Vec<char> = s.chars().collect();
        if let Some(i) = chars.iter().position(|c| !c.is_ascii()) {
            let end = (i..chars.len()).find(|j| chars[*j].is_ascii()).unwrap_or(chars.len());
            let esc = |c: char| {
                let n = c as u32;
                if n < 0x100 { format!("'\\x{n:02x}'") } else if n < 0x10000 { format!("'\\u{n:04x}'") } else { format!("'\\U{n:08x}'") }
            };
            let exc = if end - i == 1 {
                format!("'ascii' codec can't encode character {} in position {i}: ordinal not in range(128)", esc(chars[i]))
            } else {
                format!("'ascii' codec can't encode characters in position {i}-{}: ordinal not in range(128)", end - 1)
            };
            return Err(ctor_err(None, None, format!("failed to convert base64 data into ascii: {exc}"), start));
        }
        match super::stdlib::b64dec(s.as_bytes(), false) {
            Ok(b) => Ok(V::Bytes(Arc::from(b))),
            Err(e) => Err(ctor_err(None, None, format!("failed to decode base64 data: {}", e.message()), start)),
        }
    }

    fn construct_timestamp(&mut self, node: usize, depth: usize) -> R {
        self.construct_scalar(node, depth)?;
        let value = match &self.nodes[node].kind {
            NK::Scalar(s) => s.clone(),
            _ => return Err(Exc::type_error("expected string or bytes-like object, got 'list'")),
        };
        let c = TIMESTAMP.captures(&value).ok_or_else(|| Exc::attr_error("'NoneType' object has no attribute 'groupdict'"))?;
        let num = |n: &str| c.name(n).map(|m| m.as_str().parse::<i64>().unwrap_or(0));
        let (year, month, day) = (num("year").unwrap(), num("month").unwrap(), num("day").unwrap());
        let Some(hour) = num("hour") else {
            return Ok(V::Date(super::methods::ymd(year, month, day)?));
        };
        let (minute, second) = (num("minute").unwrap(), num("second").unwrap());
        let mut fraction = 0;
        if let Some(f) = c.name("fraction").map(|m| m.as_str()).filter(|f| !f.is_empty()) {
            let mut f: String = f.chars().take(6).collect();
            while f.len() < 6 {
                f.push('0');
            }
            fraction = f.parse::<i64>().unwrap_or(0);
        }
        let tz = if let Some(sign) = c.name("tz_sign") {
            let secs = num("tz_hour").unwrap() * 3600 + num("tz_minute").unwrap_or(0) * 60;
            let secs = if sign.as_str() == "-" { -secs } else { secs };
            if secs.abs() >= 86400 {
                let delta = chrono::TimeDelta::seconds(secs);
                // CPython 3.14 dropped the final period of datetime's message (3.12 and 3.13 keep it, in every patch)
                return Err(Exc::value_error(format!(
                    "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not {}{}",
                    ops::repr(&V::Delta(delta))?,
                    if super::python_at_least(3, 14, 0) { "" } else { "." }
                )));
            }
            Some(if secs == 0 { super::dt::Tz::Utc } else { super::dt::Tz::Fixed(secs as i32) })
        } else if c.name("tz").is_some() {
            Some(super::dt::Tz::Utc)
        } else {
            None
        };
        let date = super::methods::ymd(year, month, day)?;
        super::methods::time_field(hour, 23, "hour")?;
        super::methods::time_field(minute, 59, "minute")?;
        super::methods::time_field(second, 59, "second")?;
        let wall = date.and_hms_micro_opt(hour as u32, minute as u32, second as u32, fraction as u32).unwrap();
        Ok(V::DateTime(match tz {
            Some(t) => super::dt::DateTime::aware(wall, t),
            None => super::dt::DateTime::naive(wall),
        }))
    }
}

static TIMESTAMP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"\A(?P<year>[0-9][0-9][0-9][0-9])-(?P<month>[0-9][0-9]?)-(?P<day>[0-9][0-9]?)",
        r"(?:(?:[Tt]|[ \t]+)(?P<hour>[0-9][0-9]?):(?P<minute>[0-9][0-9]):(?P<second>[0-9][0-9])",
        r"(?:\.(?P<fraction>[0-9]*))?(?:[ \t]*(?P<tz>Z|(?P<tz_sign>[-+])(?P<tz_hour>[0-9][0-9]?)",
        r"(?::(?P<tz_minute>[0-9][0-9]))?))?)?\n?\z"
    ))
    .unwrap()
});

// ---------------------------------------------------------------- yaml.safe_load

/// `yaml.safe_load(stream)` / `yaml.load(stream, Loader=yaml.SafeLoader)`: a str, bytes, or a readable file
/// object (open(), io.StringIO, io.BytesIO)
pub fn safe_load(stream: &V) -> R {
    let reader = reader_of(stream)?;
    let mut l = Loader { p: Parser::new(Scanner::new(reader)), nodes: vec![], anchors: HashMap::new() };
    let Some(root) = l.get_single_node()? else { return Ok(V::None) };
    let mut c = Ctor { nodes: &mut l.nodes, constructed: HashMap::new(), recursive: HashSet::new(), gens: vec![] };
    c.construct_document(root)
}

fn reader_of(stream: &V) -> R<Reader> {
    match stream {
        V::Str(s) => Reader::from_str(s),
        V::Bytes(b) => Reader::from_bytes(b),
        V::Native(n) => match &**n {
            Native::File(f) => {
                let (data, binary, name) = super::pathio::read_all(f)?;
                if binary {
                    Reader::from_stream(V::str(name), Chunks::Bytes(data, 0))
                } else {
                    let text = match super::methods::utf8_decode(&data, 0)? {
                        V::Str(s) => s,
                        _ => Arc::from(""),
                    };
                    // the text layer's universal newlines
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    Reader::from_stream(V::str(name), Chunks::Text(text.chars().collect(), 0))
                }
            }
            Native::StringIO(_) => match super::stdlib::stringio_method(match &**n {
                Native::StringIO(s) => s,
                _ => unreachable!(),
            }, "read", &[])? {
                V::Str(s) => Reader::from_stream(V::str("<file>"), Chunks::Text(s.chars().collect(), 0)),
                _ => Err(Exc::type_error("py2axum: unreadable StringIO")),
            },
            Native::BytesIO(b) => match super::files::bytesio_method(b, "read", &[])? {
                V::Bytes(d) => Reader::from_stream(V::str("<file>"), Chunks::Bytes(d.to_vec(), 0)),
                _ => Err(Exc::type_error("py2axum: unreadable BytesIO")),
            },
            _ => Err(Exc::type_error(format!("py2axum: yaml.safe_load() reads a str, bytes or a file object (open(), io.StringIO, io.BytesIO), not {}", stream.type_name()))),
        },
        V::None | V::Bool(_) | V::Int(_) | V::Float(_) | V::List(_) | V::Tuple(_) | V::Dict(_) | V::Set(_) | V::Date(_) | V::DateTime(_) => {
            Err(Exc::attr_error(format!("'{}' object has no attribute 'read'", stream.type_name())))
        }
        _ => Err(Exc::type_error(format!("py2axum: yaml.safe_load() reads a str, bytes or a file object (open(), io.StringIO, io.BytesIO), not {}", stream.type_name()))),
    }
}
