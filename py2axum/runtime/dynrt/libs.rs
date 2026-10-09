//! The closed list of library correspondences (hashlib, secrets, statistics, json, zoneinfo,
//! datetime constructors). Anything not here is refused at transpile time.
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use sha2::Digest;

use super::dt::{self, DateTime, Tz};
use super::ops;
use super::pyd;
use super::v::*;

pub enum Hasher {
    Sha256(sha2::Sha256),
    Sha1(sha1::Sha1),
    Md5(md5::Md5),
}

fn bytes_of(v: &V) -> R<Vec<u8>> {
    match v {
        V::Bytes(b) => Ok(b.to_vec()),
        V::Str(_) => Err(Exc::type_error("Strings must be encoded before hashing")),
        other => Err(Exc::type_error(format!("object supporting the buffer API required, got {}", other.type_name()))),
    }
}

pub fn hash_new(algo: &str, data: Option<&V>) -> R {
    let mut h = match algo {
        "sha256" => Hasher::Sha256(sha2::Sha256::new()),
        "sha1" => Hasher::Sha1(sha1::Sha1::new()),
        "md5" => Hasher::Md5(md5::Md5::new()),
        _ => return Err(Exc::value_error(format!("unsupported hash type {algo}"))),
    };
    if let Some(d) = data {
        hash_update(&mut h, &bytes_of(d)?);
    }
    Ok(V::native(Native::Hash(parking_lot::Mutex::new(h))))
}

pub fn hash_update(h: &mut Hasher, data: &[u8]) {
    match h {
        Hasher::Sha256(x) => x.update(data),
        Hasher::Sha1(x) => x.update(data),
        Hasher::Md5(x) => x.update(data),
    }
}

pub fn hash_method(h: &parking_lot::Mutex<Hasher>, name: &str, args: &[V]) -> R {
    let mut h = h.lock();
    match name {
        "update" => {
            hash_update(&mut h, &bytes_of(args.first().unwrap_or(&V::None))?);
            Ok(V::None)
        }
        "hexdigest" | "digest" => {
            let out: Vec<u8> = match &*h {
                Hasher::Sha256(x) => x.clone().finalize().to_vec(),
                Hasher::Sha1(x) => x.clone().finalize().to_vec(),
                Hasher::Md5(x) => x.clone().finalize().to_vec(),
            };
            if name == "digest" {
                Ok(V::Bytes(std::sync::Arc::from(out)))
            } else {
                Ok(V::str(hex::encode(out)))
            }
        }
        _ => Err(Exc::attr_error(format!("'HASH' object has no attribute '{name}'"))),
    }
}

pub fn token_urlsafe(n: &V) -> R {
    use base64::Engine;
    use rand::RngCore;
    let n = match n {
        V::Int(i) if *i < 0 => return Err(Exc::value_error("negative argument not allowed")),
        V::Int(i) => *i as usize,
        V::None => 32,
        _ => return Err(Exc::type_error("token_urlsafe() needs an int")),
    };
    let mut buf = Vec::new();
    buf.try_reserve_exact(n).map_err(|_| Exc::msg(&super::v::MEMORY_ERROR, ""))?;
    buf.resize(n, 0u8);
    rand::thread_rng().fill_bytes(&mut buf);
    Ok(V::str(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)))
}

pub fn token_hex(n: &V) -> R {
    use rand::RngCore;
    let n = match n {
        V::Int(i) if *i < 0 => return Err(Exc::value_error("negative argument not allowed")),
        V::Int(i) => *i as usize,
        V::None => 32,
        _ => return Err(Exc::type_error("token_hex() needs an int")),
    };
    let mut buf = Vec::new();
    buf.try_reserve_exact(n).map_err(|_| Exc::msg(&super::v::MEMORY_ERROR, ""))?;
    buf.resize(n, 0u8);
    rand::thread_rng().fill_bytes(&mut buf);
    Ok(V::str(hex::encode(buf)))
}

