//! python-dateutil 2.9 `dateutil.tz`: tzutc, tzoffset, tzlocal, tzfile (read from the system's zoneinfo
//! directories, else from dateutil's own dateutil-zoneinfo.tar.gz, as `gettz` does), tzstr, gettz, enfold,
//! datetime_exists / datetime_ambiguous / resolve_imaginary. Ported from dateutil/tz/tz.py and _common.py.
//!
//! A tzinfo is a `Tz` (dt.rs): `Tz::DuUtc` (the `tz.UTC` singleton), `Tz::DuOffset(seconds, name code)`,
//! `Tz::DuLocal(n)` (each `tzlocal()` its own instance), `Tz::Du(i)`: a tzfile or tzstr of the registry below.
//! gettz and tzstr cache their instances as dateutil does while they are referenced: one per name, for the
//! process's lifetime (each distinct tzstr string or zone file read stays in memory).
use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use parking_lot::{Mutex, RwLock};

use super::dt::{DateTime, Tz};
use super::ops;
use super::v::*;

// ------------------------------------------------------------------------------------------------ names

static NAMES: Mutex<Option<(Vec<&'static str>, HashMap<&'static str, u32>)>> = Mutex::new(None);

/// A tzoffset's name as a code: 0 for None, up to five capitals packed (what the parser produces), else an
/// interned string (high bit)
pub fn name_code(n: Option<&str>) -> u32 {
    let Some(n) = n else { return 0 };
    if !n.is_empty() && n.len() <= 5 && n.bytes().all(|b| b.is_ascii_uppercase()) {
        return 1 + n.bytes().fold(0u32, |a, b| a * 27 + (b - b'A' + 1) as u32);
    }
    let mut g = NAMES.lock();
    let (list, map) = g.get_or_insert_with(Default::default);
    if let Some(i) = map.get(n) {
        return 0x8000_0000 | *i;
    }
    let s: &'static str = Box::leak(n.to_string().into_boxed_str());
    list.push(s);
    map.insert(s, (list.len() - 1) as u32);
    0x8000_0000 | (list.len() - 1) as u32
}

pub fn name_of(c: u32) -> Option<String> {
    if c == 0 {
        return None;
    }
    if c & 0x8000_0000 != 0 {
        return NAMES.lock().as_ref().map(|(l, _)| l[(c & 0x7fff_ffff) as usize].to_string());
    }
    let mut v = c - 1;
    let mut out = Vec::new();
    while v > 0 {
        out.push(b'A' + (v % 27) as u8 - 1);
        v /= 27;
    }
    out.reverse();
    Some(String::from_utf8(out).unwrap_or_default())
}

// ------------------------------------------------------------------------------------------------ registry

pub struct TtInfo {
    pub offset: i32,
    /// the raw `isdst` byte of the file
    pub isdst: i8,
    pub abbr: String,
    pub isstd: bool,
    pub isgmt: bool,
    /// set by the reader on the ttinfo object itself (the last transition using it wins)
    pub dstoffset: i64,
}

impl TtInfo {
    fn same(&self, o: &TtInfo) -> bool {
        self.offset == o.offset && self.isdst == o.isdst && self.abbr == o.abbr && self.isstd == o.isstd && self.isgmt == o.isgmt && self.dstoffset == o.dstoffset
    }
}

pub struct TzFile {
    pub filename: String,
    trans_list_utc: Vec<i64>,
    trans_list: Vec<i64>,
    trans_idx: Vec<usize>,
    ttinfo_list: Vec<TtInfo>,
    std: Option<usize>,
    dst: Option<usize>,
    before: Option<usize>,
}

/// `tzstr(s)` (a tzrange): offsets in seconds, the DST rules as relativedelta-like steps
pub struct TzStr {
    s: String,
    std_abbr: Option<String>,
    dst_abbr: Option<String>,
    std_offset: i64,
    dst_offset: i64,
    start: Option<Rule>,
    end: Option<Rule>,
    hasdst: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Rule {
    month: Option<i64>,
    day: Option<i64>,
    /// (weekday 0 = Monday, n)
    weekday: Option<(i64, i64)>,
    /// `leapdays` of a relativedelta built from yearday=
    leapdays: i64,
    seconds: i64,
}

pub enum DuObj {
    File(TzFile),
    Str(TzStr),
}

static REG: RwLock<Vec<&'static DuObj>> = RwLock::new(Vec::new());

fn register(o: DuObj) -> u32 {
    let mut r = REG.write();
    r.push(Box::leak(Box::new(o)));
    (r.len() - 1) as u32
}

/// `Tz::Du(i)`: the registry index in the low 20 bits, an instance number above it (each `gettz()` without a name
/// is a new tzfile in dateutil, of the same file: the data is read once, `is` tells the instances apart)
const IDX: u32 = 0xF_FFFF;

fn obj(i: u32) -> &'static DuObj {
    REG.read()[(i & IDX) as usize]
}

/// a new instance of an entry (the instance number wraps after 4096)
fn instance(i: u32) -> u32 {
    static N: Mutex<u32> = Mutex::new(0);
    let mut n = N.lock();
    *n = (*n + 1) & 0xFFF;
    (i & IDX) | (*n << 20)
}

// ------------------------------------------------------------------------------------------------ helpers

fn micros(w: &NaiveDateTime) -> i128 {
    let t = w.and_utc();
    t.timestamp() as i128 * 1_000_000 + (t.timestamp_subsec_micros() as i128)
}

/// `bisect.bisect_right(list, timestamp) - 1` with dateutil's float timestamp (compared exactly)
fn last_le(list: &[i64], us: i128) -> i64 {
    list.partition_point(|t| (*t as i128) * 1_000_000 <= us) as i64 - 1
}

fn overflow() -> Exc {
    Exc::msg(&OVERFLOW_ERROR, "date value out of range")
}

fn add(w: NaiveDateTime, s: i64) -> R<NaiveDateTime> {
    w.checked_add_signed(Duration::seconds(s)).filter(|w| (1..=9999).contains(&w.year())).ok_or_else(overflow)
}

fn addu(w: NaiveDateTime, d: Duration) -> R<NaiveDateTime> {
    w.checked_add_signed(d).filter(|w| (1..=9999).contains(&w.year())).ok_or_else(overflow)
}

// ------------------------------------------------------------------------------------------------ tzlocal

struct Local {
    std: i64,
    dst: i64,
    hasdst: bool,
    names: [String; 2],
    timezone: i64,
}

fn tm_of(secs: i64) -> Option<libc::tm> {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    ok.then_some(tm)
}

fn tm_zone(tm: &libc::tm) -> String {
    if tm.tm_zone.is_null() {
        return String::new();
    }
    let s = unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned();
    s.chars().take(9).collect()
}

/// time.timezone / altzone / daylight / tzname as CPython's time module computes them at import (January and July of
/// the current year)
fn local() -> &'static Local {
    static L: OnceLock<Local> = OnceLock::new();
    L.get_or_init(|| {
        const YEAR: i64 = (365 * 24 + 6) * 3600;
        let now = chrono::Utc::now().timestamp();
        let t = now / YEAR * YEAR;
        let (jan, jul) = (tm_of(t), tm_of(t + YEAR / 2));
        let (janoff, janname) = jan.map(|m| (-(m.tm_gmtoff as i64), tm_zone(&m))).unwrap_or((0, "UTC".into()));
        let (juloff, julname) = jul.map(|m| (-(m.tm_gmtoff as i64), tm_zone(&m))).unwrap_or((0, "UTC".into()));
        let (timezone, altzone, names) = if janoff < juloff {
            (juloff, janoff, [julname, janname])
        } else {
            (janoff, juloff, [janname, julname])
        };
        let daylight = janoff != juloff;
        let std = -timezone;
        let dst = if daylight { -altzone } else { std };
        Local { std, dst, hasdst: dst != std, names, timezone }
    })
}

/// `time.tzname`
pub fn local_tznames() -> [String; 2] {
    local().names.clone()
}

