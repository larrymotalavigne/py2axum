//! `croniter` 6.2: a line-by-line port of `croniter/croniter.py` (expansion of the expression, `_calc` and
//! its `proc_*` steps, the DST handling of `_add_tzinfo`, `H` hashing by CRC-32, `croniter_range`,
//! `match`/`match_range`, `is_valid`, `expand`), with dateutil's `relativedelta` arithmetic and CPython's
//! `datetime.fromtimestamp` / `timedelta.total_seconds` rounding. Kept apart: `R` (random) fields raise a
//! py2axum `NotImplementedError` where Python would draw a random number (refused at generation when the
//! expression is a literal); iterating a croniter object or `all_next()`/`all_prev()` with `for` (endless).
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime as Ndt, Offset, TimeZone, Timelike};
use num_bigint::BigInt;
use parking_lot::Mutex;
use regex::Regex;

use super::dt::{DateTime, Tz};
use super::ops;
use super::v::*;

// ---------------------------------------------------------------- constants and regexes

const MINUTE: usize = 0;
const HOUR: usize = 1;
const DAY: usize = 2;
const MONTH: usize = 3;
const DOW: usize = 4;
const SECOND: usize = 5;
const YEAR: usize = 6;
const UNIX_LEN: usize = 5;
const SECOND_LEN: usize = 6;
const YEAR_LEN: usize = 7;
const RANGES: [(i64, i64); 7] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 6), (0, 59), (1970, 2099)];
const LEN_MEANS_ALL: [usize; 7] = [60, 24, 31, 12, 7, 60, 130];
const DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
/// a stand-in for integers beyond 64 bits: above every field's range, as Python's big ints are
const BIG: i64 = 1_000_000_000_000_000_000;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("croniter regex")
}

// Python's `\w` (str.isalnum() or `_`), not the regex crate's (which adds marks)
static SPECIAL_DOW: LazyLock<Regex> = LazyLock::new(|| {
    let (w, m) = (WEEKDAYS.join("|"), MONTHS.join("|"));
    re(&format!(r"^(?P<pre>((?P<he>(({w})(-({w}))?)|(({m})(-({m}))?)|[\p{{L}}\p{{N}}_]+)#)|l)(?P<last>\d+)$"))
});
static STEP_SEARCH: LazyLock<Regex> = LazyLock::new(|| re(r"^([^-]+)-([^-/]+)(/(\d+))?$"));
static ONLY_INT: LazyLock<Regex> = LazyLock::new(|| re(r"^\d+$"));
static STAR_OR_INT: LazyLock<Regex> = LazyLock::new(|| re(r"^(\d+|\*)$"));
static NEAREST_WEEKDAY: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:(\d+)w|w(\d+))$"));
static HASH_EXPR: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?P<hash_type>h|r)(\((?P<range_begin>\d+)-(?P<range_end>\d+)\))?(/(?P<divisor>\d+))?$"));
static STAR_STEP: LazyLock<Regex> = LazyLock::new(|| re(r"^\*(/.+)$"));
static ANY_STEP: LazyLock<Regex> = LazyLock::new(|| re(r"^(.+)/(.+)$"));

// ---------------------------------------------------------------- exceptions

fn bad_cron(msg: impl AsRef<str>) -> Exc {
    Exc::msg(&CRONITER_BAD_CRON_ERROR, msg)
}
fn bad_date(prev: bool) -> Exc {
    Exc::msg(&CRONITER_BAD_DATE_ERROR, if prev { "failed to find prev date" } else { "failed to find next date" })
}
fn ovf() -> Exc {
    Exc::msg(&OVERFLOW_ERROR, "date value out of range")
}
/// CPython's message (reworded in 3.14)
fn year_range(y: i64) -> Exc {
    if super::python() >= (3, 14) {
        Exc::value_error(format!("year must be in 1..9999, not {y}"))
    } else {
        Exc::value_error(format!("year {y} is out of range"))
    }
}
fn assertion() -> Exc {
    Exc::new(&ASSERTION_ERROR, vec![])
}

// ---------------------------------------------------------------- integers as Python parses them

/// the value of a Unicode decimal digit (category Nd), as `int()` reads it
fn digit(c: char) -> Option<u32> {
    if let Some(d) = c.to_digit(10) {
        return Some(d);
    }
    let cp = c as u32;
    ND_ZEROS.iter().find(|&&z| cp >= z && cp < z + 10).map(|z| cp - z)
}

/// first code point of each run of ten Unicode decimal digits (Python 3.12's unicodedata, Nd)
const ND_ZEROS: &[u32] = &[
    0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0, 0xF20, 0x1040,
    0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0,
    0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650,
    0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60, 0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8,
    0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950, 0x1FBF0,
];

/// `int(s)` of a string of decimal digits (already matched by `\d+`): CPython's 4300-digit limit
fn int_big(s: &str) -> R<BigInt> {
    let n = s.chars().count();
    if n > 4300 {
        return Err(Exc::value_error(format!(
            "Exceeds the limit (4300 digits) for integer string conversion: value has {n} digits; use sys.set_int_max_str_digits() to increase the limit"
        )));
    }
    let ascii: String = s.chars().map(|c| char::from_digit(digit(c).unwrap_or(0), 10).unwrap_or('0')).collect();
    Ok(ascii.parse::<BigInt>().unwrap_or_default())
}

/// `int(s)` saturated at `BIG` (no field accepts such a value)
fn int_sat(s: &str) -> R<i64> {
    let b = int_big(s)?;
    Ok(i64::try_from(&b).ok().filter(|v| *v < BIG).unwrap_or(BIG))
}

