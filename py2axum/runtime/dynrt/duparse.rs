//! python-dateutil 2.9 `dateutil.parser`: `parse()` (the default parser and parserinfo: `_timelex`, `_ymd`,
//! `parser._parse`, `_build_naive`, `_build_tzaware`), `isoparse()` / `isoparser`, `ParserError`. Ported from
//! dateutil/parser/_parser.py and isoparser.py; numbers go through `decimal` as dateutil's do (`Decimal % 1` past
//! 28 digits is DivisionImpossible there too), integers are unbounded until datetime refuses them.
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

use super::decimal::Dec;
use super::dt::{DateTime, Tz};
use super::methods::{py_space, str_class};
use super::ops;
use super::v::*;
use super::Cx;

fn isalpha(c: char) -> bool {
    str_class(c, |t| t.alpha, char::is_alphabetic)
}

fn isdigit(c: char) -> bool {
    str_class(c, |t| t.digit, char::is_numeric)
}

fn str_isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(isdigit)
}

// ------------------------------------------------------------------------------------------------ _timelex

/// `_timelex.split(s)`
fn timelex(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().filter(|c| *c != '\0').collect();
    let mut pos = 0;
    let mut charstack: Vec<char> = Vec::new();
    let mut tokenstack: std::collections::VecDeque<String> = Default::default();
    let mut out = Vec::new();
    let mut eof = false;
    loop {
        if let Some(t) = tokenstack.pop_front() {
            out.push(t);
            continue;
        }
        let mut seenletters = false;
        let mut token: Option<String> = None;
        let mut state: Option<&str> = None;
        while !eof {
            let next = if !charstack.is_empty() {
                Some(charstack.remove(0))
            } else if pos < chars.len() {
                pos += 1;
                Some(chars[pos - 1])
            } else {
                None
            };
            let Some(c) = next else {
                eof = true;
                break;
            };
            match state {
                None => {
                    token = Some(c.to_string());
                    if isalpha(c) {
                        state = Some("a");
                    } else if isdigit(c) {
                        state = Some("0");
                    } else if py_space(c) {
                        token = Some(" ".into());
                        break;
                    } else {
                        break;
                    }
                }
                Some("a") => {
                    seenletters = true;
                    let t = token.as_mut().unwrap();
                    if isalpha(c) {
                        t.push(c);
                    } else if c == '.' {
                        t.push(c);
                        state = Some("a.");
                    } else {
                        charstack.push(c);
                        break;
                    }
                }
                Some("0") => {
                    let t = token.as_mut().unwrap();
                    if isdigit(c) {
                        t.push(c);
                    } else if c == '.' || (c == ',' && t.chars().count() >= 2) {
                        t.push(c);
                        state = Some("0.");
                    } else {
                        charstack.push(c);
                        break;
                    }
                }
                Some("a.") => {
                    seenletters = true;
                    let t = token.as_mut().unwrap();
                    if c == '.' || isalpha(c) {
                        t.push(c);
                    } else if isdigit(c) && t.ends_with('.') {
                        t.push(c);
                        state = Some("0.");
                    } else {
                        charstack.push(c);
                        break;
                    }
                }
                _ => {
                    let t = token.as_mut().unwrap();
                    if c == '.' || isdigit(c) {
                        t.push(c);
                    } else if isalpha(c) && t.ends_with('.') {
                        t.push(c);
                        state = Some("a.");
                    } else {
                        charstack.push(c);
                        break;
                    }
                }
            }
        }
        let Some(mut t) = token else { break };
        if matches!(state, Some("a.") | Some("0.")) && (seenletters || t.matches('.').count() > 1 || t.ends_with('.') || t.ends_with(',')) {
            // re.split("([.,])", token)
            let mut parts = Vec::new();
            let mut cur = String::new();
            for ch in t.chars() {
                if ch == '.' || ch == ',' {
                    parts.push(std::mem::take(&mut cur));
                    parts.push(ch.to_string());
                } else {
                    cur.push(ch);
                }
            }
            parts.push(cur);
            t = parts[0].clone();
            for p in &parts[1..] {
                if !p.is_empty() {
                    tokenstack.push_back(p.clone());
                }
            }
        }
        if state == Some("0.") && !t.contains('.') {
            t = t.replace(',', ".");
        }
        out.push(t);
    }
    out
}

// ------------------------------------------------------------------------------------------------ numbers

/// the ASCII form of a string of decimal digits (any script) and dots, or None
fn ascii_digits(s: &str) -> Option<String> {
    s.chars().map(|c| if c == '.' { Some('.') } else { super::dutz::decimal_digit(c).map(|d| char::from(b'0' + d as u8)) }).collect()
}

