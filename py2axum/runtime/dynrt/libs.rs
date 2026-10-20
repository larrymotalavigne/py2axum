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

/// `os.urandom(-1)` (behind `secrets.token_*`): reworded in Python 3.15
fn negative_size() -> &'static str {
    if super::python() >= (3, 15) { "size cannot be negative" } else { "negative argument not allowed" }
}

/// `secrets.token_bytes(nbytes)`'s bytes, as `os.urandom` takes its size: None is DEFAULT_ENTROPY (32), a bool
/// is an int, anything else not an int a TypeError
fn random_bytes(n: &V) -> R<Vec<u8>> {
    use rand::RngCore;
    let n = match n {
        V::None => 32,
        V::Bool(b) => *b as usize,
        V::Int(i) if *i < 0 => return Err(Exc::value_error(negative_size())),
        V::Int(i) => *i as usize,
        o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    };
    let mut buf = Vec::new();
    buf.try_reserve_exact(n).map_err(|_| Exc::msg(&super::v::MEMORY_ERROR, ""))?;
    buf.resize(n, 0u8);
    rand::rngs::OsRng.fill_bytes(&mut buf);
    Ok(buf)
}

/// `secrets.token_bytes(nbytes=None)`
pub fn token_bytes(n: &V) -> R {
    Ok(V::Bytes(std::sync::Arc::from(random_bytes(n)?)))
}

/// `secrets.token_urlsafe(nbytes=None)`
pub fn token_urlsafe(n: &V) -> R {
    use base64::Engine;
    Ok(V::str(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(n)?)))
}

/// `secrets.token_hex(nbytes=None)`
pub fn token_hex(n: &V) -> R {
    Ok(V::str(hex::encode(random_bytes(n)?)))
}

/// `secrets.randbelow(exclusive_upper_bound)`: `n <= 0` tested first (its TypeError for a non-number), then
/// `n.bit_length()` (an AttributeError for a float or a Decimal)
pub fn randbelow(n: &V) -> R {
    use rand::Rng;
    if ops::truthy(&ops::le(n, &V::Int(0))?)? {
        return Err(Exc::value_error("Upper bound must be positive."));
    }
    match n {
        V::Bool(_) => Ok(V::Int(0)),
        V::Int(i) => Ok(V::Int(rand::rngs::OsRng.gen_range(0..*i))),
        o => Err(Exc::attr_error(format!("'{}' object has no attribute 'bit_length'", o.type_name()))),
    }
}

