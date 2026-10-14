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
/// the default context's exponent limits (`Emax`, `Etop` = Emax - prec + 1, `Etiny` = Emin - prec + 1)
const EMAX: i64 = 999_999;
const ETOP: i64 = EMAX - PREC as i64 + 1;
const ETINY: i64 = -EMAX - PREC as i64 + 1;
/// libmpdec's limits on a literal (`MPD_MAX_EMAX`, `MPD_MIN_ETINY`): past them, ConversionSyntax
const MAX_ADJ: i128 = 999_999_999_999_999_999;
const MIN_EXP: i128 = -1_999_999_999_999_999_997;

#[derive(Clone, Debug)]
pub struct Dec {
    pub neg: bool,
    pub coeff: BigUint,
    pub exp: i64,
}

fn ten_pow(n: u64) -> BigUint {
    num_traits::pow(BigUint::from(10u32), n as usize)
}

/// number of decimal digits, without printing the number (printing is quadratic in num-bigint)
pub fn digits(c: &BigUint) -> usize {
    if let Some(u) = c.to_u64() {
        return if u == 0 { 1 } else { u.ilog10() as usize + 1 };
    }
    // floor(log10(c)) is est - 1 or est: one comparison settles it
    let est = ((c.bits() - 1) as f64 * std::f64::consts::LOG10_2) as usize + 1;
    if *c >= ten_pow(est as u64) {
        est + 1
    } else if *c < ten_pow(est as u64 - 1) {
        // a floating-point estimate one too high (never by more)
        est - 1
    } else {
        est
    }
}

/// the decimal digits of `s` (ASCII digits only) as a number, in O(M(n) log n) (num-bigint's
/// `parse_bytes` is quadratic: 1 s for a million digits)
fn parse_digits(s: &[u8]) -> BigUint {
    if s.len() <= 4096 {
        return BigUint::parse_bytes(if s.is_empty() { b"0" } else { s }, 10).unwrap_or_default();
    }
    let low = s.len() / 2;
    let (hi, lo) = s.split_at(s.len() - low);
    parse_digits(hi) * ten_pow(low as u64) + parse_digits(lo)
}

/// the number of trailing zero digits of a non-zero number, and the number without them (doubling steps:
/// one division per zero is quadratic)
pub fn strip_zeros(c: &BigUint) -> (BigUint, i64) {
    let (mut c, mut k, mut step) = (c.clone(), 0i64, 1u64);
    if c.is_zero() {
        return (c, 0);
    }
    loop {
        let (q, r) = c.div_rem(&ten_pow(step));
        if r.is_zero() {
            c = q;
            k += step as i64;
            step *= 2;
        } else if step == 1 {
            return (c, k);
        } else {
            step = 1;
        }
    }
}

fn overflow() -> Exc {
    Exc::new(&DECIMAL_OVERFLOW, vec![V::list(vec![V::Class(&DECIMAL_OVERFLOW)])])
}

