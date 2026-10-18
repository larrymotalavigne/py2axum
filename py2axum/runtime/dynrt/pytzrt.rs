//! pytz 2026.5: `pytz.timezone()`, `utc`, `FixedOffset()`, the zone lists, and the tzinfo objects' methods
//! (`localize`, `normalize`, `fromutc`, `utcoffset`/`dst`/`tzname`), ported from pytz/__init__.py and
//! pytz/tzinfo.py. The zones are pytz's own (`_utc_transition_times`, `_transition_info`), dumped by
//! tools/gen_pytz.py into pytzdata.bin: transitions stop in 2037 and offsets are rounded to the minute, as in pytz.
//!
//! A tzinfo is a `Tz` (dt.rs): `Tz::PyUtc`, `Tz::PyFixed(seconds)`, `Tz::Py(zone, info)` where info 0 is the
//! zone `pytz.timezone()` returns and the others its localized instances (`_tzinfos`), one per transition info:
//! identity is equality, as with pytz's shared instances.
use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{Duration, NaiveDateTime};

use super::dt::{DateTime, Tz};
use super::v::*;

pub struct Info {
    pub off: i32,
    pub dst: i32,
    pub name: &'static str,
}

pub enum Kind {
    /// StaticTzInfo: one offset, one name
    Static(i32, &'static str),
    /// DstTzInfo: its transition infos (0 = `_transition_info[0]`), its UTC transitions (seconds since 1970,
    /// i64::MIN for datetime.min) with the info each starts
    Dst(Vec<Info>, Vec<i64>, Vec<u16>),
}

pub struct Zone {
    pub name: &'static str,
    pub kind: Kind,
}

pub struct Db {
    pub version: &'static str,
    pub zones: Vec<Zone>,
    pub common: Vec<u16>,
    lower: HashMap<String, u16>,
}

static DATA: &[u8] = include_bytes!("pytzdata.bin");

struct Rd<'a>(&'a [u8], usize);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        let s = &self.0[self.1..self.1 + n];
        self.1 += n;
        s
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take(2).try_into().unwrap())
    }
    fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn i64(&mut self) -> i64 {
        i64::from_le_bytes(self.take(8).try_into().unwrap())
    }
    fn str(&mut self) -> &'static str {
        let n = self.u8() as usize;
        Box::leak(String::from_utf8_lossy(self.take(n)).into_owned().into_boxed_str())
    }
}

pub fn db() -> &'static Db {
    static DB: OnceLock<Db> = OnceLock::new();
    DB.get_or_init(|| {
        let mut r = Rd(DATA, 0);
        assert_eq!(r.take(4), b"PYTZ");
        let version = r.str();
        let _olson = r.str();
        let n = r.u16() as usize;
        let names: Vec<&'static str> = (0..n).map(|_| r.str()).collect();
        let m = r.u16() as usize;
        let common = (0..m).map(|_| r.u16()).collect();
        let mut zones = Vec::with_capacity(n);
        for name in &names {
            let kind = match r.u8() {
                0 => {
                    let off = r.i32();
                    Kind::Static(off, r.str())
                }
                _ => {
                    let ni = r.u16() as usize;
                    let infos = (0..ni).map(|_| {
                        let (off, dst) = (r.i32(), r.i32());
                        Info { off, dst, name: r.str() }
                    }).collect();
                    let nt = r.u16() as usize;
                    let (mut times, mut idx) = (Vec::with_capacity(nt), Vec::with_capacity(nt));
                    for _ in 0..nt {
                        times.push(r.i64());
                        idx.push(r.u16());
                    }
                    Kind::Dst(infos, times, idx)
                }
            };
            zones.push(Zone { name, kind });
        }
        let lower = names.iter().enumerate().map(|(i, n)| (n.to_lowercase(), i as u16)).collect();
        Db { version, zones, common, lower }
    })
}