pub fn compare_digest(a: &V, b: &V) -> R {
    use subtle::ConstantTimeEq;
    let (x, y): (Vec<u8>, Vec<u8>) = match (a, b) {
        (V::Str(x), V::Str(y)) => {
            if !x.is_ascii() || !y.is_ascii() {
                return Err(Exc::type_error("comparing strings with non-ASCII characters is not supported"));
            }
            (x.as_bytes().to_vec(), y.as_bytes().to_vec())
        }
        (V::Bytes(x), V::Bytes(y)) => (x.to_vec(), y.to_vec()),
        _ => return Err(Exc::type_error("unsupported operand types for compare_digest")),
    };
    Ok(V::Bool(x.len() == y.len() && bool::from(x.ct_eq(&y))))
}

pub fn median(data: &V) -> R {
    let mut items = ops::iter(data)?;
    if items.is_empty() {
        return Err(Exc::msg(&VALUE_ERROR, "no median for empty data"));
    }
    let mut err = None;
    items.sort_by(|a, b| match ops::cmp(a, b) {
        Ok(o) => o,
        Err(e) => {
            err = Some(e);
            std::cmp::Ordering::Equal
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    let n = items.len();
    if n % 2 == 1 {
        Ok(items[n / 2].clone())
    } else {
        ops::truediv(&ops::add(&items[n / 2 - 1], &items[n / 2])?, &V::Int(2))
    }
}

pub fn json_dumps(obj: &V, kwargs: &[(String, V)]) -> R {
    let mut default_str = false;
    let mut sort_keys = false;
    let mut indent: Option<String> = None;
    let seps_given = kwargs.iter().any(|(k, _)| k == "separators");
    let mut style = pyd::JsonStyle { ensure_ascii: true, item_sep: ", ", key_sep: ": ", nan_null: false };
    for (k, v) in kwargs {
        match k.as_str() {
            "default" => {
                // only default=str is translated (any other function would be silently replaced by str)
                let is_str = matches!(v, V::Native(n) if matches!(&**n, super::v::Native::Type("str")));
                if !is_str {
                    return Err(Exc::type_error("py2axum: json.dumps(default=...) supports default=str only"));
                }
                default_str = true
            }
            "ensure_ascii" => style.ensure_ascii = ops::truthy(v)?,
            "sort_keys" => sort_keys = ops::truthy(v)?,
            "indent" => {
                indent = match v {
                    V::None => None,
                    V::Int(n) => Some(" ".repeat((*n).max(0) as usize)),
                    V::Str(s) => Some(s.to_string()),
                    other => return Err(Exc::type_error(format!("py2axum: json.dumps(indent=) of type {}", other.type_name()))),
                };
                if indent.is_some() && !seps_given {
                    style.item_sep = ",";
                }
            }
            "separators" => {
                let parts = ops::iter(v)?;
                style.item_sep = super::types::intern(&ops::str_(&parts[0])?);
                style.key_sep = super::types::intern(&ops::str_(&parts[1])?);
            }
            _ => return Err(Exc::type_error(format!("json.dumps({k}=) is not supported"))),
        }
    }
    let sorted;
    let obj = if sort_keys {
        sorted = sorted_keys(obj)?;
        &sorted
    } else {
        obj
    };
    if let Some(ind) = indent {
        let mut out = String::new();
        write_indented(&mut out, obj, &style, default_str, &ind, 0)?;
        return Ok(V::str(out));
    }
    Ok(V::str(pyd::to_json(obj, &style, default_str)?))
}

/// `json.dumps(indent=)`: CPython's `_iterencode` layout (newline + indent per level, `[]`/`{}` when empty)
fn write_indented(out: &mut String, v: &V, st: &pyd::JsonStyle, default_str: bool, ind: &str, level: usize) -> R<()> {
    super::stack_guard()?;
    let nl = |out: &mut String, lvl: usize| {
        out.push('\n');
        out.push_str(&ind.repeat(lvl));
    };
    let sep = st.item_sep;
    match v {
        V::List(_) | V::Tuple(_) => {
            let items = ops::iter(v)?;
            if items.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                nl(out, level + 1);
                write_indented(out, x, st, default_str, ind, level + 1)?;
            }
            nl(out, level);
            out.push(']');
        }
        V::Dict(d) => {
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            if items.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push('{');
            for (i, (k, x)) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                nl(out, level + 1);
                out.push_str(&pyd::to_json(&V::str(pyd::dumps_key(k)?), st, false)?);
                out.push_str(st.key_sep);
                write_indented(out, x, st, default_str, ind, level + 1)?;
            }
            nl(out, level);
            out.push('}');
        }
        other => out.push_str(&pyd::to_json(other, st, default_str)?),
    }
    Ok(())
}

/// `sort_keys=True`: every dict's items in `sorted(d.items())` order (keys of mixed types raise like CPython)
fn sorted_keys(v: &V) -> R {
    Ok(match v {
        V::Dict(d) => {
            let mut items: Vec<(V, V)> = d.lock().values().cloned().collect();
            let mut err = None;
            items.sort_by(|a, b| {
                ops::cmp(&a.0, &b.0).unwrap_or_else(|e| {
                    err.get_or_insert(e);
                    std::cmp::Ordering::Equal
                })
            });
            if let Some(e) = err {
                return Err(e);
            }
            V::dict_from(items.into_iter().map(|(k, x)| Ok((k, sorted_keys(&x)?))).collect::<R<Vec<_>>>()?)?
        }
        V::List(l) => V::list(l.lock().iter().map(sorted_keys).collect::<R<Vec<_>>>()?),
        V::Tuple(t) => V::Tuple(std::sync::Arc::new(t.iter().map(sorted_keys).collect::<R<Vec<_>>>()?)),
        other => other.clone(),
    })
}

// ---------------------------------------------------------------- datetime

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn as_i(v: &V) -> R<i64> {
    match v {
        V::Int(i) => Ok(*i),
        V::Bool(b) => Ok(*b as i64),
        other => Err(Exc::type_error(format!("an integer is required (got type {})", other.type_name()))),
    }
}

fn as_f(v: &V) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Float(f) => Ok(*f),
        V::Bool(b) => Ok(*b as i64 as f64),
        other => Err(Exc::type_error(format!("unsupported type for timedelta: {}", other.type_name()))),
    }
}

