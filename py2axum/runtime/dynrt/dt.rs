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
    pub fn checked_add(&self, d: Duration) -> Option<Self> {
        Some(DateTime { wall: self.wall.checked_add_signed(d)?, tz: self.tz, fold: 0 })
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
    DateTooSmall,
    DateTooLarge,
    TzSign,
    NaN,
    TimeNumTooLarge,
    TimeNegative,
    DurNumber,
    DurTRepeated,
    DurFraction,
    DurTimeUnit,
    DurDateUnit,
    DurDays,
    DurTooLarge,
    DurHoursTooLarge,
    DurNumTooLarge,
}

impl PErr {
    pub fn text(&self) -> &'static str {
        match self {
            PErr::TooShort => "input is too short",
            PErr::DateTooSmall => "dates before 0000 are not supported as unix timestamps",
            PErr::DateTooLarge => "dates after 9999 are not supported as unix timestamps",
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
            PErr::TzSign => "invalid timezone sign",
            PErr::NaN => "NaN values not permitted",
            PErr::TimeNumTooLarge => "numeric times may not exceed 86,399 seconds",
            PErr::TimeNegative => "time in seconds should be positive",
            PErr::DurNumber => "invalid digit in duration",
            PErr::DurTRepeated => "`t` character repeated in duration",
            PErr::DurFraction => "quantity fraction invalid in duration",
            PErr::DurTimeUnit => "quantity invalid in time part of duration",
            PErr::DurDateUnit => "quantity invalid in date part of duration",
            PErr::DurDays => "\"day\" identifier in duration not correctly formatted",
            PErr::DurTooLarge => "durations may not exceed 999,999,999 days",
            PErr::DurHoursTooLarge => "durations may not exceed 999,999,999 hours",
            PErr::DurNumTooLarge => "a numeric value in the duration is too large",
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

/// speedate's numeric timestamps: `[+-]digits` that fit an i64, or a float with a point
/// (`[+-]digits?.digits?` with a digit, then an optional `e[+-]digits`) that stays finite; nothing else
/// (no surrounding spaces, no exponent without a point).
fn numeric(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let body = b.strip_prefix(b"+").or_else(|| b.strip_prefix(b"-")).unwrap_or(b);
    if body.is_empty() {
        return None;
    }
    if body.iter().all(u8::is_ascii_digit) {
        return s.trim_start_matches('+').parse::<i64>().ok().map(|i| i as f64);
    }
    let (mant, exp) = match body.iter().position(|c| *c == b'e' || *c == b'E') {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let dot = mant.iter().position(|c| *c == b'.')?;
    let (int, frac) = (&mant[..dot], &mant[dot + 1..]);
    if !int.iter().chain(frac).all(u8::is_ascii_digit) || int.len() + frac.len() == 0 {
        return None;
    }
    if let Some(e) = exp {
        let digits = e.strip_prefix(b"+").or_else(|| e.strip_prefix(b"-")).unwrap_or(e);
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
    }
    s.trim_start_matches('+').parse::<f64>().ok().filter(|f| f.is_finite())
}

/// Unix timestamp (seconds, or milliseconds above 2e10) to an aware UTC datetime; speedate's bounds (years
/// 0000 to 9999) checked on the seconds.
pub fn from_timestamp(ts: f64) -> Result<DateTime, PErr> {
    let secs_f = if ts.abs() > 2e10 { ts / 1000.0 } else { ts };
    let whole = secs_f.floor();
    if whole < -62_167_219_200.0 {
        return Err(PErr::DateTooSmall);
    }
    if whole > 253_402_300_799.0 {
        return Err(PErr::DateTooLarge);
    }
    // speedate: whole seconds rounded down, microseconds from the fraction cut off towards zero (so -1.0000001
    // is -2 s, -6e-7 is -1 s + 1 µs), a rounded 1e6 carried into the seconds
    let mut whole = whole as i64;
    let mut us = ((secs_f - secs_f.trunc()).abs() * 1e6).round() as i64;
    if us >= 1_000_000 {
        whole += 1;
        us -= 1_000_000;
    }
    let base = chrono::DateTime::from_timestamp(whole, 0).ok_or(PErr::DateTooLarge)?.naive_utc();
    Ok(DateTime::aware(base + Duration::microseconds(us), Tz::Utc))
}

/// speedate `DateTime::parse_str` (RFC 3339 and friends, or a numeric timestamp).
pub fn parse_datetime(s: &str) -> Result<DateTime, PErr> {
    if let Some(ts) = numeric(s) {
        return from_timestamp(ts);
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


/// speedate's `MM[:SS[.ffffff]]` after the hour: (minute, second, microsecond, next index). A missing or
/// non-digit character is "invalid character in ..."; fraction digits beyond 6 are dropped.
fn time_rest(b: &[u8], mut i: usize) -> Result<(u32, u32, u32, usize), PErr> {
    let two = |b: &[u8], i: usize, e: PErr| -> Result<u32, PErr> {
        if b.len() < i + 2 {
            return Err(e);
        }
        digits(b, i, 2, e)
    };
    let mi = two(b, i, PErr::Minute)?;
    i += 2;
    let (mut sec, mut us) = (0, 0);
    if i < b.len() && b[i] == b':' {
        sec = two(b, i + 1, PErr::Second)?;
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
    Ok((mi, sec, us, i))
}

/// speedate `Time::parse_str` (Pydantic's `time` from a str): `HH:MM[:SS[.f]]` then an optional `Z` or
/// `±HH[:]MM`. Returns the wall time and the UTC offset in seconds when one is given.
pub fn parse_time(s: &str) -> Result<(NaiveTime, Option<i32>), PErr> {
    let b = s.as_bytes();
    if b.len() < 5 {
        return Err(PErr::TooShort);
    }
    let h = digits(b, 0, 2, PErr::Hour)?;
    if b[2] != b':' {
        return Err(PErr::TimeSep);
    }
    let (mi, sec, us, mut i) = time_rest(b, 3)?;
    if h > 23 {
        return Err(PErr::RangeHour);
    }
    if mi > 59 {
        return Err(PErr::RangeMinute);
    }
    if sec > 59 {
        return Err(PErr::RangeSecond);
    }
    let mut tz = None;
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => {
                tz = Some(0);
                i += 1;
            }
            b'+' | b'-' => {
                let sign: i32 = if b[i] == b'-' { -1 } else { 1 };
                if b.len() < i + 3 {
                    return Err(PErr::TzHour);
                }
                let th = digits(b, i + 1, 2, PErr::TzHour)? as i32;
                i += 3;
                if i < b.len() && b[i] == b':' {
                    i += 1;
                }
                if b.len() < i + 2 {
                    return Err(PErr::TzMinute);
                }
                let tm = digits(b, i, 2, PErr::TzMinute)? as i32;
                i += 2;
                let off = th * 3600 + tm * 60;
                if off >= 86400 {
                    return Err(PErr::RangeTz);
                }
                tz = Some(sign * off);
            }
            _ => return Err(PErr::TzSign),
        }
    }
    if i < b.len() {
        return Err(PErr::Extra);
    }
    Ok((NaiveTime::from_hms_micro_opt(h, mi, sec, us).ok_or(PErr::RangeSecond)?, tz))
}

/// speedate's numeric time (seconds since midnight, a UTC time): (wall time, offset 0)
pub fn time_from_seconds(x: f64) -> Result<NaiveTime, PErr> {
    if x.is_nan() {
        return Err(PErr::NaN);
    }
    if x < 0.0 {
        return Err(PErr::TimeNegative);
    }
    if x >= 86400.0 {
        return Err(PErr::TimeNumTooLarge);
    }
    let whole = x.trunc() as u32;
    let us = ((x - x.trunc()) * 1e6).round() as u32;
    let (whole, us) = if us >= 1_000_000 { (whole + 1, us - 1_000_000) } else { (whole, us) };
    if whole >= 86400 {
        return Err(PErr::TimeNumTooLarge);
    }
    Ok(NaiveTime::from_num_seconds_from_midnight_opt(whole, us * 1000).ok_or(PErr::TimeNumTooLarge)?)
}

const MAX_DURATION_DAYS: i128 = 999_999_999;

/// A signed duration in microseconds to a TimeDelta, within speedate's 999,999,999 days.
fn duration_us(us: i128) -> Result<Duration, PErr> {
    if us.abs() / 86_400_000_000 > MAX_DURATION_DAYS {
        return Err(PErr::DurTooLarge);
    }
    // beyond i64 microseconds (~106 751 days): seconds, then the rest
    Ok(Duration::seconds(us.div_euclid(1_000_000) as i64) + Duration::microseconds(us.rem_euclid(1_000_000) as i64))
}

/// A TimeDelta in microseconds, without the i64 limit of `num_microseconds`
pub fn micros_wide(d: &Duration) -> i128 {
    d.num_seconds() as i128 * 1_000_000 + d.subsec_nanos() as i128 / 1000
}

/// speedate's numeric duration (seconds, a float rounded to the microsecond)
pub fn duration_from_seconds(x: f64) -> Result<Duration, PErr> {
    if x.is_nan() {
        return Err(PErr::NaN);
    }
    if !x.is_finite() || x.abs() / 86400.0 > (MAX_DURATION_DAYS + 1) as f64 {
        return Err(PErr::DurTooLarge);
    }
    // whole seconds, then the fraction rounded to the microsecond (a huge float keeps its exact seconds)
    let whole = x.trunc();
    duration_us(whole as i128 * 1_000_000 + ((x - whole) * 1e6).round() as i128)
}

/// `digits` of a duration quantity (an overflow is "durations may not exceed 999,999,999 days")
fn dur_number(b: &[u8], i: usize) -> Result<(u64, usize), PErr> {
    match b.get(i) {
        Some(c) if c.is_ascii_digit() => {}
        _ => return Err(PErr::DurNumber),
    }
    let mut v: u64 = 0;
    let mut j = i;
    while let Some(c) = b.get(j).filter(|c| c.is_ascii_digit()) {
        v = v.checked_mul(10).and_then(|v| v.checked_add((c - b'0') as u64)).filter(|v| *v <= u32::MAX as u64).ok_or(PErr::DurNumTooLarge)?;
        j += 1;
    }
    Ok((v, j))
}

/// speedate `Duration::parse_str`: an ISO 8601 duration (`P1Y2M3W4DT5H6M7.5S`, a fraction on the last quantity
/// only, `Y` = 365 days, `M` = 30), `[D[ ]d[ay[s]][,][ ]]HH:MM[:SS[.f]]` (hours unbounded), or `D[ ]d[ay[s]]`;
/// an optional sign in front. Microseconds as signed total.
pub fn parse_duration(s: &str) -> Result<Duration, PErr> {
    let b = s.as_bytes();
    let (neg, start) = match b.first() {
        None => return Err(PErr::TooShort),
        Some(b'+') => (false, 1),
        Some(b'-') => (true, 1),
        _ => (false, 0),
    };
    if start == b.len() {
        return Err(PErr::TooShort);
    }
    let us: i128 = if b.get(start) == Some(&b'P') {
        let (mut days, mut secs, mut micros, mut got_t, mut frac_seen, mut any) = (0i128, 0i128, 0i128, false, false, false);
        let mut i = start + 1;
        while i < b.len() {
            if b[i] == b'T' {
                if got_t {
                    return Err(PErr::DurTRepeated);
                }
                got_t = true;
                i += 1;
                continue;
            }
            let (value, mut j) = dur_number(b, i)?;
            if frac_seen {
                return Err(PErr::DurFraction);
            }
            let mut fraction: Option<f64> = None;
            if matches!(b.get(j), Some(b'.') | Some(b',')) {
                // speedate: digit by digit, as f64
                let (mut f, mut mult) = (0.0f64, 0.1f64);
                j += 1;
                while let Some(c) = b.get(j).filter(|c| c.is_ascii_digit()) {
                    f += (c - b'0') as f64 * mult;
                    mult /= 10.0;
                    j += 1;
                }
                fraction = Some(f);
                frac_seen = true;
            }
            let unit = b.get(j).copied();
            let seconds_per = if got_t {
                match unit {
                    Some(b'H') => 3600i128,
                    Some(b'M') => 60,
                    Some(b'S') => 1,
                    _ => return Err(PErr::DurTimeUnit),
                }
            } else {
                match unit {
                    Some(b'Y') => 365 * 86400i128,
                    Some(b'M') => 30 * 86400,
                    Some(b'W') => 7 * 86400,
                    Some(b'D') => 86400,
                    _ => return Err(PErr::DurDateUnit),
                }
            };
            if seconds_per % 86400 == 0 {
                days += value as i128 * (seconds_per / 86400);
            } else {
                secs += value as i128 * seconds_per;
            }
            if let Some(f) = fraction {
                let extra = f * seconds_per as f64;
                let full = extra.trunc();
                secs += full as i128;
                micros += ((extra - full) * 1_000_000.0).round() as i128;
            }
            any = true;
            i = j + 1;
        }
        if !any {
            return Err(PErr::TooShort);
        }
        days * 86_400_000_000 + secs * 1_000_000 + micros
    } else if b[start..].iter().any(|c| *c == b'd' || *c == b'D') || b.len() - start < 5 {
        // days, then an optional time
        let (days, mut i) = dur_number(b, start).map_err(|e| if e == PErr::DurNumber { PErr::DurNumber } else { e })?;
        if b.get(i) == Some(&b' ') {
            i += 1;
        }
        match b.get(i) {
            Some(b'd') | Some(b'D') => match b.get(i + 1) {
                Some(b'a') | Some(b'A') => match b.get(i + 2) {
                    Some(b'y') | Some(b'Y') => {
                        i += if matches!(b.get(i + 3), Some(b's') | Some(b'S')) { 4 } else { 3 };
                    }
                    _ => return Err(PErr::DurDays),
                },
                _ => i += 1,
            },
            _ => return Err(PErr::DurDays),
        }
        if b.get(i) == Some(&b',') {
            i += 1;
        }
        if b.get(i) == Some(&b' ') {
            i += 1;
        }
        let t = if i < b.len() { duration_time(b, i)? } else { 0 };
        days as i128 * 86_400_000_000 + t
    } else {
        duration_time(b, start)?
    };
    duration_us(if neg { -us } else { us })
}

/// speedate's duration time `H...H:MM[:SS[.f]]` (microseconds): at least 5 characters, any number of hour digits
fn duration_time(b: &[u8], i: usize) -> Result<i128, PErr> {
    if b.len() - i < 5 {
        return Err(PErr::TooShort);
    }
    // the hours are what comes before the first `:` (none: "invalid character in hour", before any range check)
    if !b[i..].contains(&b':') {
        return Err(PErr::Hour);
    }
    let mut j = i;
    let mut hours: i128 = 0;
    while j < b.len() && b[j] != b':' {
        if !b[j].is_ascii_digit() {
            return Err(PErr::Hour);
        }
        hours = hours * 10 + (b[j] - b'0') as i128;
        if hours > MAX_DURATION_DAYS {
            return Err(PErr::DurHoursTooLarge);
        }
        j += 1;
    }
    if j >= b.len() {
        return Err(PErr::Hour);
    }
    let (mi, sec, us, k) = time_rest(b, j + 1)?;
    if mi > 59 {
        return Err(PErr::RangeMinute);
    }
    if sec > 59 {
        return Err(PErr::RangeSecond);
    }
    if k < b.len() {
        return Err(PErr::Extra);
    }
    Ok(((hours * 3600 + mi as i128 * 60 + sec as i128) * 1_000_000) + us as i128)
}

/// Pydantic's JSON form of a timedelta (speedate's `Duration` display): `[-]P[nY][nD][T[nH][nM][n[.f]S]]`, `PT0S`
pub fn delta_iso(d: &Duration) -> String {
    let us = micros_wide(d);
    let neg = us < 0;
    let us = us.abs();
    let mut days = us / 86_400_000_000;
    let rem = us % 86_400_000_000;
    let (secs, frac) = (rem / 1_000_000, rem % 1_000_000);
    let mut s = String::from(if neg { "-P" } else { "P" });
    if days >= 365 {
        s += &format!("{}Y", days / 365);
        days %= 365;
    }
    if days != 0 {
        s += &format!("{days}D");
    }
    if secs != 0 || frac != 0 {
        s += "T";
        let (h, m, sec) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        if h != 0 {
            s += &format!("{h}H");
        }
        if m != 0 {
            s += &format!("{m}M");
        }
        if sec != 0 || frac != 0 {
            if frac != 0 {
                s += &format!("{sec}.{}S", format!("{frac:06}").trim_end_matches('0'));
            } else {
                s += &format!("{sec}S");
            }
        }
    }
    if us == 0 {
        s += "T0S";
    }
    s
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