fn unknown(v: V) -> Exc {
    Exc::new(&PYTZ_UNKNOWN_TIME_ZONE_ERROR, vec![v])
}

/// `pytz.timezone(zone)`
pub fn timezone(args: &[V], kwargs: &[(String, V)]) -> R {
    let zone = match (args, kwargs) {
        ([z], []) => z,
        ([], [(k, z)]) if k == "zone" => z,
        _ => return Err(Exc::type_error("timezone() takes 1 positional argument")),
    };
    let s = match zone {
        V::Str(s) => {
            if s.to_uppercase() == "UTC" {
                return Ok(V::Tz(Tz::PyUtc));
            }
            if !s.is_ascii() {
                return Err(unknown(zone.clone()));
            }
            s.to_string()
        }
        V::Bytes(b) => match b.iter().position(|c| !c.is_ascii()) {
            Some(i) => return Err(Exc::new(&UNICODE_DECODE_ERROR, vec![V::str("ascii"), zone.clone(), V::Int(i as i64),
                                                                     V::Int(i as i64 + 1), V::str("ordinal not in range(128)")])),
            None => String::from_utf8_lossy(b).into_owned(),
        },
        _ => return Err(unknown(zone.clone())),
    };
    let s = s.replace("_plus_", "+").replace("_minus_", "-");
    let d = db();
    match d.lower.get(&s.to_lowercase()) {
        Some(&i) => Ok(V::Tz(Tz::Py(i, 0))),
        None => Err(unknown(V::str(s))),
    }
}

/// `pytz.FixedOffset(minutes)`: UTC for 0, one instance per offset; minutes must make whole seconds
pub fn fixed_offset(args: &[V], kwargs: &[(String, V)]) -> R {
    let off = match (args, kwargs) {
        ([o], []) => o,
        ([], [(k, o)]) if k == "offset" => o,
        _ => return Err(Exc::type_error("FixedOffset() takes 1 positional argument")),
    };
    let minutes = match off {
        V::Int(i) => *i as f64,
        V::Bool(b) => *b as i64 as f64,
        V::Float(f) => *f,
        o => return Err(Exc::type_error(format!("bad operand type for abs(): '{}'", o.type_name()))),
    };
    if minutes == 0.0 {
        return Ok(V::Tz(Tz::PyUtc));
    }
    if minutes.abs() >= 1440.0 {
        return Err(Exc::new(&VALUE_ERROR, vec![V::str("absolute offset is too large"), off.clone()]));
    }
    let secs = minutes * 60.0;
    if secs.fract() != 0.0 {
        return Err(Exc::type_error("py2axum: pytz.FixedOffset() with a fraction of a second is not supported"));
    }
    Ok(V::Tz(Tz::PyFixed(secs as i32)))
}

pub fn all_timezones() -> V {
    static L: OnceLock<V> = OnceLock::new();
    L.get_or_init(|| V::list(db().zones.iter().map(|z| V::str(z.name)).collect())).clone()
}

pub fn common_timezones() -> V {
    static L: OnceLock<V> = OnceLock::new();
    L.get_or_init(|| V::list(db().common.iter().map(|&i| V::str(db().zones[i as usize].name)).collect())).clone()
}

fn set_of(l: &V) -> V {
    super::methods::b_set(std::slice::from_ref(l)).unwrap_or(V::None)
}

pub fn all_timezones_set() -> V {
    static L: OnceLock<V> = OnceLock::new();
    L.get_or_init(|| set_of(&all_timezones())).clone()
}

pub fn common_timezones_set() -> V {
    static L: OnceLock<V> = OnceLock::new();
    L.get_or_init(|| set_of(&common_timezones())).clone()
}

fn zone(z: u16) -> &'static Zone {
    &db().zones[z as usize]
}