/// `float(token)` succeeds: decimal digits with at most one dot, or a word float() reads (inf, nan)
fn floats(s: &str) -> bool {
    let low = s.to_lowercase();
    if matches!(low.as_str(), "inf" | "nan" | "infinity") {
        return true;
    }
    match ascii_digits(s) {
        Some(a) => a.matches('.').count() <= 1 && a.chars().any(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// `Decimal(token)`, finite (dateutil's `_to_decimal`): the error is a ValueError the parse turns into a failure
fn to_dec(s: &str) -> Result<Dec, Fail> {
    let a = ascii_digits(s).ok_or(Fail::Value)?;
    if a.is_empty() || a == "." {
        return Err(Fail::Value);
    }
    Dec::parse(&a).map_err(|_| Fail::Value)
}

/// `int(s)` on a string, unbounded
fn big_int(s: &str) -> Result<BigInt, Fail> {
    let t = s.trim_matches(py_space);
    let (neg, body) = match t.chars().next() {
        Some('-') => (true, &t[1..]),
        Some('+') => (false, &t[1..]),
        _ => (false, t),
    };
    let mut ds = String::new();
    let mut prev_us = true;
    for c in body.chars() {
        if c == '_' {
            if prev_us {
                return Err(Fail::Value);
            }
            prev_us = true;
            continue;
        }
        ds.push(char::from(b'0' + super::dutz::decimal_digit(c).ok_or(Fail::Value)? as u8));
        prev_us = false;
    }
    if ds.is_empty() || prev_us {
        return Err(Fail::Value);
    }
    let v: BigInt = ds.parse().map_err(|_| Fail::Value)?;
    Ok(if neg { -v } else { v })
}

/// what stops `_parse`: a ValueError or IndexError (the parse fails), anything else raised as is
enum Fail {
    Value,
    Exc(Exc),
}

impl From<Exc> for Fail {
    fn from(e: Exc) -> Self {
        if e.isinstance(&VALUE_ERROR) || e.isinstance(&INDEX_ERROR) { Fail::Value } else { Fail::Exc(e) }
    }
}

fn dmod1(d: &Dec) -> Result<Dec, Fail> {
    Ok(d.divmod(&Dec::from_i64(1)).map_err(Fail::Exc)?.1)
}

fn dmul60(d: &Dec) -> Result<Dec, Fail> {
    d.mul(&Dec::from_i64(60)).map_err(Fail::Exc)
}

fn dcmp(d: &Dec, i: i64) -> std::cmp::Ordering {
    d.cmp(&Dec::from_i64(i))
}

// ------------------------------------------------------------------------------------------------ parserinfo

const JUMP: [&str; 18] = [" ", ".", ",", ";", "-", "/", "'", "at", "on", "and", "ad", "m", "t", "of", "st", "nd", "rd", "th"];
const WEEKDAYS: [&[&str]; 7] = [&["Mon", "Monday"], &["Tue", "Tuesday"], &["Wed", "Wednesday"], &["Thu", "Thursday"], &["Fri", "Friday"], &["Sat", "Saturday"], &["Sun", "Sunday"]];
const MONTHS: [&[&str]; 12] = [&["Jan", "January"], &["Feb", "February"], &["Mar", "March"], &["Apr", "April"], &["May", "May"], &["Jun", "June"], &["Jul", "July"], &["Aug", "August"], &["Sep", "Sept", "September"], &["Oct", "October"], &["Nov", "November"], &["Dec", "December"]];
const HMS: [&[&str]; 3] = [&["h", "hour", "hours"], &["m", "minute", "minutes"], &["s", "second", "seconds"]];
const AMPM: [&[&str]; 2] = [&["am", "a"], &["pm", "p"]];
const UTCZONE: [&str; 4] = ["UTC", "GMT", "Z", "z"];

fn lookup(table: &[&[&str]], name: &str) -> Option<i64> {
    let low = name.to_lowercase();
    table.iter().position(|alts| alts.iter().any(|a| a.to_lowercase() == low)).map(|i| i as i64)
}

fn jump(name: &str) -> bool {
    let low = name.to_lowercase();
    JUMP.iter().any(|j| *j == low)
}

fn weekday(name: &str) -> Option<i64> {
    lookup(&WEEKDAYS, name)
}

fn month(name: &str) -> Option<i64> {
    lookup(&MONTHS, name).map(|m| m + 1)
}

fn hms(name: &str) -> Option<i64> {
    lookup(&HMS, name)
}

fn ampm(name: &str) -> Option<i64> {
    lookup(&AMPM, name)
}

fn pertain(name: &str) -> bool {
    name.to_lowercase() == "of"
}

fn utczone(name: &str) -> bool {
    let low = name.to_lowercase();
    UTCZONE.iter().any(|u| u.to_lowercase() == low)
}

/// `parserinfo.tzoffset(name)`: the raw name against the lowercased UTC names (only "z" matches)
fn info_tzoffset(name: &str) -> Option<BigInt> {
    if ["utc", "gmt", "z"].contains(&name) { Some(BigInt::zero()) } else { None }
}

/// the year parserinfo saw when DEFAULTPARSER was built (dateutil's import)
fn this_year() -> i64 {
    static Y: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *Y.get_or_init(|| chrono::Local::now().year() as i64)
}

fn convertyear(year: &BigInt, century_specified: bool) -> BigInt {
    let y = this_year();
    let century = y / 100 * 100;
    if *year < BigInt::from(100) && !century_specified {
        let mut v = year + century;
        if v >= BigInt::from(y + 50) {
            v -= 100;
        } else if v < BigInt::from(y - 50) {
            v += 100;
        }
        return v;
    }
    year.clone()
}

// ------------------------------------------------------------------------------------------------ _ymd

enum YVal<'a> {
    Str(&'a str),
    Int(BigInt),
    Dec(&'a Dec),
}

#[derive(Default)]
struct Ymd {
    vals: Vec<BigInt>,
    century_specified: bool,
    dstridx: Option<usize>,
    mstridx: Option<usize>,
    ystridx: Option<usize>,
}

fn monthrange_days(year: &BigInt, month: &BigInt) -> R<i64> {
    let m = match month.to_i64().filter(|m| (1..=12).contains(m)) {
        Some(m) => m,
        None => {
            let v = month.to_i64().map(V::Int).unwrap_or_else(|| V::str(month.to_string()));
            return Err(Exc::new(&CALENDAR_ILLEGAL_MONTH, vec![v]));
        }
    };
    let leap = {
        let r = |n: i64| (year % n).is_zero();
        r(4) && (!r(100) || r(400))
    };
    Ok([31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(m - 1) as usize] + (m == 2 && leap) as i64)
}

impl Ymd {
    fn could_be_day(&self, value: &Dec) -> Result<bool, Fail> {
        if self.dstridx.is_some() {
            return Ok(false);
        }
        let hi = match (self.mstridx, self.ystridx) {
            (None, _) => 31,
            (Some(m), None) => monthrange_days(&BigInt::from(2000), &self.vals[m])?,
            (Some(m), Some(y)) => monthrange_days(&self.vals[y], &self.vals[m])?,
        };
        Ok(dcmp(value, 1).is_ge() && dcmp(value, hi).is_le())
    }

    fn append(&mut self, val: YVal, label: Option<char>) -> Result<(), Fail> {
        let mut label = label;
        let v = match val {
            YVal::Str(s) => {
                if str_isdigit(s) && s.chars().count() > 2 {
                    self.century_specified = true;
                    if !matches!(label, None | Some('Y')) {
                        return Err(Fail::Value);
                    }
                    label = Some('Y');
                }
                big_int(s)?
            }
            YVal::Int(i) => {
                if i > BigInt::from(100) {
                    self.century_specified = true;
                    if !matches!(label, None | Some('Y')) {
                        return Err(Fail::Value);
                    }
                    label = Some('Y');
                }
                i
            }
            YVal::Dec(d) => {
                if dcmp(d, 100).is_gt() {
                    self.century_specified = true;
                    if !matches!(label, None | Some('Y')) {
                        return Err(Fail::Value);
                    }
                    label = Some('Y');
                }
                d.to_int()
            }
        };
        self.vals.push(v);
        let at = Some(self.vals.len() - 1);
        match label {
            Some('M') => {
                if self.mstridx.is_some() {
                    return Err(Fail::Value);
                }
                self.mstridx = at;
            }
            Some('D') => {
                if self.dstridx.is_some() {
                    return Err(Fail::Value);
                }
                self.dstridx = at;
            }
            Some('Y') => {
                if self.ystridx.is_some() {
                    return Err(Fail::Value);
                }
                self.ystridx = at;
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn resolve(&self, yearfirst: bool, dayfirst: bool) -> Result<(Option<BigInt>, Option<BigInt>, Option<BigInt>), Fail> {
        let n = self.vals.len();
        let s = &self.vals;
        let strids: Vec<(char, usize)> = [('y', self.ystridx), ('m', self.mstridx), ('d', self.dstridx)].iter().filter_map(|(k, v)| v.map(|v| (*k, v))).collect();
        if (n == strids.len() && n > 0) || (n == 3 && strids.len() == 2) {
            let mut strids = strids;
            if n == 3 && strids.len() == 2 {
                let missing: Vec<usize> = (0..3).filter(|x| !strids.iter().any(|(_, v)| v == x)).collect();
                let key: Vec<char> = ['y', 'm', 'd'].into_iter().filter(|k| !strids.iter().any(|(c, _)| c == k)).collect();
                strids.push((key[0], missing[0]));
            }
            let get = |k: char| strids.iter().find(|(c, _)| *c == k).map(|(_, i)| s[*i].clone());
            return Ok((get('y'), get('m'), get('d')));
        }
        let gt = |i: usize, v: i64| s[i] > BigInt::from(v);
        let le = |i: usize, v: i64| s[i] <= BigInt::from(v);
        let (mut year, mut month, mut day) = (None, None, None);
        let c = |i: usize| Some(s[i].clone());
        if n > 3 {
            return Err(Fail::Value);
        } else if n == 1 || (self.mstridx.is_some() && n == 2) {
            let other;
            if let Some(m) = self.mstridx {
                month = c(m);
                // self[mstridx - 1]: the last value when the month comes first
                other = s[(m as i64 - 1).rem_euclid(n as i64) as usize].clone();
            } else {
                other = s[0].clone();
            }
            if n > 1 || self.mstridx.is_none() {
                if other > BigInt::from(31) {
                    year = Some(other);
                } else {
                    day = Some(other);
                }
            }
        } else if n == 2 {
            if gt(0, 31) {
                year = c(0);
                month = c(1);
            } else if gt(1, 31) {
                month = c(0);
                year = c(1);
            } else if dayfirst && le(1, 12) {
                day = c(0);
                month = c(1);
            } else {
                month = c(0);
                day = c(1);
            }
        } else if n == 3 {
            match self.mstridx {
                Some(0) => {
                    if gt(1, 31) {
                        (month, year, day) = (c(0), c(1), c(2));
                    } else {
                        (month, day, year) = (c(0), c(1), c(2));
                    }
                }
                Some(1) => {
                    if gt(0, 31) || (yearfirst && le(2, 31)) {
                        (year, month, day) = (c(0), c(1), c(2));
                    } else {
                        (day, month, year) = (c(0), c(1), c(2));
                    }
                }
                Some(2) => {
                    if gt(1, 31) {
                        (day, year, month) = (c(0), c(1), c(2));
                    } else {
                        (year, day, month) = (c(0), c(1), c(2));
                    }
                }
                _ => {
                    if gt(0, 31) || self.ystridx == Some(0) || (yearfirst && le(1, 12) && le(2, 31)) {
                        if dayfirst && le(2, 12) {
                            (year, day, month) = (c(0), c(1), c(2));
                        } else {
                            (year, month, day) = (c(0), c(1), c(2));
                        }
                    } else if gt(0, 12) || (dayfirst && le(1, 12)) {
                        (day, month, year) = (c(0), c(1), c(2));
                    } else {
                        (month, day, year) = (c(0), c(1), c(2));
                    }
                }
            }
        }
        Ok((year, month, day))
    }
}

// ------------------------------------------------------------------------------------------------ _parse

#[derive(Default)]
struct Res {
    year: Option<BigInt>,
    month: Option<BigInt>,
    day: Option<BigInt>,
    weekday: Option<i64>,
    hour: Option<BigInt>,
    minute: Option<BigInt>,
    second: Option<BigInt>,
    microsecond: Option<BigInt>,
    tzname: Option<String>,
    tzoffset: Option<BigInt>,
    ampm: Option<i64>,
    century_specified: bool,
}

impl Res {
    fn len(&self) -> usize {
        [self.year.is_some(), self.month.is_some(), self.day.is_some(), self.weekday.is_some(), self.hour.is_some(),
         self.minute.is_some(), self.second.is_some(), self.microsecond.is_some(), self.tzname.is_some(), self.tzoffset.is_some(),
         self.ampm.is_some()].iter().filter(|x| **x).count()
    }
}

struct Opts {
    dayfirst: bool,
    yearfirst: bool,
    fuzzy: bool,
    fuzzy_with_tokens: bool,
}

fn get<'a>(l: &'a [String], i: usize) -> Result<&'a str, Fail> {
    l.get(i).map(|s| s.as_str()).ok_or(Fail::Value)
}

/// `s[a:b]` on characters
fn sl(s: &str, a: usize, b: Option<usize>) -> String {
    let cs: Vec<char> = s.chars().collect();
    let b = b.unwrap_or(cs.len()).min(cs.len());
    if a >= b { String::new() } else { cs[a..b].iter().collect() }
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

fn could_be_tzname(res: &Res, token: &str) -> bool {
    res.hour.is_some() && res.tzname.is_none() && res.tzoffset.is_none() && clen(token) <= 5
        && (token.chars().all(|c| c.is_ascii_uppercase()) || UTCZONE.contains(&token))
}

fn ampm_valid(hour: &Option<BigInt>, ampm: &Option<i64>, fuzzy: bool) -> Result<bool, Fail> {
    let mut ok = true;
    if fuzzy && ampm.is_some() {
        ok = false;
    }
    match hour {
        None => {
            if fuzzy { ok = false } else { return Err(Fail::Value) }
        }
        Some(h) if !(*h >= BigInt::zero() && *h <= BigInt::from(12)) => {
            if fuzzy { ok = false } else { return Err(Fail::Value) }
        }
        _ => {}
    }
    Ok(ok)
}

fn adjust_ampm(hour: BigInt, ampm: i64) -> BigInt {
    if hour < BigInt::from(12) && ampm == 1 {
        hour + 12
    } else if hour == BigInt::from(12) && ampm == 0 {
        BigInt::zero()
    } else {
        hour
    }
}

fn parse_min_sec(value: &Dec) -> Result<(BigInt, Option<BigInt>), Fail> {
    let minute = value.to_int();
    let rem = dmod1(value)?;
    let second = if !rem.is_zero() { Some(dmul60(&rem)?.to_int()) } else { None };
    Ok((minute, second))
}

fn parsems(value: &str) -> Result<(BigInt, BigInt), Fail> {
    if !value.contains('.') {
        return Ok((big_int(value)?, BigInt::zero()));
    }
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 2 {
        return Err(Fail::Value);
    }
    let f: String = format!("{:0<6}", parts[1]);
    let f6: String = f.chars().take(6).collect();
    Ok((big_int(parts[0])?, big_int(&f6)?))
}

fn find_hms_idx(idx: usize, tokens: &[String], allow_jump: bool) -> Option<usize> {
    let n = tokens.len();
    if idx + 1 < n && hms(&tokens[idx + 1]).is_some() {
        Some(idx + 1)
    } else if allow_jump && idx + 2 < n && tokens[idx + 1] == " " && hms(&tokens[idx + 2]).is_some() {
        Some(idx + 2)
    } else if idx > 0 && hms(&tokens[idx - 1]).is_some() {
        Some(idx - 1)
    } else if 1 < idx && idx == n - 1 && tokens[idx - 1] == " " && hms(&tokens[idx - 2]).is_some() {
        Some(idx - 2)
    } else {
        None
    }
}

fn assign_hms(res: &mut Res, value_repr: &str, h: i64) -> Result<(), Fail> {
    let value = to_dec(value_repr)?;
    match h {
        0 => {
            res.hour = Some(value.to_int());
            let rem = dmod1(&value)?;
            if !rem.is_zero() {
                res.minute = Some(dmul60(&rem)?.to_int());
            }
        }
        1 => {
            let (m, s) = parse_min_sec(&value)?;
            res.minute = Some(m);
            res.second = s;
        }
        2 => {
            let (s, us) = parsems(value_repr)?;
            res.second = Some(s);
            res.microsecond = Some(us);
        }
        _ => {}
    }
    Ok(())
}

/// `parser._parse_numeric_token`
fn parse_numeric(tokens: &[String], mut idx: usize, ymd: &mut Ymd, res: &mut Res, fuzzy: bool) -> Result<usize, Fail> {
    let value_repr = tokens[idx].clone();
    let value = to_dec(&value_repr)?;
    let len_li = clen(&value_repr);
    let len_l = tokens.len();
    if ymd.vals.len() == 3 && (len_li == 2 || len_li == 4) && res.hour.is_none()
        && (idx + 1 >= len_l || (tokens[idx + 1] != ":" && hms(&tokens[idx + 1]).is_none()))
    {
        res.hour = Some(big_int(&sl(&value_repr, 0, Some(2)))?);
        if len_li == 4 {
            res.minute = Some(big_int(&sl(&value_repr, 2, None))?);
        }
    } else if len_li == 6 || (len_li > 6 && value_repr.chars().position(|c| c == '.') == Some(6)) {
        let s = &value_repr;
        if ymd.vals.is_empty() && !s.contains('.') {
            ymd.append(YVal::Str(&sl(s, 0, Some(2))), None)?;
            ymd.append(YVal::Str(&sl(s, 2, Some(4))), None)?;
            ymd.append(YVal::Str(&sl(s, 4, None)), None)?;
        } else {
            res.hour = Some(big_int(&sl(s, 0, Some(2)))?);
            res.minute = Some(big_int(&sl(s, 2, Some(4)))?);
            let (sec, us) = parsems(&sl(s, 4, None))?;
            res.second = Some(sec);
            res.microsecond = Some(us);
        }
    } else if len_li == 8 || len_li == 12 || len_li == 14 {
        let s = &value_repr;
        ymd.append(YVal::Str(&sl(s, 0, Some(4))), Some('Y'))?;
        ymd.append(YVal::Str(&sl(s, 4, Some(6))), None)?;
        ymd.append(YVal::Str(&sl(s, 6, Some(8))), None)?;
        if len_li > 8 {
            res.hour = Some(big_int(&sl(s, 8, Some(10)))?);
            res.minute = Some(big_int(&sl(s, 10, Some(12)))?);
            if len_li > 12 {
                res.second = Some(big_int(&sl(s, 12, None))?);
            }
        }
    } else if let Some(hms_idx) = find_hms_idx(idx, tokens, true) {
        let (new_idx, h) = if hms_idx > idx {
            (hms_idx, hms(&tokens[hms_idx]))
        } else {
            (idx, hms(&tokens[hms_idx]).map(|h| h + 1))
        };
        idx = new_idx;
        if let Some(h) = h {
            assign_hms(res, &value_repr, h)?;
        }
    } else if idx + 2 < len_l && tokens[idx + 1] == ":" {
        res.hour = Some(value.to_int());
        let v2 = to_dec(&tokens[idx + 2])?;
        let (m, s) = parse_min_sec(&v2)?;
        res.minute = Some(m);
        res.second = s;
        if idx + 4 < len_l && tokens[idx + 3] == ":" {
            let (s, us) = parsems(&tokens[idx + 4])?;
            res.second = Some(s);
            res.microsecond = Some(us);
            idx += 2;
        }
        idx += 2;
    } else if idx + 1 < len_l && ["-", "/", "."].contains(&tokens[idx + 1].as_str()) {
        let sep = tokens[idx + 1].clone();
        ymd.append(YVal::Str(&value_repr), None)?;
        if idx + 2 < len_l && !jump(&tokens[idx + 2]) {
            if str_isdigit(&tokens[idx + 2]) {
                ymd.append(YVal::Str(&tokens[idx + 2]), None)?;
            } else {
                match month(&tokens[idx + 2]) {
                    Some(m) => ymd.append(YVal::Int(BigInt::from(m)), Some('M'))?,
                    None => return Err(Fail::Value),
                }
            }
            if idx + 3 < len_l && tokens[idx + 3] == sep {
                let t4 = get(tokens, idx + 4)?;
                match month(t4) {
                    Some(m) => ymd.append(YVal::Int(BigInt::from(m)), Some('M'))?,
                    None => ymd.append(YVal::Str(t4), None)?,
                }
                idx += 2;
            }
            idx += 1;
        }
        idx += 1;
    } else if idx + 1 >= len_l || jump(&tokens[idx + 1]) {
        if idx + 2 < len_l && ampm(&tokens[idx + 2]).is_some() {
            res.hour = Some(adjust_ampm(value.to_int(), ampm(&tokens[idx + 2]).unwrap()));
            idx += 1;
        } else {
            ymd.append(YVal::Dec(&value), None)?;
        }
        idx += 1;
    } else if ampm(&tokens[idx + 1]).is_some() && dcmp(&value, 0).is_ge() && dcmp(&value, 24).is_lt() {
        res.hour = Some(adjust_ampm(value.to_int(), ampm(&tokens[idx + 1]).unwrap()));
        idx += 1;
    } else if ymd.could_be_day(&value)? {
        ymd.append(YVal::Dec(&value), None)?;
    } else if !fuzzy {
        return Err(Fail::Value);
    }
    Ok(idx)
}

/// `parser._parse`: the result (None: the string did not parse) and the skipped tokens
fn parse_res(timestr: &str, o: &Opts) -> R<Option<(Res, Vec<String>)>> {
    let fuzzy = o.fuzzy || o.fuzzy_with_tokens;
    let mut res = Res::default();
    let mut l = timelex(timestr);
    let mut skipped: Vec<usize> = Vec::new();
    let mut ymd = Ymd::default();
    let len_l = l.len();
    let r: Result<(), Fail> = (|| {
        let mut i = 0;
        while i < len_l {
            let tok = l[i].clone();
            if floats(&tok) {
                i = parse_numeric(&l, i, &mut ymd, &mut res, fuzzy)?;
            } else if let Some(w) = weekday(&tok) {
                res.weekday = Some(w);
            } else if let Some(m) = month(&tok) {
                ymd.append(YVal::Int(BigInt::from(m)), Some('M'))?;
                if i + 1 < len_l {
                    if l[i + 1] == "-" || l[i + 1] == "/" {
                        let sep = l[i + 1].clone();
                        ymd.append(YVal::Str(get(&l, i + 2)?), None)?;
                        if i + 3 < len_l && l[i + 3] == sep {
                            ymd.append(YVal::Str(get(&l, i + 4)?), None)?;
                            i += 2;
                        }
                        i += 2;
                    } else if i + 4 < len_l && l[i + 1] == " " && l[i + 3] == " " && pertain(&l[i + 2]) {
                        if str_isdigit(&l[i + 4]) {
                            let v = big_int(&l[i + 4])?;
                            let y = convertyear(&v, false);
                            ymd.append(YVal::Str(&y.to_string()), Some('Y'))?;
                        }
                        i += 4;
                    }
                }
            } else if let Some(a) = ampm(&tok) {
                if ampm_valid(&res.hour, &res.ampm, fuzzy)? {
                    res.hour = Some(adjust_ampm(res.hour.clone().unwrap(), a));
                    res.ampm = Some(a);
                } else if fuzzy {
                    skipped.push(i);
                }
            } else if could_be_tzname(&res, &tok) {
                res.tzname = Some(tok.clone());
                res.tzoffset = info_tzoffset(&tok);
                if i + 1 < len_l && (l[i + 1] == "+" || l[i + 1] == "-") {
                    l[i + 1] = if l[i + 1] == "+" { "-".into() } else { "+".into() };
                    res.tzoffset = None;
                    if utczone(&tok) {
                        res.tzname = None;
                    }
                }
            } else if res.hour.is_some() && (tok == "+" || tok == "-") {
                let signal: i64 = if tok == "+" { 1 } else { -1 };
                let next = get(&l, i + 1)?.to_string();
                let len_li = clen(&next);
                let (h, m);
                if len_li == 4 {
                    h = big_int(&sl(&next, 0, Some(2)))?;
                    m = big_int(&sl(&next, 2, None))?;
                } else if i + 2 < len_l && l[i + 2] == ":" {
                    h = big_int(&next)?;
                    m = big_int(get(&l, i + 3)?)?;
                    i += 2;
                } else if len_li <= 2 {
                    h = big_int(&sl(&next, 0, Some(2)))?;
                    m = BigInt::zero();
                } else {
                    return Err(Fail::Value);
                }
                res.tzoffset = Some((h * 3600 + m * 60) * signal);
                if i + 5 < len_l && jump(&l[i + 2]) && l[i + 3] == "(" && l[i + 5] == ")" && 3 <= clen(&l[i + 4]) && {
                    let probe = Res { hour: res.hour.clone(), tzname: res.tzname.clone(), ..Default::default() };
                    could_be_tzname(&probe, &l[i + 4])
                } {
                    res.tzname = Some(l[i + 4].clone());
                    i += 4;
                }
                i += 1;
            } else if !(jump(&tok) || fuzzy) {
                return Err(Fail::Value);
            } else {
                skipped.push(i);
            }
            i += 1;
        }
        let (y, m, d) = ymd.resolve(o.yearfirst, o.dayfirst)?;
        res.century_specified = ymd.century_specified;
        res.year = y;
        res.month = m;
        res.day = d;
        Ok(())
    })();
    match r {
        Ok(()) => {}
        Err(Fail::Value) => return Ok(None),
        Err(Fail::Exc(e)) => return Err(e),
    }
    // parserinfo.validate
    if let Some(y) = &res.year {
        res.year = Some(convertyear(y, res.century_specified));
    }
    let zero = Some(BigInt::zero());
    if (res.tzoffset == zero && res.tzname.as_deref().map(|s| s.is_empty()).unwrap_or(true)) || matches!(res.tzname.as_deref(), Some("Z" | "z")) {
        res.tzname = Some("UTC".into());
        res.tzoffset = zero;
    } else if res.tzoffset != zero && res.tzname.as_deref().map(|s| !s.is_empty()).unwrap_or(false) && utczone(res.tzname.as_deref().unwrap()) {
        res.tzoffset = zero;
    }
    // _recombine_skipped
    let mut tokens: Vec<String> = Vec::new();
    let mut sorted = skipped.clone();
    sorted.sort();
    for (i, idx) in sorted.iter().enumerate() {
        if i > 0 && *idx as i64 - 1 == skipped[i - 1] as i64 {
            let last = tokens.last_mut().unwrap();
            last.push_str(&l[*idx]);
        } else {
            tokens.push(l[*idx].clone());
        }
    }
    Ok(Some((res, tokens)))
}

// ------------------------------------------------------------------------------------------------ building

fn parser_error(fmt: &str, timestr: &V) -> Exc {
    Exc::new(&DU_PARSER_ERROR, vec![V::str(fmt), timestr.clone()])
}

/// a value `default.replace(**repl)` converts to a C int: OverflowError past INT_MAX. `datetime.replace` goes through
/// Argument Clinic (`PyLong_AsInt`) from 3.13.0, PyArg_ParseTupleAndKeywords("i") before (measured on 3.12.12/13,
/// 3.13.5/6/10/16 on macOS and Debian's 3.13.5-2+deb13u5, 3.14.0, 3.15.0)
fn c_int(v: &BigInt) -> R<i64> {
    if *v <= BigInt::from(i32::MAX) && *v >= BigInt::from(i32::MIN) {
        return Ok(v.to_i64().unwrap());
    }
    let msg = if super::python() >= (3, 13) {
        "Python int too large to convert to C int"
    } else if v.to_i64().is_some() {
        "signed integer is greater than maximum"
    } else {
        "Python int too large to convert to C long"
    };
    Err(Exc::msg(&OVERFLOW_ERROR, msg))
}

/// `parser._build_naive`: the parsed fields over `default` (a ValueError comes back for the ParserError)
fn build_naive(res: &Res, default: &DateTime) -> R<DateTime> {
    let w = &default.wall;
    let mut f: [Option<BigInt>; 7] = [res.year.clone(), res.month.clone(), res.day.clone(), res.hour.clone(), res.minute.clone(), res.second.clone(), res.microsecond.clone()];
    if res.day.is_none() {
        let cyear = res.year.clone().unwrap_or(BigInt::from(w.year()));
        let cmonth = res.month.clone().unwrap_or(BigInt::from(w.month()));
        let cday = BigInt::from(w.day());
        let days = monthrange_days(&cyear, &cmonth)?;
        if cday > BigInt::from(days) {
            f[2] = Some(BigInt::from(days));
        }
    }
    let base = [w.year() as i64, w.month() as i64, w.day() as i64, w.hour() as i64, w.minute() as i64, w.second() as i64, (w.nanosecond() / 1000) as i64];
    let mut v = base;
    for (i, x) in f.iter().enumerate() {
        if let Some(x) = x {
            v[i] = c_int(x)?;
        }
    }
    let date = super::methods::ymd(v[0], v[1], v[2])?;
    for (x, hi, what) in [(v[3], 23, "hour"), (v[4], 59, "minute"), (v[5], 59, "second"), (v[6], 999_999, "microsecond")] {
        super::methods::time_field(x, hi, what)?;
    }
    let wall = date.and_hms_micro_opt(v[3] as u32, v[4] as u32, v[5] as u32, v[6] as u32).unwrap();
    let mut naive = DateTime { wall, tz: default.tz, fold: default.fold };
    if let Some(wd) = res.weekday {
        if res.day.as_ref().map(|d| d.is_zero()).unwrap_or(true) {
            // + relativedelta(weekday=wd)
            let cur = naive.wall.weekday().num_days_from_monday() as i64;
            let jump = (7 - cur + wd).rem_euclid(7);
            naive.wall = naive.wall.checked_add_signed(Duration::days(jump)).filter(|x| x.year() <= 9999)
                .ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "date value out of range"))?;
            naive.fold = 0;
        }
    }
    Ok(naive)
}

/// `_assign_tzname`: the fold whose tzname() is the parsed name
fn assign_tzname(dt: DateTime, name: &Option<String>) -> R<DateTime> {
    let cur = dt.tz.map(|t| t.tzname(&dt.wall, dt.fold)).transpose()?.flatten();
    if cur != *name {
        let n = DateTime { fold: 1, ..dt };
        if n.tz.map(|t| t.tzname(&n.wall, n.fold)).transpose()?.flatten() == *name {
            return Ok(n);
        }
    }
    Ok(dt)
}

fn tzoffset_of(name: &Option<String>, off: &BigInt) -> R<Tz> {
    let secs = off.to_i64().filter(|s| s.abs() < 86400).ok_or_else(|| {
        let td = off.to_i64().map(|s| super::dt::delta_repr(&Duration::seconds(s))).unwrap_or_else(|| off.to_string());
        let head = "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24)";
        // 3.14 adds the value
        Exc::value_error(if super::python() >= (3, 14) { format!("{head}, not {td}") } else { format!("{head}.") })
    })?;
    Ok(Tz::DuOffset(secs as i32, super::dutz::name_code(name.as_deref())))
}

/// `parser._build_tzaware`
async fn build_tzaware(cx: &Cx, naive: DateTime, res: &Res, tzinfos: &V) -> R<DateTime> {
    let callable = ops::truthy(&super::methods::b_callable(std::slice::from_ref(tzinfos))?)?;
    let in_dict = !callable && ops::truthy(tzinfos)? && match &res.tzname {
        Some(n) => ops::contains(tzinfos, &V::str(n))?,
        None => ops::contains(tzinfos, &V::None)?,
    };
    let tzname_v = res.tzname.clone().map(V::str).unwrap_or(V::None);
    if callable || in_dict {
        let tzoff_v = match &res.tzoffset {
            Some(o) => V::Int(o.to_i64().ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer beyond 64 bits"))?),
            None => V::None,
        };
        let data = if callable {
            super::methods::call_value(cx, tzinfos, vec![tzname_v.clone(), tzoff_v], vec![]).await?
        } else {
            super::methods::call_method(cx, tzinfos, "get", vec![tzname_v.clone()], vec![]).await?
        };
        let tz = match &data {
            V::None => None,
            V::Tz(t) => Some(*t),
            V::Str(s) => Some(Tz::Du(super::dutz::tzstr_new(s, false)?)),
            V::Int(i) => Some(tzoffset_of(&res.tzname, &BigInt::from(*i))?),
            V::Bool(b) => Some(tzoffset_of(&res.tzname, &BigInt::from(*b as i64))?),
            _ => return Err(Exc::type_error("Offset must be tzinfo subclass, tz string, or int offset.")),
        };
        return assign_tzname(DateTime { tz, ..naive }, &res.tzname);
    }
    let local = super::dutz::local_tznames();
    if let Some(n) = res.tzname.as_ref().filter(|n| !n.is_empty() && local.contains(n)) {
        let Ok(V::Tz(l)) = Ok::<V, Exc>(super::dutz::tzlocal()) else { unreachable!() };
        let mut aware = assign_tzname(DateTime { tz: Some(l), ..naive }, &res.tzname)?;
        let now = aware.tz.unwrap().tzname(&aware.wall, aware.fold)?;
        if now.as_deref() != Some(n.as_str()) && UTCZONE.contains(&n.as_str()) {
            aware.tz = Some(Tz::DuUtc);
        }
        return Ok(aware);
    }
    match &res.tzoffset {
        Some(o) if o.is_zero() => Ok(DateTime { tz: Some(Tz::DuUtc), ..naive }),
        Some(o) => Ok(DateTime { tz: Some(tzoffset_of(&res.tzname, o)?), ..naive }),
        // a name not understood: UnknownTimezoneWarning (not printed), the naive value
        None => Ok(naive),
    }
}

fn bool_kw(kwargs: &[(String, V)], name: &str) -> R<Option<bool>> {
    match kwargs.iter().find(|(k, _)| k == name) {
        Some((_, V::None)) | None => Ok(None),
        Some((_, v)) => Ok(Some(ops::truthy(v)?)),
    }
}

/// `dateutil.parser.parse(timestr, parserinfo=None, **kwargs)`
pub async fn parse(cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let (timestr, rest) = match (args.len(), kwargs.iter().position(|(k, _)| k == "timestr")) {
        (0, Some(i)) => (kwargs[i].1.clone(), args),
        (0, None) => return Err(Exc::type_error("parse() missing 1 required positional argument: 'timestr'")),
        _ => (args[0].clone(), args[1..].to_vec()),
    };
    if rest.iter().skip(1).count() > 0 || rest.first().map(|p| !p.is_none()).unwrap_or(false) {
        return Err(Exc::type_error("py2axum: dateutil.parser.parse() with a parserinfo is not supported"));
    }
    let known = ["timestr", "parserinfo", "default", "ignoretz", "tzinfos", "dayfirst", "yearfirst", "fuzzy", "fuzzy_with_tokens"];
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !known.contains(&k.as_str())) {
        let names = ["dayfirst", "yearfirst", "fuzzy", "fuzzy_with_tokens"];
        let hint = match super::suggestion(k, &names) {
            Some(s) if super::python() >= (3, 13) => format!(". Did you mean '{s}'?"),
            _ => String::new(),
        };
        return Err(Exc::type_error(format!("parser._parse() got an unexpected keyword argument '{k}'{hint}")));
    }
    if let Some((_, p)) = kwargs.iter().find(|(k, _)| k == "parserinfo") {
        if !p.is_none() {
            return Err(Exc::type_error("py2axum: dateutil.parser.parse() with a parserinfo is not supported"));
        }
    }
    let s = match &timestr {
        V::Str(s) => s.to_string(),
        V::Bytes(b) => String::from_utf8(b.to_vec()).map_err(|_| Exc::msg(&UNICODE_DECODE_ERROR, "'utf-8' codec can't decode bytes"))?,
        V::Native(n) if matches!(&**n, Native::StringIO(_)) => {
            return Err(Exc::type_error("py2axum: dateutil.parser.parse() of a stream is not supported"));
        }
        o => return Err(Exc::type_error(format!("Parser must be a string or character stream, not {}", o.type_name()))),
    };
    let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let default = match kw("default") {
        Some(V::DateTime(d)) => d,
        Some(V::None) | None => {
            let now = DateTime::now(None);
            DateTime::naive(now.wall.date().and_hms_opt(0, 0, 0).unwrap())
        }
        Some(o) => return Err(Exc::attr_error(format!("'{}' object has no attribute 'replace'", o.type_name()))),
    };
    let o = Opts {
        dayfirst: bool_kw(&kwargs, "dayfirst")?.unwrap_or(false),
        yearfirst: bool_kw(&kwargs, "yearfirst")?.unwrap_or(false),
        fuzzy: bool_kw(&kwargs, "fuzzy")?.unwrap_or(false),
        fuzzy_with_tokens: bool_kw(&kwargs, "fuzzy_with_tokens")?.unwrap_or(false),
    };
    let Some((res, skipped)) = parse_res(&s, &o)? else {
        return Err(parser_error("Unknown string format: %s", &timestr));
    };
    if res.len() == 0 {
        return Err(parser_error("String does not contain a date: %s", &timestr));
    }
    let mut ret = match build_naive(&res, &default) {
        Ok(d) => d,
        Err(e) if e.isinstance(&VALUE_ERROR) => {
            let e2 = parser_error(&format!("{}: %s", e.message()), &timestr);
            e2.0.attrs.lock().insert("__cause__".into(), V::Exc(e));
            return Err(e2);
        }
        Err(e) => return Err(e),
    };
    let ignoretz = match kw("ignoretz") {
        Some(v) => ops::truthy(&v)?,
        None => false,
    };
    if !ignoretz {
        ret = build_tzaware(cx, ret, &res, &kw("tzinfos").unwrap_or(V::None)).await?;
    }
    if o.fuzzy_with_tokens {
        return Ok(V::tuple(vec![V::DateTime(ret), V::tuple(skipped.into_iter().map(V::str).collect())]));
    }
    Ok(V::DateTime(ret))
}

/// `ParserError.__str__`: `args[0] % args[1:]`
pub fn parser_error_str(e: &Exc) -> Option<String> {
    if !e.isinstance(&DU_PARSER_ERROR) {
        return None;
    }
    let a = e.args();
    if a.len() >= 2 {
        if let V::Str(f) = &a[0] {
            if f.matches("%s").count() == a.len() - 1 && !f.replace("%s", "").contains('%') {
                let mut out = f.to_string();
                for x in &a[1..] {
                    out = out.replacen("%s", &ops::str_(x).ok()?, 1);
                }
                return Some(out);
            }
        }
    }
    None
}

/// `ParserError.__repr__`: each argument as `'%s' % arg`
pub fn parser_error_repr(e: &Exc) -> Option<String> {
    if !e.isinstance(&DU_PARSER_ERROR) || !std::ptr::eq(e.0.class, &DU_PARSER_ERROR) {
        return None;
    }
    let parts: Vec<String> = e.args().iter().map(|a| format!("'{}'", ops::str_(a).unwrap_or_default())).collect();
    Some(format!("ParserError({})", parts.join(", ")))
}

// ------------------------------------------------------------------------------------------------ isoparser

fn int_bytes(b: &[u8]) -> R<i64> {
    let bad = || Exc::value_error(format!("invalid literal for int() with base 10: {}", ops::repr(&V::Bytes(b.to_vec().into())).unwrap_or_default()));
    let t = std::str::from_utf8(b).map_err(|_| bad())?;
    super::dutz::py_int(t).filter(|_| t.is_ascii()).ok_or_else(bad)
}

fn bsl(b: &[u8], a: usize, e: usize) -> &[u8] {
    let e = e.min(b.len());
    if a >= e { &[] } else { &b[a..e] }
}

pub struct IsoParser {
    sep: Option<u8>,
}

fn isodate_common(s: &[u8]) -> R<([i64; 3], usize)> {
    let n = s.len();
    let mut c = [1i64, 1, 1];
    if n < 4 {
        return Err(Exc::value_error("ISO string too short"));
    }
    c[0] = int_bytes(&s[0..4])?;
    let mut pos = 4;
    if pos >= n {
        return Ok((c, pos));
    }
    let has_sep = s[pos] == b'-';
    if has_sep {
        pos += 1;
    }
    if n - pos < 2 {
        return Err(Exc::value_error("Invalid common month"));
    }
    c[1] = int_bytes(&s[pos..pos + 2])?;
    pos += 2;
    if pos >= n {
        return if has_sep { Ok((c, pos)) } else { Err(Exc::value_error("Invalid ISO format")) };
    }
    if has_sep {
        if s[pos] != b'-' {
            return Err(Exc::value_error("Invalid separator in ISO string"));
        }
        pos += 1;
    }
    if n < pos + 2 {
        return Err(Exc::value_error("Invalid common day"));
    }
    c[2] = int_bytes(&s[pos..pos + 2])?;
    Ok((c, pos + 2))
}

fn isodate_uncommon(s: &[u8]) -> R<([i64; 3], usize)> {
    if s.len() < 4 {
        return Err(Exc::value_error("ISO string too short"));
    }
    let year = int_bytes(&s[0..4])?;
    let has_sep = bsl(s, 4, 5) == b"-";
    let mut pos = 4 + has_sep as usize;
    let base: NaiveDate;
    if bsl(s, pos, pos + 1) == b"W" {
        pos += 1;
        let weekno = int_bytes(bsl(s, pos, pos + 2))?;
        pos += 2;
        let mut dayno = 1;
        if s.len() > pos {
            if (bsl(s, pos, pos + 1) == b"-") != has_sep {
                return Err(Exc::value_error("Inconsistent use of dash separator"));
            }
            pos += has_sep as usize;
            dayno = int_bytes(bsl(s, pos, pos + 1))?;
            pos += 1;
        }
        if !(0 < weekno && weekno < 54) {
            return Err(Exc::value_error(format!("Invalid week: {weekno}")));
        }
        if !(0 < dayno && dayno < 8) {
            return Err(Exc::value_error(format!("Invalid weekday: {dayno}")));
        }
        let jan4 = super::methods::ymd(year, 1, 4)?;
        let week1 = jan4 - Duration::days(jan4.weekday().number_from_monday() as i64 - 1);
        base = week1.checked_add_signed(Duration::days((weekno - 1) * 7 + (dayno - 1))).filter(|d| (1..=9999).contains(&d.year()))
            .ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "date value out of range"))?;
    } else {
        if s.len() < pos + 3 {
            return Err(Exc::value_error("Invalid ordinal day"));
        }
        let od = int_bytes(&s[pos..pos + 3])?;
        pos += 3;
        let leap = super::dutz::month_days(year, 2)? == 29;
        if od < 1 || od > 365 + leap as i64 {
            return Err(Exc::value_error(format!("Invalid ordinal day {od} for year {year}")));
        }
        base = super::methods::ymd(year, 1, 1)? + Duration::days(od - 1);
    }
    Ok(([base.year() as i64, base.month() as i64, base.day() as i64], pos))
}

fn isodate(s: &[u8]) -> R<([i64; 3], usize)> {
    match isodate_common(s) {
        Ok(r) => Ok(r),
        Err(e) if e.isinstance(&VALUE_ERROR) => isodate_uncommon(s),
        Err(e) => Err(e),
    }
}

fn iso_tzstr(s: &[u8], zero_as_utc: bool) -> R<Tz> {
    if s == b"Z" || s == b"z" {
        return Ok(Tz::DuUtc);
    }
    if ![3, 5, 6].contains(&s.len()) {
        return Err(Exc::value_error("Time zone offset must be 1, 3, 5 or 6 characters"));
    }
    let mult = match s[0] {
        b'-' => -1,
        b'+' => 1,
        _ => return Err(Exc::value_error("Time zone offset requires sign")),
    };
    let hours = int_bytes(&s[1..3])?;
    let minutes = if s.len() == 3 { 0 } else { int_bytes(&s[if s[3] == b':' { 4 } else { 3 }..])? };
    if zero_as_utc && hours == 0 && minutes == 0 {
        return Ok(Tz::DuUtc);
    }
    if minutes > 59 {
        return Err(Exc::value_error("Invalid minutes in time zone offset"));
    }
    if hours > 23 {
        return Err(Exc::value_error("Invalid hours in time zone offset"));
    }
    Ok(Tz::DuOffset((mult * (hours * 60 + minutes) * 60) as i32, 0))
}

/// `_parse_isotime`: hour, minute, second, microsecond, tzinfo
fn isotime(s: &[u8]) -> R<([i64; 4], Option<Tz>)> {
    let n = s.len();
    let mut c = [0i64; 4];
    let mut tz = None;
    let mut pos = 0;
    let mut comp: i64 = -1;
    if n < 2 {
        return Err(Exc::value_error("ISO time too short"));
    }
    let mut has_sep = false;
    while pos < n && comp < 5 {
        comp += 1;
        if b"-+Zz".contains(&s[pos]) {
            tz = Some(iso_tzstr(&s[pos..], true)?);
            pos = n;
            break;
        }
        if comp == 1 && s[pos] == b':' {
            has_sep = true;
            pos += 1;
        } else if comp == 2 && has_sep {
            if bsl(s, pos, pos + 1) != b":" {
                return Err(Exc::value_error("Inconsistent use of colon separator"));
            }
            pos += 1;
        }
        if comp < 3 {
            c[comp as usize] = int_bytes(bsl(s, pos, pos + 2))?;
            pos += 2;
        }
        if comp == 3 {
            // re.match(b'[\.,]([0-9]+)')
            let rest = bsl(s, pos, n);
            if rest.first().map(|b| *b == b'.' || *b == b',').unwrap_or(false) {
                let digits = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
                if digits == 0 {
                    continue;
                }
                let us: &[u8] = &rest[1..1 + digits.min(6)];
                c[3] = int_bytes(us)? * 10i64.pow(6 - us.len() as u32);
                pos += 1 + digits;
            } else {
                continue;
            }
        }
    }
    if pos < n {
        return Err(Exc::value_error("Unused components in ISO string"));
    }
    if c[0] == 24 && c[1..4].iter().any(|x| *x != 0) {
        return Err(Exc::value_error("Hour may only be 24 at 24:00:00.000"));
    }
    Ok((c, tz))
}

/// `_takes_ascii`: a str must be ASCII, bytes pass
fn ascii_in(v: &V) -> R<Vec<u8>> {
    match v {
        V::Str(s) if s.is_ascii() => Ok(s.as_bytes().to_vec()),
        V::Str(_) => Err(Exc::value_error("ISO-8601 strings should contain only ASCII characters")),
        V::Bytes(b) => Ok(b.to_vec()),
        o => Err(Exc::type_error(format!("object of type '{}' has no len()", o.type_name()))),
    }
}

fn datetime_of(c: &[i64; 3], t: &[i64; 4], tz: Option<Tz>) -> R<DateTime> {
    let date = super::methods::ymd(c[0], c[1], c[2])?;
    for (x, hi, what) in [(t[0], 23, "hour"), (t[1], 59, "minute"), (t[2], 59, "second"), (t[3], 999_999, "microsecond")] {
        super::methods::time_field(x, hi, what)?;
    }
    Ok(DateTime { wall: date.and_hms_micro_opt(t[0] as u32, t[1] as u32, t[2] as u32, t[3] as u32).unwrap(), tz, fold: 0 })
}

impl IsoParser {
    pub fn new(sep: &V) -> R<IsoParser> {
        let sep = match sep {
            V::None => None,
            V::Str(s) => {
                let cs: Vec<char> = s.chars().collect();
                if cs.len() != 1 || cs[0] as u32 >= 128 || cs[0].is_ascii_digit() {
                    return Err(Exc::value_error("Separator must be a single, non-numeric ASCII character"));
                }
                Some(cs[0] as u8)
            }
            o => return Err(Exc::type_error(format!("object of type '{}' has no len()", o.type_name()))),
        };
        Ok(IsoParser { sep })
    }

    pub fn isoparse(&self, v: &V) -> R {
        let s = ascii_in(v)?;
        let (c, pos) = isodate(&s)?;
        let mut t = [0i64; 4];
        let mut tz = None;
        if s.len() > pos {
            if self.sep.is_none() || s.get(pos) == self.sep.as_ref() {
                let (tt, z) = isotime(&s[pos + 1..])?;
                t = tt;
                tz = z;
            } else {
                return Err(Exc::value_error("String contains unknown ISO components"));
            }
        }
        if s.len() > pos && t[0] == 24 {
            t[0] = 0;
            let d = datetime_of(&c, &t, tz)?;
            let w = d.wall.checked_add_signed(Duration::days(1)).filter(|w| w.year() <= 9999)
                .ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "date value out of range"))?;
            return Ok(V::DateTime(DateTime { wall: w, ..d }));
        }
        Ok(V::DateTime(datetime_of(&c, &t, tz)?))
    }

    pub fn parse_isodate(&self, v: &V) -> R {
        let s = ascii_in(v)?;
        let (c, pos) = isodate(&s)?;
        if pos < s.len() {
            return Err(Exc::value_error(format!("String contains unknown ISO components: {}", ops::str_repr(&String::from_utf8_lossy(&s)))));
        }
        Ok(V::Date(super::methods::ymd(c[0], c[1], c[2])?))
    }

    pub fn parse_isotime(&self, v: &V) -> R {
        let s = ascii_in(v)?;
        let (mut c, tz) = isotime(&s)?;
        if c[0] == 24 {
            c[0] = 0;
        }
        if tz.is_some() {
            return Err(Exc::type_error("py2axum: a time with a tzinfo (parse_isotime of an offset) is not supported"));
        }
        for (x, hi, what) in [(c[0], 23, "hour"), (c[1], 59, "minute"), (c[2], 59, "second"), (c[3], 999_999, "microsecond")] {
            super::methods::time_field(x, hi, what)?;
        }
        Ok(V::Time(NaiveTime::from_hms_micro_opt(c[0] as u32, c[1] as u32, c[2] as u32, c[3] as u32).unwrap()))
    }

    pub fn parse_tzstr(&self, v: &V, zero_as_utc: bool) -> R {
        let s = ascii_in(v)?;
        Ok(V::Tz(iso_tzstr(&s, zero_as_utc)?))
    }
}

