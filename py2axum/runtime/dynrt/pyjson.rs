//! `json.loads` as CPython's `_json` C scanner does it: same accepted texts (NaN, Infinity, any
//! whitespace among ` \t\n\r`, big exponents), same `JSONDecodeError` message and position (in code
//! points) for every malformed text, the trailing-comma messages of 3.13+. Iterative, so deep nesting
//! cannot overflow the stack: past `MAX_DEPTH` it raises RecursionError, as CPython does at a depth
//! that depends on its C stack (documented in supported.md).
use super::v::*;
use indexmap::IndexMap;
use parking_lot::Mutex;
use std::sync::Arc;

pub const MAX_DEPTH: usize = 10_000;

pub enum Fail {
    /// a JSONDecodeError: its `msg` and `pos`
    Decode(&'static str, usize),
    /// RecursionError while decoding a JSON object/array
    Recursion(&'static str),
    /// an integer beyond 64 bits (a Python int has no bound, `V::Int` has): OverflowError
    Overflow(String),
}

/// `JSONDecodeError.__str__`: `msg: line L column C (char P)`
pub fn message(text: &[char], msg: &str, pos: usize) -> String {
    let pos = pos.min(text.len());
    let line = text[..pos].iter().filter(|c| **c == '\n').count() + 1;
    let col = match text[..pos].iter().rposition(|c| *c == '\n') {
        Some(i) => pos - i,
        None => pos + 1,
    };
    format!("{msg}: line {line} column {col} (char {pos})")
}

/// The exception `json.loads` raises for a failure.
pub fn exc(text: &[char], f: Fail) -> Exc {
    match f {
        Fail::Decode(msg, pos) => Exc::msg(&JSON_DECODE_ERROR, message(text, msg, pos)),
        Fail::Recursion(what) => Exc::msg(&RECURSION_ERROR, format!("maximum recursion depth exceeded while decoding a JSON {what} from a unicode string")),
        Fail::Overflow(text) => overflow(&text),
    }
}

/// `json.loads(s)` for a str (`JSONDecoder.decode`, after the BOM check of `json.loads`).
pub fn decode(s: &[char]) -> Result<V, Fail> {
    if s.first() == Some(&'\u{feff}') {
        return Err(Fail::Decode("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    let mut idx = ws(s, 0);
    let v = scan(s, &mut idx)?;
    idx = ws(s, idx);
    if idx != s.len() {
        return Err(Fail::Decode("Extra data", idx));
    }
    Ok(v)
}

/// `json.loads` on a str, raising like CPython.
pub fn loads(text: &str) -> R {
    let chars: Vec<char> = text.chars().collect();
    decode(&chars).map_err(|f| exc(&chars, f))
}

fn ws(s: &[char], mut i: usize) -> usize {
    while i < s.len() && matches!(s[i], ' ' | '\t' | '\n' | '\r') {
        i += 1;
    }
    i
}

fn trailing_comma_messages() -> bool {
    super::python() >= (3, 13)
}

/// A `\uXXXX` escape ending the text: before a CPython patch release (measured: 3.13.12 and 3.14.3
/// still, 3.13.16 and 3.14.8 no longer), "Invalid \uXXXX escape"; since, the string is unterminated.
/// The patch the application runs on is not known statically: the newest behaviour, unless
/// `PY2AXUM_PYTHON_VERSION` (e.g. `3.14.0`) names an older patch. 3.12 kept the old one.
fn u_escape_at_end_is_invalid() -> bool {
    static OLD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OLD.get_or_init(|| {
        let (major, minor) = super::python();
        if (major, minor) < (3, 13) {
            return true;
        }
        let Ok(v) = std::env::var("PY2AXUM_PYTHON_VERSION") else { return false };
        let n: Vec<u32> = v.split('.').map(|x| x.parse().unwrap_or(0)).collect();
        let patch = n.get(2).copied().unwrap_or(u32::MAX);
        match (n.first().copied(), n.get(1).copied()) {
            (Some(3), Some(13)) => patch <= 12,
            (Some(3), Some(14)) => patch <= 3,
            (Some(3), Some(m)) => m < 13,
            _ => false,
        }
    })
}

enum Frame {
    List(Vec<V>),
    /// the object so far and the key whose value is being read
    Obj(IndexMap<Key, (V, V)>, Arc<str>),
}

fn new_dict(m: IndexMap<Key, (V, V)>) -> V {
    V::Dict(Arc::new(Mutex::new(m)))
}

/// `scan_once` from `idx`: one value, containers read with an explicit stack.
fn scan(s: &[char], idx: &mut usize) -> Result<V, Fail> {
    let end = s.len();
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        // read one term at *idx; a container start pushes a frame and loops
        let mut value: Option<V> = None;
        if *idx >= end {
            return Err(Fail::Decode("Expecting value", *idx));
        }
        let at = |i: usize, lit: &str| lit.chars().enumerate().all(|(k, c)| s.get(i + k) == Some(&c));
        match s[*idx] {
            '"' => {
                let (t, next) = scanstring(s, *idx + 1)?;
                *idx = next;
                value = Some(V::str(t));
            }
            '{' => {
                if stack.len() >= MAX_DEPTH {
                    return Err(Fail::Recursion("object"));
                }
                let mut i = ws(s, *idx + 1);
                if i < end && s[i] == '}' {
                    *idx = i + 1;
                    value = Some(new_dict(IndexMap::new()));
                } else {
                    let key = read_key(s, &mut i)?;
                    stack.push(Frame::Obj(IndexMap::new(), key));
                    *idx = i;
                    continue;
                }
            }
            '[' => {
                if stack.len() >= MAX_DEPTH {
                    return Err(Fail::Recursion("array"));
                }
                let i = ws(s, *idx + 1);
                if i < end && s[i] == ']' {
                    *idx = i + 1;
                    value = Some(V::list(Vec::new()));
                } else {
                    stack.push(Frame::List(Vec::new()));
                    *idx = i;
                    continue;
                }
            }
            'n' if at(*idx, "null") => {
                *idx += 4;
                value = Some(V::None);
            }
            't' if at(*idx, "true") => {
                *idx += 4;
                value = Some(V::Bool(true));
            }
            'f' if at(*idx, "false") => {
                *idx += 5;
                value = Some(V::Bool(false));
            }
            'N' if at(*idx, "NaN") => {
                *idx += 3;
                value = Some(V::Float(f64::NAN));
            }
            'I' if at(*idx, "Infinity") => {
                *idx += 8;
                value = Some(V::Float(f64::INFINITY));
            }
            '-' if at(*idx, "-Infinity") => {
                *idx += 9;
                value = Some(V::Float(f64::NEG_INFINITY));
            }
            _ => {}
        }
        let mut v = match value {
            Some(v) => v,
            None => number(s, idx)?,
        };
        // hand the value to the enclosing containers, closing those that end here
        loop {
            match stack.last_mut() {
                None => return Ok(v),
                Some(Frame::List(items)) => {
                    items.push(v);
                    let i = ws(s, *idx);
                    if i < end && s[i] == ']' {
                        *idx = i + 1;
                        let Some(Frame::List(items)) = stack.pop() else { unreachable!() };
                        v = V::list(items);
                        continue;
                    }
                    if i >= end || s[i] != ',' {
                        return Err(Fail::Decode("Expecting ',' delimiter", i));
                    }
                    let j = ws(s, i + 1);
                    if trailing_comma_messages() && j < end && s[j] == ']' {
                        return Err(Fail::Decode("Illegal trailing comma before end of array", i));
                    }
                    *idx = j;
                    break;
                }
                Some(Frame::Obj(m, key)) => {
                    // a key seen again keeps its first position, with the last value (dict assignment)
                    m.insert(Key::Str(key.clone()), (V::Str(key.clone()), v));
                    let i = ws(s, *idx);
                    if i < end && s[i] == '}' {
                        *idx = i + 1;
                        let Some(Frame::Obj(m, _)) = stack.pop() else { unreachable!() };
                        v = new_dict(m);
                        continue;
                    }
                    if i >= end || s[i] != ',' {
                        return Err(Fail::Decode("Expecting ',' delimiter", i));
                    }
                    let mut j = ws(s, i + 1);
                    if trailing_comma_messages() && j < end && s[j] == '}' {
                        return Err(Fail::Decode("Illegal trailing comma before end of object", i));
                    }
                    *key = read_key(s, &mut j)?;
                    *idx = j;
                    break;
                }
            }
        }
    }
}

/// `"key"` then `:` (whitespace around): leaves `i` on the value.
fn read_key(s: &[char], i: &mut usize) -> Result<Arc<str>, Fail> {
    if *i >= s.len() || s[*i] != '"' {
        return Err(Fail::Decode("Expecting property name enclosed in double quotes", *i));
    }
    let (k, next) = scanstring(s, *i + 1)?;
    let j = ws(s, next);
    if j >= s.len() || s[j] != ':' {
        return Err(Fail::Decode("Expecting ':' delimiter", j));
    }
    *i = ws(s, j + 1);
    Ok(Arc::from(k.as_str()))
}

/// `_match_number_unicode`: the longest JSON number at `idx`, int or float.
fn number(s: &[char], idx: &mut usize) -> Result<V, Fail> {
    let start = *idx;
    let last = s.len() - 1; // end_idx in CPython
    let digit = |i: usize| s.get(i).is_some_and(|c| c.is_ascii_digit());
    let mut i = start;
    if s[i] == '-' {
        i += 1;
        if i > last {
            return Err(Fail::Decode("Expecting value", start));
        }
    }
    if ('1'..='9').contains(&s[i]) {
        i += 1;
        while i <= last && digit(i) {
            i += 1;
        }
    } else if s[i] == '0' {
        i += 1;
    } else {
        return Err(Fail::Decode("Expecting value", start));
    }
    let mut float = false;
    if i < last && s[i] == '.' && digit(i + 1) {
        float = true;
        i += 2;
        while i <= last && digit(i) {
            i += 1;
        }
    }
    if i < last && (s[i] == 'e' || s[i] == 'E') {
        let e_start = i;
        i += 1;
        if i < last && (s[i] == '-' || s[i] == '+') {
            i += 1;
        }
        while i <= last && digit(i) {
            i += 1;
        }
        if digit(i - 1) {
            float = true;
        } else {
            i = e_start;
        }
    }
    let text: String = s[start..i].iter().collect();
    *idx = i;
    if float {
        return Ok(V::Float(text.parse::<f64>().unwrap_or(f64::NAN)));
    }
    text.parse::<i64>().map(V::Int).map_err(|_| Fail::Overflow(text))
}

pub fn overflow(text: &str) -> Exc {
    Exc::msg(&OVERFLOW_ERROR, format!("py2axum: the JSON integer {text} does not fit in 64 bits"))
}

/// `scanstring_unicode(s, end, strict=True)`: the string whose opening quote is at `end - 1`, and the
/// index after its closing quote. A lone surrogate escape (`"\ud83d"`) becomes U+FFFD (a Rust string
/// cannot hold it; documented in supported.md).
fn scanstring(s: &[char], mut end: usize) -> Result<(String, usize), Fail> {
    let begin = end - 1;
    let len = s.len();
    let mut out = String::new();
    loop {
        let mut next = end;
        let mut c = '\0';
        while next < len {
            c = s[next];
            if c == '"' || c == '\\' {
                break;
            }
            if (c as u32) <= 0x1f {
                return Err(Fail::Decode("Invalid control character at", next));
            }
            next += 1;
        }
        if next >= len || !(c == '"' || c == '\\') {
            return Err(Fail::Decode("Unterminated string starting at", begin));
        }
        out.extend(&s[end..next]);
        next += 1;
        if c == '"' {
            return Ok((out, next));
        }
        if next == len {
            return Err(Fail::Decode("Unterminated string starting at", begin));
        }
        let e = s[next];
        if e != 'u' {
            end = next + 1;
            out.push(match e {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                _ => return Err(Fail::Decode("Invalid \\escape", end - 2)),
            });
            continue;
        }
        next += 1;
        end = next + 4;
        if end > len || (end == len && u_escape_at_end_is_invalid()) {
            return Err(Fail::Decode("Invalid \\uXXXX escape", next - 1));
        }
        let hex4 = |from: usize, to: usize| -> Result<u32, Fail> {
            let mut v = 0u32;
            for k in from..to {
                v = (v << 4) | s[k].to_digit(16).ok_or(Fail::Decode("Invalid \\uXXXX escape", to - 5))?;
            }
            Ok(v)
        };
        let mut u = hex4(next, end)?;
        next = end;
        if (0xD800..0xDC00).contains(&u) && end + 6 < len && s[next] == '\\' && s[next + 1] == 'u' {
            let u2 = hex4(next + 2, end + 6)?;
            if (0xDC00..0xE000).contains(&u2) {
                u = 0x10000 + ((u - 0xD800) << 10) + (u2 - 0xDC00);
                end += 6;
            }
        }
        out.push(char::from_u32(u).unwrap_or('\u{fffd}'));
    }
}