/// (utcoffset, dst, tzname) of a pytz tzinfo (its `_utcoffset`, `_dst`, `_tzname`; FixedOffset: no name)
pub fn info(t: &Tz) -> (i32, i32, Option<&'static str>) {
    match t {
        Tz::PyUtc => (0, 0, Some("UTC")),
        Tz::PyFixed(s) => (*s, 0, None),
        Tz::Py(z, i) => match &zone(*z).kind {
            Kind::Static(off, n) => (*off, 0, Some(*n)),
            Kind::Dst(infos, ..) => {
                let f = &infos[*i as usize];
                (f.off, f.dst, Some(f.name))
            }
        },
        _ => unreachable!(),
    }
}

fn secs_of(w: &NaiveDateTime) -> i64 {
    w.and_utc().timestamp()
}

/// `max(0, bisect_right(_utc_transition_times, dt) - 1)`: the info starting last at or before `dt`
fn info_at(times: &[i64], idx: &[u16], w: &NaiveDateTime) -> u16 {
    let s = secs_of(w);
    let n = times.partition_point(|t| *t <= s);
    idx[n.saturating_sub(1)]
}

/// `fromutc` of a pytz tzinfo on a UTC wall time, OverflowError past the datetime range
pub fn fromutc(utc: NaiveDateTime, t: Tz) -> R<DateTime> {
    let d = from_utc(utc, t);
    in_range(d.wall)?;
    Ok(d)
}

/// `fromutc` of a pytz tzinfo on a UTC wall time (no checks)
pub fn from_utc(utc: NaiveDateTime, t: Tz) -> DateTime {
    match t {
        Tz::Py(z, _) => match &zone(z).kind {
            Kind::Static(off, _) => DateTime::aware(utc + Duration::seconds(*off as i64), t),
            Kind::Dst(infos, times, idx) => {
                let i = info_at(times, idx, &utc);
                DateTime::aware(utc + Duration::seconds(infos[i as usize].off as i64), Tz::Py(z, i))
            }
        },
        _ => DateTime::aware(utc + Duration::seconds(info(&t).0 as i64), t),
    }
}

/// CPython's names in a pytz message (`'datetime.timezone' object has no attribute '_utcoffset'`)
fn cpython_type(v: &V) -> String {
    match v {
        V::DateTime(_) => "datetime.datetime".into(),
        V::Date(_) => "datetime.date".into(),
        V::Time(_) => "datetime.time".into(),
        V::Delta(_) => "datetime.timedelta".into(),
        V::Tz(Tz::Utc | Tz::Fixed(_)) => "datetime.timezone".into(),
        V::Tz(Tz::Zone(_)) => "zoneinfo.ZoneInfo".into(),
        V::Tz(t) => tz_type(t).into(),
        o => o.type_name().into(),
    }
}

fn no_attr(v: &V, name: &str) -> Exc {
    Exc::attr_error(format!("'{}' object has no attribute '{name}'", cpython_type(v)))
}

/// `dt.tzinfo` where pytz reads it: a datetime, else the AttributeError
fn need_dt(v: &V) -> R<DateTime> {
    match v {
        V::DateTime(d) => Ok(*d),
        o => Err(no_attr(o, "tzinfo")),
    }
}

fn overflow() -> Exc {
    Exc::msg(&OVERFLOW_ERROR, "date value out of range")
}

fn in_range(w: NaiveDateTime) -> R<NaiveDateTime> {
    use chrono::Datelike;
    if (1..=9999).contains(&w.year()) { Ok(w) } else { Err(overflow()) }
}

fn add(w: NaiveDateTime, d: Duration) -> R<NaiveDateTime> {
    w.checked_add_signed(d).ok_or_else(overflow).and_then(in_range)
}

/// `type(tz).__name__`
pub fn tz_type(t: &Tz) -> &'static str {
    match t {
        Tz::PyUtc => "UTC",
        Tz::PyFixed(_) => "_FixedOffset",
        Tz::Py(z, _) => zone(*z).name,
        _ => super::dutz::tz_type(t),
    }
}

