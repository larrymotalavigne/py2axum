//! `decimal.Decimal` as CPython's `_pydecimal` computes it in the default context (28 significant
//! digits, ROUND_HALF_EVEN): exact addition and multiplication then rounding, CPython's division
//! algorithm (ideal exponent when exact), `str()`, `quantize`, `round`, and FastAPI's `decimal_encoder`.
//! NaN and infinities are refused.
use std::cmp::Ordering;
use std::sync::Arc;

use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};

use super::v::*;

pub const PREC: usize = 28;

#[derive(Clone, Debug)]
pub struct Dec {
    pub neg: bool,
    pub coeff: BigUint,
    pub exp: i64,
}

fn ten_pow(n: u64) -> BigUint {
    num_traits::pow(BigUint::from(10u32), n as usize)
}

fn digits(c: &BigUint) -> usize {
    if c.is_zero() {
        1
    } else {
        c.to_str_radix(10).len()
    }
}

fn invalid() -> Exc {
    Exc::new(&DECIMAL_INVALID_OPERATION, vec![V::list(vec![V::Class(&DECIMAL_CONVERSION_SYNTAX)])])
}

#[derive(Clone, Copy, PartialEq)]
pub enum Rounding {
    HalfEven,
    HalfUp,
    HalfDown,
    Down,
    Up,
    Floor,
    Ceiling,
    ZeroFiveUp,
}

impl Rounding {
    pub fn of(name: &str) -> R<Rounding> {
        Ok(match name {
            "ROUND_HALF_EVEN" => Rounding::HalfEven,
            "ROUND_HALF_UP" => Rounding::HalfUp,
            "ROUND_HALF_DOWN" => Rounding::HalfDown,
            "ROUND_DOWN" => Rounding::Down,
            "ROUND_UP" => Rounding::Up,
            "ROUND_FLOOR" => Rounding::Floor,
            "ROUND_CEILING" => Rounding::Ceiling,
            "ROUND_05UP" => Rounding::ZeroFiveUp,
            _ => return Err(Exc::type_error("valid values for rounding are: [ROUND_CEILING, ROUND_FLOOR, ROUND_UP, ROUND_DOWN, ROUND_HALF_UP, ROUND_HALF_DOWN, ROUND_HALF_EVEN, ROUND_05UP]")),
        })
    }
}

impl Dec {
    pub fn zero() -> Dec {
        Dec { neg: false, coeff: BigUint::zero(), exp: 0 }
    }

    pub fn from_i64(i: i64) -> Dec {
        Dec { neg: i < 0, coeff: BigUint::from(i.unsigned_abs()), exp: 0 }
    }

    pub fn from_bigint(i: &BigInt, exp: i64) -> Dec {
        Dec { neg: i.sign() == Sign::Minus, coeff: i.magnitude().clone(), exp }
    }

    pub fn signed(&self) -> BigInt {
        let b = BigInt::from(self.coeff.clone());
        if self.neg {
            -b
        } else {
            b
        }
    }

    /// `Decimal(float)`: the exact binary value
    pub fn from_f64(f: f64) -> R<Dec> {
        if !f.is_finite() {
            return Err(Exc::value_error("py2axum: Decimal of a NaN or infinite float is not supported"));
        }
        if f == 0.0 {
            return Ok(Dec { neg: f.is_sign_negative(), coeff: BigUint::zero(), exp: 0 });
        }
        let bits = f.to_bits();
        let neg = bits >> 63 == 1;
        let e = ((bits >> 52) & 0x7ff) as i64;
        let m = bits & ((1u64 << 52) - 1);
        let (mut mant, mut e2) = if e == 0 { (m, -1074) } else { (m | (1u64 << 52), e - 1075) };
        while mant % 2 == 0 && e2 < 0 {
            mant /= 2;
            e2 += 1;
        }
        if e2 >= 0 {
            return Ok(Dec { neg, coeff: BigUint::from(mant) << (e2 as usize), exp: 0 });
        }
        // m * 2^-k = m * 5^k / 10^k
        let k = (-e2) as u32;
        Ok(Dec { neg, coeff: BigUint::from(mant) * num_traits::pow(BigUint::from(5u32), k as usize), exp: -(k as i64) })
    }

