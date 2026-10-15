//! `orjson` 3.11–3.13: `loads` and `dumps` (options `OPT_*`, `default=`), and `ORJSONResponse`'s body.
//!
//! `loads` is a port of the yyjson reader as orjson builds it (`YYJSON_DISABLE_NON_STANDARD`, no flags,
//! `YYJSON_READER_DEPTH_LIMIT` 1024): same grammar, same error messages and positions (in bytes, turned into
//! characters as orjson does), the "truncated input" rule (`is_truncated_end`) that reports an error at the end
//! of a cut document as "unexpected end of data". Integers that fit in 64 bits (`u64` above, `i64` below) are
//! ints, others are floats, like yyjson; an int in (2^63, 2^64) is refused like the JSON decoder's big ints.
//!
//! `dumps` follows orjson's serializer (`serialize/writer/container.rs`): compact or `OPT_INDENT_2` output,
//! zmij float formatting (the same crate and version as orjson 3.13), datetime/date/time/UUID/Enum/dataclass,
//! `OPT_NON_STR_KEYS` key strings, `OPT_SORT_KEYS` by UTF-8 bytes, recursion limits (254 containers, 255 nested
//! `default` calls) and its error messages. `orjson.JSONEncodeError` is `TypeError` itself.

use std::sync::Arc;

use futures_util::FutureExt;
use indexmap::IndexMap;

use super::v::*;
use super::Cx;

pub const OPT_INDENT_2: i64 = 1;
pub const OPT_NAIVE_UTC: i64 = 2;
pub const OPT_NON_STR_KEYS: i64 = 4;
pub const OPT_OMIT_MICROSECONDS: i64 = 8;
pub const OPT_SERIALIZE_NUMPY: i64 = 16;
pub const OPT_SORT_KEYS: i64 = 32;
pub const OPT_STRICT_INTEGER: i64 = 64;
pub const OPT_UTC_Z: i64 = 128;
pub const OPT_PASSTHROUGH_SUBCLASS: i64 = 256;
pub const OPT_PASSTHROUGH_DATETIME: i64 = 512;
pub const OPT_APPEND_NEWLINE: i64 = 1024;
pub const OPT_PASSTHROUGH_DATACLASS: i64 = 2048;
const MAX_OPT: i64 = 4095;

// ------------------------------------------------------------------------------------------------ loads

const INPUT_TYPE: &str = "Input must be bytes, bytearray, memoryview, or str";

/// `orjson.loads(obj)`
pub fn loads(obj: &V) -> R {
    let text: &str = match obj {
        V::Str(s) => s,
        V::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => s,
            Err(_) => return Err(decode_error("str is not valid UTF-8: surrogates not allowed", "", 0)),
        },
        _ => return Err(decode_error(INPUT_TYPE, "", 0)),
    };
    if text.is_empty() {
        return Err(decode_error("Input is a zero-length, empty document", "", 0));
    }
    match text.as_bytes() {
        b"[]" => return Ok(V::list(Vec::new())),
        b"{}" => return Ok(V::empty_dict()),
        b"\"\"" => return Ok(V::str("")),
        _ => {}
    }
    let mut buf = Vec::with_capacity(text.len() + 4);
    buf.extend_from_slice(text.as_bytes());
    buf.extend_from_slice(&[0, 0, 0, 0]);
    let r = Reader { s: &buf, eof: text.len() };
    match r.read() {
        Ok(v) => Ok(v),
        Err(Fail::Overflow(t)) => Err(super::pyjson::overflow(&t)),
        Err(Fail::At(pos, msg)) => {
            let chars = text.get(..pos).map(|p| p.chars().count()).unwrap_or(pos);
            Err(decode_error(msg, text, chars))
        }
    }
}

/// `orjson.JSONDecodeError(msg, doc, pos)` (a `json.JSONDecodeError`): its message and attributes.
fn decode_error(msg: &str, doc: &str, pos: usize) -> Exc {
    let chars: Vec<char> = doc.chars().collect();
    let p = pos.min(chars.len());
    let line = chars[..p].iter().filter(|c| **c == '\n').count() + 1;
    let col = match chars[..p].iter().rposition(|c| *c == '\n') {
        Some(i) => p - i,
        None => p + 1,
    };
    let e = Exc::msg(&ORJSON_DECODE_ERROR, format!("{msg}: line {line} column {col} (char {pos})"));
    {
        let mut a = e.0.attrs.lock();
        a.insert("msg".into(), V::str(msg));
        a.insert("doc".into(), V::str(doc));
        a.insert("pos".into(), V::Int(pos as i64));
        a.insert("lineno".into(), V::Int(line as i64));
        a.insert("colno".into(), V::Int(col as i64));
    }
    e
}