pub fn tz_of(v: &V) -> R<Option<Tz>> {
    match v {
        V::None => Ok(None),
        V::Tz(t) => Ok(Some(*t)),
        other => Err(Exc::type_error(format!("tzinfo argument must be None or of a tzinfo subclass, not {}", other.type_name()))),
    }
}

/// `timedelta(days, seconds, microseconds, milliseconds, minutes, hours, weeks)`
pub fn timedelta(args: &[V], kwargs: &[(String, V)]) -> R {
    let names = ["days", "seconds", "microseconds", "milliseconds", "minutes", "hours", "weeks"];
    let mult = [86_400_000_000f64, 1_000_000.0, 1.0, 1_000.0, 60_000_000.0, 3_600_000_000.0, 604_800_000_000.0];
    let mut us = 0f64;
    for (i, a) in args.iter().enumerate() {
        us += as_f(a)? * mult[i];
    }
    for (k, v) in kwargs {
        let i = names.iter().position(|n| n == k).ok_or_else(|| Exc::type_error(format!("'{k}' is an invalid keyword argument for __new__()")))?;
        us += as_f(v)? * mult[i];
    }
    Ok(V::Delta(Duration::microseconds(us.round() as i64)))
}

/// `datetime(year, month, day, hour=0, minute=0, second=0, microsecond=0, tzinfo=None)`
pub fn datetime_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let names = ["year", "month", "day", "hour", "minute", "second", "microsecond", "tzinfo"];
    let mut vals: Vec<Option<V>> = vec![None; 8];
    for (i, a) in args.iter().enumerate() {
        vals[i] = Some(a.clone());
    }
    for (k, v) in kwargs {
        let i = names.iter().position(|n| n == k).ok_or_else(|| Exc::type_error(format!("'{k}' is an invalid keyword argument")))?;
        vals[i] = Some(v.clone());
    }
    let g = |i: usize, d: i64| -> R<i64> { vals[i].as_ref().map(as_i).unwrap_or(Ok(d)) };
    let date = super::methods::ymd(g(0, 1)?, g(1, 1)?, g(2, 1)?)?;
    let new = super::python() >= (3, 14);
    for (i, name, max) in [(3, "hour", 23), (4, "minute", 59), (5, "second", 59), (6, "microsecond", 999_999)] {
        let x = g(i, 0)?;
        if !(0..=max).contains(&x) {
            return Err(Exc::value_error(if new { format!("{name} must be in 0..{max}, not {x}") } else { format!("{name} must be in 0..{max}") }));
        }
    }
    let time = NaiveTime::from_hms_micro_opt(g(3, 0)? as u32, g(4, 0)? as u32, g(5, 0)? as u32, g(6, 0)? as u32).ok_or_else(|| Exc::value_error("time out of range"))?;
    let tz = match &vals[7] {
        Some(v) => tz_of(v)?,
        None => None,
    };
    Ok(V::DateTime(DateTime { wall: NaiveDateTime::new(date, time), tz, fold: 0 }))
}