/// `dateutil.parser.isoparse(dt_str)` (the default isoparser)
pub fn isoparse(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("isoparser.isoparse()", 1, args.to_vec(), kwargs.to_vec(), &[("dt_str", super::P_POS, false)])?;
    IsoParser { sep: None }.isoparse(&s[0].clone().unwrap())
}

/// `isoparser(sep=None)`
pub fn isoparser_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("isoparser.__init__()", 1, args.to_vec(), kwargs.to_vec(), &[("sep", super::P_POS, true)])?;
    let p = IsoParser::new(&s[0].clone().unwrap_or(V::None))?;
    Ok(V::native(Native::IsoParser(p.sep)))
}

/// the methods of an isoparser object
pub fn isoparser_method(sep: Option<u8>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let p = IsoParser { sep };
    let one = |pname: &str| -> R<V> {
        let s = super::bind_params(&format!("isoparser.{name}()"), 1, args.to_vec(), kwargs.to_vec(), &[(pname, super::P_POS, false)])?;
        Ok(s[0].clone().unwrap())
    };
    match name {
        "isoparse" => p.isoparse(&one("dt_str")?),
        "parse_isodate" => p.parse_isodate(&one("datestr")?),
        "parse_isotime" => p.parse_isotime(&one("timestr")?),
        "parse_tzstr" => {
            let s = super::bind_params("isoparser.parse_tzstr()", 1, args.to_vec(), kwargs.to_vec(),
                                       &[("tzstr", super::P_POS, false), ("zero_as_utc", super::P_POS, true)])?;
            let z = match &s[1] {
                Some(v) => ops::truthy(v)?,
                None => true,
            };
            p.parse_tzstr(&s[0].clone().unwrap(), z)
        }
        _ => Err(Exc::attr_error(format!("'isoparser' object has no attribute '{name}'"))),
    }
}