fn local_naive_is_dst(w: &NaiveDateTime) -> bool {
    let l = local();
    let us = micros(w);
    let secs = (us.div_euclid(1_000_000)) as i64 + l.timezone;
    tm_of(secs).map(|m| m.tm_isdst > 0).unwrap_or(false)
}

/// `tzlocal.is_ambiguous` (`dt - self._dst_saved` overflows within an hour of datetime.min)
fn local_is_ambiguous(w: &NaiveDateTime) -> R<bool> {
    let l = local();
    let naive = local_naive_is_dst(w);
    let back = addu(*w, -Duration::seconds(l.dst - l.std))?;
    Ok(!naive && naive != local_naive_is_dst(&back))
}

fn local_isdst_r(w: &NaiveDateTime, fold: u8) -> R<bool> {
    let l = local();
    if !l.hasdst {
        return Ok(false);
    }
    let dstval = local_naive_is_dst(w);
    if local_is_ambiguous(w)? {
        return Ok(fold == 0);
    }
    Ok(dstval)
}

fn local_isdst(w: &NaiveDateTime, fold: u8) -> bool {
    local_isdst_r(w, fold).unwrap_or(false)
}

static LOCALS: Mutex<u32> = Mutex::new(0);

/// `tzlocal()`: a new instance
pub fn tzlocal() -> V {
    let mut n = LOCALS.lock();
    *n += 1;
    V::Tz(Tz::DuLocal(*n))
}

// ------------------------------------------------------------------------------------------------ tzfile

struct Buf<'a>(&'a [u8], usize);

fn struct_err(n: usize) -> Exc {
    Exc::msg(&STRUCT_ERROR, format!("unpack requires a buffer of {n} bytes"))
}

impl<'a> Buf<'a> {
    fn read(&mut self, n: usize) -> &'a [u8] {
        let end = (self.1 + n).min(self.0.len());
        let s = &self.0[self.1..end];
        self.1 = end;
        s
    }
    fn exact(&mut self, n: usize) -> R<&'a [u8]> {
        let s = self.read(n);
        if s.len() != n {
            return Err(struct_err(n));
        }
        Ok(s)
    }
}

fn utf8(b: &[u8]) -> R<String> {
    String::from_utf8(b.to_vec()).map_err(|e| {
        let at = e.utf8_error().valid_up_to();
        let what = match e.utf8_error().error_len() {
            None => "unexpected end of data",
            Some(_) if (0x80..0xc2).contains(&b[at]) || b[at] > 0xf4 => "invalid start byte",
            Some(_) => "invalid continuation byte",
        };
        Exc::msg(&UNICODE_DECODE_ERROR, format!("'utf-8' codec can't decode byte 0x{:02x} in position {at}: {what}", b[at]))
    })
}

/// Python's `s[a:b]` on characters
fn py_slice(s: &[char], a: i64, b: i64) -> String {
    let n = s.len() as i64;
    let norm = |x: i64| if x < 0 { (x + n).max(0) } else { x.min(n) };
    let (a, b) = (norm(a), norm(b));
    if a >= b { String::new() } else { s[a as usize..b as usize].iter().collect() }
}

/// `str.find(sub, start)` on characters
fn py_find_nul(s: &[char], start: i64) -> i64 {
    let n = s.len() as i64;
    let st = if start < 0 { (start + n).max(0) } else { start };
    if st > n {
        return -1;
    }
    s[st as usize..].iter().position(|c| *c == '\0').map(|p| p as i64 + st).unwrap_or(-1)
}

/// `tzfile._read_tzfile`
fn read_tzfile(data: &[u8], filename: String) -> R<TzFile> {
    let mut f = Buf(data, 0);
    if utf8(f.read(4))? != "TZif" {
        return Err(Exc::value_error("magic not found"));
    }
    f.read(16);
    let h = f.exact(24)?;
    let c: Vec<i64> = (0..6).map(|i| i32::from_be_bytes(h[i * 4..i * 4 + 4].try_into().unwrap()) as i64).collect();
    let (ttisgmtcnt, ttisstdcnt, leapcnt, timecnt, typecnt, charcnt) = (c[0], c[1], c[2], c[3], c[4], c[5]);
    let cnt = |n: i64| n.max(0) as usize;
    let mut trans_list_utc = Vec::new();
    if timecnt > 0 {
        let b = f.exact(cnt(timecnt) * 4)?;
        trans_list_utc = b.chunks(4).map(|x| i32::from_be_bytes(x.try_into().unwrap()) as i64).collect();
    }
    let mut raw_idx: Vec<usize> = Vec::new();
    if timecnt > 0 {
        raw_idx = f.exact(cnt(timecnt))?.iter().map(|b| *b as usize).collect();
    }
    let mut ttinfo = Vec::new();
    for _ in 0..cnt(typecnt) {
        let b = f.exact(6)?;
        ttinfo.push((i32::from_be_bytes(b[..4].try_into().unwrap()), b[4] as i8, b[5] as i8));
    }
    let abbr: Vec<char> = utf8(f.read(cnt(charcnt)))?.chars().collect();
    if leapcnt > 0 {
        f.read(cnt(leapcnt) * 8);
    }
    let isstd: Vec<i8> = if ttisstdcnt > 0 { f.exact(cnt(ttisstdcnt))?.iter().map(|b| *b as i8).collect() } else { vec![] };
    let isgmt: Vec<i8> = if ttisgmtcnt > 0 { f.exact(cnt(ttisgmtcnt))?.iter().map(|b| *b as i8).collect() } else { vec![] };
    let mut list = Vec::new();
    for (i, (gmtoff, isdst, abbrind)) in ttinfo.iter().enumerate() {
        let ai = *abbrind as i64;
        list.push(TtInfo {
            offset: *gmtoff,
            isdst: *isdst,
            abbr: py_slice(&abbr, ai, py_find_nul(&abbr, ai)),
            isstd: (ttisstdcnt as usize) > i && isstd[i] != 0,
            isgmt: (ttisgmtcnt as usize) > i && isgmt[i] != 0,
            dstoffset: 0,
        });
    }
    for i in &raw_idx {
        if *i >= list.len() {
            return Err(Exc::msg(&INDEX_ERROR, "list index out of range"));
        }
    }
    let trans_idx = raw_idx;
    let (mut std, mut dst, mut before) = (None, None, None);
    if !list.is_empty() {
        if trans_list_utc.is_empty() {
            std = Some(0);
        } else {
            let mut broke = false;
            for i in (0..trans_idx.len()).rev() {
                let t = trans_idx[i];
                if std.is_none() && list[t].isdst == 0 {
                    std = Some(t);
                } else if dst.is_none() && list[t].isdst != 0 {
                    dst = Some(t);
                }
                if std.is_some() && dst.is_some() {
                    broke = true;
                    break;
                }
            }
            if !broke && dst.is_some() && std.is_none() {
                std = dst;
            }
            before = Some(list.iter().position(|t| t.isdst == 0).unwrap_or(0));
        }
    }
    let (mut lastdst, mut lastoffset, mut lastdstoffset, mut lastbaseoffset): (Option<i8>, i64, i64, Option<i64>) = (None, 0, 0, None);
    let mut trans_list = Vec::with_capacity(trans_idx.len());
    for (i, t) in trans_idx.iter().enumerate() {
        let offset = list[*t].offset as i64;
        let isdst = list[*t].isdst;
        let mut dstoffset = 0;
        if let Some(ld) = lastdst {
            if isdst != 0 {
                if ld == 0 {
                    dstoffset = offset - lastoffset;
                }
                if dstoffset == 0 && lastdstoffset != 0 {
                    dstoffset = lastdstoffset;
                }
                list[*t].dstoffset = dstoffset;
                lastdstoffset = dstoffset;
            }
        }
        let baseoffset = offset - dstoffset;
        let mut adjustment = baseoffset;
        if let Some(lb) = lastbaseoffset {
            if baseoffset != lb && Some(isdst) != lastdst {
                adjustment = lb;
            }
        }
        lastdst = Some(isdst);
        lastoffset = offset;
        lastbaseoffset = Some(baseoffset);
        trans_list.push(trans_list_utc[i] + adjustment);
    }
    Ok(TzFile { filename, trans_list_utc, trans_list, trans_idx, ttinfo_list: list, std, dst, before })
}

