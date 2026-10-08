//! `dateutil.relativedelta.relativedelta` (python-dateutil 2.9), relative fields only: years, months,
//! weeks, days, hours, minutes, seconds, microseconds (integers). The absolute fields (year=, day=,
//! weekday=, ...) are refused when transpiling.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};

use super::dt::DateTime;
use super::v::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelDelta {
    pub years: i64,
    pub months: i64,
    pub days: i64,
    pub hours: i64,
    pub minutes: i64,
    pub seconds: i64,
    pub microseconds: i64,
}

const FIELDS: [&str; 7] = ["years", "months", "days", "hours", "minutes", "seconds", "microseconds"];

impl RelDelta {
    fn get(&self, i: usize) -> i64 {
        [self.years, self.months, self.days, self.hours, self.minutes, self.seconds, self.microseconds][i]
    }

    fn from_fields(f: [i64; 7]) -> RelDelta {
        let mut r = RelDelta { years: f[0], months: f[1], days: f[2], hours: f[3], minutes: f[4], seconds: f[5], microseconds: f[6] };
        r.fix();
        r
    }

    /// dateutil's `_fix`: carries toward the larger unit, keeping the sign (days never carry into months)
    fn fix(&mut self) {
        fn carry(small: &mut i64, big: &mut i64, limit: i64) {
            if small.abs() > limit - 1 {
                let s = small.signum();
                let (div, m) = ((*small * s).div_euclid(limit), (*small * s).rem_euclid(limit));
                *small = m * s;
                *big += div * s;
            }
        }
        carry(&mut self.microseconds, &mut self.seconds, 1_000_000);
        carry(&mut self.seconds, &mut self.minutes, 60);
        carry(&mut self.minutes, &mut self.hours, 60);
        carry(&mut self.hours, &mut self.days, 24);
        carry(&mut self.months, &mut self.years, 12);
    }

    fn has_time(&self) -> bool {
        self.hours != 0 || self.minutes != 0 || self.seconds != 0 || self.microseconds != 0
    }

    fn neg(&self) -> RelDelta {
        RelDelta::from_fields(std::array::from_fn(|i| -self.get(i)))
    }

    fn time_delta(&self) -> Duration {
        Duration::days(self.days) + Duration::hours(self.hours) + Duration::minutes(self.minutes) + Duration::seconds(self.seconds) + Duration::microseconds(self.microseconds)
    }

    /// the date with years and months added, the day clamped to the month's length
    fn shift(&self, d: NaiveDate) -> R<NaiveDate> {
        let mut year = d.year() as i64 + self.years;
        let mut month = d.month() as i64;
        if self.months != 0 {
            month += self.months;
            if month > 12 {
                year += 1;
                month -= 12;
            } else if month < 1 {
                year -= 1;
                month += 12;
            }
        }
        if !(1..=9999).contains(&year) {
            return Err(Exc::value_error(format!("year must be in 1..9999, not {year}")));
        }
        let last = (28..=31u32).rev().find(|dd| NaiveDate::from_ymd_opt(year as i32, month as u32, *dd).is_some()).unwrap();
        Ok(NaiveDate::from_ymd_opt(year as i32, month as u32, d.day().min(last)).unwrap())
    }

    fn overflow() -> Exc {
        Exc::msg(&OVERFLOW_ERROR, "date value out of range")
    }

    /// `other + self` (a date or a datetime)
    fn apply(&self, other: &V) -> Option<R> {
        Some(match other {
            V::Date(d) if !self.has_time() => self.shift(*d).and_then(|x| {
                x.checked_add_signed(Duration::days(self.days)).map(V::Date).ok_or_else(RelDelta::overflow)
            }),
            V::Date(d) => self.shift(*d).and_then(|x| {
                NaiveDateTime::new(x, NaiveTime::MIN)
                    .checked_add_signed(self.time_delta())
                    .map(|w| V::DateTime(DateTime::naive(w)))
                    .ok_or_else(RelDelta::overflow)
            }),
            V::DateTime(dt) => self.shift(dt.wall.date()).and_then(|x| {
                NaiveDateTime::new(x, dt.wall.time())
                    .checked_add_signed(self.time_delta())
                    .map(|w| V::DateTime(DateTime { wall: w, tz: dt.tz, fold: 0 }))
                    .ok_or_else(RelDelta::overflow)
            }),
            _ => return None,
        })
    }
}

fn rd(v: &V) -> Option<RelDelta> {
    match v {
        V::Native(n) => match &**n {
            Native::RelDelta(r) => Some(*r),
            _ => None,
        },
        _ => None,
    }
}

fn val(r: RelDelta) -> V {
    V::native(Native::RelDelta(r))
}