pub fn tz_repr(t: &Tz) -> String {
    match t {
        Tz::PyUtc => "<UTC>".into(),
        Tz::PyFixed(s) => format!("pytz.FixedOffset({})", s / 60),
        Tz::Py(z, _) => match &zone(*z).kind {
            Kind::Static(..) => format!("<StaticTzInfo '{}'>", zone(*z).name),
            Kind::Dst(..) => {
                let (off, dst, name) = info(t);
                let td = super::dt::delta_str(&Duration::seconds(off as i64));
                format!("<DstTzInfo '{}' {}{}{} {}>", zone(*z).name, name.unwrap_or(""), if off > 0 { "+" } else { "" }, td,
                        if dst != 0 { "DST" } else { "STD" })
            }
        },
        _ => unreachable!(),
    }
}

pub fn tz_str(t: &Tz) -> String {
    match t {
        Tz::PyUtc => "UTC".into(),
        Tz::PyFixed(_) => tz_repr(t),
        Tz::Py(z, _) => zone(*z).name.into(),
        _ => unreachable!(),
    }
}

fn is_dst_kind(t: &Tz) -> bool {
    matches!(t, Tz::Py(z, _) if matches!(zone(*z).kind, Kind::Dst(..)))
}

/// DstTzInfo.normalize: the wall time of `dt` moved to the info in effect at its instant
fn normalize_dst(t: &Tz, d: &DateTime) -> R<DateTime> {
    let Some(src) = d.tz else { return Err(Exc::value_error("Naive time - no tzinfo set")) };
    let off = utcoffset_attr(&src)?;
    let utc = add(d.wall, -Duration::seconds(off as i64))?;
    fromutc(utc, *t)
}

/// `dt.tzinfo._utcoffset` (pytz tzinfos only)
fn utcoffset_attr(t: &Tz) -> R<i32> {
    match t {
        Tz::PyUtc | Tz::PyFixed(_) | Tz::Py(..) => Ok(info(t).0),
        o => Err(no_attr(&V::Tz(*o), "_utcoffset")),
    }
}

/// DstTzInfo.localize
fn localize_dst(z: u16, dt: NaiveDateTime, is_dst: &V) -> R<DateTime> {
    let Kind::Dst(infos, times, idx) = &zone(z).kind else { unreachable!() };
    let mut possible: Vec<DateTime> = Vec::new();
    for delta in [Duration::days(-1), Duration::days(1)] {
        let Ok(loc) = add(dt, delta) else { continue };
        let i = info_at(times, idx, &loc);
        let tz = Tz::Py(z, i);
        let utc = add(dt, -Duration::seconds(infos[i as usize].off as i64))?;
        let loc = fromutc(utc, tz)?;
        if loc.wall == dt && !possible.iter().any(|p| p.utc() == loc.utc()) {
            possible.push(loc);
        }
    }
    if possible.len() == 1 {
        return Ok(possible[0]);
    }
    if possible.is_empty() {
        if is_dst.is_none() {
            return Err(Exc::new(&PYTZ_NON_EXISTENT_TIME_ERROR, vec![V::DateTime(DateTime::naive(dt))]));
        }
        let six = Duration::hours(6);
        return if super::ops::truthy(is_dst)? {
            let r = localize_dst(z, add(dt, six)?, is_dst)?;
            Ok(DateTime { wall: add(r.wall, -six)?, ..r })
        } else {
            let r = localize_dst(z, add(dt, -six)?, is_dst)?;
            Ok(DateTime { wall: add(r.wall, six)?, ..r })
        };
    }
    if is_dst.is_none() {
        return Err(Exc::new(&PYTZ_AMBIGUOUS_TIME_ERROR, vec![V::DateTime(DateTime::naive(dt))]));
    }
    let mut filtered = Vec::new();
    for p in &possible {
        if super::ops::eq_bool(&V::Bool(info(&p.tz.unwrap()).1 != 0), is_dst) {
            filtered.push(*p);
        }
    }
    if filtered.len() == 1 {
        return Ok(filtered[0]);
    }
    if filtered.is_empty() {
        filtered = possible;
    }
    let pick_max = !super::ops::truthy(is_dst)?;
    let best = filtered.iter().copied().reduce(|a, b| {
        if (b.utc() > a.utc()) == pick_max { b } else { a }
    });
    Ok(best.unwrap())
}