impl TzFile {
    fn find_last(&self, w: &NaiveDateTime, in_utc: bool) -> Option<i64> {
        if self.trans_list.is_empty() {
            return None;
        }
        Some(last_le(if in_utc { &self.trans_list_utc } else { &self.trans_list }, micros(w)))
    }
    fn get(&self, idx: Option<i64>) -> Option<&TtInfo> {
        let i = match idx {
            None => return self.std.map(|s| &self.ttinfo_list[s]),
            Some(i) if i + 1 >= self.trans_list.len() as i64 => return self.std.map(|s| &self.ttinfo_list[s]),
            Some(i) if i < 0 => return self.before.map(|s| &self.ttinfo_list[s]),
            Some(i) => i,
        };
        Some(&self.ttinfo_list[self.trans_idx[i as usize]])
    }
    fn is_ambiguous(&self, w: &NaiveDateTime, idx: Option<i64>) -> bool {
        let idx = match idx {
            Some(i) => Some(i),
            None => self.find_last(w, false),
        };
        let us = micros(w);
        let Some(i) = idx else { return false };
        if i <= 0 {
            return false;
        }
        let (Some(tti), Some(prev)) = (self.get(Some(i)), self.get(Some(i - 1))) else { return false };
        let od = (prev.offset - tti.offset) as i64;
        let tt = self.trans_list[i as usize];
        us < ((tt + od) as i128) * 1_000_000
    }
    fn resolve(&self, w: &NaiveDateTime, fold: u8) -> Option<i64> {
        let idx = self.find_last(w, false);
        match idx {
            None | Some(0) => idx,
            Some(i) => Some(i - (fold == 0 && self.is_ambiguous(w, Some(i))) as i64),
        }
    }
    fn find_ttinfo(&self, w: &NaiveDateTime, fold: u8) -> Option<&TtInfo> {
        self.get(self.resolve(w, fold))
    }
    fn offset(&self, w: &NaiveDateTime, fold: u8) -> i64 {
        if self.std.is_none() {
            return 0;
        }
        self.find_ttinfo(w, fold).map(|t| t.offset as i64).unwrap_or(0)
    }
    fn dst(&self, w: &NaiveDateTime, fold: u8) -> i64 {
        if self.dst.is_none() {
            return 0;
        }
        match self.find_ttinfo(w, fold) {
            Some(t) if t.isdst != 0 => t.dstoffset,
            _ => 0,
        }
    }
    fn tzname(&self, w: &NaiveDateTime, fold: u8) -> Option<String> {
        if self.std.is_none() {
            return None;
        }
        self.find_ttinfo(w, fold).map(|t| t.abbr.clone())
    }
    fn fromutc(&self, utc: &NaiveDateTime) -> R<(NaiveDateTime, u8)> {
        let idx = self.find_last(utc, true);
        let off = self.get(idx).map(|t| t.offset as i64).ok_or_else(|| Exc::attr_error("'NoneType' object has no attribute 'offset'"))?;
        let out = add(*utc, off)?;
        Ok((out, self.is_ambiguous(&out, idx) as u8))
    }
    fn same(&self, o: &TzFile) -> bool {
        self.trans_list == o.trans_list
            && self.trans_idx.len() == o.trans_idx.len()
            && self.trans_idx.iter().zip(&o.trans_idx).all(|(a, b)| self.ttinfo_list[*a].same(&o.ttinfo_list[*b]))
            && self.ttinfo_list.len() == o.ttinfo_list.len()
            && self.ttinfo_list.iter().zip(&o.ttinfo_list).all(|(a, b)| a.same(b))
    }
}

// ------------------------------------------------------------------------------------------------ tzstr

#[derive(Default, Clone)]
struct Attr {
    month: Option<i64>,
    week: Option<i64>,
    weekday: Option<i64>,
    yday: Option<i64>,
    jyday: Option<i64>,
    day: Option<i64>,
    time: Option<i64>,
}

#[derive(Default)]
struct TzRes {
    stdabbr: Option<String>,
    stdoffset: Option<i64>,
    dstabbr: Option<String>,
    dstoffset: Option<i64>,
    start: Attr,
    end: Attr,
    any_unused_tokens: bool,
    deprecated: bool,
}

/// `re.split(r'([,:.]|[a-zA-Z]+|[0-9]+)', s)` without the empty pieces
fn tz_split(s: &str) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    let (mut out, mut other) = (Vec::new(), String::new());
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let run = |f: fn(&char) -> bool, i: &mut usize| {
            let st = *i;
            while *i < cs.len() && f(&cs[*i]) {
                *i += 1;
            }
            cs[st..*i].iter().collect::<String>()
        };
        let tok = if matches!(c, ',' | ':' | '.') {
            i += 1;
            Some(c.to_string())
        } else if c.is_ascii_alphabetic() {
            Some(run(|c| c.is_ascii_alphabetic(), &mut i))
        } else if c.is_ascii_digit() {
            Some(run(|c| c.is_ascii_digit(), &mut i))
        } else {
            other.push(c);
            i += 1;
            None
        };
        if let Some(t) = tok {
            if !other.is_empty() {
                out.push(std::mem::take(&mut other));
            }
            out.push(t);
        }
    }
    if !other.is_empty() {
        out.push(other);
    }
    out
}

/// `int(s)` as Python reads a string (spaces, sign, underscores between digits, any decimal digit)
pub fn py_int(s: &str) -> Option<i64> {
    let t = s.trim_matches(super::methods::py_space);
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let mut v: i128 = 0;
    let mut prev_us = true;
    let mut any = false;
    for c in body.chars() {
        if c == '_' {
            if prev_us {
                return None;
            }
            prev_us = true;
            continue;
        }
        let d = decimal_digit(c)?;
        v = v * 10 + d as i128;
        if v > i64::MAX as i128 + 1 {
            return None;
        }
        prev_us = false;
        any = true;
    }
    if !any || prev_us {
        return None;
    }
    let v = if neg { -v } else { v };
    i64::try_from(v).ok()
}

/// the value of a Unicode decimal digit (category Nd), as `int()` reads it
pub fn decimal_digit(c: char) -> Option<u32> {
    if let Some(d) = c.to_digit(10) {
        return Some(d);
    }
    if c.is_ascii() || !super::methods::str_class(c, |t| t.decimal, |_| false) {
        return None;
    }
    // decimal digits come in runs of ten from 0 (U+0660..U+0669, ...)
    let mut z = c as u32;
    while z > 0 && super::methods::str_class(char::from_u32(z - 1).unwrap_or('\0'), |t| t.decimal, |_| false) {
        z -= 1;
    }
    Some((c as u32 - z) % 10)
}

fn int_tok(s: &str) -> Result<i64, ()> {
    py_int(s).ok_or(())
}

