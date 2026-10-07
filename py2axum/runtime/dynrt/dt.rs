//! `datetime` / `date` / `timedelta` with CPython and Pydantic semantics.
//!
//! A datetime is a wall time plus an optional tzinfo, like in Python: arithmetic happens on the
//! wall time, comparisons of aware values on the UTC instant. Values read from Postgres carry the
//! session TimeZone (psycopg returns them in it), values built in Python keep their own tzinfo:
//! this is what makes `+02:00` and `Z` come out exactly where FastAPI prints them.
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Timelike};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tz {
    /// datetime.timezone.utc / UTC
    Utc,
    /// datetime.timezone(timedelta(seconds=...)) and Pydantic's TzInfo
    Fixed(i32),
    /// zoneinfo.ZoneInfo
    Zone(chrono_tz::Tz),
}

impl Tz {
    pub fn offset_for_wall(&self, wall: &NaiveDateTime, fold: u8) -> i32 {
        match self {
            Tz::Utc => 0,
            Tz::Fixed(s) => *s,
            Tz::Zone(z) => match z.offset_from_local_datetime(wall) {
                chrono::LocalResult::Single(o) => o.fix().local_minus_utc(),
                chrono::LocalResult::Ambiguous(a, b) => {
                    if fold == 0 { a.fix().local_minus_utc() } else { b.fix().local_minus_utc() }
                }
                chrono::LocalResult::None => {
                    // gap: CPython (fold=0) uses the offset before the transition
                    z.offset_from_utc_datetime(&(*wall - Duration::hours(3))).fix().local_minus_utc()
                }
            },
        }
    }
    pub fn name(&self) -> String {
        match self {
            Tz::Utc => "UTC".into(),
            Tz::Fixed(s) => format!("UTC{}", fmt_offset(*s, true)),
            Tz::Zone(z) => z.name().into(),
        }
    }
    pub fn zone(name: &str) -> Option<Tz> {
        if name == "UTC" || name == "Etc/UTC" || name == "utc" {
            return Some(Tz::Zone(chrono_tz::UTC));
        }
        name.parse::<chrono_tz::Tz>().ok().map(Tz::Zone)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DateTime {
    pub wall: NaiveDateTime,
    pub tz: Option<Tz>,
    pub fold: u8,
}

pub fn micros(d: &Duration) -> i64 {
    d.num_microseconds().unwrap_or(i64::MAX)
}

impl DateTime {
    pub fn naive(wall: NaiveDateTime) -> Self {
        DateTime { wall, tz: None, fold: 0 }
    }
    pub fn aware(wall: NaiveDateTime, tz: Tz) -> Self {
        DateTime { wall, tz: Some(tz), fold: 0 }
    }
    /// From a UTC instant, expressed in `tz` (fold set for the second occurrence of an ambiguous hour).
    pub fn from_utc(utc: NaiveDateTime, tz: Tz) -> Self {
        match tz {
            Tz::Utc => DateTime::aware(utc, tz),
            Tz::Fixed(s) => DateTime::aware(utc + Duration::seconds(s as i64), tz),
            Tz::Zone(z) => {
                let local = z.from_utc_datetime(&utc);
                let wall = local.naive_local();
                let off = local.offset().fix().local_minus_utc();
                let fold = match z.offset_from_local_datetime(&wall) {
                    chrono::LocalResult::Ambiguous(a, _) if a.fix().local_minus_utc() != off => 1,
                    _ => 0,
                };
                DateTime { wall, tz: Some(tz), fold }
            }
        }
    }
    pub fn now(tz: Option<Tz>) -> Self {
        let utc = chrono::Utc::now().naive_utc();
        let utc = utc.with_nanosecond(utc.nanosecond() / 1000 * 1000).unwrap_or(utc);
        match tz {
            Some(tz) => DateTime::from_utc(utc, tz),
            None => {
                let local = chrono::Local::now().naive_local();
                DateTime::naive(local.with_nanosecond(local.nanosecond() / 1000 * 1000).unwrap_or(local))
            }
        }
    }
    pub fn offset(&self) -> Option<i32> {
        self.tz.map(|t| t.offset_for_wall(&self.wall, self.fold))
    }
    /// UTC instant of an aware value (wall time for a naive one).
    pub fn utc(&self) -> NaiveDateTime {
        match self.offset() {
            Some(o) => self.wall - Duration::seconds(o as i64),
            None => self.wall,
        }
    }
    pub fn key_micros(&self) -> i64 {
        self.utc().and_utc().timestamp_micros()
    }
    pub fn add(&self, d: Duration) -> Self {
        DateTime { wall: self.wall + d, tz: self.tz, fold: 0 }
    }
    pub fn date(&self) -> NaiveDate {
        self.wall.date()
    }
    pub fn astimezone(&self, tz: Tz) -> Self {
        DateTime::from_utc(self.utc(), tz)
    }

    /// `datetime.isoformat(sep, timespec)`
    pub fn isoformat(&self, sep: char, timespec: &str) -> String {
        let w = &self.wall;
        let mut s = format!("{:04}-{:02}-{:02}{}", w.year(), w.month(), w.day(), sep);
        let us = w.nanosecond() / 1000;
        match timespec {
            "hours" => s += &format!("{:02}", w.hour()),
            "minutes" => s += &format!("{:02}:{:02}", w.hour(), w.minute()),
            "seconds" => s += &format!("{:02}:{:02}:{:02}", w.hour(), w.minute(), w.second()),
            "milliseconds" => s += &format!("{:02}:{:02}:{:02}.{:03}", w.hour(), w.minute(), w.second(), us / 1000),
            "microseconds" => s += &format!("{:02}:{:02}:{:02}.{:06}", w.hour(), w.minute(), w.second(), us),
            _ => {
                s += &format!("{:02}:{:02}:{:02}", w.hour(), w.minute(), w.second());
                if us != 0 {
                    s += &format!(".{:06}", us);
                }
            }
        }
        if let Some(o) = self.offset() {
            s += &fmt_offset(o, false);
        }
        s
    }

    /// Pydantic v2 JSON form: like isoformat, but a zero offset is written `Z`.
    pub fn pydantic(&self) -> String {
        let w = &self.wall;
        let mut s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            w.year(), w.month(), w.day(), w.hour(), w.minute(), w.second()
        );
        let us = w.nanosecond() / 1000;
        if us != 0 {
            s += &format!(".{:06}", us);
        }
        match self.offset() {
            Some(0) => s.push('Z'),
            Some(o) => s += &fmt_offset(o, false),
            None => {}
        }
        s
    }
}

pub fn fmt_offset(secs: i32, utc_word: bool) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    let (h, m, s) = (a / 3600, (a % 3600) / 60, a % 60);
    let _ = utc_word;
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{sign}{h:02}:{m:02}")
    }
}