/// `tz.localize(naive, is_dst)` of any pytz tzinfo (croniter's `_add_tzinfo`)
pub fn localize_naive(t: &Tz, w: NaiveDateTime, is_dst: &V) -> R<DateTime> {
    match t {
        Tz::Py(z, _) if is_dst_kind(t) => localize_dst(*z, w, is_dst),
        _ => Ok(DateTime { wall: w, tz: Some(*t), fold: 0 }),
    }
}

/// `tz.localize(dt, is_dst=False)`
fn localize(t: &Tz, dt: &V, is_dst: &V) -> R {
    let d = need_dt(dt)?;
    if d.tz.is_some() {
        return Err(Exc::value_error("Not naive datetime (tzinfo is already set)"));
    }
    Ok(V::DateTime(match t {
        Tz::Py(z, _) if is_dst_kind(t) => localize_dst(*z, d.wall, is_dst)?,
        _ => DateTime { wall: d.wall, tz: Some(*t), fold: d.fold },
    }))
}

/// Python's binding of a pytz method's arguments: `dt`, then `is_dst` (with its default) when the method has it
fn bind(qual: &str, args: &[V], kwargs: &[(String, V)], is_dst: Option<V>) -> R<(V, V)> {
    use super::P_POS;
    let params: &[(&str, u8, bool)] = if is_dst.is_some() { &[("dt", P_POS, false), ("is_dst", P_POS, true)] } else { &[("dt", P_POS, false)] };
    let s = super::bind_params(&format!("{qual}()"), 1, args.to_vec(), kwargs.to_vec(), params)?;
    Ok((s[0].clone().unwrap(), s.get(1).cloned().flatten().or(is_dst).unwrap_or(V::None)))
}