/// `_tzparser.parse`
fn parse_tz(tzstr: &str) -> Option<TzRes> {
    let mut res = TzRes::default();
    let mut l = tz_split(tzstr);
    let mut used: Vec<usize> = Vec::new();
    let r: Result<(), ()> = (|| {
        let len_l = l.len();
        let mut i = 0;
        let has = |x: &str, set: &str| x.chars().any(|c| set.contains(c));
        while i < len_l {
            let mut j = i;
            while j < len_l && !has(&l[j], "0123456789:,-+") {
                j += 1;
            }
            if j != i {
                let dst = res.stdabbr.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
                let joined = l[i..j].concat();
                if !dst {
                    res.stdabbr = Some(joined);
                } else {
                    res.dstabbr = Some(joined);
                }
                used.extend(0..j);
                i = j;
                if i < len_l && (l[i] == "+" || l[i] == "-" || l[i].chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)) {
                    let signal: i64;
                    if l[i] == "+" || l[i] == "-" {
                        signal = if l[i] == "+" { -1 } else { 1 };
                        used.push(i);
                        i += 1;
                    } else {
                        signal = -1;
                    }
                    let li = l.get(i).ok_or(())?.clone();
                    let len_li = li.chars().count();
                    let val;
                    if len_li == 4 {
                        let cs: Vec<char> = li.chars().collect();
                        val = (int_tok(&cs[..2].iter().collect::<String>())? * 3600 + int_tok(&cs[2..].iter().collect::<String>())? * 60) * signal;
                    } else if i + 1 < len_l && l[i + 1] == ":" {
                        val = (int_tok(&li)? * 3600 + int_tok(l.get(i + 2).ok_or(())?)? * 60) * signal;
                        used.push(i);
                        i += 2;
                    } else if len_li <= 2 {
                        let cs: Vec<char> = li.chars().take(2).collect();
                        val = int_tok(&cs.iter().collect::<String>())? * 3600 * signal;
                    } else {
                        return Err(());
                    }
                    if !dst {
                        res.stdoffset = Some(val);
                    } else {
                        res.dstoffset = Some(val);
                    }
                    used.push(i);
                    i += 1;
                }
                if res.dstabbr.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
                    break;
                }
            } else {
                break;
            }
        }
        if i < len_l {
            for x in l.iter_mut().skip(i) {
                if x == ";" {
                    *x = ",".into();
                }
            }
            if l[i] != "," {
                return Err(());
            }
            i += 1;
        }
        let commas = l.iter().filter(|x| *x == ",").count();
        if i >= len_l {
        } else if (8..=9).contains(&commas) && !l[i..].iter().filter(|x| *x != ",").any(|x| x.chars().any(|y| !"0123456789+-".contains(y))) {
            for which in 0..2 {
                let mut x = Attr::default();
                x.month = Some(int_tok(l.get(i).ok_or(())?)?);
                used.push(i);
                i += 2;
                let value;
                if l.get(i).ok_or(())? == "-" {
                    value = int_tok(l.get(i + 1).ok_or(())?)? * -1;
                    used.push(i);
                    i += 1;
                } else {
                    value = int_tok(l.get(i).ok_or(())?)?;
                }
                used.push(i);
                i += 2;
                if value != 0 {
                    x.week = Some(value);
                    x.weekday = Some((int_tok(l.get(i).ok_or(())?)? - 1).rem_euclid(7));
                } else {
                    x.day = Some(int_tok(l.get(i).ok_or(())?)?);
                }
                used.push(i);
                i += 2;
                x.time = Some(int_tok(l.get(i).ok_or(())?)?);
                used.push(i);
                i += 2;
                if which == 0 { res.start = x } else { res.end = x }
            }
            if i < len_l {
                let signal;
                if l[i] == "-" || l[i] == "+" {
                    signal = if l[i] == "+" { 1 } else { -1 };
                    used.push(i);
                    i += 1;
                } else {
                    signal = 1;
                }
                used.push(i);
                let v = int_tok(l.get(i).ok_or(())?)?;
                res.dstoffset = Some(res.stdoffset.ok_or(())? + v * signal);
            }
            res.deprecated = true;
        } else if commas == 2 && l[i..].iter().filter(|x| *x == "/").count() <= 2
            && !l[i..].iter().filter(|x| ![",", "/", "J", "M", ".", "-", ":"].contains(&x.as_str())).any(|x| x.chars().any(|y| !y.is_ascii_digit() && !"0123456789".contains(y)))
        {
            for which in 0..2 {
                let mut x = Attr::default();
                let g = |l: &Vec<String>, i: usize| l.get(i).cloned().ok_or(());
                if g(&l, i)? == "J" {
                    used.push(i);
                    i += 1;
                    x.jyday = Some(int_tok(&g(&l, i)?)?);
                } else if g(&l, i)? == "M" {
                    used.push(i);
                    i += 1;
                    x.month = Some(int_tok(&g(&l, i)?)?);
                    used.push(i);
                    i += 1;
                    if !["-", "."].contains(&g(&l, i)?.as_str()) {
                        return Err(());
                    }
                    used.push(i);
                    i += 1;
                    let mut w = int_tok(&g(&l, i)?)?;
                    if w == 5 {
                        w = -1;
                    }
                    x.week = Some(w);
                    used.push(i);
                    i += 1;
                    if !["-", "."].contains(&g(&l, i)?.as_str()) {
                        return Err(());
                    }
                    used.push(i);
                    i += 1;
                    x.weekday = Some((int_tok(&g(&l, i)?)? - 1).rem_euclid(7));
                } else {
                    x.yday = Some(int_tok(&g(&l, i)?)? + 1);
                }
                used.push(i);
                i += 1;
                if i < len_l && l[i] == "/" {
                    used.push(i);
                    i += 1;
                    let li = g(&l, i)?;
                    let len_li = li.chars().count();
                    if len_li == 4 {
                        let cs: Vec<char> = li.chars().collect();
                        x.time = Some(int_tok(&cs[..2].iter().collect::<String>())? * 3600 + int_tok(&cs[2..].iter().collect::<String>())? * 60);
                    } else if i + 1 < len_l && l[i + 1] == ":" {
                        let mut t = int_tok(&li)? * 3600 + int_tok(&g(&l, i + 2)?)? * 60;
                        used.push(i);
                        i += 2;
                        if i + 1 < len_l && l[i + 1] == ":" {
                            used.push(i);
                            i += 2;
                            t += int_tok(&g(&l, i)?)?;
                        }
                        x.time = Some(t);
                    } else if len_li <= 2 {
                        let cs: String = li.chars().take(2).collect();
                        x.time = Some(int_tok(&cs)? * 3600);
                    } else {
                        return Err(());
                    }
                    used.push(i);
                    i += 1;
                }
                if !(i == len_l || l.get(i).map(|x| x == ",").unwrap_or(false)) {
                    return Err(());
                }
                i += 1;
                if which == 0 { res.start = x } else { res.end = x }
            }
            if i < len_l {
                return Err(());
            }
        }
        Ok(())
    })();
    if r.is_err() {
        return None;
    }
    let unused: Vec<&String> = (0..l.len()).filter(|n| !used.contains(n)).map(|n| &l[n]).collect();
    res.any_unused_tokens = !unused.iter().all(|x| *x == "," || *x == ":");
    Some(res)
}

/// `tzstr._delta`: (month, day, weekday, yearday) as a relativedelta, its seconds
fn tz_delta(x: &Attr, isend: bool, delta: i64) -> R<Rule> {
    let mut r = Rule { month: None, day: None, weekday: None, leapdays: 0, seconds: 0 };
    let mut any = false;
    if let Some(m) = x.month {
        r.month = Some(m);
        any = true;
        if let Some(wd) = x.weekday {
            let n = x.week.unwrap_or(0);
            r.weekday = Some((wd, n));
            r.day = Some(if n > 0 { 1 } else { 31 });
        } else if let Some(d) = x.day.filter(|d| *d != 0) {
            r.day = Some(d);
        }
    } else if let Some(y) = x.yday {
        any = true;
        set_yday(&mut r, y, true)?;
    } else if let Some(y) = x.jyday {
        any = true;
        set_yday(&mut r, y, false)?;
    }
    if !any {
        if !isend {
            r.month = Some(4);
            r.day = Some(1);
            r.weekday = Some((6, 1));
        } else {
            r.month = Some(10);
            r.day = Some(31);
            r.weekday = Some((6, -1));
        }
    }
    r.seconds = x.time.unwrap_or(7200);
    if isend {
        r.seconds -= delta;
    }
    Ok(r)
}

/// relativedelta(yearday= / nlyearday=): month and day, `leapdays` = -1 past February for yearday
fn set_yday(r: &mut Rule, yday: i64, yearday: bool) -> R<()> {
    if yearday && yday > 59 {
        r.leapdays = -1;
    }
    if yday == 0 {
        return Ok(());
    }
    const IDX: [i64; 12] = [31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 366];
    for (i, ydays) in IDX.iter().enumerate() {
        if yday <= *ydays {
            r.month = Some(i as i64 + 1);
            r.day = Some(if i == 0 { yday } else { yday - IDX[i - 1] });
            return Ok(());
        }
    }
    Err(Exc::value_error(format!("invalid year day ({yday})")))
}