fn int_field(name: &str, v: &V) -> R<i64> {
    match v {
        V::Bool(b) => Ok(*b as i64),
        V::Int(i) => Ok(*i),
        V::Float(f) if matches!(name, "years" | "months") => {
            if f.fract() != 0.0 {
                return Err(Exc::value_error("Non-integer years and months are ambiguous and not currently supported."));
            }
            Ok(*f as i64)
        }
        V::Float(_) => Err(Exc::type_error(format!("py2axum: relativedelta({name}=<float>) is not supported (integers only)"))),
        other => Err(Exc::type_error(format!("unsupported operand type(s) for +: 'int' and '{}'", other.type_name()))),
    }
}

/// `relativedelta(years=, months=, weeks=, days=, hours=, minutes=, seconds=, microseconds=)`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: relativedelta(dt1, dt2) is not supported"));
    }
    let mut f = [0i64; 7];
    let mut weeks = 0;
    for (k, v) in kwargs {
        match FIELDS.iter().position(|n| n == k) {
            Some(i) => f[i] = int_field(k, v)?,
            None if k == "weeks" => weeks = int_field(k, v)?,
            None => return Err(Exc::type_error(format!("py2axum: relativedelta({k}=) is not supported"))),
        }
    }
    f[2] += weeks * 7;
    Ok(val(RelDelta::from_fields(f)))
}

/// `+`, `-`, `*`, `==`, `!=` with a relativedelta operand; None when neither operand is one.
pub fn binop(a: &V, op: &str, b: &V) -> Option<R> {
    let (x, y) = (rd(a), rd(b));
    if x.is_none() && y.is_none() {
        return None;
    }
    // date and datetime are C types: their qualified names in the message
    let tn = |v: &V| match v {
        V::Date(_) => "datetime.date".to_string(),
        V::DateTime(_) => "datetime.datetime".to_string(),
        other => other.type_name().to_string(),
    };
    let unsupported = || Err(Exc::type_error(format!("unsupported operand type(s) for {op}: '{}' and '{}'", tn(a), tn(b))));
    Some(match (x, op, y) {
        (Some(p), "+", Some(q)) => Ok(val(RelDelta::from_fields(std::array::from_fn(|i| p.get(i) + q.get(i))))),
        (Some(p), "-", Some(q)) => Ok(val(RelDelta::from_fields(std::array::from_fn(|i| p.get(i) - q.get(i))))),
        (Some(p), "+", None) => p.apply(b).unwrap_or_else(unsupported),
        (None, "+", Some(q)) => q.apply(a).unwrap_or_else(unsupported),
        (None, "-", Some(q)) => q.neg().apply(a).unwrap_or_else(unsupported),
        (Some(p), "*", None) | (None, "*", Some(p)) => {
            let n = if x.is_some() { b } else { a };
            let f = match n {
                V::Int(i) => *i as f64,
                V::Bool(v) => *v as i64 as f64,
                V::Float(f) => *f,
                _ => return Some(unsupported()),
            };
            Ok(val(RelDelta::from_fields(std::array::from_fn(|i| (p.get(i) as f64 * f) as i64))))
        }
        (Some(p), "==", Some(q)) => Ok(V::Bool(p == q)),
        (Some(p), "!=", Some(q)) => Ok(V::Bool(p != q)),
        (_, "==", _) => Ok(V::Bool(false)),
        (_, "!=", _) => Ok(V::Bool(true)),
        _ => unsupported(),
    })
}

pub fn neg(r: &RelDelta) -> V {
    val(r.neg())
}

pub fn truthy(r: &RelDelta) -> bool {
    *r != RelDelta::default()
}

pub fn attr(r: &RelDelta, name: &str) -> Option<V> {
    if let Some(i) = FIELDS.iter().position(|n| *n == name) {
        return Some(V::Int(r.get(i)));
    }
    match name {
        // int(self.days / 7): truncated toward zero
        "weeks" => Some(V::Int(r.days / 7)),
        "leapdays" => Some(V::Int(0)),
        "year" | "month" | "day" | "weekday" | "hour" | "minute" | "second" | "microsecond" => Some(V::None),
        _ => None,
    }
}

/// `relativedelta(years=+1, days=-2)`: each non-zero field as `{:+g}`
pub fn repr(r: &RelDelta) -> String {
    let parts: Vec<String> = FIELDS
        .iter()
        .enumerate()
        .filter(|(i, _)| r.get(*i) != 0)
        .map(|(i, n)| format!("{n}={}", super::ops::format_spec(&V::Int(r.get(i)), "+g").unwrap_or_default()))
        .collect();
    format!("relativedelta({})", parts.join(", "))
}