    /// `Decimal("...")`
    pub fn parse(s: &str) -> R<Dec> {
        let t = s.trim().replace('_', "");
        if s.trim().starts_with('_') || s.trim().ends_with('_') || s.contains("__") {
            return Err(invalid());
        }
        let (neg, body) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(&t)),
        };
        let low = body.to_ascii_lowercase();
        if matches!(low.as_str(), "nan" | "snan" | "inf" | "infinity") {
            return Err(Exc::value_error("py2axum: Decimal NaN/Infinity is not supported"));
        }
        let (mant, e) = match low.find('e') {
            Some(i) => (&body[..i], body[i + 1..].parse::<i64>().map_err(|_| invalid())?),
            None => (body, 0),
        };
        let (ip, fp) = match mant.split_once('.') {
            Some((a, b)) => (a, b),
            None => (mant, ""),
        };
        if (ip.is_empty() && fp.is_empty()) || !ip.chars().all(|c| c.is_ascii_digit()) || !fp.chars().all(|c| c.is_ascii_digit()) {
            return Err(invalid());
        }
        let ds = format!("{ip}{fp}");
        let coeff = BigUint::parse_bytes(if ds.is_empty() { b"0" } else { ds.as_bytes() }, 10).ok_or_else(invalid)?;
        Ok(Dec { neg, coeff, exp: e - fp.len() as i64 })
    }

    pub fn is_zero(&self) -> bool {
        self.coeff.is_zero()
    }

    /// `_fix`: rounded to the context precision
    pub fn fix(mut self) -> Dec {
        let n = digits(&self.coeff);
        if n > PREC {
            let drop = (n - PREC) as u64;
            self = self.round_drop(drop, Rounding::HalfEven);
            if digits(&self.coeff) > PREC {
                self.coeff /= BigUint::from(10u32);
                self.exp += 1;
            }
        }
        self
    }

    /// drop `k` low digits with a rounding mode (exponent + k)
    fn round_drop(&self, k: u64, mode: Rounding) -> Dec {
        if k == 0 {
            return self.clone();
        }
        let div = ten_pow(k);
        let (q, r) = self.coeff.div_rem(&div);
        // 10^k is even: half of it is exact
        let half = &div / BigUint::from(2u32);
        let cmp_half = (&r).cmp(&half);
        let up = match mode {
            Rounding::Down => false,
            Rounding::Up => !r.is_zero(),
            Rounding::Floor => self.neg && !r.is_zero(),
            Rounding::Ceiling => !self.neg && !r.is_zero(),
            Rounding::HalfUp => cmp_half != Ordering::Less,
            Rounding::HalfDown => cmp_half == Ordering::Greater,
            Rounding::HalfEven => cmp_half == Ordering::Greater || (cmp_half == Ordering::Equal && q.is_odd()),
            Rounding::ZeroFiveUp => !r.is_zero() && (&q % BigUint::from(10u32)).to_u32().is_some_and(|d| d == 0 || d == 5),
        };
        let q = if up { q + BigUint::one() } else { q };
        Dec { neg: self.neg, coeff: q, exp: self.exp + k as i64 }
    }

    fn aligned(a: &Dec, b: &Dec) -> (BigInt, BigInt, i64) {
        let e = a.exp.min(b.exp);
        let x = a.signed() * BigInt::from(ten_pow((a.exp - e) as u64));
        let y = b.signed() * BigInt::from(ten_pow((b.exp - e) as u64));
        (x, y, e)
    }

    pub fn add(&self, o: &Dec) -> Dec {
        let (x, y, e) = Dec::aligned(self, o);
        let s = x + y;
        let mut d = Dec::from_bigint(&s, e);
        if s.is_zero() {
            d.neg = self.neg && o.neg;
        }
        d.fix()
    }

    /// `-d` (a zero comes out positive, as in the default context)
    pub fn negate(&self) -> Dec {
        Dec { neg: !self.neg && !self.coeff.is_zero(), coeff: self.coeff.clone(), exp: self.exp }
    }

    pub fn sub(&self, o: &Dec) -> Dec {
        let mut n = o.clone();
        n.neg = !n.neg;
        self.add(&n)
    }

    pub fn mul(&self, o: &Dec) -> Dec {
        Dec { neg: self.neg != o.neg, coeff: &self.coeff * &o.coeff, exp: self.exp + o.exp }.fix()
    }

    /// `_pydecimal.Decimal.__truediv__`
    pub fn div(&self, o: &Dec) -> R<Dec> {
        if o.is_zero() {
            if self.is_zero() {
                return Err(Exc::new(&DECIMAL_INVALID_OPERATION, vec![V::list(vec![V::Class(&DECIMAL_DIVISION_UNDEFINED)])]));
            }
            return Err(Exc::new(&DECIMAL_DIVISION_BY_ZERO, vec![V::list(vec![V::Class(&DECIMAL_DIVISION_BY_ZERO)])]));
        }
        let neg = self.neg != o.neg;
        if self.is_zero() {
            return Ok(Dec { neg, coeff: BigUint::zero(), exp: self.exp - o.exp }.fix());
        }
        let shift = digits(&o.coeff) as i64 - digits(&self.coeff) as i64 + PREC as i64 + 1;
        let mut exp = self.exp - o.exp - shift;
        let (mut coeff, rem) = if shift >= 0 {
            (&self.coeff * ten_pow(shift as u64)).div_rem(&o.coeff)
        } else {
            self.coeff.div_rem(&(&o.coeff * ten_pow((-shift) as u64)))
        };
        if !rem.is_zero() {
            if (&coeff % BigUint::from(5u32)).is_zero() {
                coeff += BigUint::one();
            }
        } else {
            let ideal = self.exp - o.exp;
            let ten = BigUint::from(10u32);
            while exp < ideal && (&coeff % &ten).is_zero() {
                coeff /= &ten;
                exp += 1;
            }
        }
        Ok(Dec { neg, coeff, exp }.fix())
    }

    /// integer quotient truncated toward zero and remainder with the dividend's sign (`//`, `%`)
    pub fn divmod(&self, o: &Dec) -> R<(Dec, Dec)> {
        if o.is_zero() {
            return Err(Exc::new(&DECIMAL_INVALID_OPERATION, vec![V::list(vec![V::Class(&DECIMAL_DIVISION_UNDEFINED)])]));
        }
        let (x, y, e) = Dec::aligned(self, o);
        let (q, r) = x.abs().div_rem(&y.abs());
        let quot = Dec { neg: self.neg != o.neg && !q.is_zero(), coeff: q.magnitude().clone(), exp: 0 };
        let rem = Dec { neg: self.neg && !r.is_zero(), coeff: r.magnitude().clone(), exp: e };
        Ok((quot, rem.fix()))
    }

    pub fn cmp(&self, o: &Dec) -> Ordering {
        let (x, y, _) = Dec::aligned(self, o);
        x.cmp(&y)
    }

    /// `quantize(exp_of(other), rounding)`
    pub fn quantize(&self, exp: i64, mode: Rounding) -> R<Dec> {
        let d = if exp > self.exp {
            self.round_drop((exp - self.exp) as u64, mode)
        } else {
            Dec { neg: self.neg, coeff: &self.coeff * ten_pow((self.exp - exp) as u64), exp }
        };
        if digits(&d.coeff) > PREC {
            return Err(Exc::new(&DECIMAL_INVALID_OPERATION, vec![V::list(vec![V::Class(&DECIMAL_INVALID_OPERATION)])]));
        }
        Ok(d)
    }

    /// `int(d)`: truncated
    pub fn to_int(&self) -> BigInt {
        if self.exp >= 0 {
            self.signed() * BigInt::from(ten_pow(self.exp as u64))
        } else {
            let q = &self.coeff / ten_pow((-self.exp) as u64);
            let b = BigInt::from(q);
            if self.neg {
                -b
            } else {
                b
            }
        }
    }

    pub fn to_f64(&self) -> f64 {
        self.to_string().parse::<f64>().unwrap_or(f64::NAN)
    }

    pub fn is_integral(&self) -> bool {
        self.exp >= 0 || (&self.coeff % ten_pow((-self.exp) as u64)).is_zero()
    }
}