pub fn date_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let mut v = [1i64; 3];
    for (i, a) in args.iter().enumerate().take(3) {
        v[i] = as_i(a)?;
    }
    for (k, x) in kwargs {
        let i = ["year", "month", "day"].iter().position(|n| n == k).ok_or_else(|| Exc::type_error(format!("'{k}' is an invalid keyword argument")))?;
        v[i] = as_i(x)?;
    }
    super::methods::ymd(v[0], v[1], v[2]).map(V::Date)
}

/// `datetime.time(hour=0, minute=0, second=0, microsecond=0)` (naive; tzinfo/fold refused)
pub fn time_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let names = ["hour", "minute", "second", "microsecond"];
    let mut v = [0i64; 4];
    if args.len() > 4 {
        return Err(Exc::type_error("py2axum: time() with tzinfo is not supported"));
    }
    for (i, a) in args.iter().enumerate() {
        v[i] = as_i(a)?;
    }
    for (k, x) in kwargs {
        match names.iter().position(|n| n == k) {
            Some(i) => v[i] = as_i(x)?,
            None if k == "tzinfo" && x.is_none() => {}
            None if k == "fold" && matches!(x, V::Int(0)) => {}
            None => return Err(Exc::type_error(format!("py2axum: time({k}=) is not supported"))),
        }
    }
    let checks = [(0, 23, "hour"), (0, 59, "minute"), (0, 59, "second"), (0, 999_999, "microsecond")];
    for (i, (lo, hi, n)) in checks.iter().enumerate() {
        if v[i] < *lo || v[i] > *hi {
            return Err(Exc::value_error(format!("{n} must be in {lo}..{hi}")));
        }
    }
    Ok(V::Time(chrono::NaiveTime::from_hms_micro_opt(v[0] as u32, v[1] as u32, v[2] as u32, v[3] as u32).unwrap()))
}

/// `datetime.combine(date, time, tzinfo=time.tzinfo)`
pub fn combine(args: &[V], kwargs: &[(String, V)]) -> R {
    let get = |i: usize, n: &str| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v));
    let d = match get(0, "date") {
        Some(V::Date(d)) => *d,
        Some(V::DateTime(x)) => x.wall.date(),
        _ => return Err(Exc::type_error("combine() argument 1 must be datetime.date")),
    };
    let t = match get(1, "time") {
        Some(V::Time(t)) => *t,
        _ => return Err(Exc::type_error("combine() argument 2 must be datetime.time")),
    };
    let tz = match get(2, "tzinfo") {
        None | Some(V::None) => None,
        Some(z) => tz_of(z)?,
    };
    Ok(V::DateTime(dt::DateTime { wall: d.and_time(t), tz, fold: 0 }))
}

/// `str.maketrans(x[, y[, z]])`
pub fn maketrans(args: &[V]) -> R {
    let mut items: Vec<(V, V)> = Vec::new();
    match args {
        [V::Dict(d)] => {
            for (_, (k, v)) in d.lock().iter() {
                let k = match k {
                    V::Str(s) if s.chars().count() == 1 => V::Int(s.chars().next().unwrap() as i64),
                    V::Int(i) => V::Int(*i),
                    _ => return Err(Exc::value_error("string keys in translate table must be of length 1")),
                };
                items.push((k, v.clone()));
            }
        }
        [V::Str(x), V::Str(y), rest @ ..] => {
            if x.chars().count() != y.chars().count() {
                return Err(Exc::value_error("the first two maketrans arguments must have equal length"));
            }
            for (a, b) in x.chars().zip(y.chars()) {
                items.push((V::Int(a as i64), V::Int(b as i64)));
            }
            if let [V::Str(z)] = rest {
                for c in z.chars() {
                    items.push((V::Int(c as i64), V::None));
                }
            }
        }
        _ => return Err(Exc::type_error("py2axum: str.maketrans() arguments not supported")),
    }
    V::dict_from(items)
}