/// `secrets.randbits(k)` (`SystemRandom.getrandbits`): `k < 0` tested first, then the byte count
/// `(k + 7) // 8` given to `os.urandom`; beyond 63 bits the integer does not fit the runtime's
pub fn randbits(k: &V) -> R {
    use rand::Rng;
    if ops::truthy(&ops::lt(k, &V::Int(0))?)? {
        return Err(Exc::value_error("number of bits must be non-negative"));
    }
    let k = match k {
        V::Bool(b) => *b as i64,
        V::Int(i) => *i,
        o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    };
    if k > 63 {
        return Err(Exc::msg(&OVERFLOW_ERROR, format!("py2axum: secrets.randbits({k}) is beyond 64 bits (integers are 64-bit)")));
    }
    Ok(V::Int(if k == 0 { 0 } else { (rand::rngs::OsRng.gen::<u64>() >> (64 - k)) as i64 }))
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
    let mut fold = 0u8;
    for (k, v) in kwargs {
        if k == "fold" {
            fold = super::methods::fold_arg(v)?;
            continue;
        }
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
    Ok(V::DateTime(DateTime { wall: NaiveDateTime::new(date, time), tz, fold }))
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

/// `strftime` as CPython renders it: %f, %z, %:z and %Z by CPython itself, every other directive by the C library
/// in the C locale (CPython never sets LC_TIME). Measured on glibc (Debian trixie 3.13.5, 3.12.15–3.15.0) and macOS
/// (3.12.13–3.15.0): the outputs below are identical everywhere except where noted. Anything else (flags, widths, E/O
/// modifiers, %s that reads the local zone, letters the C library does not know) is refused at generation for a literal
/// format (`dyn.STRFTIME_DIRECTIVES`) and raises here for a computed one: glibc and macOS render them differently.
pub fn strftime(wall: &NaiveDateTime, offset: Option<i32>, tzname: Option<String>, fmt: &str) -> R<String> {
    strftime_with(wall, offset, tzname, fmt, true)
}

/// logging's `formatTime` (`time.strftime`): an unsupported directive is copied as is (glibc's rendering of a letter it
/// does not know), as before; its formats are not checked at generation
pub fn strftime_lenient(wall: &NaiveDateTime, offset: Option<i32>, tzname: Option<String>, fmt: &str) -> String {
    strftime_with(wall, offset, tzname, fmt, false).unwrap_or_default()
}

fn strftime_with(wall: &NaiveDateTime, offset: Option<i32>, tzname: Option<String>, fmt: &str, strict: bool) -> R<String> {
    let mut out = String::new();
    let mut it = fmt.chars().peekable();
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    // glibc writes a year below 1000 (%Y, %G, %C, %F) without padding (`1`), macOS pads it (`0001`); from 3.14
    // CPython pads them itself on every platform, but not the year inside %c
    let linux = cfg!(target_os = "linux");
    let pad = !linux || super::python() >= (3, 14);
    let year = |y: i32, padded: bool| if padded { format!("{y:04}") } else { y.to_string() };
    let wday = wall.weekday().num_days_from_monday() as usize;
    let h12 = if wall.hour() % 12 == 0 { 12 } else { wall.hour() % 12 };
    let ampm = if wall.hour() < 12 { "AM" } else { "PM" };
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('Y') => out += &year(wall.year(), pad),
            Some('G') => out += &year(wall.iso_week().year(), pad),
            Some('C') => out += &if pad { format!("{:02}", wall.year() / 100) } else { (wall.year() / 100).to_string() },
            Some('F') => out += &format!("{}-{:02}-{:02}", year(wall.year(), pad), wall.month(), wall.day()),
            Some('c') => out += &format!("{} {} {:>2} {:02}:{:02}:{:02} {}", &DAYS[wday][..3], &MONTHS[wall.month0() as usize][..3],
                                         wall.day(), wall.hour(), wall.minute(), wall.second(), year(wall.year(), !linux)),
            Some('y') => out += &format!("{:02}", wall.year() % 100),
            Some('g') => out += &format!("{:02}", wall.iso_week().year() % 100),
            Some('m') => out += &format!("{:02}", wall.month()),
            Some('d') => out += &format!("{:02}", wall.day()),
            Some('e') => out += &format!("{:>2}", wall.day()),
            Some('H') => out += &format!("{:02}", wall.hour()),
            Some('k') => out += &format!("{:>2}", wall.hour()),
            Some('I') => out += &format!("{h12:02}"),
            Some('l') => out += &format!("{h12:>2}"),
            Some('p') => out += ampm,
            // glibc's lower-case am/pm; macOS does not know %P and writes the letter alone
            Some('P') if linux => out += &ampm.to_lowercase(),
            Some('P') => out.push('P'),
            Some('M') => out += &format!("{:02}", wall.minute()),
            Some('S') => out += &format!("{:02}", wall.second()),
            Some('f') => out += &format!("{:06}", wall.nanosecond() / 1000),
            Some('j') => out += &format!("{:03}", wall.ordinal()),
            Some('A') => out += DAYS[wday],
            Some('a') => out += &DAYS[wday][..3],
            Some('B') => out += MONTHS[wall.month0() as usize],
            Some('b') | Some('h') => out += &MONTHS[wall.month0() as usize][..3],
            Some('u') => out += &(wday + 1).to_string(),
            Some('w') => out += &((wday + 1) % 7).to_string(),
            Some('U') => out += &format!("{:02}", (wall.ordinal0() as usize + 7 - (wday + 1) % 7) / 7),
            Some('W') => out += &format!("{:02}", (wall.ordinal0() as usize + 7 - wday) / 7),
            Some('V') => out += &format!("{:02}", wall.iso_week().week()),
            Some('D') | Some('x') => out += &format!("{:02}/{:02}/{:02}", wall.month(), wall.day(), wall.year() % 100),
            Some('T') | Some('X') => out += &format!("{:02}:{:02}:{:02}", wall.hour(), wall.minute(), wall.second()),
            Some('R') => out += &format!("{:02}:{:02}", wall.hour(), wall.minute()),
            Some('r') => out += &format!("{h12:02}:{:02}:{:02} {ampm}", wall.minute(), wall.second()),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('z') => {
                if let Some(o) = offset {
                    out += &dt::fmt_offset(o, false).replace(':', "");
                }
            }
            Some(':') if it.peek() == Some(&'z') => {
                it.next();
                if let Some(o) = offset {
                    out += &dt::fmt_offset(o, false);
                }
            }
            Some('Z') => out += &tzname.clone().unwrap_or_default(),
            Some('%') => out.push('%'),
            // a lone % at the end is kept by both C libraries
            None => out.push('%'),
            Some(other) if !strict => {
                out.push('%');
                out.push(other);
            }
            Some(other) => {
                return Err(Exc::value_error(format!("strftime directive '%{other}' is not supported by py2axum (glibc and \
                                                     macOS render it differently, or it reads the local time zone)")))
            }
        }
    }
    Ok(out)
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
    ser_bytes: pyd::BytesMode::Utf8,
    computed: &[],
    json_schema: None,
    x: pyd::SchemaX::NONE,
};