/// `Decimal.__str__` (scientific notation as CPython chooses it)
impl std::fmt::Display for Dec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ds = self.coeff.to_str_radix(10);
        let left = self.exp + ds.len() as i64;
        let dot = if self.exp <= 0 && left > -6 { left } else { 1 };
        let (ip, fp) = if dot <= 0 {
            ("0".to_string(), format!(".{}{ds}", "0".repeat((-dot) as usize)))
        } else if dot as usize >= ds.len() {
            (format!("{ds}{}", "0".repeat(dot as usize - ds.len())), String::new())
        } else {
            (ds[..dot as usize].to_string(), format!(".{}", &ds[dot as usize..]))
        };
        let e = if left == dot { String::new() } else { format!("E{:+}", left - dot) };
        write!(f, "{}{ip}{fp}{e}", if self.neg { "-" } else { "" })
    }
}

pub fn v(d: Dec) -> V {
    V::Decimal(Arc::new(d))
}

/// a number as a Decimal for mixed arithmetic (`Decimal + int`); float is refused like CPython
pub fn coerce(x: &V) -> Option<Dec> {
    match x {
        V::Decimal(d) => Some((**d).clone()),
        V::Int(i) => Some(Dec::from_i64(*i)),
        V::Bool(b) => Some(Dec::from_i64(*b as i64)),
        _ => None,
    }
}