fn invalid_operation() -> Exc {
    Exc::new(&DECIMAL_INVALID_OPERATION, vec![V::list(vec![V::Class(&DECIMAL_INVALID_OPERATION)])])
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
        // libmpdec's bounds on the exponent (CPython: ConversionSyntax past them); i128, so nothing wraps
        let exp = e as i128 - fp.len() as i128;
        let lead = ds.trim_start_matches('0').len().max(1) as i128;
        if exp + lead - 1 > MAX_ADJ || exp < MIN_EXP {
            return Err(invalid());
        }
        let coeff = parse_digits(ds.as_bytes());
        Ok(Dec { neg, coeff, exp: exp as i64 })
    }

    /// `adjusted()`: the exponent of the most significant digit
    pub fn adjusted(&self) -> i64 {
        self.exp + digits(&self.coeff) as i64 - 1
    }

    pub fn is_zero(&self) -> bool {
        self.coeff.is_zero()
    }

    /// `_fix` in the default context: rounded to 28 digits (half even), the exponent kept within Etiny and
    /// Emax (Overflow past it; below Etiny the value is rounded at Etiny, as subnormals are)
    pub fn fix(self) -> R<Dec> {
        if self.coeff.is_zero() {
            return Ok(Dec { exp: self.exp.clamp(ETINY, EMAX), ..self });
        }
        let n = digits(&self.coeff) as i64;
        let mut exp_min = n + self.exp - PREC as i64;
        if exp_min > ETOP {
            return Err(overflow());
        }
        exp_min = exp_min.max(ETINY);
        if self.exp >= exp_min {
            return Ok(self);
        }
        let mut d = self.round_drop((exp_min - self.exp) as u64, Rounding::HalfEven);
        if digits(&d.coeff) > PREC {
            d.coeff /= BigUint::from(10u32);
            d.exp += 1;
            if d.exp > ETOP {
                return Err(overflow());
            }
        }
        Ok(d)
    }

    /// drop `k` low digits with a rounding mode (exponent + k)
    fn round_drop(&self, k: u64, mode: Rounding) -> Dec {
        if k == 0 {
            return self.clone();
        }
        let (q, r, cmp_half) = if k > digits(&self.coeff) as u64 + 1 {
            // every digit dropped and the value below half a unit: no 10^k (k may come from a request)
            (BigUint::zero(), self.coeff.clone(), Ordering::Less)
        } else {
            let div = ten_pow(k);
            let (q, r) = self.coeff.div_rem(&div);
            // 10^k is even: half of it is exact
            let half = &div / BigUint::from(2u32);
            let c = (&r).cmp(&half);
            (q, r, c)
        };
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

    /// both at the smaller exponent; callers make sure the exponents are close (the gap is bounded by the
    /// digits of the operands, never by an exponent a request chose)
    fn aligned(a: &Dec, b: &Dec) -> (BigInt, BigInt, i64) {
        let e = a.exp.min(b.exp);
        let x = a.signed() * BigInt::from(ten_pow((a.exp - e) as u64));
        let y = b.signed() * BigInt::from(ten_pow((b.exp - e) as u64));
        (x, y, e)
    }

    /// `self` at a smaller exponent (`_rescale` without rounding)
    fn widen(&self, exp: i64) -> Dec {
        if self.coeff.is_zero() {
            return Dec { exp, ..self.clone() };
        }
        Dec { neg: self.neg, coeff: &self.coeff * ten_pow((self.exp - exp) as u64), exp }
    }

    /// `_pydecimal.Decimal.__add__`: zero operands rescaled, then `_normalize` (an operand far below the
    /// other's precision is replaced by a single digit just under it: the same rounding, no 10^gap)
    pub fn add(&self, o: &Dec) -> R<Dec> {
        let exp = self.exp.min(o.exp);
        match (self.coeff.is_zero(), o.coeff.is_zero()) {
            (true, true) => return Dec { neg: self.neg && o.neg, coeff: BigUint::zero(), exp }.fix(),
            (true, false) => return o.widen(exp.max(o.exp - PREC as i64 - 1)).fix(),
            (false, true) => return self.widen(exp.max(self.exp - PREC as i64 - 1)).fix(),
            _ => {}
        }
        let (tmp, other) = if self.exp < o.exp { (o, self) } else { (self, o) };
        let tmp_len = digits(&tmp.coeff) as i64;
        let floor = tmp.exp + (-1i64).min(tmp_len - PREC as i64 - 2);
        let small;
        let other = if digits(&other.coeff) as i64 + other.exp - 1 < floor {
            small = Dec { neg: other.neg, coeff: BigUint::one(), exp: floor };
            &small
        } else {
            other
        };
        let (x, y, e) = Dec::aligned(tmp, other);
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

    pub fn sub(&self, o: &Dec) -> R<Dec> {
        let mut n = o.clone();
        n.neg = !n.neg;
        self.add(&n)
    }

    pub fn mul(&self, o: &Dec) -> R<Dec> {
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
            return Dec { neg, coeff: BigUint::zero(), exp: self.exp - o.exp }.fix();
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
        Dec { neg, coeff, exp }.fix()
    }

    /// integer quotient truncated toward zero and remainder with the dividend's sign (`//`, `%`)
    pub fn divmod(&self, o: &Dec) -> R<(Dec, Dec)> {
        if o.is_zero() {
            return Err(Exc::new(&DECIMAL_INVALID_OPERATION, vec![V::list(vec![V::Class(&DECIMAL_DIVISION_UNDEFINED)])]));
        }
        // `_pydecimal.Decimal._divide`: decided on the adjusted exponents before any alignment
        let neg = self.neg != o.neg;
        let ideal = self.exp.min(o.exp);
        let expdiff = if self.coeff.is_zero() { i64::MIN } else { self.adjusted() - o.adjusted() };
        if expdiff <= -2 {
            return Ok((Dec { neg, coeff: BigUint::zero(), exp: 0 }, self.widen(ideal).fix()?));
        }
        if expdiff <= PREC as i64 {
            let (x, y, _) = Dec::aligned(self, o);
            let (q, r) = x.abs().div_rem(&y.abs());
            if digits(q.magnitude()) <= PREC {
                let quot = Dec { neg, coeff: q.magnitude().clone(), exp: 0 };
                let rem = Dec { neg: self.neg, coeff: r.magnitude().clone(), exp: ideal };
                return Ok((quot, rem.fix()?));
            }
        }
        Err(Exc::new(&DECIMAL_INVALID_OPERATION, vec![V::list(vec![V::Class(&DECIMAL_DIVISION_IMPOSSIBLE)])]))
    }

    /// compared on the sign, then the adjusted exponent: digits are aligned only when those are equal
    pub fn cmp(&self, o: &Dec) -> Ordering {
        let (za, zb) = (self.coeff.is_zero(), o.coeff.is_zero());
        let sign = |d: &Dec, z: bool| if z { 0 } else if d.neg { -1 } else { 1 };
        let (sa, sb) = (sign(self, za), sign(o, zb));
        if sa != sb || sa == 0 {
            return sa.cmp(&sb);
        }
        let by_size = self.adjusted().cmp(&o.adjusted()).then_with(|| {
            let (x, y, _) = Dec::aligned(&Dec { neg: false, ..self.clone() }, &Dec { neg: false, ..o.clone() });
            x.cmp(&y)
        });
        if sa < 0 {
            by_size.reverse()
        } else {
            by_size
        }
    }

    /// `quantize(exp_of(other), rounding)`
    pub fn quantize(&self, exp: i64, mode: Rounding) -> R<Dec> {
        // `_pydecimal.Decimal.quantize`: the bounds checked before the coefficient is scaled
        if !(ETINY..=EMAX).contains(&exp) {
            return Err(invalid_operation());
        }
        if self.coeff.is_zero() {
            return Dec { exp, ..self.clone() }.fix();
        }
        let adj = self.adjusted();
        if adj > EMAX || adj - exp + 1 > PREC as i64 {
            return Err(invalid_operation());
        }
        let d = if exp > self.exp { self.round_drop((exp - self.exp) as u64, mode) } else { self.widen(exp) };
        if digits(&d.coeff) > PREC || (!d.coeff.is_zero() && d.adjusted() > EMAX) {
            return Err(invalid_operation());
        }
        d.fix()
    }

    /// `int(d)` when it fits in 64 bits (sizes checked on the exponent first: `int(Decimal("1e999999999"))`
    /// would build a billion-digit number)
    pub fn to_i64(&self) -> Option<i64> {
        if self.coeff.is_zero() || (self.exp < 0 && -self.exp > digits(&self.coeff) as i64) {
            return Some(0);
        }
        if self.adjusted() > 19 {
            return None;
        }
        self.to_int().to_i64()
    }

    /// `int(d)`: truncated (callers bound the size: `to_i64`)
    pub fn to_int(&self) -> BigInt {
        if self.exp < 0 && -self.exp > digits(&self.coeff) as i64 {
            return BigInt::zero();
        }
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
        if self.exp >= 0 || self.coeff.is_zero() {
            return true;
        }
        // 10^k past the digits: the remainder is the coefficient itself
        -self.exp <= digits(&self.coeff) as i64 && (&self.coeff % ten_pow((-self.exp) as u64)).is_zero()
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
        match d.to_i64() {
            Some(i) => Ok(V::Int(i)),
            None => Err(Exc::msg(&OVERFLOW_ERROR, format!("py2axum: the integer {d} is outside the signed 64-bit range"))),
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
        "%" => (d.mul(&Dec::from_i64(100))?, "%"),
        _ => return Err(Exc::value_error(format!("py2axum: Decimal format '{spec}' is not supported"))),
    };
    let q = match prec {
        Some(p) => val.round_drop_to(-p)?,
        None => val.clone(),
    };
    let mut s = q.plain()?;
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
    fn round_drop_to(&self, exp: i64) -> R<Dec> {
        if exp > self.exp {
            Ok(self.round_drop((exp - self.exp) as u64, Rounding::HalfEven))
        } else {
            // as many zeros as the text will hold: MemoryError (as CPython) rather than a failed allocation
            super::ops::repeat_room(1, (self.exp - exp) as usize, "cannot fit 'int' into an index-sized integer")?;
            Ok(self.widen(exp))
        }
    }

    /// fixed-point text (format 'f')
    fn plain(&self) -> R<String> {
        let sign = if self.neg { "-" } else { "" };
        let zeros = if self.coeff.is_zero() { 0 } else { self.exp.max(-self.exp) };
        super::ops::repeat_room(1, zeros as usize, "cannot fit 'int' into an index-sized integer")?;
        let ds = self.coeff.to_str_radix(10);
        if self.exp >= 0 {
            return Ok(format!("{sign}{ds}{}", "0".repeat(self.exp as usize)));
        }
        let k = (-self.exp) as usize;
        let ds = if ds.len() <= k { format!("{}{ds}", "0".repeat(k + 1 - ds.len())) } else { ds };
        Ok(format!("{sign}{}.{}", &ds[..ds.len() - k], &ds[ds.len() - k..]))
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
            let mut x = d.clone().fix()?;
            if x.coeff.is_zero() {
                return Ok(v(Dec { neg: x.neg, coeff: BigUint::zero(), exp: 0 }));
            }
            let ten = BigUint::from(10u32);
            // at most 28 digits after fix(); the exponent stays within Emax (clamp off, as the default context)
            while (&x.coeff % &ten).is_zero() && x.exp < EMAX {
                x.coeff /= &ten;
                x.exp += 1;
            }
            Ok(v(x))
        }
        _ => Err(Exc::attr_error(format!("'decimal.Decimal' object has no attribute '{name}'"))),
    }
}