pub fn timezone_new(offset: &V) -> R {
    match offset {
        V::Delta(d) => {
            let s = dt::micros(d) / 1_000_000;
            Ok(V::Tz(if s == 0 { Tz::Utc } else { Tz::Fixed(s as i32) }))
        }
        _ => Err(Exc::type_error("timezone() argument 1 must be datetime.timedelta")),
    }
}

pub fn zoneinfo(name: &V) -> R {
    let n = ops::str_(name)?;
    Tz::zone(&n).map(V::Tz).ok_or_else(|| Exc::msg(&KEY_ERROR, format!("No time zone found with key {n}")))
}

/// A string shaped like an ISO date (`YYYY-MM-DD`, then `T`/space and `HH:MM[:SS]`) whose values are out of
/// range: CPython's `fromisoformat` names the field (`month must be in 1..12, not 13`), else None
fn iso_range_error(t: &str) -> Option<Exc> {
    let b = t.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = t.get(r)?;
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok()).flatten()
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    if let Err(e) = super::methods::ymd(y, m, d) {
        return Some(e);
    }
    if b.len() >= 16 && matches!(b[10], b'T' | b' ') && b[13] == b':' {
        let (h, mi) = (digits(11..13)?, digits(14..16)?);
        let sec = if b.len() >= 19 && b[16] == b':' { digits(17..19) } else { Some(0) };
        for (v, hi, what) in [(h, 23, "hour"), (mi, 59, "minute"), (sec?, 59, "second")] {
            if let Err(e) = super::methods::time_field(v, hi, what) {
                return Some(e);
            }
        }
    }
    None
}

pub fn fromisoformat_dt(s: &V) -> R {
    let t = ops::str_(s)?;
    match dt::parse_datetime(&t) {
        Ok(d) => Ok(V::DateTime(DateTime { tz: d.tz.map(|z| if let Tz::Fixed(0) = z { Tz::Utc } else { z }), ..d })),
        Err(_) => match dt::parse_date(&t) {
            Ok(d) => Ok(V::DateTime(DateTime::naive(d.and_hms_opt(0, 0, 0).unwrap()))),
            Err(_) => Err(iso_range_error(&t).unwrap_or_else(|| Exc::value_error(format!("Invalid isoformat string: {}", ops::str_repr(&t))))),
        },
    }
}

pub fn fromisoformat_date(s: &V) -> R {
    let t = ops::str_(s)?;
    dt::parse_date(&t).map(V::Date).map_err(|_| iso_range_error(&t).unwrap_or_else(|| Exc::value_error(format!("Invalid isoformat string: {}", ops::str_repr(&t)))))
}