/// `decimal.Decimal(value=0)`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    if kwargs.iter().any(|(k, _)| k != "value") || args.len() > 1 {
        return Err(Exc::type_error("py2axum: Decimal(value) only (no context)"));
    }
    let x = args.first().or_else(|| kwargs.first().map(|(_, v)| v));
    Ok(v(match x {
        None => Dec::zero(),
        Some(V::Decimal(d)) => (**d).clone(),
        Some(V::Int(i)) => Dec::from_i64(*i),
        Some(V::Bool(b)) => Dec::from_i64(*b as i64),
        Some(V::Float(f)) => Dec::from_f64(*f)?,
        Some(V::Str(s)) => Dec::parse(s)?,
        Some(V::Tuple(_)) => return Err(Exc::type_error("py2axum: Decimal(tuple) is not supported")),
        Some(o) => return Err(Exc::type_error(format!("conversion from {} to Decimal is not supported", o.type_name()))),
    }))
}

/// FastAPI's `decimal_encoder`: int when the exponent is >= 0, else float
/// FastAPI's `decimal_encoder`: `int(d)` when the exponent is not negative, else `float(d)`. An int beyond
/// 64 bits is an OverflowError here (docs/supported.md), never a float in its place.
pub fn jsonable(d: &Dec) -> R {
    if d.exp >= 0 {
        match d.to_int().to_i64() {
            Some(i) => Ok(V::Int(i)),
            None => Err(Exc::msg(&OVERFLOW_ERROR, format!("py2axum: the integer {} is outside the signed 64-bit range", d.to_int()))),
        }
    } else {
        Ok(V::Float(d.to_f64()))
    }
}

/// `format(d, spec)`: '', '[,][.N]f', '.N%' ; others refused
pub fn format(d: &Dec, spec: &str) -> R<String> {
    if spec.is_empty() {
        return Ok(d.to_string());
    }
    let comma = spec.starts_with(',');
    let rest = spec.trim_start_matches(',');
    let (prec, kind) = match rest.strip_prefix('.') {
        Some(r) => {
            let k = r.trim_start_matches(|c: char| c.is_ascii_digit());
            (Some(r[..r.len() - k.len()].parse::<i64>().map_err(|_| Exc::value_error("Invalid format specifier"))?), k)
        }
        None => (None, rest),
    };
    let (val, suffix) = match kind {
        "f" | "F" => (d.clone(), ""),
        "%" => (d.mul(&Dec::from_i64(100)), "%"),
        _ => return Err(Exc::value_error(format!("py2axum: Decimal format '{spec}' is not supported"))),
    };
    let q = match prec {
        Some(p) => val.round_drop_to(-p),
        None => val.clone(),
    };
    let mut s = q.plain();
    if comma {
        let (sign, body) = if let Some(b) = s.strip_prefix('-') { ("-", b.to_string()) } else { ("", s.clone()) };
        let (ip, fp) = match body.split_once('.') {
            Some((a, b)) => (a.to_string(), format!(".{b}")),
            None => (body.clone(), String::new()),
        };
        let mut g = String::new();
        for (i, c) in ip.chars().enumerate() {
            if i > 0 && (ip.len() - i) % 3 == 0 {
                g.push(',');
            }
            g.push(c);
        }
        s = format!("{sign}{g}{fp}");
    }
    Ok(s + suffix)
}