/// `sys.modules`: one table for the process, holding only what the project stores in it (the translation admits
/// only keys of modules the project's Python cannot import: absent in CPython too)
pub fn sys_modules() -> V {
    static M: std::sync::LazyLock<V> = std::sync::LazyLock::new(|| V::dict_from(vec![]).expect("empty dict"));
    M.clone()
}

static MODULE_CLASS: Class = Class { name: "module", qualname: "module", bases: &[], kind: ClassKind::Schema(&MODULE) };
/// `types.ModuleType(name, doc=None)`: an empty module object (a stub put in `sys.modules`), attributes only
pub static MODULE: pyd::SchemaDesc = pyd::SchemaDesc {
    name: "module",
    class: &MODULE_CLASS,
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
    hash: pyd::HashKind::Id,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    ser_bytes: pyd::BytesMode::Utf8,
    computed: &[],
    json_schema: None,
    x: pyd::SchemaX::NONE,
};

pub fn module_type(args: &[V], kwargs: &[(String, V)]) -> R {
    let (name, doc) = match (args, kwargs) {
        ([n], []) => (n.clone(), V::None),
        ([n, d], []) => (n.clone(), d.clone()),
        ([n], [(k, d)]) if k == "doc" => (n.clone(), d.clone()),
        ([], [(k, n)]) if k == "name" => (n.clone(), V::None),
        _ => return Err(Exc::type_error("py2axum: types.ModuleType(name, doc=None) takes the name and an optional doc")),
    };
    if !matches!(name, V::Str(_)) {
        return Err(Exc::type_error(format!("module.__init__() argument 'name' must be str, not {}", name.type_name())));
    }
    let o = pyd::object_new(&MODULE);
    super::methods::setattr(&o, "__name__", name)?;
    super::methods::setattr(&o, "__doc__", doc)?;
    Ok(o)
}

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

static THREAD_LOCAL_CLASS: Class = Class { name: "_local", qualname: "_thread._local", bases: &[], kind: ClassKind::Schema(&THREAD_LOCAL) };
/// `threading.local()`: an open instance. Every endpoint runs on the event loop's thread in uvicorn, so the binary
/// keeps one state for the whole process (a sync endpoint, run in a thread pool by FastAPI, sees its thread's own
/// state in CPython: docs/supported.md)
pub static THREAD_LOCAL: pyd::SchemaDesc = pyd::SchemaDesc {
    name: "_local",
    class: &THREAD_LOCAL_CLASS,
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
    hash: pyd::HashKind::Id,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    ser_bytes: pyd::BytesMode::Utf8,
    computed: &[],
    json_schema: None,
    x: pyd::SchemaX::NONE,
};