enum Fail {
    At(usize, &'static str),
    Overflow(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Code {
    Str,
    Num,
    Lit,
    Char,
    Other,
}

const MSG_NOT_END: &str = "unexpected end of data";
const MSG_CHAR: &str = "unexpected character, expected a JSON value";
const MSG_ARR_END: &str = "unexpected character, expected ',' or ']'";
const MSG_OBJ_KEY: &str = "unexpected character, expected a string key";
const MSG_OBJ_SEP: &str = "unexpected character, expected ':' after key";
const MSG_OBJ_END: &str = "unexpected character, expected ',' or '}'";
const MSG_GARBAGE: &str = "unexpected content after document";
const MSG_COMMA: &str = "trailing comma is not allowed";
const MSG_DEPTH: &str = "depth limit exceeded";
const MSG_MALLOC: &str = "failed to allocate memory";
const MSG_BOM: &str = "UTF-8 byte order mark (BOM) is not supported";
const DEPTH_LIMIT: u32 = 1024;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_num(c: u8) -> bool {
    matches!(c, b'-' | b'+' | b'.' | b'0'..=b'9')
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

enum Frame {
    Arr(Vec<V>),
    Obj(IndexMap<Key, (V, V)>, Option<V>),
}

struct Reader<'a> {
    /// the document and 4 zero bytes (yyjson's padding: reads past the end see NUL)
    s: &'a [u8],
    eof: usize,
}

impl Reader<'_> {
    /// `return_err` of the root readers: an error in a cut document is "unexpected end of data" at its end
    fn fail(&self, pos: usize, code: Code, msg: &'static str) -> Fail {
        if self.truncated(pos, code) {
            Fail::At(self.eof, MSG_NOT_END)
        } else {
            Fail::At(pos, msg)
        }
    }

    /// `is_truncated_end` without the non-standard flags
    fn truncated(&self, cur: usize, code: Code) -> bool {
        let (s, eof) = (self.s, self.eof);
        if cur >= eof {
            return true;
        }
        if code == Code::Lit {
            let trunc = |lit: &[u8]| cur + lit.len() > eof && s[cur..eof] == lit[..eof - cur];
            if trunc(b"true") || trunc(b"false") || trunc(b"null") {
                return true;
            }
        }
        if code == Code::Str && s[cur] == b'\\' {
            let len = eof - cur;
            if len == 1 {
                return true;
            }
            if len <= 5 {
                return s[cur + 1] == b'u' && s[cur + 2..eof].iter().all(|c| is_hex(*c));
            }
            if len <= 11 {
                if s[cur + 1] != b'u' || !s[cur + 2..cur + 6].iter().all(|c| is_hex(*c)) {
                    return false;
                }
                let hi = hex4(&s[cur + 2..cur + 6]);
                if hi & 0xF800 != 0xD800 {
                    return false;
                }
                let mut c = cur + 6;
                if c >= eof {
                    return true;
                }
                for want in [b"\\".as_slice(), b"u", b"dD", b"cdefCDEF"] {
                    if !want.contains(&s[c]) {
                        return false;
                    }
                    c += 1;
                    if c >= eof {
                        return true;
                    }
                }
                return is_hex(s[c]);
            }
            return false;
        }
        false
    }

    fn read(&self) -> Result<V, Fail> {
        let s = self.s;
        let mut cur = 0;
        if !matches!(s[cur], b'{' | b'[') {
            while is_space(s[cur]) {
                cur += 1;
            }
            if cur >= self.eof {
                return Err(Fail::At(0, "input data is empty"));
            }
        }
        let r = if matches!(s[cur], b'{' | b'[') { self.container(cur) } else { self.single(cur) };
        match r {
            Err(Fail::At(0, _)) if s.starts_with(&[0xEF, 0xBB, 0xBF]) => Err(Fail::At(0, MSG_BOM)),
            r => r,
        }
    }

    fn doc_end(&self, mut cur: usize) -> Result<(), Fail> {
        if cur < self.eof {
            while is_space(self.s[cur]) {
                cur += 1;
            }
            if cur < self.eof {
                return Err(self.fail(cur, Code::Other, MSG_GARBAGE));
            }
        }
        Ok(())
    }

    fn single(&self, cur: usize) -> Result<V, Fail> {
        let (v, end) = match self.s[cur] {
            c if is_num(c) => self.num(cur)?,
            b'"' => self.string(cur)?,
            b't' | b'f' | b'n' => self.literal(cur)?,
            _ => return Err(self.fail(cur, Code::Char, MSG_CHAR)),
        };
        self.doc_end(end)?;
        Ok(v)
    }

    /// a scalar value at `cur` (a number, a string or a literal), and the position after it
    fn scalar(&self, cur: usize) -> Result<Option<(V, usize)>, Fail> {
        Ok(Some(match self.s[cur] {
            c if is_num(c) => self.num(cur)?,
            b'"' => self.string(cur)?,
            b't' | b'f' | b'n' => self.literal(cur)?,
            _ => return Ok(None),
        }))
    }

    fn literal(&self, cur: usize) -> Result<(V, usize), Fail> {
        let s = self.s;
        match s[cur] {
            b't' if &s[cur..cur + 4] == b"true" => Ok((V::Bool(true), cur + 4)),
            b'f' if &s[cur + 1..cur + 5] == b"alse" => Ok((V::Bool(false), cur + 5)),
            b'n' if &s[cur..cur + 4] == b"null" => Ok((V::None, cur + 4)),
            b't' => Err(self.fail(cur, Code::Lit, "invalid literal, expected 'true'")),
            b'f' => Err(self.fail(cur, Code::Lit, "invalid literal, expected 'false'")),
            _ => Err(self.fail(cur, Code::Lit, "invalid literal, expected 'null'")),
        }
    }

    fn container(&self, mut cur: usize) -> Result<V, Fail> {
        #[derive(Clone, Copy)]
        enum St {
            ArrVal,
            ArrEnd,
            ObjKey,
            ObjSep,
            ObjVal,
            ObjEnd,
        }
        let s = self.s;
        let mut stack: Vec<Frame> = Vec::new();
        let mut depth: u32 = 0;
        // orjson parses in a fixed pool (`buffer_capacity_to_allocate`): the copied input, then the value array,
        // grown by half (`val_incr`) in place while the pool has room, else "failed to allocate memory"
        let len = self.eof;
        let cap = ((len / 2) * 24 + 256 + 4095) & !4095;
        let avail = cap.saturating_sub(16 + ((len + 4 + 15) & !15) + 16);
        let ratio = if is_space(s[cur + 1]) && is_space(s[cur + 2]) { 16 } else { 6 };
        let mut alc = 4 + (self.eof - cur) / ratio + 4;
        if alc * 16 + 16 > avail {
            return Err(self.fail(cur, Code::Other, MSG_MALLOC));
        }
        let mut val = 4usize;
        let incr = |val: &mut usize, alc: &mut usize, at: usize| -> Result<(), Fail> {
            *val += 1;
            if *val >= *alc - 2 {
                *alc += *alc / 2;
                if *alc * 16 + 16 > avail {
                    return Err(self.fail(at, Code::Other, MSG_MALLOC));
                }
            }
            Ok(())
        };
        let mut st = if s[cur] == b'{' {
            stack.push(Frame::Obj(IndexMap::new(), None));
            St::ObjKey
        } else {
            stack.push(Frame::Arr(Vec::new()));
            St::ArrVal
        };
        cur += 1;
        loop {
            // the value just completed (a scalar, or a closed container), to store in the current frame
            let done: V;
            match st {
                St::ArrVal | St::ObjVal => {
                    let c = s[cur];
                    if c == b'{' || c == b'[' {
                        cur += 1;
                        depth += 1;
                        if depth >= DEPTH_LIMIT {
                            return Err(self.fail(cur, Code::Other, MSG_DEPTH));
                        }
                        incr(&mut val, &mut alc, cur)?;
                        if c == b'{' {
                            stack.push(Frame::Obj(IndexMap::new(), None));
                            st = St::ObjKey;
                        } else {
                            stack.push(Frame::Arr(Vec::new()));
                            st = St::ArrVal;
                        }
                        continue;
                    }
                    if matches!(c, b'"' | b't' | b'f' | b'n') || is_num(c) {
                        // an object's value takes the slot reserved with its key (`val++`, unchecked)
                        if matches!(st, St::ArrVal) {
                            incr(&mut val, &mut alc, cur)?;
                        } else {
                            val += 1;
                        }
                    }
                    if let Some((v, end)) = self.scalar(cur)? {
                        cur = end;
                        done = v;
                    } else if is_space(c) {
                        while is_space(s[cur]) {
                            cur += 1;
                        }
                        continue;
                    } else if c == b']' && matches!(st, St::ArrVal) {
                        let Some(Frame::Arr(items)) = stack.last() else { unreachable!() };
                        if !items.is_empty() {
                            let mut p = cur;
                            while s[p] != b',' {
                                p -= 1;
                            }
                            return Err(self.fail(p, Code::Other, MSG_COMMA));
                        }
                        cur += 1;
                        let Some(Frame::Arr(items)) = stack.pop() else { unreachable!() };
                        done = V::list(items);
                        if stack.is_empty() {
                            self.doc_end(cur)?;
                            return Ok(done);
                        }
                        depth -= 1;
                    } else {
                        return Err(self.fail(cur, Code::Char, MSG_CHAR));
                    }
                }
                St::ObjKey => {
                    let c = s[cur];
                    if c == b'"' {
                        incr(&mut val, &mut alc, cur)?;
                        let (k, end) = self.string(cur)?;
                        cur = end;
                        let Some(Frame::Obj(_, key)) = stack.last_mut() else { unreachable!() };
                        *key = Some(k);
                        st = St::ObjSep;
                        continue;
                    }
                    if c == b'}' {
                        let Some(Frame::Obj(m, _)) = stack.last() else { unreachable!() };
                        if !m.is_empty() {
                            let mut p = cur;
                            while s[p] != b',' {
                                p -= 1;
                            }
                            return Err(self.fail(p, Code::Other, MSG_COMMA));
                        }
                        cur += 1;
                        let Some(Frame::Obj(m, _)) = stack.pop() else { unreachable!() };
                        done = V::Dict(Arc::new(parking_lot::Mutex::new(m)));
                        if stack.is_empty() {
                            self.doc_end(cur)?;
                            return Ok(done);
                        }
                        depth -= 1;
                    } else if is_space(c) {
                        while is_space(s[cur]) {
                            cur += 1;
                        }
                        continue;
                    } else {
                        return Err(self.fail(cur, Code::Char, MSG_OBJ_KEY));
                    }
                }
                St::ObjSep => {
                    match s[cur] {
                        b':' => {
                            cur += 1;
                            st = St::ObjVal;
                        }
                        c if is_space(c) => {
                            while is_space(s[cur]) {
                                cur += 1;
                            }
                        }
                        _ => return Err(self.fail(cur, Code::Char, MSG_OBJ_SEP)),
                    }
                    continue;
                }
                St::ArrEnd | St::ObjEnd => {
                    let arr = matches!(st, St::ArrEnd);
                    match s[cur] {
                        b',' => {
                            cur += 1;
                            st = if arr { St::ArrVal } else { St::ObjKey };
                            continue;
                        }
                        b']' if arr => {}
                        b'}' if !arr => {}
                        c if is_space(c) => {
                            while is_space(s[cur]) {
                                cur += 1;
                            }
                            continue;
                        }
                        _ => return Err(self.fail(cur, Code::Char, if arr { MSG_ARR_END } else { MSG_OBJ_END })),
                    }
                    cur += 1;
                    done = match stack.pop() {
                        Some(Frame::Arr(items)) => V::list(items),
                        Some(Frame::Obj(m, _)) => V::Dict(Arc::new(parking_lot::Mutex::new(m))),
                        None => unreachable!(),
                    };
                    if stack.is_empty() {
                        self.doc_end(cur)?;
                        return Ok(done);
                    }
                    depth -= 1;
                }
            }
            // store the completed value in the enclosing container
            match stack.last_mut() {
                Some(Frame::Arr(items)) => {
                    items.push(done);
                    st = St::ArrEnd;
                }
                Some(Frame::Obj(m, key)) => {
                    let k = key.take().unwrap_or(V::None);
                    let kk = match &k {
                        V::Str(t) => Key::Str(t.clone()),
                        _ => unreachable!(),
                    };
                    match m.get_mut(&kk) {
                        // a repeated key keeps its first place, the last value wins (dict assignment)
                        Some(slot) => slot.1 = done,
                        None => {
                            m.insert(kk, (k, done));
                        }
                    }
                    st = St::ObjEnd;
                }
                None => unreachable!(),
            }
        }
    }

    /// `read_num`: the number at `cur` (strict JSON grammar), and the position after it
    fn num(&self, cur: usize) -> Result<(V, usize), Fail> {
        let s = self.s;
        let hdr = cur;
        let neg = s[cur] == b'-';
        let mut c = cur + neg as usize;
        let err = |p: usize, m: &'static str| Err(self.fail(p, Code::Num, m));
        let digit = |x: u8| x.is_ascii_digit();
        let mut float = false;
        if !matches!(s[c], b'1'..=b'9') {
            if s[c] != b'0' {
                return err(c, "no digit after sign");
            }
            c += 1;
            if !(digit(s[c]) || matches!(s[c], b'.' | b'e' | b'E')) {
                return Ok((V::Int(0), c));
            }
            if digit(s[c]) {
                return err(c - 1, "number with leading zero is not allowed");
            }
        } else {
            while digit(s[c]) {
                c += 1;
            }
        }
        if s[c] == b'.' {
            float = true;
            c += 1;
            if !digit(s[c]) {
                return err(c, "no digit after decimal point");
            }
            while digit(s[c]) {
                c += 1;
            }
        }
        if matches!(s[c], b'e' | b'E') {
            float = true;
            c += 1;
            if matches!(s[c], b'+' | b'-') {
                c += 1;
            }
            if !digit(s[c]) {
                return err(c, "no digit after exponent sign");
            }
            while digit(s[c]) {
                c += 1;
            }
        }
        // the grammar guarantees ASCII
        let text = std::str::from_utf8(&s[hdr..c]).unwrap_or("0");
        if !float {
            if neg {
                if let Ok(i) = text.parse::<i64>() {
                    return Ok((V::Int(i), c));
                }
            } else if let Ok(u) = text.parse::<u64>() {
                return match i64::try_from(u) {
                    Ok(i) => Ok((V::Int(i), c)),
                    Err(_) => Err(Fail::Overflow(text.to_string())),
                };
            }
        }
        let f: f64 = text.parse().unwrap_or(f64::NAN);
        if f.is_infinite() {
            return err(hdr, "number is infinity when parsed as double");
        }
        Ok((V::Float(f), c))
    }

    /// `read_str`: the string whose opening quote is at `cur`, and the position after its closing quote
    fn string(&self, cur: usize) -> Result<(V, usize), Fail> {
        let s = self.s;
        let mut src = cur + 1;
        let mut out: Vec<u8> = Vec::new();
        let err = |p: usize, m: &'static str| Err(self.fail(p, Code::Str, m));
        loop {
            let b = s[src];
            if b == b'"' {
                // the input is valid UTF-8 and escapes produce whole characters
                let t = String::from_utf8(out).unwrap_or_default();
                return Ok((V::str(t), src + 1));
            }
            if b == b'\\' {
                let e = s[src + 1];
                let one = match e {
                    b'"' => Some(b'"'),
                    b'\\' => Some(b'\\'),
                    b'/' => Some(b'/'),
                    b'b' => Some(8),
                    b'f' => Some(12),
                    b'n' => Some(b'\n'),
                    b'r' => Some(b'\r'),
                    b't' => Some(b'\t'),
                    _ => None,
                };
                if let Some(x) = one {
                    out.push(x);
                    src += 2;
                    continue;
                }
                if e != b'u' {
                    return err(src, "invalid escaped sequence in string");
                }
                if !s[src + 2..src + 6].iter().all(|c| is_hex(*c)) {
                    return err(src, "invalid escaped sequence in string");
                }
                let hi = hex4(&s[src + 2..src + 6]) as u32;
                let ch = if hi & 0xF800 != 0xD800 {
                    src += 6;
                    hi
                } else {
                    if hi & 0xFC00 != 0xD800 {
                        return err(src, "invalid high surrogate in string");
                    }
                    if &s[src + 6..src + 8] != b"\\u" {
                        return err(src, "no low surrogate in string");
                    }
                    if !s[src + 8..src + 12].iter().all(|c| is_hex(*c)) {
                        return err(src, "invalid escape in string");
                    }
                    let lo = hex4(&s[src + 8..src + 12]) as u32;
                    if lo & 0xFC00 != 0xDC00 {
                        return err(src, "invalid low surrogate in string");
                    }
                    src += 12;
                    0x10000 + (((hi - 0xD800) << 10) | (lo - 0xDC00))
                };
                let mut tmp = [0u8; 4];
                out.extend_from_slice(char::from_u32(ch).unwrap_or('\u{fffd}').encode_utf8(&mut tmp).as_bytes());
                continue;
            }
            if b < 0x20 {
                return err(src, "unexpected control character in string");
            }
            out.push(b);
            src += 1;
        }
    }
}

fn hex4(h: &[u8]) -> u16 {
    h.iter().fold(0u16, |a, c| a * 16 + (*c as char).to_digit(16).unwrap_or(0) as u16)
}

// ------------------------------------------------------------------------------------------------ dumps

enum SerErr {
    Msg(&'static str),
    Unsupported(String, Option<Exc>),
    /// an exception raised while serializing (a `default` that the binary could not call...)
    Exc(Exc),
}

struct Ser<'a> {
    out: Vec<u8>,
    opts: i64,
    indent: usize,
    recursion: u32,
    default_calls: u32,
    default: Option<&'a V>,
    cx: Option<&'a Cx>,
}

/// `orjson.dumps(obj, default=None, option=None)`
pub async fn dumps(cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let mut obj = None;
    let mut default = None;
    let mut option = None;
    for (i, a) in args.into_iter().enumerate() {
        match i {
            0 => obj = Some(a),
            1 => default = Some(a),
            2 => option = Some(a),
            _ => return Err(Exc::type_error(format!("dumps() takes at most 3 arguments ({} given)", i + 1))),
        }
    }
    for (k, v) in kwargs {
        match k.as_str() {
            "default" if default.is_none() => default = Some(v),
            "default" => return Err(Exc::type_error("dumps() got multiple values for argument: 'default'")),
            "option" if option.is_none() => option = Some(v),
            "option" => return Err(Exc::type_error("dumps() got multiple values for argument: 'option'")),
            _ => return Err(Exc::type_error("dumps() got an unexpected keyword argument")),
        }
    }
    let Some(obj) = obj else {
        return Err(Exc::type_error("dumps() missing 1 required positional argument: 'obj'"));
    };
    let opts = match option {
        None | Some(V::None) => 0,
        Some(V::Int(n)) if (0..=MAX_OPT).contains(&n) => n,
        Some(V::Bool(b)) => b as i64,
        Some(_) => return Err(Exc::type_error("Invalid opts")),
    };
    // an explicit `default=None` is called like any default (orjson: "'NoneType' object is not callable")
    dumps_with(Some(cx), &obj, opts, default.as_ref()).await.map(|b| V::Bytes(Arc::from(b)))
}

/// The serialized bytes of `obj` (no `default`): `ORJSONResponse.render`.
pub fn dumps_bytes(obj: &V, opts: i64) -> R<Vec<u8>> {
    match dumps_with(None, obj, opts, None).now_or_never() {
        Some(r) => r,
        None => Err(Exc::runtime("py2axum: orjson serialization did not complete")),
    }
}

async fn dumps_with(cx: Option<&Cx>, obj: &V, opts: i64, default: Option<&V>) -> R<Vec<u8>> {
    let mut s = Ser { out: Vec::with_capacity(64), opts, indent: 0, recursion: 0, default_calls: 0, default, cx };
    match s.ser(obj).await {
        Ok(()) => {
            if opts & OPT_APPEND_NEWLINE != 0 {
                s.out.push(b'\n');
            }
            Ok(s.out)
        }
        Err(SerErr::Msg(m)) => Err(Exc::type_error(m)),
        Err(SerErr::Exc(e)) => Err(e),
        Err(SerErr::Unsupported(m, cause)) => {
            let e = Exc::type_error(m);
            if let Some(c) = cause {
                e.0.attrs.lock().insert("__cause__".into(), V::Exc(c));
            }
            Err(e)
        }
    }
}

/// `type(obj).__name__` as `tp_name` spells it (C types carry their module)
fn tp_name(v: &V) -> String {
    match v {
        V::Decimal(_) => "decimal.Decimal".into(),
        V::DateTime(_) => "datetime.datetime".into(),
        V::Date(_) => "datetime.date".into(),
        V::Time(_) => "datetime.time".into(),
        V::Delta(_) => "datetime.timedelta".into(),
        V::Tz(super::dt::Tz::Zone(_)) => "zoneinfo.ZoneInfo".into(),
        V::Tz(_) => "datetime.timezone".into(),
        V::Exc(e) => e.0.class.name.into(),
        V::Native(n) => match &**n {
            Native::Deque(_) => "collections.deque".into(),
            Native::File(_) => "_io.TextIOWrapper".into(),
            Native::BytesIO(_) => "_io.BytesIO".into(),
            _ => v.type_name().into(),
        },
        _ => v.type_name().into(),
    }
}

fn unsupported(v: &V) -> String {
    format!("Type is not JSON serializable: {}", tp_name(v))
}

fn two(out: &mut Vec<u8>, n: u32) {
    out.push(b'0' + (n / 10 % 10) as u8);
    out.push(b'0' + (n % 10) as u8);
}

fn write_date(out: &mut Vec<u8>, d: &chrono::NaiveDate) {
    use chrono::Datelike;
    out.extend_from_slice(format!("{:04}", d.year()).as_bytes());
    out.push(b'-');
    two(out, d.month());
    out.push(b'-');
    two(out, d.day());
}

fn write_time(out: &mut Vec<u8>, t: &chrono::NaiveTime, opts: i64) {
    use chrono::Timelike;
    two(out, t.hour());
    out.push(b':');
    two(out, t.minute());
    out.push(b':');
    two(out, t.second());
    if opts & OPT_OMIT_MICROSECONDS == 0 {
        let us = t.nanosecond() / 1000;
        if us != 0 {
            out.push(b'.');
            // orjson pads a time's microseconds to six digits below 10 000 only (".12345" for 12 345 us)
            let pad = match us {
                0..=9 => "00000",
                10..=99 => "0000",
                100..=999 => "000",
                1000..=9999 => "00",
                _ => "",
            };
            out.extend_from_slice(pad.as_bytes());
            out.extend_from_slice(us.to_string().as_bytes());
        }
    }
}

fn write_datetime(out: &mut Vec<u8>, d: &super::dt::DateTime, opts: i64) {
    use chrono::Timelike;
    write_date(out, &d.wall.date());
    out.push(b'T');
    let t = d.wall.time();
    two(out, t.hour());
    out.push(b':');
    two(out, t.minute());
    out.push(b':');
    two(out, t.second());
    if opts & OPT_OMIT_MICROSECONDS == 0 {
        let us = t.nanosecond() / 1000;
        if us != 0 {
            out.extend_from_slice(format!(".{us:06}").as_bytes());
        }
    }
    let off = match &d.tz {
        Some(tz) => Some(tz.offset_for_wall(&d.wall, d.fold)),
        None if opts & OPT_NAIVE_UTC != 0 => Some(0),
        None => None,
    };
    if let Some(off) = off {
        if off == 0 {
            out.extend_from_slice(if opts & OPT_UTC_Z != 0 { b"Z" } else { b"+00:00" });
        } else {
            out.push(if off < 0 { b'-' } else { b'+' });
            let a = off.unsigned_abs();
            let (h, mut m) = (a / 3600, (a / 60) % 60);
            if a - (m * 60 + h * 3600) >= 30 {
                m += 1;
            }
            two(out, h);
            out.push(b':');
            two(out, m);
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            8 => out.extend_from_slice(b"\\b"),
            9 => out.extend_from_slice(b"\\t"),
            10 => out.extend_from_slice(b"\\n"),
            12 => out.extend_from_slice(b"\\f"),
            13 => out.extend_from_slice(b"\\r"),
            0..=0x1f => out.extend_from_slice(format!("\\u{b:04x}").as_bytes()),
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

fn write_float(out: &mut Vec<u8>, f: f64) {
    if f.is_finite() {
        let mut b = zmij::Buffer::new();
        out.extend_from_slice(b.format_finite(f).as_bytes());
    } else {
        out.extend_from_slice(b"null");
    }
}

const STRICT_INT: i64 = 9007199254740991;

type BoxFut<'b> = futures_util::future::BoxFuture<'b, Result<(), SerErr>>;

impl Ser<'_> {
    fn open(&mut self, c: u8) {
        self.out.push(c);
        if self.opts & OPT_INDENT_2 != 0 {
            self.indent += 2;
            self.out.push(b'\n');
            self.out.extend(std::iter::repeat_n(b' ', self.indent));
        }
    }

    fn close(&mut self, c: u8) {
        if self.opts & OPT_INDENT_2 != 0 {
            self.indent -= 2;
            self.out.push(b'\n');
            self.out.extend(std::iter::repeat_n(b' ', self.indent));
        }
        self.out.push(c);
    }

    fn sep(&mut self) {
        self.out.push(b',');
        if self.opts & OPT_INDENT_2 != 0 {
            self.out.push(b'\n');
            self.out.extend(std::iter::repeat_n(b' ', self.indent));
        }
    }

    fn colon(&mut self) {
        self.out.extend_from_slice(if self.opts & OPT_INDENT_2 != 0 { b": " } else { b":" });
    }

    fn int(&mut self, i: i64) -> Result<(), SerErr> {
        if self.opts & OPT_STRICT_INTEGER != 0 && !(-STRICT_INT..=STRICT_INT).contains(&i) {
            return Err(SerErr::Msg("Integer exceeds 53-bit range"));
        }
        self.out.extend_from_slice(i.to_string().as_bytes());
        Ok(())
    }

    fn ser<'b>(&'b mut self, v: &'b V) -> BoxFut<'b> {
        Box::pin(async move { self.ser_inner(v).await })
    }

    async fn ser_inner<'b>(&'b mut self, v: &'b V) -> Result<(), SerErr> {
        let dt_pass = self.opts & OPT_PASSTHROUGH_DATETIME != 0;
        match v {
            V::Str(s) => write_str(&mut self.out, s),
            V::Bool(b) => self.out.extend_from_slice(if *b { b"true" } else { b"false" }),
            V::Int(i) => self.int(*i)?,
            V::None => self.out.extend_from_slice(b"null"),
            V::Float(f) => write_float(&mut self.out, *f),
            V::List(l) => {
                if self.recursion == 254 {
                    return Err(SerErr::Msg("Recursion limit reached"));
                }
                let items = l.lock().clone();
                self.array(&items).await?;
            }
            V::Tuple(t) => {
                let items: Vec<V> = (**t).clone();
                self.array(&items).await?;
            }
            V::Dict(d) => {
                if self.recursion == 254 {
                    return Err(SerErr::Msg("Recursion limit reached"));
                }
                let items: Vec<(V, V)> = d.lock().values().cloned().collect();
                self.map(items).await?;
            }
            V::DateTime(d) if !dt_pass => {
                self.out.push(b'"');
                write_datetime(&mut self.out, d, self.opts);
                self.out.push(b'"');
            }
            V::Date(d) if !dt_pass => {
                self.out.push(b'"');
                write_date(&mut self.out, d);
                self.out.push(b'"');
            }
            V::Time(t) if !dt_pass => {
                self.out.push(b'"');
                write_time(&mut self.out, t, self.opts);
                self.out.push(b'"');
            }
            V::Enum(e, i) => {
                let value = e.value(*i);
                self.ser(&value).await?;
            }
            V::Native(n) if matches!(&**n, Native::Uuid(_)) => {
                let Native::Uuid(u) = &**n else { unreachable!() };
                write_str(&mut self.out, &super::pathio::uuid_str(*u));
            }
            V::Inst(i) if i.desc.dataclass && self.opts & OPT_PASSTHROUGH_DATACLASS == 0 => {
                self.recursion += 1;
                if self.recursion == 255 {
                    return Err(SerErr::Msg("Recursion limit reached"));
                }
                let mut items: Vec<(V, V)> = i.desc.fields.iter().zip(i.vals.lock().clone())
                    .map(|(f, x)| (V::str(f.name), x)).collect();
                if i.desc.slots.is_empty() {
                    items.extend(i.extra.lock().iter().map(|(k, x)| (V::str(k), x.clone())));
                }
                let items: Vec<(V, V)> = items.into_iter()
                    .filter(|(k, _)| !matches!(k, V::Str(s) if s.starts_with('_'))).collect();
                if items.is_empty() {
                    self.out.extend_from_slice(b"{}");
                } else {
                    self.open(b'{');
                    for (n, (k, x)) in items.iter().enumerate() {
                        if n > 0 {
                            self.sep();
                        }
                        write_str(&mut self.out, k.as_str().unwrap_or(""));
                        self.colon();
                        self.ser(x).await?;
                    }
                    self.close(b'}');
                }
                self.recursion -= 1;
            }
            _ => self.unknown(v).await?,
        }
        Ok(())
    }

    async fn unknown<'b>(&'b mut self, v: &'b V) -> Result<(), SerErr> {
        let Some(f) = self.default else {
            return Err(SerErr::Unsupported(unsupported(v), None));
        };
        if self.default_calls == 255 {
            return Err(SerErr::Msg("default serializer exceeds recursion limit"));
        }
        let Some(cx) = self.cx else {
            return Err(SerErr::Exc(Exc::runtime("py2axum: orjson default= without a request context")));
        };
        self.default_calls += 1;
        let r = super::methods::call_value(cx, f, vec![v.clone()], Vec::new()).await;
        match r {
            Ok(d) => {
                self.ser(&d).await?;
                self.default_calls -= 1;
                Ok(())
            }
            Err(e) => Err(SerErr::Unsupported(unsupported(v), Some(e))),
        }
    }

    async fn array<'b>(&'b mut self, items: &'b [V]) -> Result<(), SerErr> {
        if items.is_empty() {
            self.out.extend_from_slice(b"[]");
            return Ok(());
        }
        self.recursion += 1;
        self.open(b'[');
        for (n, x) in items.iter().enumerate() {
            if n > 0 {
                self.sep();
            }
            self.ser(x).await?;
        }
        self.close(b']');
        self.recursion -= 1;
        Ok(())
    }

    /// `OPT_NON_STR_KEYS`: a key as orjson spells it
    fn key_str(&self, k: &V) -> Result<String, SerErr> {
        let opts = self.opts & !(OPT_PASSTHROUGH_DATETIME | OPT_PASSTHROUGH_SUBCLASS | OPT_PASSTHROUGH_DATACLASS);
        let mut out = Vec::new();
        match k {
            V::Str(s) => return Ok(s.to_string()),
            V::None => return Ok("null".into()),
            V::Bool(b) => return Ok(if *b { "true" } else { "false" }.into()),
            V::Int(i) => return Ok(i.to_string()),
            V::Float(f) => write_float(&mut out, *f),
            V::DateTime(d) => write_datetime(&mut out, d, opts),
            V::Date(d) => write_date(&mut out, d),
            V::Time(t) => write_time(&mut out, t, opts),
            V::Native(n) if matches!(&**n, Native::Uuid(_)) => {
                let Native::Uuid(u) = &**n else { unreachable!() };
                return Ok(super::pathio::uuid_str(*u));
            }
            V::Enum(e, i) => return self.key_str(&e.value(*i)),
            _ => return Err(SerErr::Msg("Dict key must a type serializable with OPT_NON_STR_KEYS")),
        }
        Ok(String::from_utf8(out).unwrap_or_default())
    }

    async fn map<'b>(&'b mut self, items: Vec<(V, V)>) -> Result<(), SerErr> {
        if items.is_empty() {
            self.out.extend_from_slice(b"{}");
            return Ok(());
        }
        self.recursion += 1;
        let r = if self.opts & (OPT_SORT_KEYS | OPT_NON_STR_KEYS) == 0 {
            // generic: keys checked one at a time, while writing
            self.open(b'{');
            for (n, (k, x)) in items.iter().enumerate() {
                if n > 0 {
                    self.sep();
                }
                let V::Str(ks) = k else {
                    return Err(SerErr::Msg("Dict key must be str"));
                };
                write_str(&mut self.out, ks);
                self.colon();
                self.ser(x).await?;
            }
            self.close(b'}');
            Ok(())
        } else {
            let mut keyed: Vec<(String, &V)> = Vec::with_capacity(items.len());
            for (k, x) in &items {
                let ks = if self.opts & OPT_NON_STR_KEYS != 0 {
                    self.key_str(k)?
                } else {
                    match k {
                        V::Str(s) => s.to_string(),
                        _ => return Err(SerErr::Msg("Dict key must be str")),
                    }
                };
                keyed.push((ks, x));
            }
            if self.opts & OPT_SORT_KEYS != 0 {
                keyed.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            }
            self.open(b'{');
            for (n, (k, x)) in keyed.iter().enumerate() {
                if n > 0 {
                    self.sep();
                }
                write_str(&mut self.out, k);
                self.colon();
                self.ser(x).await?;
            }
            self.close(b'}');
            Ok(())
        };
        self.recursion -= 1;
        r
    }
}