impl Dec {
    /// rounded half-even to exponent `exp` (formatting), never into scientific notation
    fn round_drop_to(&self, exp: i64) -> Dec {
        if exp > self.exp {
            self.round_drop((exp - self.exp) as u64, Rounding::HalfEven)
        } else {
            Dec { neg: self.neg, coeff: &self.coeff * ten_pow((self.exp - exp) as u64), exp }
        }
    }

    /// fixed-point text (format 'f')
    fn plain(&self) -> String {
        let ds = self.coeff.to_str_radix(10);
        let sign = if self.neg { "-" } else { "" };
        if self.exp >= 0 {
            return format!("{sign}{ds}{}", "0".repeat(self.exp as usize));
        }
        let k = (-self.exp) as usize;
        let ds = if ds.len() <= k { format!("{}{ds}", "0".repeat(k + 1 - ds.len())) } else { ds };
        format!("{sign}{}.{}", &ds[..ds.len() - k], &ds[ds.len() - k..])
    }
}

/// Decimal methods
pub fn method(d: &Dec, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    // context= (positional or keyword) and any other keyword: refused, never ignored
    if matches!(name, "quantize" | "to_integral_value" | "to_integral" | "to_integral_exact") {
        let max = if name == "quantize" { 2 } else { 1 };
        if args.len() > max || kwargs.iter().any(|(k, _)| k != "rounding" && !(name == "quantize" && k == "exp")) {
            return Err(Exc::type_error(format!("py2axum: Decimal.{name}() supports exp and rounding only (no context)")));
        }
    }
    match name {
        "quantize" => {
            let target = match args.first() {
                Some(V::Decimal(t)) => t.exp,
                Some(o) => coerce(o).ok_or_else(|| Exc::type_error("conversion to Decimal is not supported"))?.exp,
                None => return Err(Exc::type_error("quantize() missing required argument 'exp'")),
            };
            let mode = match args.get(1).cloned().or_else(|| kw("rounding")) {
                Some(V::Str(s)) => Rounding::of(&s)?,
                None | Some(V::None) => Rounding::HalfEven,
                Some(o) => return Err(Exc::type_error(format!("valid values for rounding are strings, not {}", o.type_name()))),
            };
            Ok(v(d.quantize(target, mode)?))
        }
        "to_integral_value" | "to_integral" => {
            let mode = match args.first().cloned().or_else(|| kw("rounding")) {
                Some(V::Str(s)) => Rounding::of(&s)?,
                _ => Rounding::HalfEven,
            };
            Ok(v(if d.exp >= 0 { d.clone() } else { d.round_drop((-d.exp) as u64, mode) }))
        }
        "is_zero" => Ok(V::Bool(d.is_zero())),
        "is_signed" => Ok(V::Bool(d.neg)),
        "copy_abs" => Ok(v(Dec { neg: false, ..d.clone() })),
        "normalize" => {
            let mut x = d.clone().fix();
            if x.coeff.is_zero() {
                return Ok(v(Dec { neg: x.neg, coeff: BigUint::zero(), exp: 0 }));
            }
            let ten = BigUint::from(10u32);
            while (&x.coeff % &ten).is_zero() {
                x.coeff /= &ten;
                x.exp += 1;
            }
            Ok(v(x))
        }
        _ => Err(Exc::attr_error(format!("'decimal.Decimal' object has no attribute '{name}'"))),
    }
}