/// `threading.local()`
pub fn thread_local(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() || !kwargs.is_empty() {
        return Err(Exc::type_error("Initialization arguments are not supported"));
    }
    Ok(pyd::object_new(&THREAD_LOCAL))
}

/// `operator.index(a)`: an int (a bool is one); project classes with `__index__` are refused at translation
pub fn operator_index(args: &[V]) -> R {
    match args {
        [V::Int(i)] => Ok(V::Int(*i)),
        [V::Bool(b)] => Ok(V::Int(*b as i64)),
        [o] => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
        _ => Err(Exc::type_error(format!("index expected 1 argument, got {}", args.len()))),
    }
}

/// `dict.fromkeys(iterable, value=None)`
pub fn dict_fromkeys(args: &[V]) -> R {
    let keys = ops::iter(args.first().ok_or_else(|| Exc::type_error("fromkeys expected at least 1 argument, got 0"))?)?;
    let value = args.get(1).cloned().unwrap_or(V::None);
    V::dict_from(keys.into_iter().map(|k| (k, value.clone())).collect())
}


// ---------------------------------------------------------------- datetime.strptime / fromtimestamp, secrets.choice

/// `datetime.strptime(s, fmt)`: chrono parsing with Python's defaults (1900-01-01 00:00) and errors
/// The checks `_strptime` makes on the directives themselves: `%e` (3.13.6+), and `%d` without a year
/// (`%Y`, `%y`, `%G`), refused from Python 3.15
fn strptime_directives(f: &str) -> R<()> {
    let cs: Vec<char> = f.chars().collect();
    let (mut i, mut year, mut day_d) = (0, false, false);
    while i < cs.len() {
        if cs[i] != '%' {
            i += 1;
            continue;
        }
        // `%[-_0^#]*[0-9]*([OE]?[:\\]?.?)`: flags or a width make the directive another one
        let mut j = i + 1;
        while j < cs.len() && (matches!(cs[j], '-' | '_' | '0' | '^' | '#') || cs[j].is_ascii_digit()) {
            j += 1;
        }
        let plain = j == i + 1;
        if j < cs.len() && matches!(cs[j], 'O' | 'E') {
            j += 1;
        }
        if j < cs.len() && matches!(cs[j], ':' | '\\') {
            j += 1;
        }
        let d: String = cs[i + 1..(j + 1).min(cs.len())].iter().collect();
        if plain {
            match d.as_str() {
                "Y" | "y" | "G" => year = true,
                "d" => day_d = true,
                // accepted from 3.13.6
                "e" if !super::python_feature("strptime_e").unwrap_or_else(|| super::python_at_least(3, 13, 6)) => {
                    return Err(Exc::value_error(format!("'e' is a bad directive in format {}", ops::str_repr(f))));
                }
                _ => {}
            }
        }
        i = j + 1;
    }
    if day_d && !year && super::python() >= (3, 15) {
        return Err(Exc::value_error(
            "Day of month directive '%d' may not be used without a year directive. Parsing dates involving a day of month without a year is ambiguous and fails to parse leap day. Add a year to the input and format. See https://github.com/python/cpython/issues/70647.",
        ));
    }
    Ok(())
}

