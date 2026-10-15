//! `fractions.Fraction` as a Pydantic field validates it (pydantic-core's fraction validator calls `Fraction(x)`):
//! an int, a float (exactly), a Decimal, a Fraction or a str of CPython 3.12+'s grammar; normalized, shown
//! `n/d` (or `n`) by `str()` and in dumps, `Fraction(n, d)` by `repr()`.
use std::sync::{Arc, OnceLock};

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{One, Signed, Zero};

use super::v::*;

pub struct Fraction {
    pub num: BigInt,
    pub den: BigInt,
}

impl Fraction {
    pub fn new(num: BigInt, den: BigInt) -> Option<Fraction> {
        if den.is_zero() {
            return None;
        }
        let g = num.gcd(&den);
        let (mut num, mut den) = if g.is_zero() { (num, den) } else { (num / &g, den / &g) };
        if den.is_negative() {
            num = -num;
            den = -den;
        }
        Some(Fraction { num, den })
    }
    pub fn text(&self) -> String {
        if self.den.is_one() { self.num.to_string() } else { format!("{}/{}", self.num, self.den) }
    }
    pub fn repr(&self) -> String {
        format!("Fraction({}, {})", self.num, self.den)
    }
}

pub fn value(f: Fraction) -> V {
    V::native(Native::Fraction(Arc::new(f)))
}

/// a finite float exactly (`float.as_integer_ratio()`); None for nan and infinities
pub fn from_f64(x: f64) -> Option<Fraction> {
    if !x.is_finite() {
        return None;
    }
    if x == 0.0 {
        return Fraction::new(BigInt::zero(), BigInt::one());
    }
    let bits = x.to_bits();
    let sign = if bits >> 63 == 1 { -1 } else { 1 };
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & ((1u64 << 52) - 1);
    let (mant, e) = if exp == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp - 1075) };
    let mant = BigInt::from(mant) * sign;
    if e >= 0 {
        Fraction::new(mant << e as usize, BigInt::one())
    } else {
        Fraction::new(mant, BigInt::one() << (-e) as usize)
    }
}

/// why a text is not read as a fraction although it has the form of one
pub enum Bad {
    /// digits other than ASCII, an exponent past 100 000: not reproduced
    Unsupported,
    /// `int()`'s limit on decimal text (4300 digits, CPython's default `sys.set_int_max_str_digits`)
    Limit(usize),
}

fn digits(s: &str) -> Result<BigInt, Bad> {
    let n = s.chars().filter(|c| *c != '_').count();
    if n > 4300 {
        return Err(Bad::Limit(n));
    }
    let mut out = BigInt::zero();
    for c in s.chars().filter(|c| *c != '_') {
        // a non-ASCII decimal digit (CPython accepts any Unicode Nd): not reproduced
        let d = c.to_digit(10).ok_or(Bad::Unsupported)?;
        out = out * 10 + d;
    }
    Ok(out)
}

/// `Fraction(text)`: Ok(None) when the text is not a fraction, Err for a form the runtime does not read or a
/// number past `int()`'s digit limit
pub fn parse(s: &str) -> Result<Option<Fraction>, Bad> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*(?P<sign>[-+]?)(?P<num>\d*|\d+(?:_\d+)*)(?:(?:\s*/\s*(?P<denom>\d+(?:_\d+)*))?|(?:\.(?P<decimal>\d*|\d+(?:_\d+)*))?(?:E(?P<exp>[-+]?\d+(?:_\d+)*))?)\s*$",
        )
        .expect("fraction regex")
    });
    let Some(m) = re.captures(s) else { return Ok(None) };
    // CPython's lookahead `(?=\d|\.\d)` after the sign
    let rest = &s[m.name("num").map(|x| x.start()).unwrap_or(0)..];
    let mut it = rest.chars();
    let ok = match it.next() {
        Some(c) if c.is_numeric() && c != '_' => true,
        Some('.') => it.next().is_some_and(|c| c.is_numeric()),
        _ => false,
    };
    if !ok || m.name("num").is_none() {
        return Ok(None);
    }
    let num_s = m.name("num").map(|x| x.as_str()).unwrap_or("");
    let mut num = if num_s.is_empty() { BigInt::zero() } else { digits(num_s)? };
    let mut den = BigInt::one();
    if let Some(d) = m.name("denom") {
        den = digits(d.as_str())?;
        if den.is_zero() {
            return Ok(None);
        }
    } else {
        if let Some(dec) = m.name("decimal") {
            let dec = dec.as_str().replace('_', "");
            if !dec.is_empty() {
                num = num * BigInt::from(10).pow(dec.chars().count() as u32) + digits(&dec)?;
                den = BigInt::from(10).pow(dec.chars().count() as u32);
            }
        }
        if let Some(e) = m.name("exp") {
            let e: i64 = e.as_str().replace('_', "").parse().map_err(|_| Bad::Unsupported)?;
            if e.unsigned_abs() > 100_000 {
                return Err(Bad::Unsupported);
            }
            if e >= 0 {
                num *= BigInt::from(10).pow(e as u32);
            } else {
                den *= BigInt::from(10).pow((-e) as u32);
            }
        }
    }
    if m.name("sign").is_some_and(|x| x.as_str() == "-") {
        num = -num;
    }
    Ok(Fraction::new(num, den))
}

/// `Fraction(numerator=0, denominator=None)` with ints, or `Fraction(str | float | Decimal | Fraction)`
pub fn call(args: &[V], kwargs: &[(String, V)]) -> R {
    if !kwargs.is_empty() {
        return Err(Exc::type_error("py2axum: Fraction() with keyword arguments is not supported"));
    }
    let made = match args {
        [] => Fraction::new(BigInt::zero(), BigInt::one()),
        [V::Int(n), V::Int(d)] => {
            if *d == 0 {
                return Err(Exc::msg(&ZERO_DIVISION_ERROR, format!("Fraction({n}, 0)")));
            }
            Fraction::new((*n).into(), (*d).into())
        }
        [V::Int(n)] => Fraction::new((*n).into(), BigInt::one()),
        [V::Bool(b)] => Fraction::new((*b as i64).into(), BigInt::one()),
        [V::Native(n)] if matches!(&**n, Native::Fraction(_)) => return Ok(args[0].clone()),
        [V::Float(f)] => match from_f64(*f) {
            Some(x) => Some(x),
            None => return Err(Exc::value_error(format!("cannot convert {} to integer ratio", super::ops::repr(&args[0])?))),
        },
        [x @ (V::Str(_) | V::Decimal(_))] => match parse(&super::ops::str_(x)?) {
            Ok(Some(f)) => Some(f),
            Ok(None) => return Err(Exc::value_error(format!("Invalid literal for Fraction: {}", super::ops::repr(x)?))),
            Err(Bad::Unsupported) => return Err(Exc::runtime("py2axum: a Fraction with non-ASCII digits or an exponent past 100000 is not supported")),
            Err(Bad::Limit(n)) => return Err(Exc::value_error(format!("Exceeds the limit (4300 digits) for integer string conversion: value has {n} digits; use sys.set_int_max_str_digits() to increase the limit"))),
        },
        _ => return Err(Exc::type_error("py2axum: these Fraction() arguments are not supported")),
    };
    Ok(value(made.expect("non-zero denominator")))
}