pub fn date_iso(d: &NaiveDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())
}

pub fn time_iso(t: &NaiveTime) -> String {
    let us = t.nanosecond() / 1000;
    if us != 0 {
        format!("{:02}:{:02}:{:02}.{:06}", t.hour(), t.minute(), t.second(), us)
    } else {
        format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
    }
}

/// `str(timedelta)`: `[D day[s], ]H:MM:SS[.ffffff]`
pub fn delta_str(d: &Duration) -> String {
    let total = micros(d);
    let days = total.div_euclid(86_400_000_000);
    let rem = total.rem_euclid(86_400_000_000);
    let (secs, us) = (rem / 1_000_000, rem % 1_000_000);
    let mut s = String::new();
    if days != 0 {
        s += &format!("{} day{}, ", days, if days.abs() != 1 { "s" } else { "" });
    }
    s += &format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60);
    if us != 0 {
        s += &format!(".{:06}", us);
    }
    s
}

// ---------------------------------------------------------------- parsing (speedate, as used by Pydantic)

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PErr {
    TooShort,
    Extra,
    DateTimeSep,
    DateSep,
    Year,
    Month,
    Day,
    TimeSep,
    Hour,
    Minute,
    Second,
    FracMissing,
    TzHour,
    TzMinute,
    RangeMonth,
    RangeDay,
    RangeHour,
    RangeMinute,
    RangeSecond,
    RangeTz,
}