// ---------------------------------------------------------------- the expanded expression

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum It {
    I(i64),
    Star,
    L,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum Nth {
    N(i64),
    L,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum El {
    S(String),
    I(i64),
}

impl El {
    fn text(&self) -> String {
        match self {
            El::S(s) => s.clone(),
            El::I(i) => i.to_string(),
        }
    }
}

#[derive(Clone)]
struct Expanded {
    expanded: Vec<Vec<It>>,
    /// `nth_weekday_of_month`: (weekday, set of nth), in insertion order
    nth: Vec<(i64, Vec<Nth>)>,
    expressions: Vec<String>,
    nearest: Vec<i64>,
}

#[derive(Clone)]
enum HashId {
    None,
    Bytes(Vec<u8>),
    /// a `str` given to `croniter.expand()` (not encoded there: crc32 refuses it)
    Str,
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// `HashExpander.do`
fn hash_do(idx: usize, hash_type: &str, hash_id: &HashId, begin: Option<BigInt>, end: Option<BigInt>) -> R<BigInt> {
    let end = end.unwrap_or_else(|| BigInt::from(RANGES[idx].1));
    let begin = begin.unwrap_or_else(|| BigInt::from(RANGES[idx].0));
    let crc = if hash_type == "r" {
        return Err(Exc::msg(
            &NOT_IMPLEMENTED_ERROR,
            "py2axum: croniter's R (random) expressions are not supported (Python draws a random number: not reproducible)",
        ));
    } else {
        match hash_id {
            HashId::Bytes(b) => crc32(b),
            HashId::Str => return Err(Exc::type_error("a bytes-like object is required, not 'str'")),
            HashId::None => return Err(Exc::type_error("a bytes-like object is required, not 'NoneType'")),
        }
    };
    let m = &end - &begin + 1;
    let x = BigInt::from(crc >> idx);
    let r = ((x % &m) + &m) % &m;
    Ok(r + begin)
}

/// `HashExpander.expand`
fn hash_expand(idx: usize, expr: &str, hash_id: &HashId) -> R<String> {
    let Some(c) = HASH_EXPR.captures(expr) else { return Ok(expr.to_string()) };
    let ht = c.name("hash_type").map(|m| m.as_str()).unwrap_or("h");
    if ht == "h" && matches!(hash_id, HashId::None) {
        return Err(bad_cron("Hashed definitions must include hash_id"));
    }
    let rb = c.name("range_begin").map(|m| m.as_str());
    let rend = c.name("range_end").map(|m| m.as_str());
    let div = c.name("divisor").map(|m| m.as_str());
    if let (Some(b), Some(e)) = (rb, rend) {
        if int_big(b)? >= int_big(e)? {
            return Err(bad_cron("Range end must be greater than range begin"));
        }
    }
    if let (Some(b), Some(e), Some(d)) = (rb, rend, div) {
        let dv = int_big(d)?;
        if dv == BigInt::from(0) {
            return Err(bad_cron(format!("Bad expression: {expr}")));
        }
        let bv = int_big(b)?;
        let x = hash_do(idx, ht, hash_id, Some(bv.clone()), Some(&dv - 1 + &bv))?;
        return Ok(format!("{x}-{}/{dv}", int_big(e)?));
    }
    if let (Some(b), Some(e)) = (rb, rend) {
        return Ok(hash_do(idx, ht, hash_id, Some(int_big(b)?), Some(int_big(e)?))?.to_string());
    }
    if let Some(d) = div {
        let dv = int_big(d)?;
        if dv == BigInt::from(0) {
            return Err(bad_cron(format!("Bad expression: {expr}")));
        }
        let lo = BigInt::from(RANGES[idx].0);
        let x = hash_do(idx, ht, hash_id, Some(lo.clone()), Some(&dv - 1 + lo))?;
        return Ok(format!("{x}-{}/{dv}", RANGES[idx].1));
    }
    Ok(hash_do(idx, ht, hash_id, None, None)?.to_string())
}

fn alphaconv(index: usize, key: &str, expressions: &[String]) -> R<It> {
    let found = match index {
        DAY if key == "l" => Some(It::L),
        MONTH => MONTHS.iter().position(|m| *m == key).map(|i| It::I(i as i64 + 1)),
        DOW => WEEKDAYS.iter().position(|m| *m == key).map(|i| It::I(i as i64)),
        _ => None,
    };
    found.ok_or_else(|| Exc::msg(&CRONITER_NOT_ALPHA_ERROR, format!("[{}] is not acceptable", expressions.join(" "))))
}

fn it_text(t: It) -> String {
    match t {
        It::I(i) => i.to_string(),
        It::Star => "*".into(),
        It::L => "l".into(),
    }
}

/// `croniter.value_alias`
fn value_alias(val: i64, field: usize, n: usize) -> i64 {
    let low = match field {
        DAY | MONTH if val == 0 => Some(1),
        DOW if val == 7 => Some(0),
        _ => None,
    };
    if let Some(x) = low {
        let keep = ((field == DAY || field == MONTH) && n == UNIX_LEN)
            || ((field == MONTH || field == DOW) && n == SECOND_LEN)
            || ((field == DAY || field == MONTH || field == DOW) && n == YEAR_LEN);
        if !keep {
            return x;
        }
    }
    val
}

/// Python's `str.isspace()`
fn py_space(c: char) -> bool {
    matches!(c, '\t'..='\r' | '\x1c'..='\x1f' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

fn low_from_timestamp(field: usize, step: i64, ts: i64) -> R<i64> {
    let dt = utc_from_secs(ts, 0)?;
    let step = step.max(1);
    Ok(match field {
        MINUTE => (dt.minute() as i64).rem_euclid(step),
        HOUR => (dt.hour() as i64).rem_euclid(step),
        DAY => (dt.day() as i64 - 1).rem_euclid(step) + 1,
        MONTH => (dt.month() as i64 - 1).rem_euclid(step) + 1,
        DOW => (dt.weekday().number_from_monday() as i64 % 7).rem_euclid(step),
        _ => return Err(Exc::value_error("Can't get current date number for index larger than 4")),
    })
}

fn int_list_repr(v: &[i64]) -> String {
    format!("[{}]", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "))
}

fn nth_set_repr(s: &[Nth]) -> String {
    let mut ints: Vec<i64> = s.iter().filter_map(|n| if let Nth::N(i) = n { Some(*i) } else { None }).collect();
    ints.sort();
    let mut parts: Vec<String> = ints.iter().map(|i| i.to_string()).collect();
    if s.contains(&Nth::L) {
        parts.push("'l'".into());
    }
    format!("{{{}}}", parts.join(", "))
}

fn is_leap(y: i64) -> bool {
    y % 400 == 0 || (y % 4 == 0 && y % 100 != 0)
}

fn last_day(year: i64, month: u32) -> u32 {
    let d = DAYS[month as usize - 1];
    if month == 2 && is_leap(year) {
        d + 1
    } else {
        d
    }
}

/// `croniter._expand`
fn expand(expr_format: &V, hash_id: &HashId, second_at_beginning: bool, from_ts: Option<&V>, strict: bool, strict_year: &V) -> R<Expanded> {
    let fmt = match expr_format {
        V::Str(s) => s.to_string(),
        V::Bytes(b) => {
            // bytes.split(): ASCII whitespace only
            let n = b.split(|c| c.is_ascii_whitespace() || *c == 0x0b).filter(|p| !p.is_empty()).count();
            if !(UNIX_LEN..=YEAR_LEN).contains(&n) {
                return Err(bad_cron("Exactly 5, 6 or 7 columns has to be specified for iterator expression."));
            }
            return Err(Exc::type_error("cannot use a string pattern on a bytes-like object"));
        }
        other => return Err(Exc::attr_error(format!("'{}' object has no attribute 'lower'", other.type_name()))),
    };
    let mut efl = fmt.to_lowercase();
    let hashed = !matches!(hash_id, HashId::None);
    let alias = match efl.as_str() {
        "@midnight" => Some(("0 0 * * *", "h h(0-2) * * * h")),
        "@hourly" => Some(("0 * * * *", "h * * * * h")),
        "@daily" => Some(("0 0 * * *", "h h * * * h")),
        "@weekly" => Some(("0 0 * * 0", "h h * * h h")),
        "@monthly" => Some(("0 0 1 * *", "h h h * * h")),
        "@yearly" | "@annually" => Some(("0 0 1 1 *", "h h h h * h")),
        _ => None,
    };
    if let Some((plain, h)) = alias {
        efl = if hashed { h.to_string() } else { plain.to_string() };
    }
    let mut expressions: Vec<String> = efl.split(py_space).filter(|p| !p.is_empty()).map(|s| s.to_string()).collect();
    if !(UNIX_LEN..=YEAR_LEN).contains(&expressions.len()) {
        return Err(bad_cron("Exactly 5, 6 or 7 columns has to be specified for iterator expression."));
    }
    if expressions.len() > UNIX_LEN && second_at_beginning {
        let s = expressions.remove(0);
        expressions.insert(SECOND, s);
    }
    let n = expressions.len();
    let mut expanded: Vec<Vec<It>> = Vec::new();
    let mut nth_map: Vec<(i64, Vec<Nth>)> = Vec::new();
    let mut nearest: Vec<i64> = Vec::new();

    for field in 0..n {
        let mut expr = hash_expand(field, &expressions[field], hash_id)?;
        if expr.contains('?') {
            if expr != "?" {
                return Err(bad_cron(format!("[{fmt}] is not acceptable. Question mark can not used with other characters")));
            }
            if field != DAY && field != DOW {
                return Err(bad_cron(format!("[{fmt}] is not acceptable. Question mark can only used in day_of_month or day_of_week")));
            }
            expr = "*".into();
        }
        let mut e_list: Vec<El> = expr.split(',').map(|s| El::S(s.to_string())).collect();
        let mut res: Vec<It> = Vec::new();
        let mut seen: HashSet<El> = HashSet::new();
        let (lo_r, hi_r) = RANGES[field];

        while let Some(mut e) = e_list.pop() {
            let mut nth: Option<Nth> = None;
            if field == DOW {
                let es = e.text();
                if let Some(c) = SPECIAL_DOW.captures(&es) {
                    let last = c.name("last").map(|m| m.as_str()).unwrap_or("");
                    if let Some(he) = c.name("he").map(|m| m.as_str()).filter(|s| !s.is_empty()) {
                        e = El::S(he.to_string());
                        let v = match int_big(last) {
                            Ok(v) => v,
                            Err(_) => return Err(bad_cron(format!("[{fmt}] is not acceptable. Invalid day_of_week value: 'None'"))),
                        };
                        if v > BigInt::from(5) || v < BigInt::from(1) {
                            return Err(bad_cron(format!("[{fmt}] is not acceptable. Invalid day_of_week value: '{v}'")));
                        }
                        nth = Some(Nth::N(i64::try_from(&v).unwrap_or(1)));
                    } else if !last.is_empty() {
                        e = El::S(last.to_string());
                        nth = Some(Nth::L);
                    }
                }
            }
            if field == DAY {
                let es = e.text();
                if let Some(c) = NEAREST_WEEKDAY.captures(&es) {
                    let g = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
                    let w = int_big(g)?;
                    if w < BigInt::from(1) || w > BigInt::from(31) {
                        return Err(bad_cron(format!("[{fmt}] is not acceptable, nearest weekday day value '{w}' out of range")));
                    }
                    if !e_list.is_empty() || !res.is_empty() {
                        return Err(bad_cron(format!(
                            "[{fmt}] is not acceptable. 'W' can only be used with a single day value, not in a list or range"
                        )));
                    }
                    let w = i64::try_from(&w).unwrap_or(1);
                    if !nearest.contains(&w) {
                        nearest.push(w);
                    }
                    res.push(It::I(w));
                    continue;
                }
            }
            let es = e.text();
            let mut t = STAR_STEP.replace(&es, format!("{lo_r}-{hi_r}${{1}}").as_str()).into_owned();
            let mut m = STEP_SEARCH.captures(&t).map(|c| (c[1].to_string(), c[2].to_string(), c.get(4).map(|x| x.as_str().to_string())));
            if m.is_none() {
                t = ANY_STEP.replace(&es, format!("${{1}}-{hi_r}/${{2}}").as_str()).into_owned();
                m = STEP_SEARCH.captures(&t).map(|c| (c[1].to_string(), c[2].to_string(), c.get(4).map(|x| x.as_str().to_string())));
            }
            if let Some((mut low, mut high, step)) = m {
                if field == DAY && high == "l" {
                    high = "31".into();
                }
                if !ONLY_INT.is_match(&low) {
                    low = it_text(alphaconv(field, &low, &expressions)?);
                }
                if !ONLY_INT.is_match(&high) {
                    high = it_text(alphaconv(field, &high, &expressions)?);
                }
                let step = match &step {
                    Some(s) => int_sat(s)?,
                    None => 1,
                };
                if step == 0 {
                    return Err(bad_cron(format!("[{fmt}] step '0' in field {field} is not acceptable")));
                }
                for band in [&low, &high] {
                    if !ONLY_INT.is_match(band) {
                        return Err(bad_cron(format!("[{fmt}] bands '{low}-{high}' in field {field} are not acceptable")));
                    }
                }
                let mut low = value_alias(int_sat(&low)?, field, n);
                let high = value_alias(int_sat(&high)?, field, n);
                if low.max(high) > lo_r.max(hi_r) {
                    return Err(bad_cron(format!("{fmt} is out of bands")));
                }
                if let Some(ts) = from_ts {
                    if ops::truthy(ts)? {
                        let ti = match ts {
                            V::Int(i) => *i,
                            V::Bool(b) => *b as i64,
                            V::Float(f) if f.is_nan() => return Err(Exc::value_error("cannot convert float NaN to integer")),
                            V::Float(f) if f.is_infinite() => return Err(Exc::msg(&OVERFLOW_ERROR, "cannot convert float infinity to integer")),
                            V::Float(f) => f.trunc() as i64,
                            _ => return Err(Exc::type_error("py2axum: croniter start time of this type is not supported")),
                        };
                        low = low_from_timestamp(field, step, ti)?;
                    }
                }
                let rng: Vec<i64> = if low > high {
                    let whole: Vec<i64> = (lo_r..=hi_r).collect();
                    let mut rng: Vec<i64> = range(low, hi_r + 1, step);
                    let mut to_skip = 0;
                    if let Some(&lastv) = rng.last() {
                        let already = whole.iter().rev().position(|x| *x == lastv).unwrap_or(0) as i64;
                        let curpos = whole.iter().position(|x| *x == lastv).unwrap_or(0) as i64;
                        if curpos.saturating_add(step) > whole.len() as i64 && already < step {
                            to_skip = step - already;
                        }
                    }
                    rng.extend(range(lo_r + to_skip, high + 1, step));
                    rng
                } else if low == high {
                    range(lo_r, hi_r + 1, step)
                } else {
                    range(low, high + 1, step)
                };
                let items: Vec<El> = match nth {
                    Some(Nth::N(k)) if field == DOW => rng.iter().map(|i| El::S(format!("{i}#{k}"))).collect(),
                    _ => rng.iter().map(|i| El::I(*i)).collect(),
                };
                for a in &items {
                    if !seen.contains(a) {
                        e_list.push(a.clone());
                    }
                }
                seen.extend(items);
            } else {
                if t.starts_with('-') {
                    return Err(bad_cron(format!("[{fmt}] is not acceptable, negative numbers not allowed")));
                }
                let mut tv = if !STAR_OR_INT.is_match(&t) {
                    alphaconv(field, &t, &expressions)?
                } else if t == "*" {
                    It::Star
                } else {
                    It::I(int_sat(&t)?)
                };
                if let It::I(i) = tv {
                    tv = It::I(value_alias(i, field, n));
                    let i = value_alias(i, field, n);
                    if i < lo_r || i > hi_r {
                        return Err(bad_cron(format!("[{fmt}] is not acceptable, out of range")));
                    }
                }
                res.push(tv);
                if field == DOW {
                    if let (Some(k), It::I(t)) = (nth, tv) {
                        match nth_map.iter_mut().find(|(w, _)| *w == t) {
                            Some((_, s)) => {
                                if !s.contains(&k) {
                                    s.push(k);
                                }
                            }
                            None => nth_map.push((t, vec![k])),
                        }
                    }
                }
            }
        }
        // set, then sorted by f"{i:02}" for ints, the string itself for '*' and 'l'
        let mut uniq: Vec<It> = Vec::new();
        for r in res {
            if !uniq.contains(&r) {
                uniq.push(r);
            }
        }
        uniq.sort_by_key(|x| match x {
            It::Star => (0, 0),
            It::I(i) => (1, *i),
            It::L => (2, 0),
        });
        let mut res = uniq;
        if res.len() == LEN_MEANS_ALL[field] {
            let keep = (field == DAY && !expressions[DOW].contains('*')) || (field == DOW && !expressions[DAY].contains('*'));
            if !keep {
                res = vec![It::Star];
            }
        }
        expanded.push(res);
    }

    if !nth_map.is_empty() {
        let all: Vec<It> = expanded[DOW].clone();
        let mut distinct: Vec<It> = Vec::new();
        for x in &all {
            if !distinct.contains(x) {
                distinct.push(*x);
            }
        }
        let mut rest: Vec<i64> = distinct
            .iter()
            .filter_map(|x| match x {
                It::I(i) if !nth_map.iter().any(|(w, _)| w == i) => Some(*i),
                _ => None,
            })
            .collect();
        let has_l = distinct.contains(&It::L);
        rest.sort();
        if (!rest.is_empty() || has_l) && distinct.len() != LEN_MEANS_ALL[DOW] {
            let mut parts: Vec<String> = rest.iter().map(|i| i.to_string()).collect();
            if has_l {
                parts.push("'l'".into());
            }
            let nth_txt = nth_map.iter().map(|(w, s)| format!("{w}: {}", nth_set_repr(s))).collect::<Vec<_>>().join(", ");
            return Err(Exc::msg(
                &CRONITER_UNSUPPORTED_SYNTAX_ERROR,
                format!(
                    "day-of-week field does not support mixing literal values and nth day of week syntax.  Cron: '{fmt}'    dow={{{}}} vs nth={{{nth_txt}}}",
                    parts.join(", ")
                ),
            ));
        }
    }

    if strict {
        let days = &expanded[DAY];
        let months = &expanded[MONTH];
        if days != &vec![It::Star] && days != &vec![It::L] && months != &vec![It::Star] {
            let int_days: Vec<i64> = days.iter().filter_map(|x| if let It::I(i) = x { Some(*i) } else { None }).collect();
            let int_months: Vec<i64> = months.iter().filter_map(|x| if let It::I(i) = x { Some(*i) } else { None }).collect();
            if !int_days.is_empty() && !int_months.is_empty() {
                let mut dim: Vec<i64> = DAYS.iter().map(|d| *d as i64).collect();
                if int_months.contains(&2) {
                    let mut leap = true;
                    if !strict_year.is_none() {
                        leap = match strict_year {
                            V::Int(y) => is_leap(*y),
                            V::Bool(b) => is_leap(*b as i64),
                            other => {
                                let mut any = false;
                                for y in ops::iter(other)? {
                                    let y = match y {
                                        V::Int(y) => y,
                                        V::Bool(b) => b as i64,
                                        o => return Err(Exc::type_error(format!("unsupported operand type(s) for %: '{}' and 'int'", o.type_name()))),
                                    };
                                    if is_leap(y) {
                                        any = true;
                                        break;
                                    }
                                }
                                any
                            }
                        };
                    } else if expanded.len() > YEAR {
                        let years = &expanded[YEAR];
                        if years != &vec![It::Star] {
                            let iy: Vec<i64> = years.iter().filter_map(|x| if let It::I(i) = x { Some(*i) } else { None }).collect();
                            if !iy.is_empty() {
                                leap = iy.iter().any(|y| is_leap(*y));
                            }
                        }
                    }
                    if leap {
                        dim[1] = 29;
                    }
                }
                let min_day = *int_days.iter().min().unwrap_or(&0);
                let max_possible = int_months.iter().map(|m| dim[(*m as usize).clamp(1, 12) - 1]).max().unwrap_or(0);
                if min_day > max_possible {
                    return Err(bad_cron(format!(
                        "[{fmt}] is not acceptable. Day(s) {} can never occur in month(s) {}",
                        int_list_repr(&int_days),
                        int_list_repr(&int_months)
                    )));
                }
            }
        }
    }
    Ok(Expanded { expanded, nth: nth_map, expressions, nearest })
}

/// `list(range(a, b, step))`, step > 0
fn range(a: i64, b: i64, step: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let mut x = a;
    while x < b {
        out.push(x);
        x = match x.checked_add(step) {
            Some(v) => v,
            None => break,
        };
    }
    out
}

// ---------------------------------------------------------------- datetimes

fn chk(d: Ndt) -> R<Ndt> {
    if (1..=9999).contains(&d.year()) {
        Ok(d)
    } else {
        Err(ovf())
    }
}

fn add_us(d: Ndt, us: i64) -> R<Ndt> {
    d.checked_add_signed(Duration::microseconds(us)).ok_or_else(ovf).and_then(chk)
}

/// a dateutil `relativedelta` with the fields croniter uses
#[derive(Default, Clone, Copy)]
struct Rd {
    years: i64,
    months: i64,
    days: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
    micros: i64,
    month: Option<u32>,
    day: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<u32>,
}

/// `relativedelta.__add__(datetime)`
fn rd_add(d: Ndt, r: Rd) -> R<Ndt> {
    let mut year = d.year() as i64 + r.years;
    let mut month = r.month.unwrap_or(d.month()) as i64;
    if r.months != 0 {
        let t = year * 12 + (month - 1) + r.months;
        year = t.div_euclid(12);
        month = t.rem_euclid(12) + 1;
    }
    let day = last_day(year, month as u32).min(r.day.unwrap_or(d.day()));
    if !(1..=9999).contains(&year) {
        return Err(year_range(year));
    }
    let date = NaiveDate::from_ymd_opt(year as i32, month as u32, day).ok_or_else(ovf)?;
    let base = date
        .and_hms_nano_opt(r.hour.unwrap_or(d.hour()), r.minute.unwrap_or(d.minute()), r.second.unwrap_or(d.second()), d.nanosecond())
        .ok_or_else(ovf)?;
    let us = (r.days as i128 * 86_400 + r.hours as i128 * 3600 + r.minutes as i128 * 60 + r.seconds as i128) * 1_000_000 + r.micros as i128;
    let us = i64::try_from(us).map_err(|_| ovf())?;
    add_us(base, us)
}

/// UTC wall time of a (seconds, microseconds) timestamp: `gmtime` and the datetime year check
fn utc_from_secs(sec: i64, us: i64) -> R<Ndt> {
    let days = sec.div_euclid(86_400);
    let rem = sec.rem_euclid(86_400);
    // civil from days (Howard Hinnant), in i128 to keep the year of any time_t
    let z = days as i128 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    if y - 1900 > i32::MAX as i128 || y - 1900 < i32::MIN as i128 {
        return Err(os_overflow());
    }
    if !(1..=9999).contains(&y) {
        return Err(year_range(y as i64));
    }
    let base = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    base.checked_add_signed(Duration::days(days) + Duration::seconds(rem) + Duration::microseconds(us)).ok_or_else(ovf)
}

fn os_overflow() -> Exc {
    // gmtime's EOVERFLOW (its number and text are the platform's)
    if cfg!(target_os = "macos") {
        Exc::msg(&OS_ERROR, "[Errno 84] Value too large to be stored in data type")
    } else {
        Exc::msg(&OS_ERROR, "[Errno 75] Value too large for defined data type")
    }
}

/// `_PyTime_RoundHalfEven`
fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - r).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        r
    }
}

/// `datetime.fromtimestamp(ts, tz=tzutc()).replace(tzinfo=None)`
fn fromtimestamp_utc(ts: &V) -> R<Ndt> {
    let (sec, us) = match ts {
        V::Int(i) => (*i, 0),
        V::Bool(b) => (*b as i64, 0),
        V::Float(f) => {
            if f.is_nan() {
                return Err(Exc::value_error("Invalid value NaN (not a number)"));
            }
            let mut ip = f.trunc();
            let mut fp = round_half_even((f - ip) * 1e6);
            if fp >= 1e6 {
                fp -= 1e6;
                ip += 1.0;
            } else if fp < 0.0 {
                fp += 1e6;
                ip -= 1.0;
            }
            if !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&ip) {
                return Err(Exc::msg(&OVERFLOW_ERROR, "timestamp out of range for platform time_t"));
            }
            (ip as i64, fp as i64)
        }
        other => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", other.type_name()))),
    };
    utc_from_secs(sec, us)
}

/// `timedelta.total_seconds()` of a number of microseconds: the correctly rounded quotient
fn secs(us: i64) -> f64 {
    const LIM: i64 = 1 << 53;
    if us.abs() <= LIM {
        return us as f64 / 1e6;
    }
    let neg = us < 0;
    let a = (us as i128).unsigned_abs();
    let d: u128 = 1_000_000;
    let mut k = 0u32;
    while ((a << (k + 1)) / d) < (1u128 << 53) {
        k += 1;
    }
    let mut m = (a << k) / d;
    let rem = (a << k) % d;
    if 2 * rem > d || (2 * rem == d && m & 1 == 1) {
        m += 1;
    }
    let v = m as f64 / (2f64).powi(k as i32);
    if neg {
        -v
    } else {
        v
    }
}

/// the UTC offset (seconds) of a wall time in `tz`, PEP 495 fold included (gap: before/after the transition)
fn utcoffset(tz: &Tz, w: &Ndt, fold: u8) -> i32 {
    match tz {
        Tz::Utc => 0,
        Tz::Fixed(s) => *s,
        Tz::Zone(z) => match z.offset_from_local_datetime(w) {
            chrono::LocalResult::Single(o) => o.fix().local_minus_utc(),
            chrono::LocalResult::Ambiguous(a, b) => {
                if fold == 0 {
                    a.fix().local_minus_utc()
                } else {
                    b.fix().local_minus_utc()
                }
            }
            chrono::LocalResult::None => {
                let probe = if fold == 0 { *w - Duration::hours(26) } else { *w + Duration::hours(26) };
                z.offset_from_utc_datetime(&probe).fix().local_minus_utc()
            }
        },
        // pytz / dateutil tzinfos: their own utcoffset()
        _ => tz.offset_for_wall(w, fold),
    }
}

fn exists(tz: &Tz, w: &Ndt) -> bool {
    match tz {
        Tz::Zone(z) => !matches!(z.offset_from_local_datetime(w), chrono::LocalResult::None),
        Tz::Utc | Tz::Fixed(_) => true,
        // the round trip through UTC gives the wall time back (dateutil's datetime_exists)
        _ => {
            let off = tz.offset_for_wall(w, 0) as i64;
            super::dt::from_utc_checked(*w - Duration::seconds(off), *tz).map(|d| d.wall == *w).unwrap_or(true)
        }
    }
}

fn utc_us(d: &DateTime) -> i64 {
    let off = d.tz.map(|t| utcoffset(&t, &d.wall, d.fold)).unwrap_or(0);
    (d.wall - Duration::seconds(off as i64)).and_utc().timestamp_micros()
}

fn wall_us(w: &Ndt) -> i64 {
    w.and_utc().timestamp_micros()
}

/// `datetime_to_timestamp(d)`
fn dt_to_ts(d: &DateTime) -> R<f64> {
    let w = match d.tz {
        Some(tz) => {
            let off = utcoffset(&tz, &d.wall, d.fold);
            add_us(d.wall, -(off as i64) * 1_000_000)?
        }
        None => d.wall,
    };
    Ok(secs(wall_us(&w)))
}

/// `datetime.astimezone(tz)` of a UTC wall time
fn from_utc(utc: Ndt, tz: Tz) -> R<DateTime> {
    match tz {
        Tz::Utc => Ok(DateTime { wall: utc, tz: Some(tz), fold: 0 }),
        Tz::Fixed(s) => Ok(DateTime { wall: add_us(utc, s as i64 * 1_000_000)?, tz: Some(tz), fold: 0 }),
        _ => {
            let d = super::dt::from_utc_checked(utc, tz)?;
            chk(d.wall)?;
            Ok(d)
        }
    }
}

/// `croniter.timestamp_to_datetime(ts)`
fn ts_to_dt(ts: &V, tz: Option<Tz>) -> R<DateTime> {
    let utc = fromtimestamp_utc(ts)?;
    match tz {
        Some(tz) => from_utc(utc, tz),
        None => Ok(DateTime::naive(utc)),
    }
}

/// `a < b` of two datetimes: the wall times under the same tzinfo, else the UTC instants
fn dt_cmp(a: &DateTime, b: &DateTime) -> R<std::cmp::Ordering> {
    match (a.tz, b.tz) {
        (None, None) => Ok(a.wall.cmp(&b.wall)),
        (Some(x), Some(y)) if x == y => Ok(a.wall.cmp(&b.wall)),
        (Some(_), Some(_)) => Ok(utc_us(a).cmp(&utc_us(b))),
        _ => Err(Exc::type_error("can't compare offset-naive and offset-aware datetimes")),
    }
}

/// `a - b` of two datetimes, in microseconds
fn dt_sub(a: &DateTime, b: &DateTime) -> R<i64> {
    match (a.tz, b.tz) {
        (None, None) => Ok(wall_us(&a.wall) - wall_us(&b.wall)),
        (Some(x), Some(y)) if x == y => Ok(wall_us(&a.wall) - wall_us(&b.wall)),
        (Some(_), Some(_)) => Ok(utc_us(a) - utc_us(b)),
        _ => Err(Exc::type_error("can't subtract offset-naive and offset-aware datetimes")),
    }
}

// ---------------------------------------------------------------- the croniter object

struct St {
    ret_type: V,
    day_or: bool,
    implement_cron_bug: bool,
    expand_from_start_time: bool,
    max_years_explicit: bool,
    max_years: i64,
    tzinfo: Option<Tz>,
    start_time: V,
    dst_start_time: V,
    cur: V,
    ex: Expanded,
    is_prev: bool,
    second_at_beginning: bool,
}

pub struct Cron {
    st: Mutex<St>,
}

/// all_next() / all_prev() generators and croniter_range()
pub struct Gen {
    kind: GenKind,
    state: Mutex<GState>,
}

enum GenKind {
    All { cron: Arc<Cron>, prev: bool, ret_type: V, update_current: V },
    /// croniter_range's arguments: its body runs on the first next(), as a Python generator's
    Range(Vec<Option<V>>),
}

struct GState {
    done: bool,
    start_time: V,
    /// croniter_range once started: (croniter, forward, stop, float results)
    range: Option<(Arc<Cron>, bool, DateTime, bool)>,
}

/// Python's binding of (args, kwargs) to named parameters
fn bind(func: &str, args: Vec<V>, kwargs: Vec<(String, V)>, names: &[&str], required: usize) -> R<Vec<Option<V>>> {
    if args.len() > names.len() {
        let total = names.len() + 1;
        let req = required + 1;
        let takes = if req == total { format!("{total} positional argument{}", if total == 1 { "" } else { "s" }) } else { format!("from {req} to {total} positional arguments") };
        return Err(Exc::type_error(format!("{func}() takes {takes} but {} were given", args.len() + 1)));
    }
    let mut out: Vec<Option<V>> = args.into_iter().map(Some).collect();
    out.resize(names.len(), None);
    for (k, v) in kwargs {
        match names.iter().position(|n| *n == k) {
            Some(i) if out[i].is_some() => return Err(Exc::type_error(format!("{func}() got multiple values for argument '{k}'"))),
            Some(i) => out[i] = Some(v),
            None => return Err(Exc::type_error(format!("{func}() got an unexpected keyword argument '{k}'"))),
        }
    }
    for (i, o) in out.iter().enumerate().take(required) {
        if o.is_none() {
            return Err(Exc::type_error(format!("{func}() missing 1 required positional argument: '{}'", names[i])));
        }
    }
    Ok(out)
}

fn opt(v: &Option<V>) -> V {
    v.clone().unwrap_or(V::None)
}

fn truthy_or(v: &Option<V>, default: bool) -> R<bool> {
    match v {
        None => Ok(default),
        Some(x) => ops::truthy(x),
    }
}

fn hash_id_of(v: &V, encode: bool) -> R<HashId> {
    match v {
        V::None => Ok(HashId::None),
        V::Bytes(b) => Ok(HashId::Bytes(b.to_vec())),
        V::Str(s) if encode => Ok(HashId::Bytes(s.as_bytes().to_vec())),
        V::Str(_) => Ok(HashId::Str),
        _ => Err(Exc::type_error("hash_id must be bytes or UTF-8 string")),
    }
}

enum Ret {
    Float,
    Dt,
}

/// `issubclass(ret_type, (float, datetime))`
fn ret_kind(v: &V) -> R<Option<Ret>> {
    match v {
        V::Native(n) => match &**n {
            Native::Type("float") => Ok(Some(Ret::Float)),
            Native::Type("datetime") => Ok(Some(Ret::Dt)),
            Native::Type(_) | Native::ExtType(_) => Ok(None),
            _ => Err(Exc::type_error("issubclass() arg 1 must be a class")),
        },
        V::Class(_) => Ok(None),
        _ => Err(Exc::type_error("issubclass() arg 1 must be a class")),
    }
}

fn is_float_type(v: &V) -> bool {
    matches!(v, V::Native(n) if matches!(&**n, Native::Type("float")))
}

fn dt_type() -> V {
    V::native(Native::Type("datetime"))
}

impl St {
    /// `set_current(start_time, force=True)`
    fn set_current(&mut self, start: &V) -> R<()> {
        if start.is_none() {
            return Ok(());
        }
        let ts = match start {
            V::DateTime(d) => {
                self.tzinfo = d.tz;
                V::Float(dt_to_ts(d)?)
            }
            V::Int(_) | V::Float(_) | V::Bool(_) => start.clone(),
            other => return Err(Exc::type_error(format!("py2axum: croniter start time of type {} is not supported", other.type_name()))),
        };
        self.start_time = ts.clone();
        self.dst_start_time = ts.clone();
        self.cur = ts;
        Ok(())
    }

    fn get_next(&mut self, ret_type: &V, start_time: &V, is_prev: Option<bool>, update_current: bool) -> R {
        self.set_current(start_time)?;
        let is_prev = is_prev.unwrap_or(self.is_prev);
        self.is_prev = is_prev;
        let rt = if ops::truthy(ret_type)? { ret_type.clone() } else { self.ret_type.clone() };
        let Some(kind) = ret_kind(&rt)? else {
            return Err(Exc::type_error("Invalid ret_type, only 'float' or 'datetime' is acceptable."));
        };
        let result = self.calc_next(is_prev)?;
        let ts = dt_to_ts(&result)?;
        if update_current {
            self.cur = V::Float(ts);
        }
        Ok(match kind {
            Ret::Dt => V::DateTime(result),
            Ret::Float => V::Float(ts),
        })
    }

    fn get_current(&self, ret_type: &V) -> R {
        let rt = if ops::truthy(ret_type)? { ret_type.clone() } else { self.ret_type.clone() };
        if matches!(ret_kind(&rt)?, Some(Ret::Dt)) {
            return Ok(V::DateTime(ts_to_dt(&self.cur, self.tzinfo)?));
        }
        Ok(self.cur.clone())
    }

    fn calc_next(&self, is_prev: bool) -> R<DateTime> {
        let current = ts_to_dt(&self.cur, self.tzinfo)?;
        let mut expanded = self.ex.expanded.clone();
        let nth = &self.ex.nth;
        if expanded[DAY][0] != It::Star && expanded[DOW][0] != It::Star && self.day_or {
            let bug = self.implement_cron_bug && (self.ex.expressions[DAY].starts_with('*') || self.ex.expressions[DOW].starts_with('*'));
            if !bug {
                let bak = expanded[DOW].clone();
                expanded[DOW] = vec![It::Star];
                let t1 = self.calc(&current, &expanded, nth, is_prev)?;
                expanded[DOW] = bak;
                expanded[DAY] = vec![It::Star];
                let t2 = self.calc(&current, &expanded, nth, is_prev)?;
                let gt = dt_cmp(&t1, &t2)? == std::cmp::Ordering::Greater;
                let lt = dt_cmp(&t1, &t2)? == std::cmp::Ordering::Less;
                return Ok(if is_prev { if gt { t1 } else { t2 } } else if lt { t1 } else { t2 });
            }
        }
        self.calc(&current, &expanded, nth, is_prev)
    }

    fn calc(&self, now: &DateTime, expanded: &[Vec<It>], nth: &[(i64, Vec<Nth>)], is_prev: bool) -> R<DateTime> {
        let long = expanded.len() > UNIX_LEN;
        let offset = if is_prev { -1 } else if long { 1_000_000 } else { 60_000_000 };
        let mut t = add_us(now.wall, offset)?;
        t = if long { t.with_nanosecond(0).unwrap() } else { t.with_second(0).unwrap().with_nanosecond(0).unwrap() };
        let (mut month, mut year) = (t.month(), t.year() as i64);
        let current_year = year;
        let next_diff = |x: i64, c: &[It], r: Option<i64>| if is_prev { prev_nearest(x, c, r) } else { next_nearest(x, c, r) };
        let star = |f: usize| expanded[f].contains(&It::Star);
        let hms = |prev: bool| if prev { (Some(23), Some(59), Some(59)) } else { (Some(0), Some(0), Some(0)) };
        let mut found = false;
        'outer: while (year - current_year).abs() <= self.max_years {
            for p in 0..7 {
                // Ok(None): stop; Ok(Some(true)): changed
                let d = t;
                let changed: Option<bool> = match p {
                    0 => {
                        if expanded.len() == YEAR_LEN && !star(YEAR) {
                            match next_diff(d.year() as i64, &expanded[YEAR], None)? {
                                None => None,
                                Some(0) => Some(false),
                                Some(diff) => {
                                    let (h, mi, s) = hms(is_prev);
                                    let (mo, da) = if is_prev { (12, 31) } else { (1, 1) };
                                    t = rd_add(d, Rd { years: diff, month: Some(mo), day: Some(da), hour: h, minute: mi, second: s, ..Rd::default() })?;
                                    Some(true)
                                }
                            }
                        } else {
                            Some(false)
                        }
                    }
                    1 => {
                        if !star(MONTH) {
                            match next_diff(d.month() as i64, &expanded[MONTH], Some(12))? {
                                Some(diff) if diff != 0 => {
                                    if is_prev {
                                        let d2 = rd_add(d, Rd { months: diff, ..Rd::default() })?;
                                        let rday = last_day(d2.year() as i64, d2.month());
                                        t = rd_add(d2, Rd { day: Some(rday), hour: Some(23), minute: Some(59), second: Some(59), ..Rd::default() })?;
                                    } else {
                                        t = rd_add(d, Rd { months: diff, day: Some(1), hour: Some(0), minute: Some(0), second: Some(0), ..Rd::default() })?;
                                    }
                                    Some(true)
                                }
                                _ => Some(false),
                            }
                        } else {
                            Some(false)
                        }
                    }
                    2 if !self.ex.nearest.is_empty() => {
                        let mut cands = Vec::new();
                        for &w in &self.ex.nearest {
                            let c = nearest_weekday(d.year() as i64, d.month(), w) as i64;
                            if (is_prev && c <= d.day() as i64) || (!is_prev && d.day() as i64 <= c) {
                                cands.push(c);
                            }
                        }
                        Some(self.day_candidates(&mut t, d, cands, is_prev, year, month)?)
                    }
                    2 => {
                        if !star(DAY) {
                            let days = last_day(year, month) as i64;
                            if expanded[DAY].contains(&It::L) && days == d.day() as i64 {
                                Some(false)
                            } else {
                                let diff = if is_prev {
                                    let pm = (month as i64 - 2).rem_euclid(12) as u32 + 1;
                                    let py = if month == 1 { year - 1 } else { year };
                                    prev_nearest(d.day() as i64, &expanded[DAY], Some(last_day(py, pm) as i64))?
                                } else {
                                    next_nearest(d.day() as i64, &expanded[DAY], Some(days))?
                                };
                                match diff {
                                    Some(diff) if diff != 0 => {
                                        let (h, mi, s) = hms(is_prev);
                                        t = rd_add(d, Rd { days: diff, hour: h, minute: mi, second: s, ..Rd::default() })?;
                                        Some(true)
                                    }
                                    _ => Some(false),
                                }
                            }
                        } else {
                            Some(false)
                        }
                    }
                    3 if !nth.is_empty() => {
                        let mut cands = Vec::new();
                        for (wday, ns) in nth {
                            let c = nth_weekdays(d.year() as i64, d.month(), *wday);
                            for n in ns {
                                let cand = match n {
                                    Nth::L => *c.last().unwrap_or(&0) as i64,
                                    Nth::N(k) if (c.len() as i64) < *k => continue,
                                    Nth::N(k) => c[(*k - 1) as usize] as i64,
                                };
                                if (is_prev && cand <= d.day() as i64) || (!is_prev && d.day() as i64 <= cand) {
                                    cands.push(cand);
                                }
                            }
                        }
                        Some(self.day_candidates(&mut t, d, cands, is_prev, year, month)?)
                    }
                    3 => {
                        if !star(DOW) {
                            let wd = d.weekday().number_from_monday() as i64 % 7;
                            match next_diff(wd, &expanded[DOW], Some(7))? {
                                Some(diff) if diff != 0 => {
                                    let (h, mi, s) = hms(is_prev);
                                    t = rd_add(d, Rd { days: diff, hour: h, minute: mi, second: s, ..Rd::default() })?;
                                    Some(true)
                                }
                                _ => Some(false),
                            }
                        } else {
                            Some(false)
                        }
                    }
                    4 => {
                        if !star(HOUR) {
                            match next_diff(d.hour() as i64, &expanded[HOUR], Some(24))? {
                                Some(diff) if diff != 0 => {
                                    let (mi, s) = if is_prev { (59, 59) } else { (0, 0) };
                                    t = rd_add(d, Rd { hours: diff, minute: Some(mi), second: Some(s), ..Rd::default() })?;
                                    Some(true)
                                }
                                _ => Some(false),
                            }
                        } else {
                            Some(false)
                        }
                    }
                    5 => {
                        if !star(MINUTE) {
                            match next_diff(d.minute() as i64, &expanded[MINUTE], Some(60))? {
                                Some(diff) if diff != 0 => {
                                    let s = if is_prev { 59 } else { 0 };
                                    t = rd_add(d, Rd { minutes: diff, second: Some(s), ..Rd::default() })?;
                                    Some(true)
                                }
                                _ => Some(false),
                            }
                        } else {
                            Some(false)
                        }
                    }
                    _ => {
                        if long {
                            if !star(SECOND) {
                                match next_diff(d.second() as i64, &expanded[SECOND], Some(60))? {
                                    Some(diff) if diff != 0 => {
                                        t = rd_add(d, Rd { seconds: diff, ..Rd::default() })?;
                                        Some(true)
                                    }
                                    _ => Some(false),
                                }
                            } else {
                                Some(false)
                            }
                        } else {
                            t = rd_add(d, Rd { second: Some(0), ..Rd::default() })?;
                            Some(false)
                        }
                    }
                };
                match changed {
                    None => break 'outer,
                    Some(true) => {
                        month = t.month();
                        year = t.year() as i64;
                        continue 'outer;
                    }
                    Some(false) => {}
                }
            }
            found = true;
            break;
        }
        if !found {
            return Err(bad_date(is_prev));
        }
        let unaware = t.with_nanosecond(0).unwrap();
        let Some(tz) = now.tz else { return Ok(DateTime::naive(unaware)) };
        let (mut aware, mut ex) = add_tzinfo(unaware, now, tz, is_prev)?;
        if !ex && (!is_successor(&aware, now, is_prev) || expanded[HOUR].contains(&It::Star)) {
            let mut un = unaware;
            while !ex {
                un = self.calc(&DateTime::naive(un), expanded, nth, is_prev)?.wall;
                (aware, ex) = add_tzinfo(un, now, tz, is_prev)?;
            }
        }
        // `_timezone_delta(now, aware_time)`: each value's own tzinfo (a pytz localization has its own)
        let delta = utcoffset(&aware.tz.unwrap_or(tz), &aware.wall, aware.fold) - utcoffset(&tz, &now.wall, now.fold);
        if delta == 0 {
            return Ok(aware);
        }
        let alt_un = add_us(now.wall, delta as i64 * 1_000_000)?;
        let alt_un = self.calc(&DateTime::naive(alt_un), expanded, nth, is_prev)?.wall;
        let (alt, _) = add_tzinfo(alt_un, now, tz, is_prev)?;
        if !is_successor(&alt, now, is_prev) {
            return Ok(aware);
        }
        if is_successor(&aware, &alt, is_prev) {
            return Ok(alt);
        }
        Ok(aware)
    }

    /// the end of `proc_day_of_week_nth` / `proc_nearest_weekday`
    fn day_candidates(&self, t: &mut Ndt, d: Ndt, mut cands: Vec<i64>, is_prev: bool, year: i64, month: u32) -> R<bool> {
        if cands.is_empty() {
            *t = if is_prev {
                rd_add(d, Rd { days: -(d.day() as i64), hour: Some(23), minute: Some(59), second: Some(59), ..Rd::default() })?
            } else {
                let days = last_day(year, month) as i64;
                rd_add(d, Rd { days: days - d.day() as i64 + 1, hour: Some(0), minute: Some(0), second: Some(0), ..Rd::default() })?
            };
            return Ok(true);
        }
        cands.sort();
        let diff = (if is_prev { *cands.last().unwrap() } else { cands[0] }) - d.day() as i64;
        if diff != 0 {
            *t = if is_prev {
                rd_add(d, Rd { days: diff, hour: Some(23), minute: Some(59), second: Some(59), ..Rd::default() })?
            } else {
                rd_add(d, Rd { days: diff, hour: Some(0), minute: Some(0), second: Some(0), ..Rd::default() })?
            };
            return Ok(true);
        }
        Ok(false)
    }
}

fn sub_err(a: &str) -> Exc {
    Exc::type_error(format!("unsupported operand type(s) for -: '{a}' and 'int'"))
}

/// `_get_next_nearest_diff`
fn next_nearest(x: i64, to_check: &[It], range_val: Option<i64>) -> R<Option<i64>> {
    for d in to_check {
        let d = match (d, range_val) {
            (It::L, Some(r)) => r,
            (It::I(v), Some(r)) if *v > r => continue,
            (It::I(v), _) => *v,
            (It::L, None) => return Err(Exc::type_error("'>=' not supported between instances of 'str' and 'int'")),
            (It::Star, _) => return Err(Exc::type_error("'>' not supported between instances of 'str' and 'int'")),
        };
        if d >= x {
            return Ok(Some(d - x));
        }
    }
    let Some(r) = range_val else { return Ok(None) };
    match to_check.first() {
        Some(It::I(v)) => Ok(Some(v - x + r)),
        Some(_) => Err(sub_err("str")),
        None => Err(Exc::msg(&INDEX_ERROR, "list index out of range")),
    }
}

/// `_get_prev_nearest_diff`
fn prev_nearest(x: i64, to_check: &[It], range_val: Option<i64>) -> R<Option<i64>> {
    let cands: Vec<It> = to_check.iter().rev().copied().collect();
    for d in &cands {
        match d {
            It::I(v) if *v <= x => return Ok(Some(v - x)),
            It::Star => return Err(Exc::type_error("'<=' not supported between instances of 'str' and 'int'")),
            _ => {}
        }
    }
    if cands.contains(&It::L) {
        return Ok(Some(-x));
    }
    let Some(r) = range_val else { return Ok(None) };
    let Some(&first) = cands.first() else { return Err(Exc::msg(&INDEX_ERROR, "list index out of range")) };
    let mut cand = first;
    for c in &cands {
        if let It::I(v) = c {
            if *v <= r {
                cand = *c;
                break;
            }
        }
    }
    let It::I(cv) = cand else { return Err(sub_err("str")) };
    if cv > r {
        return Ok(Some(-r));
    }
    Ok(Some(cv - x - r))
}

/// `_get_nth_weekday_of_month`: the days of `month` falling on the cron weekday (0 = Sunday)
fn nth_weekdays(year: i64, month: u32, dow: i64) -> Vec<u32> {
    let target = (dow + 6).rem_euclid(7) as u32; // Monday = 0
    let w1 = weekday(year, month, 1);
    let mut d = 1 + (target + 7 - w1) % 7;
    let last = last_day(year, month);
    let mut out = Vec::new();
    while d <= last {
        out.push(d);
        d += 7;
    }
    out
}

/// `calendar.weekday` (Monday = 0)
fn weekday(year: i64, month: u32, day: u32) -> u32 {
    let y = if (1..=9999).contains(&year) { year } else { 2000 + year.rem_euclid(400) };
    NaiveDate::from_ymd_opt(y as i32, month, day).map(|d| d.weekday().num_days_from_monday()).unwrap_or(0)
}

/// `_get_nearest_weekday`
fn nearest_weekday(year: i64, month: u32, day: i64) -> u32 {
    let last = last_day(year, month);
    let day = (day.min(last as i64)).max(1) as u32;
    let wd = weekday(year, month, day);
    if wd < 5 {
        return day;
    }
    if wd == 5 {
        return if day > 1 { day - 1 } else { day + 2 };
    }
    if day < last {
        day + 1
    } else {
        day - 2
    }
}

/// `_is_successor`
fn is_successor(date: &DateTime, prev: &DateTime, is_prev: bool) -> bool {
    if is_prev {
        utc_us(date) < utc_us(prev)
    } else {
        utc_us(date) > utc_us(prev)
    }
}

/// `_add_tzinfo` (zoneinfo / timezone tzinfos: PEP 495 folds)
fn add_tzinfo(date: Ndt, prev: &DateTime, tz: Tz, is_prev: bool) -> R<(DateTime, bool)> {
    // a pytz tzinfo: `localize(date, is_dst=None)`, a minute later while it does not exist, the successor of the two
    // localizations of an ambiguous time
    if matches!(tz, Tz::PyUtc | Tz::Py(..) | Tz::PyFixed(_)) {
        let pz = super::pytzrt::localize_naive;
        return match pz(&tz, date, &V::None) {
            Ok(r) => Ok((r, true)),
            Err(e) if e.isinstance(&PYTZ_NON_EXISTENT_TIME_ERROR) => {
                let mut d = date;
                loop {
                    d = add_us(d, 60_000_000)?;
                    match pz(&tz, d, &V::None) {
                        Ok(r) => return Ok((r, false)),
                        Err(e) if e.isinstance(&PYTZ_NON_EXISTENT_TIME_ERROR) => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(e) if e.isinstance(&PYTZ_AMBIGUOUS_TIME_ERROR) => {
                let closer = pz(&tz, date, &V::Bool(!is_prev))?;
                let farther = pz(&tz, date, &V::Bool(is_prev))?;
                if (utc_us(&closer) > utc_us(&farther)) != is_prev {
                    return Err(assertion());
                }
                if is_successor(&closer, prev, is_prev) {
                    Ok((closer, true))
                } else if is_successor(&farther, prev, is_prev) {
                    Ok((farther, true))
                } else {
                    Err(assertion())
                }
            }
            Err(e) => Err(e),
        };
    }
    let mut result = DateTime { wall: date, tz: Some(tz), fold: is_prev as u8 };
    if !exists(&tz, &result.wall) {
        while !exists(&tz, &result.wall) {
            result = DateTime { wall: add_us(result.wall, 60_000_000)?, tz: Some(tz), fold: 0 };
        }
        return Ok((result, false));
    }
    let farther = DateTime { wall: date, tz: Some(tz), fold: (!is_prev) as u8 };
    if utcoffset(&tz, &result.wall, result.fold) != utcoffset(&tz, &farther.wall, farther.fold) {
        if (utc_us(&result) > utc_us(&farther)) != is_prev {
            return Err(assertion());
        }
        if !is_successor(&result, prev, is_prev) {
            if !is_successor(&farther, prev, is_prev) {
                return Err(assertion());
            }
            result = farther;
        }
    }
    Ok((result, true))
}

// ---------------------------------------------------------------- Python entry points

const INIT_PARAMS: [&str; 10] = [
    "expr_format", "start_time", "ret_type", "day_or", "max_years_between_matches", "is_prev", "hash_id", "implement_cron_bug",
    "second_at_beginning", "expand_from_start_time",
];

fn now_ts() -> V {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    V::Float(d.as_secs_f64())
}

fn new_cron(a: &[Option<V>]) -> R<Arc<Cron>> {
    let ret_type = a[2].clone().unwrap_or_else(|| V::native(Native::Type("float")));
    let day_or = truthy_or(&a[3], true)?;
    let implement_cron_bug = truthy_or(&a[7], false)?;
    let second_at_beginning = truthy_or(&a[8], false)?;
    let expand_from_start_time = truthy_or(&a[9], false)?;
    let hash_id = hash_id_of(&opt(&a[6]), true)?;
    let my = opt(&a[4]);
    let explicit = !my.is_none();
    let max_years = if explicit {
        match super::methods::b_int(&[my])? {
            V::Int(i) => i.max(1),
            _ => 1,
        }
    } else {
        50
    };
    let mut start = opt(&a[1]);
    if start.is_none() {
        start = now_ts();
    }
    let is_prev = truthy_or(&a[5], false)?;
    let empty = Expanded { expanded: vec![], nth: vec![], expressions: vec![], nearest: vec![] };
    let mut st = St {
        ret_type,
        day_or,
        implement_cron_bug,
        expand_from_start_time,
        max_years_explicit: explicit,
        max_years,
        tzinfo: None,
        start_time: V::Float(0.0),
        dst_start_time: V::Float(0.0),
        cur: V::Float(0.0),
        ex: empty,
        is_prev,
        second_at_beginning,
    };
    st.set_current(&start)?;
    let from = if expand_from_start_time { Some(st.dst_start_time.clone()) } else { None };
    st.ex = expand(&opt(&a[0]), &hash_id, second_at_beginning, from.as_ref(), false, &V::None)?;
    Ok(Arc::new(Cron { st: Mutex::new(st) }))
}

/// `croniter(expr_format, start_time=None, ...)`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("croniter.__init__", args.to_vec(), kwargs.to_vec(), &INIT_PARAMS, 1)?;
    Ok(V::native(Native::Cron(new_cron(&a)?)))
}

pub fn type_name() -> &'static str {
    "croniter"
}

/// methods of a croniter object
pub fn method(c: &Arc<Cron>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let p3 = ["ret_type", "start_time", "update_current"];
    match name {
        "get_next" | "get_prev" => {
            let a = bind(&format!("croniter.{name}"), args, kwargs, &p3, 0)?;
            let mut st = c.st.lock();
            let start = opt(&a[1]);
            if name == "get_next" && ops::truthy(&start)? && st.expand_from_start_time {
                return Err(Exc::value_error("start_time is not supported when using expand_from_start_time = True."));
            }
            let upd = match &a[2] {
                None => true,
                Some(v) if v.is_none() => true,
                Some(v) => ops::truthy(v)?,
            };
            st.get_next(&opt(&a[0]), &start, Some(name == "get_prev"), upd)
        }
        "next" | "__next__" => {
            let a = bind(&format!("croniter.{name}"), args, kwargs, &["ret_type", "start_time", "is_prev", "update_current"], 0)?;
            let is_prev = match &a[2] {
                None => None,
                Some(v) if v.is_none() => None,
                Some(v) => Some(ops::truthy(v)?),
            };
            let upd = match &a[3] {
                None => true,
                Some(v) if v.is_none() => true,
                Some(v) => ops::truthy(v)?,
            };
            c.st.lock().get_next(&opt(&a[0]), &opt(&a[1]), is_prev, upd)
        }
        "get_current" => {
            let a = bind("croniter.get_current", args, kwargs, &["ret_type"], 0)?;
            c.st.lock().get_current(&opt(&a[0]))
        }
        "set_current" => {
            let a = bind("croniter.set_current", args, kwargs, &["start_time", "force"], 1)?;
            let mut st = c.st.lock();
            if truthy_or(&a[1], true)? {
                st.set_current(&opt(&a[0]))?;
            }
            Ok(st.cur.clone())
        }
        "all_next" | "all_prev" => {
            let a = bind(&format!("croniter.{name}"), args, kwargs, &p3, 0)?;
            Ok(V::native(Native::CronGen(Arc::new(Gen {
                kind: GenKind::All { cron: c.clone(), prev: name == "all_prev", ret_type: opt(&a[0]), update_current: opt(&a[2]) },
                state: Mutex::new(GState { done: false, start_time: opt(&a[1]), range: None }),
            }))))
        }
        "timestamp_to_datetime" => {
            let a = bind("croniter.timestamp_to_datetime", args, kwargs, &["timestamp", "tzinfo"], 1)?;
            let tz = match &a[1] {
                None => c.st.lock().tzinfo,
                Some(V::None) => None,
                Some(V::Tz(t)) => Some(*t),
                Some(o) => return Err(Exc::type_error(format!("py2axum: croniter tzinfo of type {} is not supported", o.type_name()))),
            };
            Ok(V::DateTime(ts_to_dt(&opt(&a[0]), tz)?))
        }
        "datetime_to_timestamp" => datetime_to_timestamp(&args, &kwargs),
        _ => Err(Exc::type_error(format!("py2axum: croniter.{name}() is not supported"))),
    }
}

/// attributes of a croniter object
pub fn attr(c: &Arc<Cron>, name: &str) -> R {
    let st = c.st.lock();
    Ok(match name {
        "cur" => st.cur.clone(),
        "start_time" => st.start_time.clone(),
        "dst_start_time" => st.dst_start_time.clone(),
        "tzinfo" => st.tzinfo.map(V::Tz).unwrap_or(V::None),
        "expanded" => expanded_value(&st.ex.expanded),
        "nth_weekday_of_month" => nth_value(&st.ex.nth)?,
        "expressions" => V::list(st.ex.expressions.iter().map(V::str).collect()),
        "second_at_beginning" => V::Bool(st.second_at_beginning),
        _ => return Err(Exc::attr_error(format!("py2axum: croniter attribute `{name}` is not supported"))),
    })
}

fn it_value(x: &It) -> V {
    match x {
        It::I(i) => V::Int(*i),
        It::Star => V::str("*"),
        It::L => V::str("l"),
    }
}

fn expanded_value(e: &[Vec<It>]) -> V {
    V::list(e.iter().map(|f| V::list(f.iter().map(it_value).collect())).collect())
}

fn nth_value(n: &[(i64, Vec<Nth>)]) -> R {
    let mut items = Vec::new();
    for (w, s) in n {
        let mut ints: Vec<i64> = s.iter().filter_map(|x| if let Nth::N(i) = x { Some(*i) } else { None }).collect();
        ints.sort();
        let mut vals: Vec<V> = ints.into_iter().map(V::Int).collect();
        if s.contains(&Nth::L) {
            vals.push(V::str("l"));
        }
        let mut set = indexmap::IndexMap::new();
        for v in vals {
            set.insert(Key::dict_key(&v)?, v);
        }
        items.push((V::Int(*w), V::Set(Arc::new(Mutex::new(set)))));
    }
    V::dict_from(items)
}

/// `next(croniter_object)`: `_get_next()` with its defaults
pub fn next(c: &Arc<Cron>) -> R {
    c.st.lock().get_next(&V::None, &V::None, None, true)
}

/// `next(generator)`: None once exhausted
pub fn gen_next(g: &Arc<Gen>) -> R<Option<V>> {
    let mut gs = g.state.lock();
    if gs.done {
        return Ok(None);
    }
    let r = gen_step(g, &mut gs);
    if !matches!(r, Ok(Some(_))) {
        gs.done = true;
    }
    r
}

fn gen_step(g: &Arc<Gen>, gs: &mut GState) -> R<Option<V>> {
    match &g.kind {
        GenKind::All { cron, prev, ret_type, update_current } => {
            let mut st = cron.st.lock();
            st.is_prev = *prev;
            let upd = if update_current.is_none() { true } else { ops::truthy(update_current)? };
            let start = std::mem::replace(&mut gs.start_time, V::None);
            match st.get_next(ret_type, &start, None, upd) {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.isinstance(&CRONITER_BAD_DATE_ERROR) && st.max_years_explicit => Ok(None),
                Err(e) => Err(e),
            }
        }
        GenKind::Range(args) => {
            if gs.range.is_none() {
                gs.range = Some(range_start(args)?);
            }
            let (cron, forward, stop, float) = gs.range.as_ref().unwrap();
            let mut st = cron.st.lock();
            let dt = match st.get_next(&V::None, &V::None, Some(!*forward), true) {
                Ok(V::DateTime(d)) => d,
                Ok(_) => return Err(Exc::type_error("py2axum: croniter_range step")),
                Err(e) if e.isinstance(&CRONITER_BAD_DATE_ERROR) => return Ok(None),
                Err(e) => return Err(e),
            };
            let o = dt_cmp(&dt, stop)?;
            let cont = if *forward { o == std::cmp::Ordering::Less } else { o == std::cmp::Ordering::Greater };
            if !cont {
                return Ok(None);
            }
            Ok(Some(if *float { st.cur.clone() } else { V::DateTime(dt) }))
        }
    }
}

/// `for x in gen` / `list(gen)`: every remaining value (all_next()/all_prev() are endless: refused)
pub fn gen_iter(g: &Arc<Gen>) -> R<Vec<V>> {
    if matches!(g.kind, GenKind::All { .. }) {
        return Err(Exc::type_error(
            "py2axum: iterating croniter's all_next()/all_prev() is not supported (endless): call next() on it",
        ));
    }
    let mut out = Vec::new();
    while let Some(v) = gen_next(g)? {
        out.push(v);
    }
    Ok(out)
}

fn class_repr(v: &V) -> String {
    let n = match v {
        V::DateTime(_) => "datetime.datetime",
        V::Date(_) => "datetime.date",
        V::Time(_) => "datetime.time",
        V::Delta(_) => "datetime.timedelta",
        other => other.type_name(),
    };
    format!("<class '{n}'>")
}

/// `croniter_range(start, stop, expr_format, ret_type=None, day_or=True, exclude_ends=False, ...)`: a generator
pub fn croniter_range(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind(
        "croniter_range",
        args.to_vec(),
        kwargs.to_vec(),
        &["start", "stop", "expr_format", "ret_type", "day_or", "exclude_ends", "_croniter", "second_at_beginning", "expand_from_start_time"],
        3,
    )?;
    if a[6].as_ref().is_some_and(|v| !v.is_none()) {
        return Err(Exc::type_error("py2axum: croniter_range(_croniter=) is not supported"));
    }
    Ok(V::native(Native::CronGen(Arc::new(Gen {
        kind: GenKind::Range(a),
        state: Mutex::new(GState { done: false, start_time: V::None, range: None }),
    }))))
}

/// the body of croniter_range up to its loop
fn range_start(a: &[Option<V>]) -> R<(Arc<Cron>, bool, DateTime, bool)> {
    let (start, stop) = (opt(&a[0]), opt(&a[1]));
    let num = |v: &V| matches!(v, V::Int(_) | V::Float(_) | V::Bool(_));
    let same = std::mem::discriminant(&start) == std::mem::discriminant(&stop)
        || (matches!(start, V::Int(_)) && matches!(stop, V::Bool(_)))
        || (matches!(start, V::Bool(_)) && matches!(stop, V::Int(_)));
    if !same {
        return Err(Exc::msg(
            &CRONITER_BAD_TYPE_RANGE_ERROR,
            format!("The start and stop must be same type.  {} != {}", class_repr(&start), class_repr(&stop)),
        ));
    }
    let mut float_auto = false;
    let (mut s, mut e) = match (&start, &stop) {
        (V::DateTime(x), V::DateTime(y)) => (*x, *y),
        _ if num(&start) && num(&stop) => {
            float_auto = true;
            (DateTime::naive(fromtimestamp_utc(&start)?), DateTime::naive(fromtimestamp_utc(&stop)?))
        }
        _ => return Err(Exc::type_error(format!("py2axum: croniter_range() of {} is not supported", start.type_name()))),
    };
    let ret_type = opt(&a[3]);
    let float = if ret_type.is_none() { float_auto } else { is_float_type(&ret_type) };
    if !truthy_or(&a[5], false)? {
        let (ds, de) = if dt_cmp(&s, &e)? == std::cmp::Ordering::Less { (-1, 1) } else { (1, -1) };
        s = DateTime { wall: add_us(s.wall, ds)?, tz: s.tz, fold: 0 };
        e = DateTime { wall: add_us(e.wall, de)?, tz: e.tz, fold: 0 };
    }
    let span = (e.wall.year() as i64 - s.wall.year() as i64).abs() + 1;
    let init = vec![
        Some(opt(&a[2])),
        Some(V::DateTime(s)),
        Some(dt_type()),
        Some(V::Bool(truthy_or(&a[4], true)?)),
        Some(V::Int(span)),
        None,
        None,
        None,
        Some(V::Bool(truthy_or(&a[7], false)?)),
        Some(V::Bool(truthy_or(&a[8], false)?)),
    ];
    let cron = new_cron(&init)?;
    let forward = dt_cmp(&s, &e)? == std::cmp::Ordering::Less;
    Ok((cron, forward, e, float))
}

/// `croniter.is_valid(expression, hash_id=None, encoding="UTF-8", second_at_beginning=False, strict=False, strict_year=None)`
pub fn is_valid(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind(
        "croniter.is_valid",
        args.to_vec(),
        kwargs.to_vec(),
        &["expression", "hash_id", "encoding", "second_at_beginning", "strict", "strict_year"],
        1,
    )?;
    let hid = opt(&a[1]);
    let hash_id = if ops::truthy(&hid)? {
        if let Some(enc) = &a[2] {
            let ok = matches!(enc, V::Str(s) if matches!(s.to_ascii_lowercase().replace(['-', '_'], "").as_str(), "utf8"));
            if !ok {
                return Err(Exc::type_error("py2axum: croniter.is_valid(encoding=) other than UTF-8 is not supported"));
            }
        }
        hash_id_of(&hid, true)?
    } else {
        hash_id_of(&hid, false)?
    };
    match expand_checked(&opt(&a[0]), &hash_id, truthy_or(&a[3], false)?, truthy_or(&a[4], false)?, &opt(&a[5])) {
        Ok(_) => Ok(V::Bool(true)),
        Err(e) if e.isinstance(&CRONITER_ERROR) => Ok(V::Bool(false)),
        Err(e) => Err(e),
    }
}

/// `croniter.expand`: a ValueError that is not a CroniterError becomes a CroniterBadCronError
fn expand_checked(expr: &V, hash_id: &HashId, sab: bool, strict: bool, strict_year: &V) -> R<Expanded> {
    expand(expr, hash_id, sab, None, strict, strict_year).map_err(|e| {
        if e.isinstance(&VALUE_ERROR) && !e.isinstance(&CRONITER_ERROR) {
            bad_cron(e.message())
        } else {
            e
        }
    })
}

/// `croniter.expand(expr_format, hash_id=None, second_at_beginning=False, from_timestamp=None, strict=False, strict_year=None)`
pub fn expand_fn(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind(
        "croniter.expand",
        args.to_vec(),
        kwargs.to_vec(),
        &["expr_format", "hash_id", "second_at_beginning", "from_timestamp", "strict", "strict_year"],
        1,
    )?;
    if a[3].as_ref().is_some_and(|v| !v.is_none()) {
        return Err(Exc::type_error("py2axum: croniter.expand(from_timestamp=) is not supported"));
    }
    let hash_id = hash_id_of(&opt(&a[1]), false)?;
    let ex = expand_checked(&opt(&a[0]), &hash_id, truthy_or(&a[2], false)?, truthy_or(&a[4], false)?, &opt(&a[5]))?;
    Ok(V::tuple(vec![expanded_value(&ex.expanded), nth_value(&ex.nth)?]))
}

/// `croniter.match(cron_expression, testdate, day_or=True, second_at_beginning=False, precision_in_seconds=None)`
pub fn match_fn(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind(
        "croniter.match",
        args.to_vec(),
        kwargs.to_vec(),
        &["cron_expression", "testdate", "day_or", "second_at_beginning", "precision_in_seconds"],
        2,
    )?;
    let d = opt(&a[1]);
    match_range_impl(&opt(&a[0]), &d, &d, &a[2], &a[3], &opt(&a[4]))
}

/// `croniter.match_range(cron_expression, from_datetime, to_datetime, day_or=True, second_at_beginning=False, precision_in_seconds=None)`
pub fn match_range(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind(
        "croniter.match_range",
        args.to_vec(),
        kwargs.to_vec(),
        &["cron_expression", "from_datetime", "to_datetime", "day_or", "second_at_beginning", "precision_in_seconds"],
        3,
    )?;
    match_range_impl(&opt(&a[0]), &opt(&a[1]), &opt(&a[2]), &a[3], &a[4], &opt(&a[5]))
}

fn match_range_impl(expr: &V, from: &V, to: &V, day_or: &Option<V>, sab: &Option<V>, precision: &V) -> R {
    let init = vec![
        Some(expr.clone()),
        Some(to.clone()),
        Some(dt_type()),
        Some(V::Bool(truthy_or(day_or, true)?)),
        None,
        None,
        None,
        None,
        Some(V::Bool(truthy_or(sab, false)?)),
        None,
    ];
    let cron = new_cron(&init)?;
    let mut st = cron.st.lock();
    let mut tdp = ts_to_dt(&st.cur, st.tzinfo)?;
    if tdp.wall.nanosecond() == 0 {
        tdp = DateTime { wall: add_us(tdp.wall, 1)?, tz: tdp.tz, fold: 0 };
    }
    st.set_current(&V::DateTime(tdp))?;
    let tdt = match st.get_next(&V::None, &V::None, Some(true), true) {
        Ok(V::DateTime(d)) => d,
        Ok(_) => unreachable!(),
        Err(e) if e.isinstance(&CRONITER_BAD_DATE_ERROR) => return Ok(V::Bool(false)),
        Err(e) => return Err(e),
    };
    let precision: f64 = match precision {
        V::None => if st.ex.expanded.len() > UNIX_LEN { 1.0 } else { 60.0 },
        V::Int(i) => *i as f64,
        V::Bool(b) => *b as i64 as f64,
        V::Float(f) => *f,
        o => return Err(Exc::type_error(format!("unsupported operand type(s) for +: 'float' and '{}'", o.type_name()))),
    };
    let duration = match (to, from) {
        (V::DateTime(t), V::DateTime(f)) => secs(dt_sub(t, f)?) + precision,
        (V::DateTime(_), o) => return Err(Exc::type_error(format!("unsupported operand type(s) for -: 'datetime.datetime' and '{}'", o.type_name()))),
        (o, _) => return Err(Exc::attr_error(format!("'{}' object has no attribute 'total_seconds'", o.type_name()))),
    };
    let (hi, lo) = if dt_cmp(&tdp, &tdt)? != std::cmp::Ordering::Less { (tdp, tdt) } else { (tdt, tdp) };
    Ok(V::Bool(secs(dt_sub(&hi, &lo)?) < duration))
}

/// `croniter.datetime_to_timestamp(d)`
pub fn datetime_to_timestamp(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("datetime_to_timestamp", args.to_vec(), kwargs.to_vec(), &["d"], 1)?;
    match opt(&a[0]) {
        V::DateTime(d) => Ok(V::Float(dt_to_ts(&d)?)),
        o => Err(Exc::attr_error(format!("'{}' object has no attribute 'tzinfo'", o.type_name()))),
    }
}