fn monthrange(y: i32, m: u32) -> i64 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).and_then(|d| d.pred_opt()).map(|d| d.day() as i64).unwrap_or(31)
}

fn isleap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// `calendar.monthrange(year, month)[1]`
pub fn month_days(year: i64, month: i64) -> R<i64> {
    if !(1..=12).contains(&month) {
        return Err(Exc::new(&CALENDAR_ILLEGAL_MONTH, vec![V::Int(month)]));
    }
    Ok(monthrange(year as i32, month as u32))
}

/// `datetime(year, 1, 1) + rule` (relativedelta.__add__)
fn apply(rule: &Rule, year: i32) -> R<NaiveDateTime> {
    let month = rule.month.filter(|m| *m != 0).unwrap_or(1);
    let day = rule.day.filter(|d| *d != 0).unwrap_or(1).min(month_days(year as i64, month)?);
    let base = super::methods::ymd(year as i64, month, day)?.and_hms_opt(0, 0, 0).unwrap();
    let mut days = 0;
    if rule.leapdays != 0 && month > 2 && isleap(year) {
        days += rule.leapdays;
    }
    let mut ret = addu(base, Duration::days(days) + Duration::seconds(rule.seconds))?;
    if let Some((wd, n)) = rule.weekday {
        let nth = if n == 0 { 1 } else { n };
        let mut jump = (nth.abs() - 1) * 7;
        let cur = ret.weekday().num_days_from_monday() as i64;
        if nth > 0 {
            jump += (7 - cur + wd).rem_euclid(7);
        } else {
            jump += (cur - wd).rem_euclid(7);
            jump = -jump;
        }
        ret = addu(ret, Duration::days(jump))?;
    }
    Ok(ret)
}

/// `tzstr(s, posix_offset=False)`
pub fn tzstr_new(s: &str, posix_offset: bool) -> R<u32> {
    static CACHE: Mutex<Option<HashMap<(String, bool), u32>>> = Mutex::new(None);
    if let Some(i) = CACHE.lock().as_ref().and_then(|c| c.get(&(s.to_string(), posix_offset)).copied()) {
        return Ok(i);
    }
    let res = match parse_tz(s) {
        Some(r) if !r.any_unused_tokens => r,
        _ => return Err(Exc::value_error("unknown string format")),
    };
    // res.deprecated: dateutil's DeprecatedTzFormatWarning, not printed (the binary prints no library warning)
    let mut stdoffset = res.stdoffset;
    if matches!(res.stdabbr.as_deref(), Some("GMT" | "UTC")) && !posix_offset {
        // `res.stdoffset *= -1`, None without an offset
        stdoffset = Some(-stdoffset.ok_or_else(|| Exc::type_error("unsupported operand type(s) for *=: 'NoneType' and 'int'"))?);
    }
    let std_offset = stdoffset.unwrap_or(0);
    let dstabbr = res.dstabbr.clone().filter(|d| !d.is_empty());
    let dst_offset = match res.dstoffset {
        Some(d) => d,
        None if dstabbr.is_some() && stdoffset.is_some() => std_offset + 3600,
        None => 0,
    };
    let (mut start, mut end) = (None, None);
    if dstabbr.is_some() {
        start = Some(tz_delta(&res.start, false, 0)?);
        end = Some(tz_delta(&res.end, true, dst_offset - std_offset)?);
    }
    let hasdst = start.is_some();
    // rules dateutil builds but cannot apply (each utcoffset() raises): refused rather than raising elsewhere
    for r in start.iter().chain(end.iter()) {
        if r.month.map(|m| !(0..=12).contains(&m)).unwrap_or(false) || r.day.map(|d| d < 0).unwrap_or(false) {
            return Err(Exc::type_error(format!("py2axum: tzstr({}) has a DST rule with a month outside 1..12 or a negative day, \
                                                which dateutil applies only to raise: not supported", ops::str_repr(s))));
        }
    }
    if std_offset.abs() >= 86400 || dst_offset.abs() >= 86400 {
        return Err(Exc::type_error(format!("py2axum: tzstr({}) has a UTC offset of a day or more, which datetime refuses: not supported", ops::str_repr(s))));
    }
    let o = TzStr { s: s.to_string(), std_abbr: res.stdabbr.clone(), dst_abbr: res.dstabbr.clone(), std_offset, dst_offset, start, end, hasdst };
    let i = register(DuObj::Str(o));
    CACHE.lock().get_or_insert_with(Default::default).insert((s.to_string(), posix_offset), i);
    Ok(i)
}

impl TzStr {
    fn base(&self) -> i64 {
        self.dst_offset - self.std_offset
    }
    fn transitions(&self, year: i32) -> R<Option<(NaiveDateTime, NaiveDateTime)>> {
        if !self.hasdst {
            return Ok(None);
        }
        Ok(Some((apply(self.start.as_ref().unwrap(), year)?, apply(self.end.as_ref().unwrap(), year)?)))
    }
    fn naive_isdst(w: &NaiveDateTime, (on, off): (NaiveDateTime, NaiveDateTime)) -> bool {
        if on < off { on <= *w && *w < off } else { !(off <= *w && *w < on) }
    }
    fn is_ambiguous(&self, w: &NaiveDateTime) -> R<bool> {
        if !self.hasdst {
            return Ok(false);
        }
        let (_, end) = self.transitions(w.year())?.unwrap();
        Ok(end <= *w && *w < addu(end, Duration::seconds(self.base()))?)
    }
    fn isdst(&self, w: &NaiveDateTime, fold: u8) -> R<bool> {
        if !self.hasdst {
            return Ok(false);
        }
        let Some(t) = self.transitions(w.year())? else { return Ok(false) };
        let isdst = Self::naive_isdst(w, t);
        if !isdst && self.is_ambiguous(w)? {
            return Ok(fold == 0);
        }
        Ok(isdst)
    }
    fn fromutc(&self, utc: &NaiveDateTime) -> R<(NaiveDateTime, u8)> {
        let Some((on, off)) = self.transitions(utc.year())? else {
            return Ok((add(*utc, self.std_offset)?, 0));
        };
        let t = (addu(on, -Duration::seconds(self.std_offset))?, addu(off, -Duration::seconds(self.std_offset))?);
        let isdst = Self::naive_isdst(utc, t);
        let wall = add(*utc, if isdst { self.dst_offset } else { self.std_offset })?;
        let fold = (!isdst && self.is_ambiguous(&wall)?) as u8;
        Ok((wall, fold))
    }
    fn same(&self, o: &TzStr) -> bool {
        self.std_abbr == o.std_abbr && self.dst_abbr == o.dst_abbr && self.std_offset == o.std_offset && self.dst_offset == o.dst_offset
            && self.start == o.start && self.end == o.end
    }
}

// ------------------------------------------------------------------------------------------------ gettz

const TZPATHS: [&str; 4] = ["/usr/share/zoneinfo", "/usr/lib/zoneinfo", "/usr/share/lib/zoneinfo", "/etc/zoneinfo"];
const TZFILES: [&str; 2] = ["/etc/localtime", "localtime"];