/// `strftime` with the common directives.
pub fn strftime(wall: &NaiveDateTime, offset: Option<i32>, tzname: Option<String>, fmt: &str) -> String {
    let mut out = String::new();
    let mut it = fmt.chars().peekable();
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('Y') => out += &format!("{:04}", wall.year()),
            Some('y') => out += &format!("{:02}", wall.year() % 100),
            Some('m') => out += &format!("{:02}", wall.month()),
            Some('d') => out += &format!("{:02}", wall.day()),
            Some('H') => out += &format!("{:02}", wall.hour()),
            Some('I') => out += &format!("{:02}", if wall.hour() % 12 == 0 { 12 } else { wall.hour() % 12 }),
            Some('p') => out += if wall.hour() < 12 { "AM" } else { "PM" },
            Some('M') => out += &format!("{:02}", wall.minute()),
            Some('S') => out += &format!("{:02}", wall.second()),
            Some('f') => out += &format!("{:06}", wall.nanosecond() / 1000),
            Some('j') => out += &format!("{:03}", wall.ordinal()),
            Some('A') => out += DAYS[wall.weekday().num_days_from_monday() as usize],
            Some('a') => out += &DAYS[wall.weekday().num_days_from_monday() as usize][..3],
            Some('B') => out += MONTHS[wall.month0() as usize],
            Some('b') => out += &MONTHS[wall.month0() as usize][..3],
            Some('z') => {
                if let Some(o) = offset {
                    out += &dt::fmt_offset(o, false).replace(':', "");
                }
            }
            Some('Z') => out += &tzname.clone().unwrap_or_default(),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

// ---------------------------------------------------------------- os

/// `os.getenv(key, default=None)` / `os.environ.get(key, default=None)`
/// `json.loads(s)`: str, or bytes decoded as `json.detect_encoding` does (UTF-8 with or without BOM,
/// UTF-16/32 by BOM or by the position of the zero bytes).
pub fn json_loads(s: &V) -> R {
    match s {
        V::Str(t) => pyd::loads(t),
        V::Bytes(b) => pyd::loads(&json_text(b)?),
        o => Err(Exc::type_error(format!("the JSON object must be str, bytes or bytearray, not {}", o.type_name()))),
    }
}

/// The text `json.loads(b)` decodes from bytes (`json.detect_encoding`), or its UnicodeDecodeError.
pub fn json_text(b: &[u8]) -> R<String> {
    let (enc, skip): (&str, usize) = if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        ("utf-8", 3)
    } else if b.starts_with(&[0xFF, 0xFE, 0, 0]) {
        ("utf-32-le", 4)
    } else if b.starts_with(&[0, 0, 0xFE, 0xFF]) {
        ("utf-32-be", 4)
    } else if b.starts_with(&[0xFF, 0xFE]) {
        ("utf-16-le", 2)
    } else if b.starts_with(&[0xFE, 0xFF]) {
        ("utf-16-be", 2)
    } else if b.len() >= 4 && b[0] == 0 && b[1] == 0 {
        ("utf-32-be", 0)
    } else if b.len() >= 2 && b[0] == 0 {
        ("utf-16-be", 0)
    } else if b.len() >= 4 && b[1] == 0 && b[2] == 0 && b[3] == 0 {
        ("utf-32-le", 0)
    } else if b.len() >= 2 && b[1] == 0 {
        ("utf-16-le", 0)
    } else {
        ("utf-8", 0)
    };
    let d = &b[skip..];
    let bad = || Exc::msg(&UNICODE_DECODE_ERROR, format!("'{}' codec can't decode bytes", enc.trim_end_matches("-le").trim_end_matches("-be")));
    let text: String = match enc {
        // CPython's message (byte, position, reason); positions count from after the BOM, as the codec sees them
        "utf-8" => ops::str_(&super::methods::utf8_decode(d, 0)?)?,
        "utf-16-le" | "utf-16-be" => {
            if d.len() % 2 != 0 {
                return Err(bad());
            }
            let units: Vec<u16> =
                d.chunks(2).map(|c| if enc == "utf-16-le" { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
            String::from_utf16(&units).map_err(|_| bad())?
        }
        _ => {
            if d.len() % 4 != 0 {
                return Err(bad());
            }
            d.chunks(4)
                .map(|c| {
                    let a = [c[0], c[1], c[2], c[3]];
                    char::from_u32(if enc == "utf-32-le" { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) })
                })
                .collect::<Option<String>>()
                .ok_or_else(bad)?
        }
    };
    Ok(text)
}

pub fn getenv(key: &V, default: Option<&V>) -> R {
    let k = match key {
        V::Str(s) => s,
        other => return Err(Exc::type_error(format!("str expected, not {}", other.type_name()))),
    };
    Ok(match std::env::var_os(&**k) {
        Some(v) => V::str(v.to_string_lossy()),
        None => default.cloned().unwrap_or(V::None),
    })
}

/// `os.environ` read as a value (`os.environ[k]`, `k in os.environ`): a snapshot dict, in the
/// process environment's order; writes are refused at compile time.
pub fn environ() -> V {
    let items: Vec<(V, V)> = std::env::vars_os().map(|(k, v)| (V::str(k.to_string_lossy()), V::str(v.to_string_lossy()))).collect();
    V::dict_from(items).expect("str keys")
}


// ---------------------------------------------------------------- calendar, types

/// `calendar.monthrange(year, month)`: (weekday of the 1st, Monday = 0; number of days)
pub fn monthrange(year: &V, month: &V) -> R {
    let (y, m) = (as_i(year)?, as_i(month)?);
    if !(1..=12).contains(&m) {
        return Err(Exc::msg(&VALUE_ERROR, format!("bad month number {m}; must be 1-12")));
    }
    let first = NaiveDate::from_ymd_opt(y as i32, m as u32, 1).ok_or_else(|| Exc::value_error(format!("year {y} is out of range")))?;
    let next = if m == 12 { NaiveDate::from_ymd_opt(y as i32 + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y as i32, m as u32 + 1, 1) }
        .ok_or_else(|| Exc::value_error(format!("year {y} is out of range")))?;
    Ok(V::tuple(vec![V::Int(first.weekday().num_days_from_monday() as i64), V::Int((next - first).num_days())]))
}

static NAMESPACE_CLASS: Class = Class { name: "SimpleNamespace", qualname: "types.SimpleNamespace", bases: &[], kind: ClassKind::Schema(&NAMESPACE) };
/// `types.SimpleNamespace`: an open instance (attributes only)
pub static NAMESPACE: pyd::SchemaDesc = pyd::SchemaDesc {
    name: "SimpleNamespace",
    class: &NAMESPACE_CLASS,
    fields: &[],
    from_attributes: false,
    extra: pyd::Extra::Allow,
    validators: &[],
    validate_assignment: false,
    populate_by_name: false,
    methods: &[],
    open: true,
    model_after: &[],
    before: &[],
    model_before: &[],
    has_before: false,
    frozen: false,
    post_init: None,
    hash: pyd::HashKind::Unhashable,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    computed: &[],
    json_schema: None,
};

/// `types.SimpleNamespace(**kwargs)`
pub fn namespace(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: SimpleNamespace(mapping) is not supported, pass keyword arguments"));
    }
    let o = pyd::object_new(&NAMESPACE);
    for (k, v) in kwargs {
        super::methods::setattr(&o, k, v.clone())?;
    }
    Ok(o)
}