/// a method of a pytz tzinfo (None: not one of pytz's)
pub fn method(t: &Tz, name: &str, args: &[V], kwargs: &[(String, V)]) -> Option<R> {
    let class = match t {
        Tz::PyUtc => "UTC",
        Tz::PyFixed(_) => "_FixedOffset",
        _ if is_dst_kind(t) => "DstTzInfo",
        _ => "StaticTzInfo",
    };
    let qual = format!("{class}.{name}");
    // UTC and FixedOffset: utcoffset/dst/tzname(dt) without is_dst; DstTzInfo.normalize(dt) too
    let simple = matches!(t, Tz::PyUtc | Tz::PyFixed(_));
    Some((|| -> R {
        match name {
            "localize" => {
                let (dt, is_dst) = bind(&qual, args, kwargs, Some(V::Bool(false)))?;
                localize(t, &dt, &is_dst)
            }
            "normalize" if is_dst_kind(t) => {
                let (dt, _) = bind(&qual, args, kwargs, None)?;
                Ok(V::DateTime(normalize_dst(t, &need_dt(&dt)?)?))
            }
            "normalize" => {
                let (dt, _) = bind(&qual, args, kwargs, Some(V::Bool(false)))?;
                let d = need_dt(&dt)?;
                match d.tz {
                    Some(x) if x == *t => Ok(dt),
                    None => Err(Exc::value_error("Naive time - no tzinfo set")),
                    Some(_) => Ok(V::DateTime(astimezone(&d, *t)?)),
                }
            }
            "fromutc" => {
                let (dt, _) = bind(&qual, args, kwargs, None)?;
                match t {
                    Tz::Py(z, _) => {
                        let d = need_dt(&dt)?;
                        let ok = match d.tz {
                            None => true,
                            Some(Tz::Py(z2, _)) if is_dst_kind(t) => z2 == *z,
                            Some(x) => x == *t,
                        };
                        if !ok {
                            return Err(Exc::value_error("fromutc: dt.tzinfo is not self"));
                        }
                        Ok(V::DateTime(fromutc(d.wall, *t)?))
                    }
                    Tz::PyUtc => {
                        let d = need_dt(&dt)?;
                        if d.tz.is_none() {
                            return Ok(V::DateTime(DateTime { tz: Some(*t), ..d }));
                        }
                        c_fromutc(t, &dt)
                    }
                    _ => c_fromutc(t, &dt),
                }
            }
            "utcoffset" | "dst" | "tzname" => {
                let (dt, is_dst) = bind(&qual, args, kwargs, if simple { None } else { Some(V::None) })?;
                let (off, dst, tzname) = info(t);
                let pick = |o: i32, d: i32, n: Option<&'static str>| match name {
                    "utcoffset" => V::Delta(Duration::seconds(o as i64)),
                    "dst" => V::Delta(Duration::seconds(d as i64)),
                    _ => n.map(V::str).unwrap_or(V::None),
                };
                if !is_dst_kind(t) {
                    return Ok(pick(off, dst, tzname));
                }
                let Tz::Py(z, _) = t else { unreachable!() };
                if dt.is_none() {
                    return Ok(if name == "tzname" { V::str(zone(*z).name) } else { V::None });
                }
                let d = need_dt(&dt)?;
                if d.tz == Some(*t) {
                    return Ok(pick(off, dst, tzname));
                }
                if d.tz.is_some() {
                    return Err(Exc::value_error("Not naive datetime (tzinfo is already set)"));
                }
                let l = localize_dst(*z, d.wall, &is_dst)?;
                let (o, ds, n) = info(&l.tz.unwrap());
                Ok(pick(o, ds, n))
            }
            _ => Err(no_attr(&V::Tz(*t), name)),
        }
    })())
}

/// `dt.astimezone(tz)` with CPython's range check
fn astimezone(d: &DateTime, t: Tz) -> R<DateTime> {
    let utc = add(d.wall, -Duration::seconds(d.offset().unwrap_or(0) as i64))?;
    super::dt::from_utc_checked(utc, t)
}

/// CPython's `tzinfo.fromutc` (pytz's UTC on an aware value, FixedOffset)
fn c_fromutc(t: &Tz, dt: &V) -> R {
    let V::DateTime(d) = dt else { return Err(Exc::type_error("fromutc: argument must be a datetime")) };
    if d.tz != Some(*t) {
        return Err(Exc::value_error("fromutc: dt.tzinfo is not self"));
    }
    Ok(V::DateTime(DateTime { wall: add(d.wall, Duration::seconds(info(t).0 as i64))?, tz: d.tz, fold: 0 }))
}

/// an attribute of a pytz tzinfo (None: not one of pytz's)
pub fn attr(t: &Tz, name: &str) -> Option<R> {
    let (off, dst, n) = info(t);
    Some(Ok(match (t, name) {
        (Tz::PyFixed(_), "zone") => V::None,
        (Tz::PyUtc, "zone") => V::str("UTC"),
        (Tz::Py(z, _), "zone") => V::str(zone(*z).name),
        (_, "_utcoffset") => V::Delta(Duration::seconds(off as i64)),
        (Tz::PyFixed(s), "_minutes") => V::Int((*s / 60) as i64),
        (Tz::PyUtc | Tz::Py(..), "_tzname") => n.map(V::str).unwrap_or(V::None),
        (Tz::PyUtc, "_dst") => V::Delta(Duration::zero()),
        (Tz::Py(..), "_dst") if is_dst_kind(t) => V::Delta(Duration::seconds(dst as i64)),
        _ => return None,
    }))
}
