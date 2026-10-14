//! `complex` as a Pydantic field validates it: a number, or a str of CPython's `complex()` grammar
//! (`complexobject.c`); `repr()` like CPython, dumped in JSON mode as pydantic-core writes it (its floats in
//! Rust's `Display`: `100000000000000000000+0.0000001j`).
use super::v::*;

pub fn value(re: f64, im: f64) -> V {
    V::native(Native::Complex(re, im))
}

/// a float as `repr()` shows it inside a complex (no `.0`)
fn py_part(x: f64) -> R<String> {
    let s = super::ops::repr(&V::Float(x))?;
    Ok(s.strip_suffix(".0").map(str::to_string).unwrap_or(s))
}

pub fn repr(re: f64, im: f64) -> R<String> {
    let ims = py_part(im)?;
    let ims = if ims.starts_with('-') { ims } else { format!("+{ims}") };
    if re == 0.0 && re.is_sign_positive() {
        return Ok(format!("{}j", ims.trim_start_matches('+')));
    }
    Ok(format!("({}{ims}j)", py_part(re)?))
}

/// pydantic-core's JSON form
pub fn json(re: f64, im: f64) -> String {
    let ims = format!("{im}");
    if re == 0.0 {
        return format!("{ims}j");
    }
    // pydantic 2.12 wrote the sign from `im >= 0` (`-1+-0j`)
    let plus = if super::pydantic_before(2, 13) { im >= 0.0 || im.is_nan() } else { !ims.starts_with('-') };
    format!("{re}{}{ims}j", if plus { "+" } else { "" })
}

/// a float token at the start of `s` (CPython's `_Py_dg_strtod` plus inf/nan): (value, bytes read)
fn float_prefix(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let lower = s[i..].to_ascii_lowercase();
    for word in ["infinity", "inf", "nan"] {
        if lower.starts_with(word) {
            let end = i + word.len();
            let neg = b.first() == Some(&b'-');
            let v = if word == "nan" { f64::NAN } else if neg { f64::NEG_INFINITY } else { f64::INFINITY };
            return Some((if word == "nan" && neg { -v } else { v }, end));
        }
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - f;
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let d = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > d {
            i = j;
        }
    }
    s[..i].parse::<f64>().ok().map(|v| (v, i))
}

/// `complex(text)`: Ok(None) when the text is not a complex, Err(()) for digits other than ASCII
pub fn parse(text: &str) -> Result<Option<(f64, f64)>, ()> {
    if text.chars().any(|c| c.is_numeric() && !c.is_ascii_digit()) {
        return Err(());
    }
    // underscores only between digits, then removed (`_Py_string_to_number_with_underscores`)
    let chars: Vec<char> = text.chars().collect();
    for (k, c) in chars.iter().enumerate() {
        if *c == '_' && !(k > 0 && chars[k - 1].is_ascii_digit() && chars.get(k + 1).is_some_and(|n| n.is_ascii_digit())) {
            return Ok(None);
        }
    }
    let cleaned: String = chars.into_iter().filter(|c| *c != '_').collect();
    let mut s = cleaned.trim();
    let bracket = s.starts_with('(');
    if bracket {
        s = s[1..].trim_start();
    }
    let (mut re, mut im) = (0.0, 0.0);
    match float_prefix(s) {
        Some((x, n)) => {
            let rest = &s[n..];
            if rest.starts_with(['+', '-']) {
                re = x;
                match float_prefix(rest) {
                    Some((y, m)) => {
                        im = y;
                        s = &rest[m..];
                    }
                    None => {
                        im = if rest.starts_with('-') { -1.0 } else { 1.0 };
                        s = &rest[1..];
                    }
                }
                if !s.starts_with(['j', 'J']) {
                    return Ok(None);
                }
                s = &s[1..];
            } else if rest.starts_with(['j', 'J']) {
                im = x;
                s = &rest[1..];
            } else {
                re = x;
                s = rest;
            }
        }
        None => {
            if (s.starts_with('+') || s.starts_with('-')) && s[1..].starts_with(['j', 'J']) {
                im = if s.starts_with('-') { -1.0 } else { 1.0 };
                s = &s[2..];
            } else if s.starts_with(['j', 'J']) {
                im = 1.0;
                s = &s[1..];
            } else {
                return Ok(None);
            }
        }
    }
    s = s.trim_start();
    if bracket {
        let Some(r) = s.strip_prefix(')') else { return Ok(None) };
        s = r.trim_start();
    }
    Ok(if s.is_empty() { Some((re, im)) } else { None })
}
