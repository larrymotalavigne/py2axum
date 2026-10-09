//! `icalendar` 7.0 (`Calendar`, `Event`, `Alarm`): `add(name, value)` of a text, URI, date or duration
//! property, `add_component(c)`, `to_ical()`. Its rules, read in its source: properties sorted by the class's
//! `canonical_order` then alphabetically (repeated names keep their order), TEXT escaping (`escape_char`),
//! `VALUE=DATE` dates, `vDuration`, content lines folded at 75 octets (`foldline`), CRLF line ends.
//! Anything else (parameters, datetimes, other property types, reading properties back) is refused.

use std::sync::Arc;

use parking_lot::Mutex;

use super::v::*;

pub struct Comp {
    pub name: &'static str,
    /// (upper-case name, `;PARAM=...` text, encoded value), in insertion order
    props: Mutex<Vec<(String, &'static str, String)>>,
    subs: Mutex<Vec<Arc<Comp>>>,
}

pub fn new(kind: &str) -> R {
    let name = match kind {
        "Calendar" => "VCALENDAR",
        "Event" => "VEVENT",
        "Alarm" => "VALARM",
        _ => return Err(Exc::type_error(format!("py2axum: icalendar.{kind} is not supported"))),
    };
    Ok(V::native(Native::ICal(Arc::new(Comp { name, props: Mutex::new(Vec::new()), subs: Mutex::new(Vec::new()) }))))
}

pub fn type_name(c: &Comp) -> &'static str {
    match c.name {
        "VCALENDAR" => "Calendar",
        "VEVENT" => "Event",
        _ => "Alarm",
    }
}

fn canonical(name: &str) -> &'static [&'static str] {
    match name {
        "VCALENDAR" => &["VERSION", "PRODID", "CALSCALE", "METHOD", "DESCRIPTION", "X-WR-CALDESC", "NAME", "X-WR-CALNAME"],
        "VEVENT" => &["SUMMARY", "DTSTART", "DTEND", "DURATION", "DTSTAMP", "UID", "RECURRENCE-ID", "SEQUENCE", "RRULE", "RDATE", "EXDATE"],
        _ => &[],
    }
}

/// `escape_char` (the order of the replacements matters)
fn escape(s: &str) -> String {
    s.replace("\\N", "\n").replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// `vDuration.to_ical` on Python's normalized timedelta (days, 0 <= seconds < 86400; microseconds ignored)
fn duration(td: &chrono::TimeDelta) -> String {
    let us = td.num_microseconds().unwrap_or(i64::MAX) as i128;
    let norm = |us: i128| (us.div_euclid(86_400_000_000), us.rem_euclid(86_400_000_000) / 1_000_000);
    let (mut days, mut secs) = norm(us);
    let mut sign = "";
    if days < 0 {
        sign = "-";
        (days, secs) = norm(-us);
    }
    let mut time = String::new();
    if secs != 0 {
        time.push('T');
        let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
        if h != 0 {
            time += &format!("{h}H");
        }
        if m != 0 || (h != 0 && s != 0) {
            time += &format!("{m}M");
        }
        if s != 0 {
            time += &format!("{s}S");
        }
    }
    if days == 0 && !time.is_empty() {
        format!("{sign}P{time}")
    } else {
        format!("{sign}P{}D{time}", days.abs())
    }
}

fn encode(name: &str, value: &V) -> R<(&'static str, String)> {
    let lower = name.to_lowercase();
    let text = matches!(lower.as_str(), "prodid" | "version" | "calscale" | "method" | "summary" | "description"
        | "location" | "uid" | "action" | "status" | "comment" | "class" | "transp" | "contact")
        || lower.starts_with("x-");
    let ddd = matches!(lower.as_str(), "dtstart" | "dtend" | "due" | "trigger" | "duration");
    Ok(match value {
        V::Str(s) if text => ("", escape(s)),
        V::Str(s) if lower == "url" => ("", s.to_string()),
        V::Date(d) if ddd && lower != "duration" && lower != "trigger" => (";VALUE=DATE", d.format("%Y%m%d").to_string()),
        V::Delta(td) if ddd && lower != "dtstart" && lower != "dtend" && lower != "due" => ("", duration(td)),
        _ => return Err(Exc::type_error(format!(
            "py2axum: icalendar property {name} with a {} value is not supported (text, URL, date, timedelta)", value.type_name()))),
    })
}

/// `foldline(line)`: 75 octets per physical line
fn fold(line: &str, out: &mut String) {
    if line.is_ascii() {
        let b = line.as_bytes();
        for (i, chunk) in b.chunks(74).enumerate() {
            if i > 0 {
                out.push_str("\r\n ");
            }
            out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        }
        return;
    }
    let mut count = 0;
    for ch in line.chars() {
        let n = ch.len_utf8();
        count += n;
        if count >= 75 {
            out.push_str("\r\n ");
            count = n;
        }
        out.push(ch);
    }
}

fn render(c: &Comp, out: &mut String) {
    fold(&format!("BEGIN:{}", c.name), out);
    out.push_str("\r\n");
    let props = c.props.lock().clone();
    let mut names: Vec<&str> = Vec::new();
    for (n, _, _) in &props {
        if !names.contains(&n.as_str()) {
            names.push(n);
        }
    }
    let order = canonical(c.name);
    let mut head: Vec<&str> = names.iter().copied().filter(|n| order.contains(n)).collect();
    head.sort_by_key(|n| order.iter().position(|o| o == n));
    let mut tail: Vec<&str> = names.iter().copied().filter(|n| !order.contains(n)).collect();
    tail.sort();
    for name in head.into_iter().chain(tail) {
        for (n, params, value) in props.iter().filter(|(n, _, _)| n == name) {
            fold(&format!("{n}{params}:{value}"), out);
            out.push_str("\r\n");
        }
    }
    for s in c.subs.lock().iter() {
        render(s, out);
    }
    fold(&format!("END:{}", c.name), out);
    out.push_str("\r\n");
}

pub fn method(c: &Arc<Comp>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if !kwargs.is_empty() {
        return Err(Exc::type_error(format!("py2axum: icalendar {name}() with keyword arguments is not supported")));
    }
    let bad = || Exc::type_error(format!("py2axum: icalendar {}.{name}() is not supported with these arguments (add(name, value), \
                                          add_component(component), to_ical())", type_name(c)));
    match name {
        "add" => {
            let [n, v] = args.as_slice() else { return Err(bad()) };
            let n = super::ops::str_(n)?;
            let (params, value) = encode(&n, v)?;
            c.props.lock().push((n.to_uppercase(), params, value));
            Ok(V::None)
        }
        "add_component" => match args.as_slice() {
            [V::Native(x)] if matches!(&**x, Native::ICal(_)) => {
                let Native::ICal(sub) = &**x else { unreachable!() };
                c.subs.lock().push(sub.clone());
                Ok(V::None)
            }
            _ => Err(bad()),
        },
        "to_ical" if args.is_empty() => {
            let mut out = String::new();
            render(c, &mut out);
            Ok(V::Bytes(Arc::from(out.into_bytes())))
        }
        _ => Err(bad()),
    }
}