fn isfile(p: &str) -> bool {
    !p.contains('\0') && std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `tzfile(path)`: reads the file (OSError as CPython's open); one registry entry per path and contents
fn tzfile_path(p: &str) -> R<u32> {
    static FILES: Mutex<Option<HashMap<(String, Vec<u8>), u32>>> = Mutex::new(None);
    let data = std::fs::read(p).map_err(|e| super::pathio::os_err(e, p))?;
    let key = (p.to_string(), data);
    if let Some(i) = FILES.lock().as_ref().and_then(|m| m.get(&key).copied()) {
        return Ok(i);
    }
    let i = register(DuObj::File(read_tzfile(&key.1, p.to_string())?));
    FILES.lock().get_or_insert_with(Default::default).insert(key, i);
    Ok(i)
}

static ZONEFILE: &[u8] = include_bytes!("dateutil-zoneinfo.tgz");

/// dateutil.zoneinfo's ZoneInfoFile: the tarball's zones (links share their target's tzfile)
fn zonefile() -> &'static HashMap<String, u32> {
    static Z: OnceLock<HashMap<String, u32>> = OnceLock::new();
    Z.get_or_init(|| {
        use std::io::Read;
        let mut raw = Vec::new();
        let _ = flate2::read::GzDecoder::new(ZONEFILE).read_to_end(&mut raw);
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut links: Vec<(String, String)> = Vec::new();
        let mut at = 0;
        let field = |b: &[u8]| String::from_utf8_lossy(b.split(|c| *c == 0).next().unwrap_or(b)).into_owned();
        while at + 512 <= raw.len() {
            let h = &raw[at..at + 512];
            if h.iter().all(|c| *c == 0) {
                break;
            }
            let mut name = field(&h[..100]);
            let prefix = field(&h[345..500]);
            if &h[257..262] == b"ustar" && !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
            let size = usize::from_str_radix(field(&h[124..136]).trim(), 8).unwrap_or(0);
            let kind = h[156];
            let body = raw.get(at + 512..at + 512 + size).unwrap_or(&[]).to_vec();
            match kind {
                b'0' | 0 => files.push((name, body)),
                b'1' | b'2' => links.push((name, field(&h[157..257]))),
                _ => {}
            }
            at += 512 + size.div_ceil(512) * 512;
        }
        let mut zones = HashMap::new();
        for (name, body) in files {
            if name == "METADATA" {
                continue;
            }
            if let Ok(f) = read_tzfile(&body, name.clone()) {
                zones.insert(name, register(DuObj::File(f)));
            }
        }
        for (name, target) in links {
            if let Some(i) = zones.get(&target).copied() {
                zones.insert(name, i);
            }
        }
        zones
    })
}

/// `gettz.nocache(name)`
fn gettz_nocache(name: Option<String>) -> R<V> {
    let mut name = name;
    if name.as_deref().map(|n| n.is_empty()).unwrap_or(true) {
        if let Ok(t) = std::env::var("TZ") {
            name = Some(t);
        }
    }
    match name.as_deref() {
        None | Some("") | Some(":") => {
            for fp in TZFILES {
                let path = if fp.starts_with('/') {
                    fp.to_string()
                } else {
                    match TZPATHS.iter().map(|p| join(p, fp)).find(|p| isfile(p)) {
                        Some(p) => p,
                        None => continue,
                    }
                };
                if isfile(&path) {
                    match tzfile_path(&path) {
                        Ok(i) => return Ok(V::Tz(Tz::Du(instance(i)))),
                        Err(e) if e.isinstance(&OS_ERROR) || e.isinstance(&VALUE_ERROR) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(tzlocal())
        }
        Some(n) => {
            let n = n.strip_prefix(':').unwrap_or(n).to_string();
            if n.starts_with('/') {
                return Ok(if isfile(&n) { V::Tz(Tz::Du(instance(tzfile_path(&n)?))) } else { V::None });
            }
            for p in TZPATHS {
                let mut fp = join(p, &n);
                if !isfile(&fp) {
                    fp = fp.replace(' ', "_");
                    if !isfile(&fp) {
                        continue;
                    }
                }
                match tzfile_path(&fp) {
                    Ok(i) => return Ok(V::Tz(Tz::Du(instance(i)))),
                    Err(e) if e.isinstance(&OS_ERROR) || e.isinstance(&VALUE_ERROR) => {}
                    Err(e) => return Err(e),
                }
            }
            if let Some(i) = zonefile().get(&n) {
                return Ok(V::Tz(Tz::Du(*i)));
            }
            if n.chars().any(|c| c.is_ascii_digit()) {
                return Ok(match tzstr_new(&n, false) {
                    Ok(i) => V::Tz(Tz::Du(i)),
                    Err(e) if e.isinstance(&VALUE_ERROR) => V::None,
                    Err(e) => return Err(e),
                });
            }
            if n == "GMT" || n == "UTC" {
                return Ok(V::Tz(Tz::DuUtc));
            }
            if local().names.iter().any(|x| *x == n) {
                return Ok(tzlocal());
            }
            Ok(V::None)
        }
    }
}

/// `tz.gettz(name=None)`
pub fn gettz(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("__call__()", 1, args.to_vec(), kwargs.to_vec(), &[("name", super::P_POS, true)])?;
    let name = match s[0].clone().unwrap_or(V::None) {
        V::None => None,
        V::Str(s) => Some(s.to_string()),
        V::Bytes(_) => return Err(Exc::type_error("gettz argument should be str, not bytes")),
        o => {
            if !ops::truthy(&o)? && std::env::var("TZ").is_ok() {
                Some(std::env::var("TZ").unwrap_or_default())
            } else {
                return Err(Exc::attr_error(format!("'{}' object has no attribute 'startswith'", o.type_name())));
            }
        }
    };
    static CACHE: Mutex<Option<HashMap<Option<String>, V>>> = Mutex::new(None);
    if let Some(v) = CACHE.lock().as_ref().and_then(|c| c.get(&name).cloned()) {
        return Ok(v);
    }
    let rv = gettz_nocache(name.clone())?;
    if name.is_some() && !matches!(rv, V::None | V::Tz(Tz::DuLocal(_))) {
        CACHE.lock().get_or_insert_with(Default::default).insert(name, rv.clone());
    }
    Ok(rv)
}

// ------------------------------------------------------------------------------------------------ constructors

/// `tzoffset(name, offset)`
pub fn tzoffset(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("__call__()", 1, args.to_vec(), kwargs.to_vec(), &[("name", super::P_POS, false), ("offset", super::P_POS, false)])?;
    let name = match s[0].clone().unwrap() {
        V::None => None,
        V::Str(x) => Some(x.to_string()),
        o => return Err(Exc::type_error(format!("py2axum: tzoffset() with a {} name is not supported (a str or None)", o.type_name()))),
    };
    let secs = match s[1].clone().unwrap() {
        V::Delta(d) => {
            let us = super::dt::micros(&d);
            if us % 1_000_000 != 0 {
                return Err(Exc::type_error("py2axum: tzoffset() with a fraction of a second is not supported"));
            }
            us / 1_000_000
        }
        V::Int(i) => i,
        V::Bool(b) => b as i64,
        V::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => f as i64,
        V::Float(_) => return Err(Exc::type_error("py2axum: tzoffset() with a fraction of a second is not supported")),
        o => return Err(Exc::type_error(format!("unsupported type for timedelta seconds component: {}", o.type_name()))),
    };
    if secs.abs() >= 86400 {
        return Err(Exc::type_error("py2axum: tzoffset() of a day or more is not supported (datetime refuses it as a UTC offset)"));
    }
    Ok(V::Tz(Tz::DuOffset(secs as i32, name_code(name.as_deref()))))
}

pub fn tzstr(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("__call__()", 1, args.to_vec(), kwargs.to_vec(), &[("s", super::P_POS, false), ("posix_offset", super::P_POS, true)])?;
    let V::Str(x) = s[0].clone().unwrap() else {
        return Err(Exc::type_error("py2axum: tzstr() of a non-str value is not supported"));
    };
    let posix = match &s[1] {
        Some(v) => ops::truthy(v)?,
        None => false,
    };
    Ok(V::Tz(Tz::Du(tzstr_new(&x, posix)?)))
}

// ------------------------------------------------------------------------------------------------ tzinfo protocol

/// `tz.utcoffset(dt)` on a wall time (seconds)
pub fn offset(t: &Tz, w: &NaiveDateTime, fold: u8) -> i32 {
    (match t {
        Tz::DuUtc => 0,
        Tz::DuOffset(s, _) => *s as i64,
        Tz::DuLocal(_) => {
            let l = local();
            if local_isdst(w, fold) { l.dst } else { l.std }
        }
        Tz::Du(i) => match obj(*i) {
            DuObj::File(f) => f.offset(w, fold),
            DuObj::Str(s) => match s.isdst(w, fold) {
                Ok(true) => s.dst_offset,
                _ => s.std_offset,
            },
        },
        _ => unreachable!(),
    }) as i32
}

/// `tz.dst(dt)` (seconds)
pub fn dst(t: &Tz, w: &NaiveDateTime, fold: u8) -> R<i64> {
    Ok(match t {
        Tz::DuUtc | Tz::DuOffset(..) => 0,
        Tz::DuLocal(_) => {
            let l = local();
            if local_isdst_r(w, fold)? { l.dst - l.std } else { 0 }
        }
        Tz::Du(i) => match obj(*i) {
            DuObj::File(f) => f.dst(w, fold),
            DuObj::Str(s) => if s.isdst(w, fold)? { s.base() } else { 0 },
        },
        _ => unreachable!(),
    })
}

pub fn tzname(t: &Tz, w: &NaiveDateTime, fold: u8) -> R<Option<String>> {
    Ok(match t {
        Tz::DuUtc => Some("UTC".into()),
        Tz::DuOffset(_, n) => name_of(*n),
        Tz::DuLocal(_) => Some(local().names[local_isdst(w, fold) as usize].clone()),
        Tz::Du(i) => match obj(*i) {
            DuObj::File(f) => f.tzname(w, fold),
            DuObj::Str(s) => if s.isdst(w, fold)? { s.dst_abbr.clone() } else { s.std_abbr.clone() },
        },
        _ => unreachable!(),
    })
}

/// `tz.fromutc(dt)` on a UTC wall time: the local wall time and its fold
pub fn fromutc(t: &Tz, utc: &NaiveDateTime, fold: u8) -> R<DateTime> {
    let (wall, fold) = match t {
        Tz::DuUtc => (*utc, fold),
        Tz::DuOffset(s, _) => (add(*utc, *s as i64)?, 0),
        Tz::DuLocal(_) => generic_fromutc(t, utc, fold)?,
        Tz::Du(i) => match obj(*i) {
            DuObj::File(f) => f.fromutc(utc)?,
            DuObj::Str(s) => s.fromutc(utc)?,
        },
        _ => unreachable!(),
    };
    Ok(DateTime { wall, tz: Some(*t), fold })
}

/// `_tzinfo.fromutc`: `_fromutc` then `_fold_status`
fn generic_fromutc(t: &Tz, utc: &NaiveDateTime, fold: u8) -> R<(NaiveDateTime, u8)> {
    if let Tz::DuLocal(_) = t {
        local_isdst_r(utc, fold)?;
    }
    let dtoff = offset(t, utc, fold) as i64;
    let dtdst = dst(t, utc, fold)?;
    let mid = add(*utc, dtoff - dtdst)?;
    let d2 = dst(t, &mid, 1)?;
    let wall = add(mid, d2)?;
    let f = if is_ambiguous(t, &wall)? {
        let delta = micros(&wall) - micros(utc);
        (delta == ((dtoff - dtdst) as i128) * 1_000_000) as u8
    } else {
        0
    };
    Ok((wall, f))
}

pub fn is_ambiguous(t: &Tz, w: &NaiveDateTime) -> R<bool> {
    Ok(match t {
        Tz::DuUtc | Tz::DuOffset(..) => false,
        Tz::DuLocal(_) => local_is_ambiguous(w)?,
        Tz::Du(i) => match obj(*i) {
            DuObj::File(f) => f.is_ambiguous(w, None),
            DuObj::Str(s) => s.is_ambiguous(w)?,
        },
        _ => unreachable!(),
    })
}

/// the class's module-qualified name (3.14's unhashable-key message): dateutil.tz.tz.X, dateutil.zoneinfo.tzfile
/// for a zone of dateutil's own tarball
pub fn tz_qualname(t: &Tz) -> String {
    match t {
        Tz::Du(i) => match obj(*i) {
            DuObj::File(f) if !f.filename.starts_with('/') => "dateutil.zoneinfo.tzfile".into(),
            _ => format!("dateutil.tz.tz.{}", tz_type(t)),
        },
        _ => format!("dateutil.tz.tz.{}", tz_type(t)),
    }
}

pub fn tz_type(t: &Tz) -> &'static str {
    match t {
        Tz::DuUtc => "tzutc",
        Tz::DuOffset(..) => "tzoffset",
        Tz::DuLocal(_) => "tzlocal",
        Tz::Du(i) => match obj(*i) {
            DuObj::File(_) => "tzfile",
            DuObj::Str(_) => "tzstr",
        },
        _ => "tzinfo",
    }
}

pub fn tz_repr(t: &Tz) -> String {
    match t {
        Tz::DuUtc => "tzutc()".into(),
        Tz::DuOffset(s, n) => format!("tzoffset({}, {s})", match name_of(*n) { Some(x) => ops::str_repr(&x), None => "None".into() }),
        Tz::DuLocal(_) => "tzlocal()".into(),
        Tz::Du(i) => match obj(*i) {
            DuObj::File(f) => format!("tzfile({})", ops::str_repr(&f.filename)),
            DuObj::Str(s) => format!("tzstr({})", ops::str_repr(&s.s)),
        },
        _ => unreachable!(),
    }
}

/// `a == b` between two tzinfos, one of them dateutil's (its `__eq__`, then the reflected one, then identity)
pub fn py_eq(a: &Tz, b: &Tz) -> bool {
    fn eq(a: &Tz, b: &Tz) -> Option<bool> {
        match (a, b) {
            (Tz::DuUtc, Tz::DuUtc) => Some(true),
            (Tz::DuUtc, Tz::DuOffset(s, _)) => Some(*s == 0),
            (Tz::DuOffset(x, _), Tz::DuOffset(y, _)) => Some(x == y),
            (Tz::DuLocal(_), Tz::DuLocal(_)) => Some(true),
            (Tz::DuLocal(_), Tz::DuUtc) => {
                let l = local();
                Some(!l.hasdst && (l.names[0] == "UTC" || l.names[0] == "GMT") && l.std == 0)
            }
            (Tz::DuLocal(_), Tz::DuOffset(s, n)) => {
                let l = local();
                Some(!l.hasdst && Some(l.names[0].clone()) == name_of(*n) && l.std == *s as i64)
            }
            (Tz::Du(x), Tz::Du(y)) => match (obj(*x), obj(*y)) {
                (DuObj::File(p), DuObj::File(q)) => Some(p.same(q)),
                (DuObj::Str(p), DuObj::Str(q)) => Some(p.same(q)),
                _ => None,
            },
            _ => None,
        }
    }
    eq(a, b).or_else(|| eq(b, a)).unwrap_or(a == b)
}

/// an attribute of a dateutil tzinfo (tzrange's `hasdst`)
pub fn attr(t: &Tz, name: &str) -> Option<R> {
    match (t, name) {
        (Tz::Du(i), "hasdst") => match obj(*i) {
            DuObj::Str(s) => Some(Ok(V::Bool(s.hasdst))),
            _ => None,
        },
        _ => None,
    }
}

/// the tzinfo methods of a dateutil tzinfo
pub fn method(t: &Tz, name: &str, args: &[V], kwargs: &[(String, V)]) -> Option<R> {
    let qual = format!("{}.{name}()", tz_type(t));
    Some((|| -> R {
        let one = |extra: bool| -> R<(V, Option<V>)> {
            let params: &[(&str, u8, bool)] = if extra { &[("dt", super::P_POS, false), ("idx", super::P_POS, true)] } else { &[("dt", super::P_POS, false)] };
            let s = super::bind_params(&qual, 1, args.to_vec(), kwargs.to_vec(), params)?;
            Ok((s[0].clone().unwrap(), s.get(1).cloned().flatten()))
        };
        match name {
            "utcoffset" | "dst" | "tzname" => {
                let (dt, _) = one(false)?;
                let (w, fold) = match &dt {
                    V::DateTime(d) => (d.wall, d.fold),
                    V::None => {
                        // dt=None: tzfile and tzstr answer None (tzfile's tzname too), tzlocal None when it has DST
                        return Ok(match (t, name) {
                            (Tz::DuUtc, "tzname") => V::str("UTC"),
                            (Tz::DuUtc, _) | (Tz::DuOffset(..), "dst") => V::Delta(Duration::zero()),
                            (Tz::DuOffset(s, _), _) if name == "utcoffset" => V::Delta(Duration::seconds(*s as i64)),
                            (Tz::DuOffset(_, n), _) => name_of(*n).map(V::str).unwrap_or(V::None),
                            (Tz::DuLocal(_), "tzname") if local().hasdst => {
                                return Err(Exc::attr_error("'NoneType' object has no attribute 'replace'"));
                            }
                            (Tz::DuLocal(_), "tzname") => V::str(&local().names[0]),
                            (Tz::DuLocal(_), _) if local().hasdst => V::None,
                            (Tz::DuLocal(_), "utcoffset") => V::Delta(Duration::seconds(local().std)),
                            (Tz::DuLocal(_), _) => V::Delta(Duration::zero()),
                            (Tz::Du(i), _) => match (obj(*i), name) {
                                (DuObj::Str(s), "tzname") => s.std_abbr.clone().map(V::str).unwrap_or(V::None),
                                (DuObj::Str(s), "utcoffset") if !s.hasdst => V::Delta(Duration::seconds(s.std_offset)),
                                (DuObj::Str(s), "dst") if !s.hasdst => V::Delta(Duration::zero()),
                                _ => V::None,
                            },
                            _ => V::None,
                        });
                    }
                    o => return Err(Exc::attr_error(format!("'{}' object has no attribute 'replace'", o.type_name()))),
                };
                if let Tz::DuLocal(_) = t {
                    local_isdst_r(&w, fold)?;
                }
                Ok(match name {
                    "utcoffset" => V::Delta(Duration::seconds(offset(t, &w, fold) as i64)),
                    "dst" => V::Delta(Duration::seconds(dst(t, &w, fold)?)),
                    _ => tzname(t, &w, fold)?.map(V::str).unwrap_or(V::None),
                })
            }
            "is_ambiguous" => {
                let (dt, idx) = one(matches!(t, Tz::Du(i) if matches!(obj(*i), DuObj::File(_))))?;
                let V::DateTime(d) = &dt else {
                    return Err(Exc::attr_error(format!("'{}' object has no attribute 'replace'", dt.type_name())));
                };
                match (t, idx) {
                    (Tz::Du(i), Some(V::Int(x))) => match obj(*i) {
                        DuObj::File(f) => Ok(V::Bool(f.is_ambiguous(&d.wall, Some(x)))),
                        _ => unreachable!(),
                    },
                    (_, Some(o)) if !o.is_none() => Err(Exc::type_error("py2axum: is_ambiguous(idx=) other than an int is not supported")),
                    _ => Ok(V::Bool(is_ambiguous(t, &d.wall)?)),
                }
            }
            "fromutc" => {
                let (dt, _) = one(false)?;
                let V::DateTime(d) = &dt else { return Err(Exc::type_error("fromutc() requires a datetime argument")) };
                if d.tz != Some(*t) {
                    return Err(Exc::value_error("dt.tzinfo is not self"));
                }
                Ok(V::DateTime(fromutc(t, &d.wall, d.fold)?))
            }
            _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", tz_type(t)))),
        }
    })())
}

// ------------------------------------------------------------------------------------------------ helpers of dateutil.tz

/// `tz.enfold(dt, fold=1)`
pub fn enfold(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("enfold()", 0, args.to_vec(), kwargs.to_vec(), &[("dt", super::P_POS, false), ("fold", super::P_POS, true)])?;
    let dt = s[0].clone().unwrap();
    let fold = super::methods::fold_arg(&s[1].clone().unwrap_or(V::Int(1)))?;
    match dt {
        V::DateTime(d) => Ok(V::DateTime(DateTime { fold, ..d })),
        o => Err(Exc::type_error(format!("'fold' is an invalid keyword argument for replace() of {}", o.type_name()))),
    }
}

fn tz_arg(dt: &DateTime, tz: &V) -> R<Tz> {
    match tz {
        V::None => dt.tz.ok_or_else(|| Exc::value_error("Datetime is naive and no time zone provided.")),
        V::Tz(t) => Ok(*t),
        o => Err(Exc::type_error(format!("tzinfo argument must be None or of a tzinfo subclass, not {}", o.type_name()))),
    }
}

fn need_dt(v: &V, what: &str) -> R<DateTime> {
    match v {
        V::DateTime(d) => Ok(*d),
        o => Err(Exc::attr_error(format!("'{}' object has no attribute '{what}'", o.type_name()))),
    }
}

/// `datetime.astimezone(tz)` (CPython: the same tzinfo returns the value itself)
pub fn astimezone(d: &DateTime, tz: Tz) -> R<DateTime> {
    if d.tz == Some(tz) {
        return Ok(*d);
    }
    let off = d.offset().unwrap_or(0) as i64;
    let utc = add(d.wall, -off)?;
    super::dt::from_utc_checked(utc, tz)
}

/// `tz.datetime_exists(dt, tz=None)`
pub fn datetime_exists(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("datetime_exists()", 0, args.to_vec(), kwargs.to_vec(), &[("dt", super::P_POS, false), ("tz", super::P_POS, true)])?;
    let d = need_dt(&s[0].clone().unwrap(), "tzinfo")?;
    let tz = tz_arg(&d, &s[1].clone().unwrap_or(V::None))?;
    let naive = d.wall;
    let a = DateTime { wall: naive, tz: Some(tz), fold: d.fold };
    let rt = astimezone(&astimezone(&a, Tz::DuUtc)?, tz)?;
    Ok(V::Bool(rt.wall == naive))
}

/// `tz.datetime_ambiguous(dt, tz=None)`
pub fn datetime_ambiguous(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("datetime_ambiguous()", 0, args.to_vec(), kwargs.to_vec(), &[("dt", super::P_POS, false), ("tz", super::P_POS, true)])?;
    let d = need_dt(&s[0].clone().unwrap(), "tzinfo")?;
    let tz = tz_arg(&d, &s[1].clone().unwrap_or(V::None))?;
    if matches!(tz, Tz::DuUtc | Tz::DuOffset(..) | Tz::DuLocal(_) | Tz::Du(_)) {
        if let Ok(b) = is_ambiguous(&tz, &d.wall) {
            return Ok(V::Bool(b));
        }
    }
    let (w0, w1) = (DateTime { wall: d.wall, tz: Some(tz), fold: 0 }, DateTime { wall: d.wall, tz: Some(tz), fold: 1 });
    let same_off = w0.offset() == w1.offset();
    let same_dst = super::dt::dst_of(&w0)? == super::dt::dst_of(&w1)?;
    Ok(V::Bool(!(same_off && same_dst)))
}

/// `tz.resolve_imaginary(dt)`
pub fn resolve_imaginary(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("resolve_imaginary()", 0, args.to_vec(), kwargs.to_vec(), &[("dt", super::P_POS, false)])?;
    let v = s[0].clone().unwrap();
    let d = need_dt(&v, "tzinfo")?;
    if d.tz.is_some() && !ops::truthy(&datetime_exists(&[v.clone()], &[])?)? {
        let day = Duration::hours(24);
        let cur = DateTime { wall: addu(d.wall, day)?, tz: d.tz, fold: 0 }.offset().unwrap_or(0) as i64;
        let old = DateTime { wall: addu(d.wall, -day)?, tz: d.tz, fold: 0 }.offset().unwrap_or(0) as i64;
        return Ok(V::DateTime(DateTime { wall: add(d.wall, cur - old)?, tz: d.tz, fold: 0 }));
    }
    Ok(v)
}