/// `dict.fromkeys(iterable, value=None)`
pub fn dict_fromkeys(args: &[V]) -> R {
    let keys = ops::iter(args.first().ok_or_else(|| Exc::type_error("fromkeys expected at least 1 argument, got 0"))?)?;
    let value = args.get(1).cloned().unwrap_or(V::None);
    V::dict_from(keys.into_iter().map(|k| (k, value.clone())).collect())
}


// ---------------------------------------------------------------- datetime.strptime / fromtimestamp, secrets.choice

/// `datetime.strptime(s, fmt)`: chrono parsing with Python's defaults (1900-01-01 00:00) and errors
pub fn strptime(s: &V, fmt: &V) -> R {
    let (s, f) = (ops::str_(s)?, ops::str_(fmt)?);
    if f.contains("%f") || f.contains("%Z") || f.contains("%U") || f.contains("%W") || f.contains("%c") || f.contains("%x") || f.contains("%X") {
        return Err(Exc::value_error(format!("py2axum: strptime format {f:?} is not supported")));
    }
    let mut p = chrono::format::Parsed::new();
    let bad = || Exc::value_error(format!("time data {} does not match format {}", ops::str_repr(&s), ops::str_repr(&f)));
    match chrono::format::parse(&mut p, &s, chrono::format::StrftimeItems::new(&f)) {
        Ok(()) => {}
        Err(e) if e.kind() == chrono::format::ParseErrorKind::TooLong => {
            // the longest prefix the format accepts: the rest is reported
            let cut = (1..s.len())
                .rev()
                .filter(|k| s.is_char_boundary(*k))
                .find(|k| chrono::format::parse(&mut chrono::format::Parsed::new(), &s[..*k], chrono::format::StrftimeItems::new(&f)).is_ok())
                .unwrap_or(0);
            return Err(Exc::value_error(format!("unconverted data remains: {}", &s[cut..])));
        }
        Err(_) => return Err(bad()),
    }
    if p.year.is_none() && p.year_div_100.is_none() {
        let _ = p.set_year(1900);
    }
    if p.month.is_none() && p.ordinal.is_none() {
        let _ = p.set_month(1);
    }
    if p.day.is_none() && p.ordinal.is_none() {
        let _ = p.set_day(1);
    }
    if p.hour_div_12.is_none() {
        let _ = p.set_hour(0);
    }
    if p.minute.is_none() {
        let _ = p.set_minute(0);
    }
    let date = p.to_naive_date().map_err(|_| bad())?;
    let time = p.to_naive_time().unwrap_or(NaiveTime::MIN);
    let wall = date.and_time(time);
    Ok(match p.offset {
        Some(off) => V::DateTime(DateTime::aware(wall, if off == 0 { Tz::Fixed(0) } else { Tz::Fixed(off) })),
        None => V::DateTime(DateTime::naive(wall)),
    })
}