impl PErr {
    pub fn text(&self) -> &'static str {
        match self {
            PErr::TooShort => "input is too short",
            PErr::Extra => "unexpected extra characters at the end of the input",
            PErr::DateTimeSep => "invalid datetime separator, expected `T`, `t`, `_` or space",
            PErr::DateSep => "invalid date separator, expected `-`",
            PErr::Year => "invalid character in year",
            PErr::Month => "invalid character in month",
            PErr::Day => "invalid character in day",
            PErr::TimeSep => "invalid time separator, expected `:`",
            PErr::Hour => "invalid character in hour",
            PErr::Minute => "invalid character in minute",
            PErr::Second => "invalid character in second",
            PErr::FracMissing => "second fraction digits missing after `.`",
            PErr::TzHour => "invalid timezone hour",
            PErr::TzMinute => "invalid timezone minute",
            PErr::RangeMonth => "month value is outside expected range of 1-12",
            PErr::RangeDay => "day value is outside expected range",
            PErr::RangeHour => "hour value is outside expected range of 0-23",
            PErr::RangeMinute => "minute value is outside expected range of 0-59",
            PErr::RangeSecond => "second value is outside expected range of 0-59",
            PErr::RangeTz => "timezone offset must be less than 24 hours",
        }
    }
}

fn digits(b: &[u8], from: usize, n: usize, e: PErr) -> Result<u32, PErr> {
    let mut v = 0u32;
    for i in from..from + n {
        let c = b[i];
        if !c.is_ascii_digit() {
            return Err(e);
        }
        v = v * 10 + (c - b'0') as u32;
    }
    Ok(v)
}

/// The first 10 bytes as `YYYY-MM-DD`.
fn date_part(b: &[u8]) -> Result<NaiveDate, PErr> {
    if b.len() < 10 {
        return Err(PErr::TooShort);
    }
    let y = digits(b, 0, 4, PErr::Year)?;
    if b[4] != b'-' {
        return Err(PErr::DateSep);
    }
    let m = digits(b, 5, 2, PErr::Month)?;
    if b[7] != b'-' {
        return Err(PErr::DateSep);
    }
    let d = digits(b, 8, 2, PErr::Day)?;
    if !(1..=12).contains(&m) {
        return Err(PErr::RangeMonth);
    }
    NaiveDate::from_ymd_opt(y as i32, m, d).ok_or(PErr::RangeDay)
}

/// Strict `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<NaiveDate, PErr> {
    let b = s.as_bytes();
    let d = date_part(b)?;
    if b.len() > 10 {
        return Err(PErr::Extra);
    }
    Ok(d)
}

fn numeric(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit() || c == b'.' || c == b'-') {
        return None;
    }
    if !t.bytes().any(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Unix timestamp (seconds, or milliseconds above 2e10) to an aware UTC datetime.
pub fn from_timestamp(ts: f64) -> Option<DateTime> {
    let secs_f = if ts.abs() > 2e10 { ts / 1000.0 } else { ts };
    let whole = secs_f.floor();
    let us = ((secs_f - whole) * 1e6).round() as i64;
    let base = chrono::DateTime::from_timestamp(whole as i64, 0)?.naive_utc();
    Some(DateTime::aware(base + Duration::microseconds(us), Tz::Utc))
}

/// speedate `DateTime::parse_str` (RFC 3339 and friends, or a numeric timestamp).
pub fn parse_datetime(s: &str) -> Result<DateTime, PErr> {
    if let Some(ts) = numeric(s) {
        return from_timestamp(ts).ok_or(PErr::TooShort);
    }
    let b = s.as_bytes();
    let date = date_part(b)?;
    if b.len() < 11 {
        return Err(PErr::TooShort);
    }
    if !matches!(b[10], b'T' | b't' | b'_' | b' ') {
        return Err(PErr::DateTimeSep);
    }
    if b.len() < 16 {
        return Err(PErr::TooShort);
    }
    let h = digits(b, 11, 2, PErr::Hour)?;
    if b[13] != b':' {
        return Err(PErr::TimeSep);
    }
    let mi = digits(b, 14, 2, PErr::Minute)?;
    let mut i = 16;
    let mut sec = 0u32;
    let mut us = 0u32;
    if i < b.len() && b[i] == b':' {
        if b.len() < i + 3 {
            return Err(PErr::TooShort);
        }
        sec = digits(b, i + 1, 2, PErr::Second)?;
        i += 3;
        if i < b.len() && (b[i] == b'.' || b[i] == b',') {
            i += 1;
            let start = i;
            let mut frac = String::new();
            while i < b.len() && b[i].is_ascii_digit() {
                if frac.len() < 6 {
                    frac.push(b[i] as char);
                }
                i += 1;
            }
            if i == start {
                return Err(PErr::FracMissing);
            }
            while frac.len() < 6 {
                frac.push('0');
            }
            us = frac.parse().unwrap_or(0);
        }
    }
    if h > 23 {
        return Err(PErr::RangeHour);
    }
    if mi > 59 {
        return Err(PErr::RangeMinute);
    }
    if sec > 59 {
        return Err(PErr::RangeSecond);
    }
    let time = NaiveTime::from_hms_micro_opt(h, mi, sec, us).ok_or(PErr::RangeSecond)?;
    let wall = NaiveDateTime::new(date, time);
    let mut tz = None;
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => {
                tz = Some(Tz::Fixed(0));
                i += 1;
            }
            b'+' | b'-' => {
                let sign: i32 = if b[i] == b'-' { -1 } else { 1 };
                if b.len() < i + 3 {
                    return Err(PErr::TzHour);
                }
                let th = digits(b, i + 1, 2, PErr::TzHour)? as i32;
                i += 3;
                let mut tm = 0i32;
                if i < b.len() {
                    if b[i] == b':' {
                        i += 1;
                    }
                    if b.len() < i + 2 {
                        return Err(PErr::TzMinute);
                    }
                    tm = digits(b, i, 2, PErr::TzMinute)? as i32;
                    i += 2;
                }
                let off = th * 3600 + tm * 60;
                if off >= 86400 {
                    return Err(PErr::RangeTz);
                }
                tz = Some(Tz::Fixed(sign * off));
            }
            _ => return Err(PErr::Extra),
        }
    }
    if i < b.len() {
        return Err(PErr::Extra);
    }
    Ok(DateTime { wall, tz, fold: 0 })
}