pub fn strptime(s: &V, fmt: &V) -> R {
    let (s, f) = (ops::str_(s)?, ops::str_(fmt)?);
    if f.contains("%f") || f.contains("%Z") || f.contains("%U") || f.contains("%W") || f.contains("%c") || f.contains("%x") || f.contains("%X") {
        return Err(Exc::value_error(format!("py2axum: strptime format {f:?} is not supported")));
    }
    strptime_directives(&f)?;
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
    // the qualified name CPython's messages use
    let qual = |o: &V| match o {
        V::Native(n) if matches!(&**n, Native::Fraction(_)) => "fractions.Fraction",
        o => o.type_name(),
    };
    let t = match ts {
        V::Int(i) => *i as f64,
        V::Bool(b) => *b as i64 as f64,
        V::Float(f) => *f,
        // 3.15: any real number (Decimal, Fraction)
        V::Decimal(d) if super::python() >= (3, 15) => d.to_f64(),
        V::Native(n) if super::python() >= (3, 15) && matches!(&**n, Native::Fraction(_)) => {
            let Native::Fraction(f) = &**n else { unreachable!() };
            use num_traits::ToPrimitive;
            f.num.to_f64().unwrap_or(f64::NAN) / f.den.to_f64().unwrap_or(f64::NAN)
        }
        o if super::python() >= (3, 15) => return Err(Exc::type_error(format!("must be real number, not {}", qual(o)))),
        o if super::python() >= (3, 14) => return Err(Exc::type_error(format!("argument must be int or float, not {}", qual(o)))),
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

/// A timeout given to a C function of CPython (`socket.settimeout`, `create_connection(timeout=)`):
/// `_PyTime_FromSecondsObject`, its messages by version; any real number from 3.15 (Decimal, Fraction)
pub fn c_seconds(v: &V) -> R<f64> {
    let qual = |o: &V| match o {
        V::Native(n) if matches!(&**n, Native::Fraction(_)) && super::python() >= (3, 14) => "fractions.Fraction",
        o => o.type_name(),
    };
    // one message for float and int since the security releases 3.13.15 / 3.14.7 (then 3.15); before, the
    // float's names `time_t`, the int's `_PyTime_t` (3.12) or `PyTime_t` (3.13, 3.14)
    let unified = super::python_feature("pytime_range").unwrap_or_else(|| match super::python() {
        (3, 13) => super::python_at_least(3, 13, 15),
        (3, 14) => super::python_at_least(3, 14, 7),
        v => v >= (3, 15),
    });
    let range = |old: &str| {
        Exc::msg(&OVERFLOW_ERROR, if unified { "timestamp out of range for C PyTime_t".to_string() } else { old.to_string() })
    };
    let int_type = if super::python() >= (3, 13) { "PyTime_t" } else { "_PyTime_t" };
    let t = match v {
        V::Bool(b) => return Ok(*b as i64 as f64),
        V::Int(i) if i.unsigned_abs() > 9_223_372_036 => {
            return Err(range(&format!("timestamp too large to convert to C {int_type}")))
        }
        V::Int(i) => return Ok(*i as f64),
        V::Float(f) => *f,
        V::Decimal(d) if super::python() >= (3, 15) => d.to_f64(),
        V::Native(n) if super::python() >= (3, 15) && matches!(&**n, Native::Fraction(_)) => {
            let Native::Fraction(f) = &**n else { unreachable!() };
            use num_traits::ToPrimitive;
            f.num.to_f64().unwrap_or(f64::NAN) / f.den.to_f64().unwrap_or(f64::NAN)
        }
        o if super::python() >= (3, 15) => return Err(Exc::type_error(format!("must be real number, not {}", qual(o)))),
        o if super::python() >= (3, 14) => {
            return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer or float", qual(o))))
        }
        o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", qual(o)))),
    };
    if t.is_nan() {
        return Err(Exc::value_error("Invalid value NaN (not a number)"));
    }
    if t.abs() * 1e9 >= 9.223372036854775807e18 {
        return Err(range("timestamp out of range for platform time_t"));
    }
    Ok(t)
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
    // Random.choice: `len(seq)` (a TypeError without one), then `seq[i]` (a set is not subscriptable, a dict
    // is indexed by key)
    let n = ops::len(seq)?;
    if n == 0 {
        return Err(Exc::msg(&INDEX_ERROR, "Cannot choose from an empty sequence"));
    }
    ops::getitem(seq, &V::Int(rand::rngs::OsRng.gen_range(0..n) as i64))
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