/// `datetime.fromtimestamp(ts, tz=None)`: naive local time without tz (the process's TZ, like CPython)
pub fn fromtimestamp(ts: &V, tz: Option<&V>) -> R {
    let t = match ts {
        V::Int(i) => *i as f64,
        V::Float(f) => *f,
        o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    };
    let whole = t.floor();
    let us = ((t - whole) * 1e6).round() as i64;
    let utc = chrono::DateTime::from_timestamp(whole as i64, 0).ok_or_else(|| Exc::value_error("year is out of range"))?.naive_utc() + Duration::microseconds(us);
    match tz {
        None | Some(V::None) => {
            let local = chrono::TimeZone::from_utc_datetime(&chrono::Local, &utc).naive_local();
            Ok(V::DateTime(DateTime::naive(local)))
        }
        Some(V::Tz(z)) => Ok(V::DateTime(DateTime::from_utc(utc, *z))),
        Some(o) => Err(Exc::type_error(format!("tzinfo argument must be None or of a tzinfo subclass, not type '{}'", o.type_name()))),
    }
}

/// `secrets.choice(seq)`
/// `random.random/uniform/randint/sample/shuffle` (an OS-seeded generator: CPython's Mersenne Twister is
/// seeded from os.urandom too, so no sequence is reproducible on either side)
pub fn random(name: &str, args: &[V]) -> R {
    use rand::Rng;
    use rand::seq::SliceRandom;
    let mut rng = rand::thread_rng();
    let num = |v: &V| -> R<f64> {
        match v {
            V::Int(i) => Ok(*i as f64),
            V::Float(f) => Ok(*f),
            other => Err(Exc::type_error(format!("must be real number, not {}", other.type_name()))),
        }
    };
    Ok(match (name, args) {
        ("random", []) => V::Float(rng.gen::<f64>()),
        ("uniform", [a, b]) => {
            let (a, b) = (num(a)?, num(b)?);
            V::Float(a + (b - a) * rng.gen::<f64>())
        }
        ("randint", [V::Int(a), V::Int(b)]) => {
            if a > b {
                return Err(Exc::value_error(format!("empty range in randrange({a}, {})", b + 1)));
            }
            V::Int(rng.gen_range(*a..=*b))
        }
        ("sample", [seq, V::Int(k)]) => {
            let items = ops::iter(seq)?;
            if *k < 0 || *k as usize > items.len() {
                return Err(Exc::value_error("Sample larger than population or is negative"));
            }
            V::list(items.choose_multiple(&mut rng, *k as usize).cloned().collect())
        }
        ("shuffle", [V::List(l)]) => {
            l.lock().shuffle(&mut rng);
            V::None
        }
        ("choice", [seq]) => return choice(seq),
        _ => return Err(Exc::type_error(format!("py2axum: random.{name}() with these arguments is not supported"))),
    })
}

pub fn choice(seq: &V) -> R {
    use rand::Rng;
    let items = ops::iter(seq)?;
    if items.is_empty() {
        return Err(Exc::msg(&INDEX_ERROR, "Cannot choose from an empty sequence"));
    }
    let i = rand::rngs::OsRng.gen_range(0..items.len());
    Ok(items[i].clone())
}


/// `OAuth2PasswordRequestForm` once its form fields are validated: the attributes, `scopes` split
pub fn oauth2_form(fields: Vec<(String, V)>) -> R {
    let mut kw = Vec::new();
    for (k, v) in fields {
        if k == "scope" {
            let scopes = match &v {
                V::Str(s) => V::list(s.split_whitespace().map(V::str).collect()),
                _ => V::list(vec![]),
            };
            kw.push(("scopes".to_string(), scopes));
        } else {
            kw.push((k, v));
        }
    }
    namespace(&[], &kw)
}