/// CPython's `repr(datetime)`: seconds and microseconds only when non-zero, then fold and tzinfo
pub fn datetime_repr(d: &DateTime) -> String {
    let w = &d.wall;
    let mut parts = vec![w.year().to_string(), w.month().to_string(), w.day().to_string(), w.hour().to_string(), w.minute().to_string()];
    let us = w.nanosecond() / 1000;
    if w.second() != 0 || us != 0 {
        parts.push(w.second().to_string());
    }
    if us != 0 {
        parts.push(us.to_string());
    }
    let mut s = format!("datetime.datetime({}", parts.join(", "));
    if d.fold != 0 {
        s += ", fold=1";
    }
    if let Some(tz) = &d.tz {
        s += &format!(", tzinfo={}", tz_repr(tz));
    }
    s + ")"
}

/// CPython's `repr(time)`
pub fn time_repr(t: &chrono::NaiveTime) -> String {
    let mut parts = vec![t.hour().to_string(), t.minute().to_string()];
    let us = t.nanosecond() / 1000;
    if t.second() != 0 || us != 0 {
        parts.push(t.second().to_string());
    }
    if us != 0 {
        parts.push(us.to_string());
    }
    format!("datetime.time({})", parts.join(", "))
}

/// CPython's `repr(timedelta)`: normalized days, seconds, microseconds; `timedelta(0)` when zero
pub fn delta_repr(d: &Duration) -> String {
    let us = micros(d);
    let (days, rest) = (us.div_euclid(86_400_000_000), us.rem_euclid(86_400_000_000));
    let (secs, micro) = (rest / 1_000_000, rest % 1_000_000);
    let mut parts = Vec::new();
    if days != 0 {
        parts.push(format!("days={days}"));
    }
    if secs != 0 {
        parts.push(format!("seconds={secs}"));
    }
    if micro != 0 {
        parts.push(format!("microseconds={micro}"));
    }
    if parts.is_empty() {
        return "datetime.timedelta(0)".into();
    }
    format!("datetime.timedelta({})", parts.join(", "))
}

/// CPython's `repr` of a tzinfo
pub fn tz_repr(t: &Tz) -> String {
    match t {
        Tz::Utc => "datetime.timezone.utc".into(),
        Tz::Fixed(s) => format!("datetime.timezone({})", delta_repr(&Duration::seconds(*s as i64))),
        Tz::Zone(z) => format!("zoneinfo.ZoneInfo(key='{}')", z.name()),
    }
}
