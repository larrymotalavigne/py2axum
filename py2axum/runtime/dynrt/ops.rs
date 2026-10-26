//! Python operators and conversions on `V` (CPython semantics).
use std::sync::Arc;

use chrono::Duration;
use indexmap::IndexMap;
use parking_lot::Mutex;

use super::dt::{self, DateTime};
use super::orm;
use super::v::*;

// ---------------------------------------------------------------- truthiness

/// a result Row, as the tuple it is
pub fn row_tuple(v: &V) -> Option<V> {
    match v {
        V::Native(n) => match &**n {
            Native::Row(_, vals) => Some(V::Tuple(Arc::new(vals.to_vec()))),
            _ => None,
        },
        _ => None,
    }
}

/// An integral float as a Python int (int(), round(), math.floor/ceil/trunc): CPython's errors for NaN and
/// infinities, an OverflowError beyond 64 bits (`as` would saturate in silence).
pub fn float_to_int(x: f64) -> R {
    if x.is_nan() {
        return Err(Exc::value_error("cannot convert float NaN to integer"));
    }
    if x.is_infinite() {
        return Err(Exc::msg(&OVERFLOW_ERROR, "cannot convert float infinity to integer"));
    }
    if !(-9.223_372_036_854_775_808e18..9.223_372_036_854_775_808e18).contains(&x) {
        return Err(Exc::msg(&OVERFLOW_ERROR, "py2axum: integer beyond 64 bits"));
    }
    Ok(V::Int(x as i64))
}

/// A value that behaves as a tuple in sequence operations: a SQLAlchemy Row, a NamedTuple instance (its own
/// repr, str and attributes aside).
pub fn seq_tuple(v: &V) -> Option<V> {
    match v {
        V::Inst(i) if i.desc.x.ntuple => Some(V::Tuple(Arc::new(i.vals.read().clone()))),
        _ => row_tuple(v),
    }
}

pub fn truthy(v: &V) -> R<bool> {
    if matches!(v, V::Native(n) if matches!(&**n, Native::NotImplemented)) {
        // 3.14: a TypeError; before, True (CPython's DeprecationWarning is not printed: docs/reference/python.md)
        if super::python() >= (3, 14) {
            return Err(Exc::type_error("NotImplemented should not be used in a boolean context"));
        }
        return Ok(true);
    }
    if let Some(r) = super::proto::truthy(v) {
        return r;
    }
    if let Some(s) = super::pgpy::as_str(v) {
        return truthy(&s);
    }
    if let Some(t) = seq_tuple(v) {
        return truthy(&t);
    }
    if let V::Native(n) = v {
        if let Native::CtCache(c) = &**n {
            return Ok(super::ctools::len(c)? > 0);
        }
    }
    Ok(match v {
        V::Unbound => return Err(Exc::msg(&UNBOUND_LOCAL_ERROR, "local variable referenced before assignment")),
        V::None => false,
        V::Bool(b) => *b,
        V::Int(i) => *i != 0,
        V::Float(f) => *f != 0.0,
        V::Decimal(d) => !d.is_zero(),
        V::Str(s) => !s.is_empty(),
        V::Bytes(b) => !b.is_empty(),
        V::List(l) => !l.lock().is_empty(),
        V::Tuple(t) => !t.is_empty(),
        V::Dict(d) => match super::stripe::list_data(d) {
            Some(data) => return truthy(&data),
            None => !d.lock().is_empty(),
        },
        V::Set(s) | V::Frozen(s) => !s.lock().is_empty(),
        V::Delta(d) => !d.is_zero(),
        V::Native(n) if matches!(&**n, Native::RelDelta(_)) => match &**n {
            Native::RelDelta(r) => super::reldelta::truthy(r),
            _ => unreachable!(),
        },
        V::Enum(e, i) => match e.kind {
            EnumKind::Plain => true,
            _ => truthy(&e.value(*i))?,
        },
        V::Sql(_) | V::Col(..) => {
            return Err(Exc::type_error("Boolean value of this clause is not defined"))
        }
        V::Native(n) if matches!(&**n, Native::Deque(_) | Native::Array(_) | Native::EmailMsg(_) | Native::DnsAnswer(_)) => len(v)? > 0,
        _ => true,
    })
}

pub fn not(v: &V) -> R {
    Ok(V::Bool(!truthy(v)?))
}

// ---------------------------------------------------------------- numbers

pub fn num(v: &V) -> Option<f64> {
    match v {
        V::Bool(b) => Some(*b as i64 as f64),
        V::Int(i) => Some(*i as f64),
        V::Float(f) => Some(*f),
        _ => None,
    }
}

fn int_of(v: &V) -> Option<i64> {
    match v {
        V::Bool(b) => Some(*b as i64),
        V::Int(i) => Some(*i),
        _ => None,
    }
}

fn is_sql(v: &V) -> bool {
    matches!(v, V::Col(..) | V::Sql(_))
}

fn binop_err(op: &str, a: &V, b: &V) -> Exc {
    unsupported(op, a, b)
}

/// The type an operator error names: a Counter or a defaultdict is not a plain dict
fn op_type_name(v: &V) -> &'static str {
    match v {
        V::Dict(d) => match default_factory(d) {
            Some("Counter") => "Counter",
            Some(_) => "collections.defaultdict",
            None => "dict",
        },
        _ => v.type_name(),
    }
}

fn overflow() -> Exc {
    Exc::msg(&OVERFLOW_ERROR, "integer overflow (py2axum ints are 64-bit)")
}

fn unenum(v: &V) -> V {
    match v {
        V::Enum(e, i) if e.kind != EnumKind::Plain => e.value(*i),
        other => other.clone(),
    }
}

/// Decimal arithmetic (Decimal with Decimal/int/bool; float is refused like CPython)
fn dec_op(a: &V, b: &V, op: &str) -> Option<R> {
    if !matches!(a, V::Decimal(_)) && !matches!(b, V::Decimal(_)) {
        return None;
    }
    use super::decimal as d;
    let (Some(x), Some(y)) = (d::coerce(a), d::coerce(b)) else {
        return Some(Err(binop_err(op, a, b)));
    };
    Some(match op {
        "+" => x.add(&y).map(d::v),
        "-" => x.sub(&y).map(d::v),
        "*" => x.mul(&y).map(d::v),
        "/" => x.div(&y).map(d::v),
        "//" => x.divmod(&y).map(|(q, _)| d::v(q)),
        "%" => x.divmod(&y).map(|(_, r)| d::v(r)),
        _ => Err(binop_err(op, a, b)),
    })
}

pub fn add(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "+", b) {
        return r;
    }
    if let Some(s) = super::pgpy::as_str(a) {
        return add(&s, b);
    }
    if let Some(s) = super::pgpy::as_str(b) {
        return add(a, &s);
    }
    if let Some(r) = counter_binop(a, "+", b) {
        return r;
    }
    // a NamedTuple concatenated: a plain tuple
    if matches!(a, V::Inst(i) if i.desc.x.ntuple) || matches!(b, V::Inst(i) if i.desc.x.ntuple) {
        return add(&seq_tuple(a).unwrap_or_else(|| a.clone()), &seq_tuple(b).unwrap_or_else(|| b.clone()));
    }
    if let Some(r) = super::tenacity::binop(a, "+", b) {
        return r;
    }
    if let Some(r) = super::reldelta::binop(a, "+", b) {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_binop(a, "+", b);
    }
    if let Some(r) = dec_op(a, b, "+") {
        return r;
    }
    if matches!(a, V::Enum(..)) || matches!(b, V::Enum(..)) {
        return add(&unenum(a), &unenum(b));
    }
    Ok(match (a, b) {
        (V::Str(x), V::Str(y)) => V::str(format!("{x}{y}")),
        (V::Bytes(x), V::Bytes(y)) => V::Bytes(Arc::from([&x[..], &y[..]].concat())),
        (V::List(x), V::List(y)) => {
            let mut v = x.lock().clone();
            v.extend(y.lock().iter().cloned());
            V::list(v)
        }
        (V::Tuple(x), V::Tuple(y)) => V::tuple(x.iter().chain(y.iter()).cloned().collect()),
        (V::DateTime(d), V::Delta(t)) | (V::Delta(t), V::DateTime(d)) => V::DateTime(dt_in_range(d.checked_add(*t))?),
        (V::Date(d), V::Delta(t)) | (V::Delta(t), V::Date(d)) => V::Date(date_plus_days(*d, td_days(t))?),
        (V::Delta(x), V::Delta(y)) => V::Delta(*x + *y),
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => V::Int(x.checked_add(y).ok_or_else(overflow)?),
            _ => match (num(a), num(b)) {
                (Some(x), Some(y)) => V::Float(x + y),
                _ => return Err(binop_err("+", a, b)),
            },
        },
    })
}

pub fn sub(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "-", b) {
        return r;
    }
    if let Some(r) = counter_binop(a, "-", b) {
        return r;
    }
    if let Some(r) = super::reldelta::binop(a, "-", b) {
        return r;
    }
    if let Some(r) = dec_op(a, b, "-") {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_binop(a, "-", b);
    }
    Ok(match (a, b) {
        (V::DateTime(d), V::Delta(t)) => V::DateTime(dt_in_range(d.checked_add(-*t))?),
        (V::DateTime(x), V::DateTime(y)) => {
            if x.tz.is_some() != y.tz.is_some() {
                return Err(Exc::type_error("can't subtract offset-naive and offset-aware datetimes"));
            }
            // the same tzinfo object: the wall times (CPython ignores the offsets and the folds)
            if x.tz == y.tz { V::Delta(x.wall - y.wall) } else { V::Delta(x.utc() - y.utc()) }
        }
        (V::Date(x), V::Date(y)) => V::Delta(*x - *y),
        // date.__sub__: `self + timedelta(-other.days)`
        (V::Date(d), V::Delta(t)) => V::Date(date_plus_days(*d, -td_days(t))?),
        (V::Delta(x), V::Delta(y)) => V::Delta(*x - *y),
        (V::Set(x) | V::Frozen(x), V::Set(y) | V::Frozen(y)) => {
            let y = y.lock();
            let m: IndexMap<Key, V> = x.lock().iter().filter(|(k, _)| !y.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            set_like(a, m)
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => V::Int(x.checked_sub(y).ok_or_else(overflow)?),
            _ => match (num(a), num(b)) {
                (Some(x), Some(y)) => V::Float(x - y),
                _ => return Err(binop_err("-", a, b)),
            },
        },
    })
}

/// `seq * n` with a size from the request: OverflowError / MemoryError as CPython, never an allocation
/// failure (which aborts the process)
pub fn repeat_room(unit: usize, k: usize, overflow_msg: &str) -> R<()> {
    let total = unit.checked_mul(k).filter(|t| *t <= isize::MAX as usize).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, overflow_msg))?;
    if total > 1 << 20 {
        let mut probe: Vec<u8> = Vec::new();
        probe.try_reserve_exact(total).map_err(|_| Exc::msg(&MEMORY_ERROR, ""))?;
    }
    Ok(())
}

pub fn mul(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "*", b) {
        return r;
    }
    if let Some(r) = super::reldelta::binop(a, "*", b) {
        return r;
    }
    if matches!(a, V::Inst(i) if i.desc.x.ntuple) || matches!(b, V::Inst(i) if i.desc.x.ntuple) {
        return mul(&seq_tuple(a).unwrap_or_else(|| a.clone()), &seq_tuple(b).unwrap_or_else(|| b.clone()));
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_binop(a, "*", b);
    }
    if let Some(r) = dec_op(a, b, "*") {
        return r;
    }
    Ok(match (a, b) {
        (V::Str(s), n) | (n, V::Str(s)) if int_of(n).is_some() => {
            let k = int_of(n).unwrap().max(0) as usize;
            repeat_room(s.len(), k, "repeated string is too long")?;
            V::str(s.repeat(k))
        }
        (V::Bytes(b), n) | (n, V::Bytes(b)) if int_of(n).is_some() => {
            let k = int_of(n).unwrap().max(0) as usize;
            repeat_room(b.len(), k, "repeated bytes are too long")?;
            V::Bytes(Arc::from(b.repeat(k)))
        }
        (V::List(l), n) | (n, V::List(l)) if int_of(n).is_some() => {
            let src = l.lock().clone();
            let k = int_of(n).unwrap().max(0) as usize;
            repeat_room(src.len() * std::mem::size_of::<V>(), k, "")?;
            V::list(src.iter().cloned().cycle().take(src.len() * k).collect())
        }
        (V::Tuple(t), n) | (n, V::Tuple(t)) if int_of(n).is_some() => {
            let k = int_of(n).unwrap().max(0) as usize;
            repeat_room(t.len() * std::mem::size_of::<V>(), k, "")?;
            V::tuple(t.iter().cloned().cycle().take(t.len() * k).collect())
        }
        (V::Delta(d), n) | (n, V::Delta(d)) if num(n).is_some() => {
            V::Delta(Duration::microseconds((dt::micros(d) as f64 * num(n).unwrap()).round() as i64))
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => V::Int(x.checked_mul(y).ok_or_else(overflow)?),
            _ => match (num(a), num(b)) {
                (Some(x), Some(y)) => V::Float(x * y),
                _ => return Err(binop_err("*", a, b)),
            },
        },
    })
}

fn zero_div(msg: &str) -> Exc {
    Exc::msg(&ZERO_DIVISION_ERROR, msg)
}

/// ZeroDivisionError of `op` ("/", "//", "%", "divmod") on ints (or timedeltas) or floats: Python 3.14 says
/// "division by zero" for all of them; before, each operator and type had its message (float modulo changed
/// in 3.13).
pub fn zero_div_of(op: &str, float: bool) -> Exc {
    if super::python() >= (3, 14) {
        return zero_div("division by zero");
    }
    zero_div(match (op, float) {
        ("/", false) => "division by zero",
        ("/", true) => "float division by zero",
        ("//" | "divmod", false) => "integer division or modulo by zero",
        ("%", false) => "integer modulo by zero",
        ("//", true) => "float floor division by zero",
        ("%", true) if super::python() >= (3, 13) => "float modulo by zero",
        ("%", true) => "float modulo",
        _ => "float divmod()",
    })
}

fn is_float(a: &V, b: &V) -> bool {
    matches!(a, V::Float(_)) || matches!(b, V::Float(_))
}

pub fn truediv(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "/", b) {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_div(a, b);
    }
    if let Some(r) = dec_op(a, b, "/") {
        return r;
    }
    if let Some(r) = super::pathio::truediv(a, b) {
        return r;
    }
    Ok(match (a, b) {
        (V::Delta(x), V::Delta(y)) => {
            if y.is_zero() {
                return Err(zero_div("division by zero"));
            }
            V::Float(dt::micros(x) as f64 / dt::micros(y) as f64)
        }
        (V::Delta(x), n) if num(n).is_some() => {
            let d = num(n).unwrap();
            if d == 0.0 {
                return Err(zero_div_of(if matches!(n, V::Float(_)) { "/" } else { "//" }, matches!(n, V::Float(_))));
            }
            V::Delta(Duration::microseconds((dt::micros(x) as f64 / d).round() as i64))
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => {
                if y == 0.0 {
                    return Err(zero_div_of("/", is_float(a, b)));
                }
                V::Float(x / y)
            }
            _ => return Err(binop_err("/", a, b)),
        },
    })
}

pub fn floordiv(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "//", b) {
        return r;
    }
    if let Some(r) = dec_op(a, b, "//") {
        return r;
    }
    Ok(match (a, b) {
        (V::Delta(x), V::Delta(y)) => {
            if y.is_zero() {
                return Err(zero_div_of("//", false));
            }
            V::Int(dt::micros(x).div_euclid(dt::micros(y)))
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => {
                if y == 0 {
                    return Err(zero_div_of("//", false));
                }
                V::Int(x.div_euclid(y) - if (x.rem_euclid(y) != 0) && (y < 0) { 1 } else { 0 })
            }
            _ => match (num(a), num(b)) {
                (Some(x), Some(y)) => {
                    if y == 0.0 {
                        return Err(zero_div_of("//", true));
                    }
                    V::Float((x / y).floor())
                }
                _ => return Err(binop_err("//", a, b)),
            },
        },
    })
}

pub fn modulo(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "%", b) {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_binop(a, "%", b);
    }
    if let Some(r) = dec_op(a, b, "%") {
        return r;
    }
    if let V::Str(fmt) = a {
        return Ok(V::str(percent_format(fmt, b)?));
    }
    match (int_of(a), int_of(b)) {
        (Some(x), Some(y)) => {
            if y == 0 {
                return Err(zero_div_of("%", false));
            }
            let r = x % y;
            Ok(V::Int(if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r }))
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => {
                if y == 0.0 {
                    return Err(zero_div_of("%", true));
                }
                let r = x % y;
                Ok(V::Float(if r != 0.0 && ((r < 0.0) != (y < 0.0)) { r + y } else { r }))
            }
            _ => Err(binop_err("%", a, b)),
        },
    }
}

pub fn neg(a: &V) -> R {
    if let Some(r) = super::proto::unary(a, "__neg__") {
        return r;
    }
    Ok(match a {
        V::Decimal(d) => super::decimal::v(d.negate().fix()?),
        V::Bool(b) => V::Int(-(*b as i64)),
        V::Int(i) => V::Int(i.checked_neg().ok_or_else(overflow)?),
        V::Float(f) => V::Float(-f),
        V::Delta(d) => V::Delta(-*d),
        V::Native(n) if matches!(&**n, Native::RelDelta(_)) => match &**n {
            Native::RelDelta(r) => super::reldelta::neg(r),
            _ => unreachable!(),
        },
        _ => return Err(Exc::type_error(format!("bad operand type for unary -: '{}'", a.type_name()))),
    })
}

pub fn pos(a: &V) -> R {
    if let Some(r) = super::proto::unary(a, "__pos__") {
        return r;
    }
    Ok(match a {
        V::Bool(b) => V::Int(*b as i64),
        V::Int(_) | V::Float(_) | V::Delta(_) => a.clone(),
        // `__pos__`: a zero loses its sign (default rounding), then `_fix`
        V::Decimal(d) => super::decimal::v(super::decimal::Dec { neg: d.neg && !d.is_zero(), ..(**d).clone() }.fix()?),
        _ => return Err(Exc::type_error(format!("bad operand type for unary +: '{}'", a.type_name()))),
    })
}

pub fn bitor(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "|", b) {
        return r;
    }
    if let Some(r) = counter_binop(a, "|", b) {
        return r;
    }
    if let Some(r) = super::tenacity::binop(a, "|", b) {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_bool(a, "OR", b);
    }
    Ok(match (a, b) {
        (V::Bool(x), V::Bool(y)) => V::Bool(*x | *y),
        (V::Dict(x), V::Dict(y)) => {
            let mut m = x.lock().clone();
            for (k, v) in y.lock().iter() {
                m.insert(k.clone(), v.clone());
            }
            V::Dict(Arc::new(Mutex::new(m)))
        }
        (V::Set(x) | V::Frozen(x), V::Set(y) | V::Frozen(y)) => {
            let mut m = x.lock().clone();
            for (k, v) in y.lock().iter() {
                m.entry(k.clone()).or_insert_with(|| v.clone());
            }
            set_like(a, m)
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => V::Int(x | y),
            _ => return Err(binop_err("|", a, b)),
        },
    })
}

fn unsupported(op: &str, a: &V, b: &V) -> Exc {
    Exc::type_error(format!("unsupported operand type(s) for {op}: '{}' and '{}'", op_type_name(a), op_type_name(b)))
}

/// `a ** b`: int ** non-negative int is an int (beyond 64 bits: OverflowError, py2axum has no big ints),
/// a negative exponent or a float gives a float
pub fn pow(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "**", b) {
        return r;
    }
    match (int_of(a), int_of(b)) {
        (Some(x), Some(y)) if y >= 0 => {
            let e = u32::try_from(y).map_err(|_| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer power beyond 64 bits"))?;
            return x.checked_pow(e).map(V::Int).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer power beyond 64 bits"));
        }
        _ => {}
    }
    let (x, y) = match (num(a), num(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(unsupported("** or pow()", a, b)),
    };
    if x == 0.0 && y < 0.0 {
        return Err(Exc::msg(&ZERO_DIVISION_ERROR, if super::python() >= (3, 14) { "zero to a negative power" } else { "0.0 cannot be raised to a negative power" }));
    }
    if x < 0.0 && y.fract() != 0.0 {
        return Err(Exc::type_error("py2axum: a negative number to a fractional power is complex"));
    }
    let r = x.powf(y);
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(Exc::msg(&OVERFLOW_ERROR, "(34, 'Numerical result out of range')"));
    }
    Ok(V::Float(r))
}

/// the result of a set operator is the left operand's type (`frozenset | set` is a frozenset, `set | frozenset` a set)
fn set_like(like: &V, m: IndexMap<Key, V>) -> V {
    let m = Arc::new(Mutex::new(m));
    if matches!(like, V::Frozen(_)) { V::Frozen(m) } else { V::Set(m) }
}

pub fn bitxor(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "^", b) {
        return r;
    }
    // Counter ^ Counter (Python 3.15): the absolute differences of the counts, zeros dropped
    if let (V::Dict(x), V::Dict(y)) = (a, b) {
        if is_counter(x) && is_counter(y) && super::python() >= (3, 15) {
            let out = new_counter()?;
            let (xs, ys) = (x.lock().clone(), y.lock().clone());
            let absdiff = |p: &V, q: &V| -> R { let d = sub(p, q)?; if cmp(&d, &V::Int(0))? == std::cmp::Ordering::Less { sub(&V::Int(0), &d) } else { Ok(d) } };
            for (k, (kv, c)) in &xs {
                let n = absdiff(c, &ys.get(k).map(|(_, v)| v.clone()).unwrap_or(V::Int(0)))?;
                if truthy(&n)? {
                    out.lock().insert(k.clone(), (kv.clone(), n));
                }
            }
            for (k, (kv, c)) in &ys {
                if !xs.contains_key(k) && truthy(c)? {
                    out.lock().insert(k.clone(), (kv.clone(), absdiff(c, &V::Int(0))?));
                }
            }
            return Ok(V::Dict(out));
        }
    }
    match (a, b) {
        (V::Bool(x), V::Bool(y)) => Ok(V::Bool(x ^ y)),
        (V::Set(x) | V::Frozen(x), V::Set(y) | V::Frozen(y)) => {
            let (x, y) = (x.lock().clone(), y.lock().clone());
            let mut m: IndexMap<Key, V> = x.iter().filter(|(k, _)| !y.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            m.extend(y.iter().filter(|(k, _)| !x.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())));
            Ok(set_like(a, m))
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => Ok(V::Int(x ^ y)),
            _ => Err(unsupported("^", a, b)),
        },
    }
}

pub fn lshift(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "<<", b) {
        return r;
    }
    match (int_of(a), int_of(b)) {
        (Some(_), Some(y)) if y < 0 => Err(Exc::value_error("negative shift count")),
        (Some(x), Some(y)) => {
            let r = if y >= 64 { None } else { x.checked_mul(1i64.checked_shl(y as u32).unwrap_or(0)).filter(|_| y < 63 || x == 0) };
            r.map(V::Int).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer shift beyond 64 bits"))
        }
        _ => Err(unsupported("<<", a, b)),
    }
}

pub fn rshift(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, ">>", b) {
        return r;
    }
    match (int_of(a), int_of(b)) {
        (Some(_), Some(y)) if y < 0 => Err(Exc::value_error("negative shift count")),
        (Some(x), Some(y)) => Ok(V::Int(if y >= 64 { if x < 0 { -1 } else { 0 } } else { x >> y })),
        _ => Err(unsupported(">>", a, b)),
    }
}

pub fn bitand(a: &V, b: &V) -> R {
    if let Some(r) = super::proto::binop(a, "&", b) {
        return r;
    }
    if let Some(r) = counter_binop(a, "&", b) {
        return r;
    }
    if let Some(r) = super::tenacity::binop(a, "&", b) {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_bool(a, "AND", b);
    }
    Ok(match (a, b) {
        (V::Bool(x), V::Bool(y)) => V::Bool(*x & *y),
        (V::Set(x) | V::Frozen(x), V::Set(y) | V::Frozen(y)) => {
            let y = y.lock();
            let m: IndexMap<Key, V> = x.lock().iter().filter(|(k, _)| y.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            set_like(a, m)
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => V::Int(x & y),
            _ => return Err(binop_err("&", a, b)),
        },
    })
}

pub fn invert(a: &V) -> R {
    if let Some(r) = super::proto::unary(a, "__invert__") {
        return r;
    }
    if is_sql(a) {
        return orm::sql_not(a);
    }
    match int_of(a) {
        Some(x) => Ok(V::Int(!x)),
        None => Err(Exc::type_error(format!("bad operand type for unary ~: '{}'", a.type_name()))),
    }
}

// ---------------------------------------------------------------- comparisons

pub fn eq_bool(a: &V, b: &V) -> bool {
    if let Some(r) = super::pgpy::eq_hook(a, b) {
        return r;
    }
    // Starlette's URL.__eq__: the other side as its str (a URL or a str)
    let url_text = |v: &V| match v {
        V::Native(n) if matches!(&**n, Native::Url(_)) => str_(v).ok(),
        V::Str(s) => Some(s.to_string()),
        _ => None,
    };
    if matches!(a, V::Native(n) if matches!(&**n, Native::Url(_))) || matches!(b, V::Native(n) if matches!(&**n, Native::Url(_))) {
        return matches!((url_text(a), url_text(b)), (Some(x), Some(y)) if x == y);
    }
    if let (V::Native(x), V::Native(y)) = (a, b) {
        if let (Native::Array(p), Native::Array(q)) = (&**x, &**y) {
            return super::arr::eq(p, q);
        }
        if let (Native::DnsName(p), Native::DnsName(q)) = (&**x, &**y) {
            return p == q;
        }
        if let (Native::DnsRdata(p), Native::DnsRdata(q)) = (&**x, &**y) {
            return p == q;
        }
    }
    if let Some(Ok(V::Bool(r))) = super::reldelta::binop(a, "==", b) {
        return r;
    }
    // a cachetools cache: Mapping.__eq__ (either side)
    for (x, y) in [(a, b), (b, a)] {
        if let V::Native(n) = x {
            if let Native::CtCache(c) = &**n {
                if let Ok(Some(r)) = super::ctools::eq(c, y) {
                    return r;
                }
            }
        }
    }
    if let Some(t) = seq_tuple(a) {
        return eq_bool(&t, b);
    }
    if let Some(t) = seq_tuple(b) {
        return eq_bool(a, &t);
    }
    if matches!(a, V::Inst(_) | V::Obj(_)) || matches!(b, V::Inst(_) | V::Obj(_)) {
        if dunder(a, "__eq__").is_some() || dunder(b, "__eq__").is_some() {
            // containers (`in`, list.index...) compare like PyObject_RichCompareBool: identity first;
            // an exception of __eq__ reads as "not equal" here
            let same = match (a, b) {
                (V::Inst(x), V::Inst(y)) => Arc::ptr_eq(x, y),
                (V::Obj(x), V::Obj(y)) => Arc::ptr_eq(x, y),
                _ => false,
            };
            return same || eq_r(a, b).unwrap_or(false);
        }
    }
    match (a, b) {
        (V::Decimal(_), _) | (_, V::Decimal(_)) if !matches!((a, b), (V::Enum(..), _) | (_, V::Enum(..))) => {
            return match (super::decimal::coerce(a), super::decimal::coerce(b)) {
                (Some(x), Some(y)) => x.cmp(&y) == std::cmp::Ordering::Equal,
                _ => match (a, b) {
                    (V::Decimal(x), V::Float(f)) | (V::Float(f), V::Decimal(x)) => super::decimal::Dec::from_f64(*f).map(|y| x.cmp(&y) == std::cmp::Ordering::Equal).unwrap_or(false),
                    _ => false,
                },
            };
        }
        (V::Enum(x, i), V::Enum(y, j)) => std::ptr::eq(*x, *y) && i == j || (x.kind != EnumKind::Plain && y.kind != EnumKind::Plain && eq_bool(&x.value(*i), &y.value(*j))),
        (V::Enum(e, i), other) | (other, V::Enum(e, i)) => e.kind != EnumKind::Plain && eq_bool(&e.value(*i), other),
        (V::None, V::None) => true,
        (V::Str(x), V::Str(y)) => x == y,
        (V::Bytes(x), V::Bytes(y)) => x == y,
        (V::Native(x), V::Native(y)) if matches!((&**x, &**y), (Native::Path(_), Native::Path(_)) | (Native::Uuid(_), Native::Uuid(_)) | (Native::Secret(_), Native::Secret(_))) => {
            match (&**x, &**y) {
                (Native::Path(p), Native::Path(q)) => p == q,
                (Native::Uuid(p), Native::Uuid(q)) => p == q,
                (Native::Secret(p), Native::Secret(q)) => p == q,
                _ => false,
            }
        }
        (V::List(x), V::List(y)) => {
            if Arc::ptr_eq(x, y) {
                return true;
            }
            let (x, y) = (x.lock().clone(), y.lock().clone());
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| eq_bool(p, q))
        }
        (V::Tuple(x), V::Tuple(y)) => x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| eq_bool(p, q)),
        (V::Dict(x), V::Dict(y)) => {
            if Arc::ptr_eq(x, y) {
                return true;
            }
            let (x, y) = (x.lock().clone(), y.lock().clone());
            x.len() == y.len() && x.iter().all(|(k, (_, v))| y.get(k).map(|(_, w)| eq_bool(v, w)).unwrap_or(false))
        }
        (V::Set(x) | V::Frozen(x), V::Set(y) | V::Frozen(y)) => {
            if Arc::ptr_eq(x, y) {
                return true;
            }
            let (x, y) = (x.lock(), y.lock());
            x.len() == y.len() && x.keys().all(|k| y.contains_key(k))
        }
        (V::DateTime(x), V::DateTime(y)) => {
            if x.tz.is_some() != y.tz.is_some() {
                false
            } else if x.tz == y.tz {
                x.wall == y.wall
            } else {
                // PEP 495: between zones, a value whose offset depends on its fold equals nothing
                !(fold_dependent(x) || fold_dependent(y)) && x.utc() == y.utc()
            }
        }
        (V::Date(x), V::Date(y)) => x == y,
        (V::Time(x), V::Time(y)) => x == y,
        (V::Delta(x), V::Delta(y)) => x == y,
        (V::Tz(x), V::Tz(y)) => x.py_eq(y),
        (V::Class(x), V::Class(y)) => std::ptr::eq(*x, *y),
        (V::Obj(x), V::Obj(y)) => Arc::ptr_eq(x, y),
        (V::Inst(x), V::Inst(y)) => Arc::ptr_eq(x, y) || x.equals(y),
        (V::Native(x), V::Native(y)) => Arc::ptr_eq(x, y) || matches!((&**x, &**y), (Native::Type(p), Native::Type(q)) if p == q),
        (V::Exc(x), V::Exc(y)) => Arc::ptr_eq(&x.0, &y.0),
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// `a OP b` with a project comparison method, CPython's rich comparison: the left operand's method, else (an SQL
/// expression on the left answers itself) the right operand's reflected one; its result as is (SQL from a
/// SQLAlchemy Comparator or a hybrid value object). None: no project method takes part.
fn richcmp(a: &V, op: &str, b: &V) -> Option<R> {
    let inst = |v: &V| matches!(v, V::Inst(_) | V::Obj(_));
    if !inst(a) && !inst(b) {
        return None;
    }
    let (name, reflected) = match op {
        "==" => ("__eq__", "__eq__"),
        "!=" => ("__ne__", "__ne__"),
        "<" => ("__lt__", "__gt__"),
        "<=" => ("__le__", "__ge__"),
        ">" => ("__gt__", "__lt__"),
        _ => ("__ge__", "__le__"),
    };
    if let Some(f) = dunder(a, name) {
        return Some(call_dunder(f, a, vec![b.clone()], name));
    }
    if is_sql(a) {
        return None;
    }
    dunder(b, reflected).map(|f| call_dunder(f, b, vec![a.clone()], reflected))
}

pub fn eq(a: &V, b: &V) -> R {
    super::stack_guard()?;
    if let Some(r) = richcmp(a, "==", b) {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_cmp(a, "=", b);
    }
    Ok(V::Bool(eq_r(a, b)?))
}

pub fn ne(a: &V, b: &V) -> R {
    if dunder(a, "__ne__").is_some() || dunder(b, "__ne__").is_some() {
        if let Some(r) = richcmp(a, "!=", b) {
            return r;
        }
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_cmp(a, "!=", b);
    }
    Ok(V::Bool(!eq_r(a, b)?))
}

pub fn cmp(a: &V, b: &V) -> R<std::cmp::Ordering> {
    if let Some(s) = super::pgpy::as_str(a) {
        return cmp(&s, b);
    }
    if let Some(s) = super::pgpy::as_str(b) {
        return cmp(a, &s);
    }
    if let Some(t) = seq_tuple(a) {
        return cmp(&t, b);
    }
    if let Some(t) = seq_tuple(b) {
        return cmp(a, &t);
    }
    use std::cmp::Ordering::*;
    // instances with `__lt__` (sorted(), min(), a sequence's order): `a < b`, then `b < a`, as list.sort compares
    if let Some(r) = richcmp(a, "<", b) {
        if truthy(&r?)? {
            return Ok(Less);
        }
        let greater = match richcmp(b, "<", a) {
            Some(r) => truthy(&r?)?,
            None => false,
        };
        return Ok(if greater { Greater } else { Equal });
    }
    if let V::Enum(e, i) = a {
        if e.kind != EnumKind::Plain {
            return cmp(&e.value(*i), b);
        }
    }
    if let V::Enum(e, i) = b {
        if e.kind != EnumKind::Plain {
            return cmp(a, &e.value(*i));
        }
    }
    if matches!(a, V::Decimal(_)) || matches!(b, V::Decimal(_)) {
        let to = |v: &V| -> R<super::decimal::Dec> {
            match v {
                V::Float(f) => super::decimal::Dec::from_f64(*f),
                o => super::decimal::coerce(o).ok_or_else(|| Exc::type_error(format!("'<' not supported between instances of '{}' and '{}'", a.type_name(), b.type_name()))),
            }
        };
        return Ok(to(a)?.cmp(&to(b)?));
    }
    Ok(match (a, b) {
        (V::Str(x), V::Str(y)) => x.cmp(y),
        (V::Bytes(x), V::Bytes(y)) => x[..].cmp(&y[..]),
        (V::Tuple(x), V::Tuple(y)) => seq_cmp(x, y)?,
        (V::List(x), V::List(y)) => seq_cmp(&x.lock().clone(), &y.lock().clone())?,
        (V::DateTime(x), V::DateTime(y)) => {
            if x.tz.is_some() != y.tz.is_some() {
                return Err(Exc::type_error("can't compare offset-naive and offset-aware datetimes"));
            }
            if x.tz == y.tz { x.wall.cmp(&y.wall) } else { x.utc().cmp(&y.utc()) }
        }
        (V::Date(x), V::Date(y)) => x.cmp(y),
        (V::Time(x), V::Time(y)) => x.cmp(y),
        (V::Delta(x), V::Delta(y)) => x.cmp(y),
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Equal),
            _ => {
                return Err(Exc::type_error(format!(
                    "'<' not supported between instances of '{}' and '{}'",
                    a.type_name(),
                    b.type_name()
                )))
            }
        },
    })
}

fn seq_cmp(x: &[V], y: &[V]) -> R<std::cmp::Ordering> {
    for (p, q) in x.iter().zip(y.iter()) {
        if !eq_bool(p, q) {
            return cmp(p, q);
        }
    }
    Ok(x.len().cmp(&y.len()))
}

macro_rules! cmp_op {
    ($name:ident, $sql:expr, $pat:pat) => {
        pub fn $name(a: &V, b: &V) -> R {
            if let Some(r) = richcmp(a, $sql, b) {
                return r;
            }
            if is_sql(a) || is_sql(b) {
                return orm::sql_cmp(a, $sql, b);
            }
            // sets: the subset relations (a partial order: not a three-way comparison)
            if let (V::Set(x) | V::Frozen(x), V::Set(y) | V::Frozen(y)) = (a, b) {
                let (x, y) = (x.lock(), y.lock());
                let sub = |p: &IndexMap<Key, V>, q: &IndexMap<Key, V>| p.keys().all(|k| q.contains_key(k));
                return Ok(V::Bool(match $sql {
                    "<" => x.len() < y.len() && sub(&x, &y),
                    "<=" => sub(&x, &y),
                    ">" => x.len() > y.len() && sub(&y, &x),
                    _ => sub(&y, &x),
                }));
            }
            match cmp(a, b) {
                Ok(o) => Ok(V::Bool(matches!(o, $pat))),
                // cmp's TypeError names `<` (the order sorted() asks for): this operator's in its place
                Err(e) if $sql != "<" && e.isinstance(&TYPE_ERROR) && e.message().starts_with("'<' not supported between") => {
                    Err(Exc::type_error(format!("'{}'{}", $sql, &e.message()[3..])))
                }
                Err(e) => Err(e),
            }
        }
    };
}
cmp_op!(lt, "<", std::cmp::Ordering::Less);
cmp_op!(le, "<=", std::cmp::Ordering::Less | std::cmp::Ordering::Equal);
cmp_op!(gt, ">", std::cmp::Ordering::Greater);
cmp_op!(ge, ">=", std::cmp::Ordering::Greater | std::cmp::Ordering::Equal);

pub fn is(a: &V, b: &V) -> bool {
    match (a, b) {
        // the application object is one object (`request.app is request.scope["app"]`)
        (V::Native(x), V::Native(y)) if matches!((&**x, &**y), (Native::Routing(p), Native::Routing(q))
            if matches!(**p, super::routing::RObj::App) && matches!(**q, super::routing::RObj::App)) => true,
        (V::None, V::None) => true,
        (V::Bool(x), V::Bool(y)) => x == y,
        (V::Int(x), V::Int(y)) => x == y && (-5..=256).contains(x),
        (V::Str(x), V::Str(y)) => Arc::ptr_eq(x, y),
        (V::List(x), V::List(y)) => Arc::ptr_eq(x, y),
        (V::Tuple(x), V::Tuple(y)) => Arc::ptr_eq(x, y),
        (V::Set(x), V::Set(y)) | (V::Frozen(x), V::Frozen(y)) => Arc::ptr_eq(x, y),
        (V::Dict(x), V::Dict(y)) => Arc::ptr_eq(x, y),
        (V::Obj(x), V::Obj(y)) => Arc::ptr_eq(x, y),
        (V::Inst(x), V::Inst(y)) => Arc::ptr_eq(x, y),
        // `e.orig is e.__cause__`, `exc is caught`
        (V::Exc(x), V::Exc(y)) => Arc::ptr_eq(&x.0, &y.0),
        (V::Class(x), V::Class(y)) => std::ptr::eq(*x, *y),
        // tzinfos: one object per value (timezone.utc, ZoneInfo's cache, pytz's and dateutil's instances)
        (V::Tz(x), V::Tz(y)) => x == y,
        // enum members are singletons: `shelf is Shelf.done`
        (V::Enum(x, i), V::Enum(y, j)) => std::ptr::eq(*x, *y) && i == j,
        // a builtin type is one object: `type(x) is str`
        (V::Native(x), V::Native(y)) => Arc::ptr_eq(x, y) || matches!((&**x, &**y), (Native::Type(p), Native::Type(q)) if p == q),
        _ => false,
    }
}

/// `x in y` on a value that is no container: CPython 3.14 says "not a container or iterable"
pub fn not_container(type_name: &str) -> String {
    if super::python() >= (3, 14) {
        format!("argument of type '{type_name}' is not a container or iterable")
    } else {
        format!("argument of type '{type_name}' is not iterable")
    }
}

pub fn contains(container: &V, item: &V) -> R<bool> {
    if let Some(r) = super::proto::contains(container, item) {
        return r;
    }
    if let Some(s) = super::pgpy::as_str(container) {
        return contains(&s, item);
    }
    if let (V::Str(_), Some(s)) = (container, super::pgpy::as_str(item)) {
        return contains(container, &s);
    }
    if let Some(t) = seq_tuple(container) {
        return contains(&t, item);
    }
    if let V::Enum(e, i) = container {
        return contains(&e.value(*i), item);
    }
    if let V::Native(n) = container {
        if let Native::EmailMsg(m) = &**n {
            return super::emailmsg::contains(m, item);
        }
        if let Native::CtCache(c) = &**n {
            return super::ctools::contains(c, item);
        }
        if let Native::Rl(x) = &**n {
            return super::rlbind::contains(x, item);
        }
    }
    Ok(match container {
        V::Class(c) => match c.kind {
            ClassKind::Enum(e) => match item {
                V::Enum(d, _) => std::ptr::eq(*d, e),
                other => e.by_value(other).is_some(),
            },
            _ => return Err(Exc::type_error(not_container("type"))),
        },
        V::Str(s) => match &unenum(item) {
            V::Str(i) => s.contains(&**i),
            _ => return Err(Exc::type_error("'in <string>' requires string as left operand")),
        },
        V::List(l) => l.lock().clone().iter().any(|x| eq_bool(x, item)),
        V::Tuple(t) => t.iter().any(|x| eq_bool(x, item)),
        V::Dict(d) => d.lock().contains_key(&Key::dict_key(item)?),
        V::Set(s) | V::Frozen(s) => s.lock().contains_key(&Key::set_elem(item)?),
        V::Session(s) => s.contains(item)?,
        V::Native(n) => match &**n {
            Native::SaColumns(t) => super::satable::contains(t, item)?,
            Native::Deque(d) => super::deque::items(d).iter().any(|x| eq_bool(x, item)),
            Native::Array(a) => super::arr::items(a).iter().any(|x| eq_bool(x, item)),
            Native::IpNet(a, l) => super::net::contains(&(*a, *l), item),
            Native::RespHeaders(r) => super::resp::headers_contains(&r.headers, item)?,
            Native::CellHeaders(c) => super::resp::headers_contains(&c.headers, item)?,
            Native::Headers(r) => match item {
                V::Str(k) => r.header(k).is_some(),
                _ => false,
            },
            _ => return Err(Exc::type_error(not_container(container.type_name()))),
        },
        _ => return Err(Exc::type_error(not_container(container.type_name()))),
    })
}

// ---------------------------------------------------------------- str / repr

pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let e = format!("{:e}", x); // shortest round-trip digits: "-1.2345e2"
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let decpt = exp + 1;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if -4 < decpt && decpt <= 16 {
        if decpt <= 0 {
            out.push_str("0.");
            out.push_str(&"0".repeat((-decpt) as usize));
            out.push_str(&digits);
        } else if decpt as usize >= digits.len() {
            out.push_str(&digits);
            out.push_str(&"0".repeat(decpt as usize - digits.len()));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt as usize]);
            out.push('.');
            out.push_str(&digits[decpt as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push_str(&format!("e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs()));
    }
    out
}

/// CPython's `repr(bytes)`: printable ASCII as is, `\t \n \r \\` and the quote escaped, the rest `\xNN`
pub fn bytes_repr(b: &[u8]) -> String {
    let q = if b.contains(&b'\'') && !b.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = String::from("b");
    out.push(q as char);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            c if c == q => {
                out.push('\\');
                out.push(c as char);
            }
            32..=126 => out.push(c as char),
            c => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push(q as char);
    out
}

pub fn str_repr(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_ascii() || super::methods::printable(c) => out.push(c),
            // unicode_repr: a non-printable character as \xNN, \uNNNN or \UNNNNNNNN
            c if (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push(q);
    out
}

/// A project dunder method of an instance (schema, plain class, dataclass or mapped class).
pub fn dunder(v: &V, name: &str) -> Option<super::pyd::MethodFn> {
    let methods = match v {
        V::Inst(i) => i.desc.methods,
        V::Obj(o) => o.desc.methods,
        // a project exception's own or inherited method (`__str__`, `__repr__`, `__bool__`)
        V::Exc(e) => {
            return match e.0.class.exc_lookup(name) {
                Some(super::v::ExcMember::Method(false, f)) => Some(f),
                _ => None,
            };
        }
        _ => return None,
    };
    methods.iter().find(|(n, prop, _)| *n == name && !prop).map(|(_, _, f)| *f)
}

/// Calls a project dunder method from synchronous code: such methods are `def`s; the only I/O one can wait on
/// is a synchronous session's lazy load (`__repr__` reading a deferred or expired column), which CPython does
/// blocking: the future is then finished on this thread, tokio told it blocks.
pub fn call_dunder(f: super::pyd::MethodFn, recv: &V, args: Vec<V>, name: &str) -> R {
    let cx = super::root_cx();
    let fut = f(&cx, recv.clone(), args);
    block_on_sync(fut, &format!("{name}() of {}", recv.type_name()))
}

/// Runs a future from synchronous code: polled once (no I/O: done), else finished on this thread, tokio told
/// it blocks (a synchronous session's lazy load, as CPython does it blocking).
pub fn block_on_sync<F: std::future::Future<Output = R>>(fut: F, what: &str) -> R {
    use std::future::Future;
    let mut fut = Box::pin(fut);
    let waker = std::task::Waker::noop();
    match fut.as_mut().poll(&mut std::task::Context::from_waker(waker)) {
        std::task::Poll::Ready(r) => r,
        std::task::Poll::Pending => match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                super::thread::blocking(|| h.block_on(fut))
            }
            _ => Err(Exc::runtime(format!("py2axum: {what} suspended (awaited I/O)"))),
        },
    }
}

fn dunder_text(v: &V, names: &[&str]) -> Option<R<String>> {
    for name in names {
        if let Some(f) = dunder(v, name) {
            return Some(call_dunder(f, v, vec![], name).and_then(|r| match r {
                V::Str(s) => Ok(s.to_string()),
                // 3.15 names the type (module included) and the method: `C.__str__() must return a str, not int`
                other if super::python() >= (3, 15) => Err(Exc::type_error(format!(
                    "{}.{name}() must return a str, not {}", super::proto::qualified(v), other.type_name()))),
                other => Err(Exc::type_error(format!("{name} returned non-string (type {})", other.type_name()))),
            }));
        }
    }
    None
}

/// `a == b` with project `__eq__` methods (left operand first, then the reflected one)
pub fn eq_r(a: &V, b: &V) -> R<bool> {
    for (x, y) in [(a, b), (b, a)] {
        if let Some(f) = dunder(x, "__eq__") {
            let r = call_dunder(f, x, vec![y.clone()], "__eq__")?;
            return truthy(&r);
        }
    }
    Ok(eq_bool(a, b))
}

pub fn str_(v: &V) -> R<String> {
    if let Some(r) = super::stripe::str_hook(v) {
        return r;
    }
    if let Some(r) = super::pgpy::str_hook(v) {
        return r;
    }
    // the text of a type expression depends on the Python (3.14 writes `Union[int, str]` as `int | str`) and
    // on the modules of its classes: refused rather than approximated
    if matches!(v, V::Native(n) if matches!(&**n, Native::TypeExpr(..))) {
        return Err(Exc::type_error("py2axum: the text of a type expression (str(), repr(), f-string) is not supported"));
    }
    if let Some(t) = row_tuple(v) {
        return str_(&t);
    }
    // a Pydantic model's str is BaseModel.__str__ (fields separated by spaces), whatever its __repr__
    if let V::Inst(i) = v {
        if !i.desc.open && !i.desc.dataclass && !i.desc.x.ntuple {
            return dunder_text(v, &["__str__"]).unwrap_or_else(|| i.str_());
        }
    }
    if let V::Exc(_) = v {
        // BaseException.__str__ is always there: a project `__repr__` alone does not make the text
        if let Some(r) = dunder_text(v, &["__str__"]) {
            return r;
        }
    } else if let Some(r) = dunder_text(v, &["__str__", "__repr__"]) {
        return r;
    }
    if let V::Decimal(d) = v {
        return Ok(d.to_string());
    }
    if let V::Native(n) = v {
        if let Native::EmailMsg(m) = &**n {
            return m.str_();
        }
        if let Native::DnsRdata(d) = &**n {
            return Ok(d.to_text());
        }
        if let Native::DnsName(x) = &**n {
            return Ok(x.to_text(false));
        }
        if let Native::PydUrl(_, u) = &**n {
            return Ok(u.as_str().to_string());
        }
        if matches!(&**n, Native::SaTable(_) | Native::SaColumns(_) | Native::SaColumn(_) | Native::SaType(_)) {
            return super::satable::text(n, false);
        }
        if let Native::YarlUrl(u) = &**n {
            return Ok(u.clone()); // yarl keeps the text it was given (no added `/`)
        }
        if let Native::HttpUrl(u) = &**n {
            return Ok(u.clone());
        }
        if let Native::Url(r) = &**n {
            // Starlette's request.url: scheme://Host header + path (+ ?query)
            let q = if r.raw_query.is_empty() { String::new() } else { format!("?{}", r.raw_query) };
            return Ok(format!("{}{}{q}", request_origin(r), r.path));
        }
        if let Native::Proc(p) = &**n {
            return Ok(super::subproc::repr(p));
        }
        if let Native::IpAddr(a) = &**n {
            return Ok(super::net::addr_str(a));
        }
        if let Native::IpNet(a, l) = &**n {
            return Ok(super::net::net_str(&(*a, *l)));
        }
        if let Native::Prom(p) = &**n {
            return Ok(super::prom::str(p));
        }
        if let Native::Log(o) = &**n {
            return Ok(super::logging::repr(o));
        }
        if let Native::RelDelta(r) = &**n {
            return Ok(super::reldelta::repr(r));
        }
        if let Native::YamlMark(m) = &**n {
            return Ok(m.text());
        }
    }
    Ok(match v {
        V::Str(s) => s.to_string(),
        V::DateTime(d) => d.isoformat(' ', "auto"),
        V::Date(d) => dt::date_iso(d),
        V::Time(t) => dt::time_iso(t),
        V::Delta(d) => dt::delta_str(d),
        V::Exc(e) => e.message(),
        V::Tz(t) => t.py_str(),
        V::Native(n) if matches!(&**n, Native::Path(_) | Native::Uuid(_) | Native::Secret(_) | Native::Fraction(_) | Native::Complex(..)) => match &**n {
            Native::Path(p) => p.clone(),
            Native::Uuid(u) => super::pathio::uuid_str(*u),
            Native::Secret(s) => if s.is_empty() { String::new() } else { "**********".into() },
            Native::Fraction(f) => f.text(),
            Native::Complex(re, im) => super::complex::repr(*re, *im)?,
            _ => unreachable!(),
        },
        // a column's text is SQLAlchemy's (`table.col`); a statement's or an expression's is SQLAlchemy's default
        // compiler (`:param_1` placeholders, generic types, line breaks before FROM/WHERE...): refused, never
        // approximated
        V::Col(..) => orm::sql_text(v)?,
        V::Sql(x) if matches!(&**x, orm::Sql::Col(..)) => orm::sql_text(v)?,
        V::Sql(_) => {
            return Err(Exc::type_error(
                "py2axum: the SQL text of a statement or an expression (str(), f-string, format) is not supported",
            ))
        }
        V::Enum(e, i) => match e.kind {
            EnumKind::StrEnum | EnumKind::IntEnum => str_(&e.value(*i))?,
            _ => format!("{}.{}", e.name, e.member_name(*i)),
        },
        _ => repr(v)?,
    })
}

/// `...`: one object, so that `x is ...` holds
pub fn ellipsis() -> V {
    static E: std::sync::LazyLock<V> = std::sync::LazyLock::new(|| V::native(Native::Ellipsis));
    E.clone()
}

/// An object's address as a repr shows it (`<X object at 0x7f...>`). It differs from CPython's anyway, but a
/// truncated repr (pydantic's `input_value`) cuts it where its width says: on 64-bit Linux CPython's have 12 hex
/// digits, mimalloc's (placed from 2 TiB) 11, so they are padded to 12 there.
pub fn addr(p: usize) -> String {
    if cfg!(all(target_os = "linux", target_pointer_width = "64")) {
        format!("{p:#014x}")
    } else {
        format!("{p:#x}")
    }
}

pub fn repr(v: &V) -> R<String> {
    if let Some(r) = super::stripe::repr_hook(v) {
        return r;
    }
    if let Some(r) = super::pgpy::repr_hook(v) {
        return r;
    }
    // the text of a type expression depends on the Python (3.14 writes `Union[int, str]` as `int | str`) and
    // on the modules of its classes: refused rather than approximated
    if matches!(v, V::Native(n) if matches!(&**n, Native::TypeExpr(..))) {
        return Err(Exc::type_error("py2axum: the text of a type expression (str(), repr(), f-string) is not supported"));
    }
    super::stack_guard()?;
    if let Some(t) = row_tuple(v) {
        return repr(&t);
    }
    if matches!(v, V::Native(n) if matches!(&**n, Native::Ellipsis)) {
        return Ok("Ellipsis".into());
    }
    if matches!(v, V::Native(n) if matches!(&**n, Native::NotImplemented)) {
        return Ok("NotImplemented".into());
    }
    if let V::Native(n) = v {
        if matches!(&**n, Native::Object) {
            return Ok(format!("<object object at {}>", addr(Arc::as_ptr(n) as *const () as usize)));
        }
        if let Native::LibInstance(m, c) = &**n {
            return Ok(format!("<{m}.{c} object at {}>", addr(Arc::as_ptr(n) as *const () as usize)));
        }
    }
    if let V::Native(n) = v {
        if let Native::CtCache(c) = &**n {
            return super::ctools::repr(c);
        }
    }
    // a named tuple of a library (`psutil.virtual_memory()`, cachetools' CacheInfo): `Name(field=value, ...)`
    if let V::Native(n) = v {
        if let Native::Record(name, f) = &**n {
            let parts = f.iter().map(|(k, x)| Ok(format!("{k}={}", repr(x)?))).collect::<R<Vec<_>>>()?;
            return Ok(format!("{name}({})", parts.join(", ")));
        }
    }
    if let Some(r) = dunder_text(v, &["__repr__"]) {
        return r;
    }
    if let V::Native(n) = v {
        if let Native::Proc(p) = &**n {
            return Ok(super::subproc::repr(p));
        }
    }
    if let V::Decimal(d) = v {
        return Ok(format!("Decimal('{d}')"));
    }
    if let V::Native(n) = v {
        if let Native::PydUrl(name, u) = &**n {
            return Ok(format!("{name}('{}')", u.as_str()));
        }
        if matches!(&**n, Native::SaTable(_) | Native::SaColumns(_) | Native::SaColumn(_) | Native::SaType(_)) {
            return super::satable::text(n, true);
        }
        if let Native::Deque(d) = &**n {
            return super::deque::repr(d);
        }
        if let Native::Array(a) = &**n {
            return super::arr::repr(a);
        }
        if let Native::DnsRdata(d) = &**n {
            return Ok(super::dnsres::rdata_repr(d));
        }
        if let Native::DnsName(x) = &**n {
            return Ok(format!("<DNS name {}>", x.to_text(false)));
        }
        if let Native::IpAddr(a) = &**n {
            return Ok(super::net::addr_repr(a));
        }
        if let Native::IpNet(a, l) = &**n {
            return Ok(super::net::net_repr(&(*a, *l)));
        }
        if let Native::Prom(p) = &**n {
            return Ok(super::prom::repr(p));
        }
        if let Native::Log(o) = &**n {
            return Ok(super::logging::repr(o));
        }
        if let Native::RelDelta(r) = &**n {
            return Ok(super::reldelta::repr(r));
        }
    }
    Ok(match v {
        V::Unbound => "<unbound>".into(),
        V::None => "None".into(),
        V::Bool(b) => if *b { "True" } else { "False" }.into(),
        V::Int(i) => i.to_string(),
        V::Float(f) => float_repr(*f),
        V::Str(s) => str_repr(s),
        V::Bytes(b) => bytes_repr(b),
        V::List(l) => {
            let items = l.lock().clone();
            format!("[{}]", items.iter().map(repr).collect::<R<Vec<_>>>()?.join(", "))
        }
        V::Tuple(t) => {
            if t.len() == 1 {
                format!("({},)", repr(&t[0])?)
            } else {
                format!("({})", t.iter().map(repr).collect::<R<Vec<_>>>()?.join(", "))
            }
        }
        V::Dict(d) => {
            let items = d.lock().clone();
            let parts = items
                .values()
                .map(|(k, v)| Ok(format!("{}: {}", repr(k)?, repr(v)?)))
                .collect::<R<Vec<_>>>()?;
            match default_factory(d) {
                // Counter.__repr__: most_common() order
                Some("Counter") if parts.is_empty() => "Counter()".to_string(),
                Some("Counter") => {
                    let parts = most_common(d)?.iter().map(|(k, v)| Ok(format!("{}: {}", repr(k)?, repr(v)?))).collect::<R<Vec<_>>>()?;
                    format!("Counter({{{}}})", parts.join(", "))
                }
                Some(f) => format!("defaultdict(<class '{f}'>, {{{}}})", parts.join(", ")),
                None => format!("{{{}}}", parts.join(", ")),
            }
        }
        V::Set(s) => {
            let items = s.lock().clone();
            if items.is_empty() {
                "set()".into()
            } else {
                format!("{{{}}}", items.values().map(repr).collect::<R<Vec<_>>>()?.join(", "))
            }
        }
        V::Frozen(s) => {
            let items = s.lock().clone();
            if items.is_empty() {
                "frozenset()".into()
            } else {
                format!("frozenset({{{}}})", items.values().map(repr).collect::<R<Vec<_>>>()?.join(", "))
            }
        }
        V::DateTime(d) => dt::datetime_repr(d),
        V::Date(d) => {
            use chrono::Datelike;
            format!("datetime.date({}, {}, {})", d.year(), d.month(), d.day())
        }
        V::Time(t) => dt::time_repr(t),
        V::Delta(d) => dt::delta_repr(d),
        V::Tz(t) => dt::tz_repr(t),
        // a library's exception class carries its module (`<class 'decimal.DivisionImpossible'>`)
        V::Class(c) => match (matches!(c.kind, ClassKind::Exception), super::sentry::exc_module(c)) {
            (true, Some(m)) if m != "builtins" => format!("<class '{m}.{}'>", c.qualname),
            _ => format!("<class '{}'>", c.qualname),
        },
        V::Native(n) if matches!(&**n, Native::AmqpQueue(_) | Native::AmqpExchange(_) | Native::AmqpFrame(..) | Native::AmqpIter(_)) => super::rmq::repr(n)?,
        V::Native(n) if matches!(&**n, Native::TempDir(_)) => match &**n {
            Native::TempDir(t) => super::pathio::tempdir_repr(t),
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::AFuture(_)) => match &**n {
            Native::AFuture(f) => super::aio::future_repr(f)?,
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::AEvent(_)) => match &**n {
            Native::AEvent(e) => format!("<asyncio.locks.Event object at {} [{}]>", addr(Arc::as_ptr(n) as usize),
                                         if super::aio::event_is_set(e) { "set" } else { "unset" }),
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::Type(_) | Native::ExtType(_)) => match &**n {
            Native::Type(t) | Native::ExtType(t) => format!("<class '{t}'>"),
            _ => unreachable!(),
        },
        V::Exc(e) if super::duparse::parser_error_repr(e).is_some() => super::duparse::parser_error_repr(e).unwrap(),
        // pydantic-core's ValidationError: `repr()` is its `str()` (title line, errors, links)
        V::Exc(e) if e.0.errors.is_some() && !e.isinstance(&VALIDATION_EXCEPTION)
            && matches!(e.0.attrs.lock().get("title"), Some(V::Str(_))) => e.base_message(),
        V::Exc(e) => match super::stripe::exc_repr(e) {
            Some(r) => r,
            None => format!("{}({})", e.0.class.name, e.args().iter().map(repr).collect::<R<Vec<_>>>()?.join(", ")),
        },
        V::Inst(i) => i.repr()?,
        V::Native(n) if matches!(&**n, Native::Url(_)) => format!("URL({})", str_repr(&str_(v)?)),
        V::Native(n) if matches!(&**n, Native::Address(..)) => match &**n {
            Native::Address(h, p) => format!("Address(host={}, port={p})", str_repr(h)),
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::HttpReq(_)) => match &**n {
            Native::HttpReq(r) => format!("<Request({}, {})>", str_repr(&r.0), str_repr(&r.1)),
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::Headers(_)) => match &**n {
            Native::Headers(r) => {
                // Starlette: `Headers({...})` of the items, `Headers(raw=[...])` when a name repeats
                let mut seen = std::collections::HashSet::new();
                if r.headers.iter().all(|(k, _)| seen.insert(k.to_lowercase())) {
                    let parts = r.headers.iter().map(|(k, x)| format!("{}: {}", str_repr(&k.to_lowercase()), str_repr(x))).collect::<Vec<_>>();
                    format!("Headers({{{}}})", parts.join(", "))
                } else {
                    let parts = r.headers.iter().map(|(k, x)| format!("(b{}, b{})", str_repr(&k.to_lowercase()), str_repr(x))).collect::<Vec<_>>();
                    format!("Headers(raw=[{}])", parts.join(", "))
                }
            }
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::Path(_) | Native::Uuid(_) | Native::Upload(_) | Native::Secret(_) | Native::Fraction(_) | Native::Complex(..)) => match &**n {
            Native::Path(p) => format!("PosixPath({})", str_repr(p)),
            Native::Secret(s) => format!("SecretStr('{}')", if s.is_empty() { "" } else { "**********" }),
            Native::Fraction(f) => f.repr(),
            Native::Complex(re, im) => super::complex::repr(*re, *im)?,
            Native::Uuid(u) => format!("UUID({})", str_repr(&super::pathio::uuid_str(*u))),
            Native::Upload(u) => super::files::repr_upload(u)?,
            _ => unreachable!(),
        },
        // CPython's default repr: the address differs from one process to the other anyway
        V::Obj(o) => format!("<{} object at {}>", o.desc.class_qualname, addr(Arc::as_ptr(o) as usize)),
        V::Native(n) if matches!(&**n, Native::RespObj(_) | Native::RespHeaders(_)) => match &**n {
            Native::RespObj(r) => format!("<{} object at {}>", r.cls, addr(Arc::as_ptr(r) as usize)),
            Native::RespHeaders(r) => super::resp::headers_repr(&r.headers.lock())?,
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::YamlMark(_)) => format!("<yaml.error.Mark object at {}>", addr(Arc::as_ptr(n) as *const () as usize)),
        V::Enum(e, i) => format!("<{}.{}: {}>", e.name, e.member_name(*i), repr(&e.value(*i))?),
        V::Native(n) if matches!(&**n, Native::Builtin(..)) => {
            let Native::Builtin(name, _) = &**n else { unreachable!() };
            format!("<built-in function {name}>")
        }
        other => format!("<{}>", other.type_name()),
    })
}

// ---------------------------------------------------------------- formatting

/// `format(value, spec)` for the spec subset used in f-strings: [[fill]align][sign][0][width][.prec][type]
/// CPython's `e` presentation of a non-negative float: `d.ddde+XX` (two exponent digits at least).
fn fmt_exp(x: f64, prec: usize, upper: bool) -> String {
    if !x.is_finite() {
        let s = if x.is_nan() { "nan" } else { "inf" };
        return if upper { s.to_uppercase() } else { s.into() };
    }
    let r = format!("{:.*e}", prec, x);
    let (m, e) = r.split_once('e').unwrap_or((&r, "0"));
    let e: i32 = e.parse().unwrap_or(0);
    let out = format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    if upper { out.to_uppercase() } else { out }
}

/// CPython's `g` presentation: `p` significant digits, scientific when the exponent is < -4 or >= p,
/// trailing zeros (and a trailing point) removed.
fn fmt_general(x: f64, prec: usize, upper: bool) -> String {
    if !x.is_finite() {
        return fmt_exp(x, 0, upper);
    }
    let p = prec.max(1);
    if x == 0.0 {
        return "0".into();
    }
    let r = format!("{:.*e}", p - 1, x);
    let exp: i32 = r.split_once('e').map(|(_, e)| e.parse().unwrap_or(0)).unwrap_or(0);
    let strip = |s: String| -> String {
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    };
    if exp < -4 || exp >= p as i32 {
        let (m, _) = r.split_once('e').unwrap();
        let out = format!("{}e{}{:02}", strip(m.to_string()), if exp < 0 { '-' } else { '+' }, exp.abs());
        if upper { out.to_uppercase() } else { out }
    } else {
        strip(format!("{:.*}", (p as i32 - 1 - exp).max(0) as usize, x))
    }
}

/// a float with a precision and no presentation type (`{:.3}`): like `g`, but scientific from an exponent
/// of p-1 and at least one digit after the point in fixed notation.
fn fmt_general_none(x: f64, prec: usize) -> String {
    let p = prec.max(1);
    if !x.is_finite() {
        return fmt_exp(x, 0, false);
    }
    let r = format!("{:.*e}", p - 1, x);
    let (m, e) = r.split_once('e').unwrap_or((&r, "0"));
    let exp: i32 = e.parse().unwrap_or(0);
    if exp < -4 || exp >= p as i32 - 1 {
        let m = if m.contains('.') { m.trim_end_matches('0').trim_end_matches('.') } else { m };
        return format!("{m}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let s = format!("{:.*}", (p as i32 - 1 - exp).max(0) as usize, x);
    let s = if s.contains('.') { s.trim_end_matches('0').to_string() } else { s };
    if s.ends_with('.') { format!("{s}0") } else if s.contains('.') { s } else { format!("{s}.0") }
}

/// `f"{x}"`: `format(x, '')`, which is `str(x)` but for a type with its own `__format__` (pgpy's PGPUID)
pub fn format_empty(v: &V) -> R<String> {
    if let Some(r) = super::pgpy::format_hook(v) {
        return r;
    }
    str_(v)
}

pub fn format_spec(v: &V, spec: &str) -> R<String> {
    if let V::Decimal(d) = v {
        return super::decimal::format(d, spec);
    }
    if let Some(r) = super::pgpy::format_hook(v) {
        return r;
    }
    if let Some(s) = super::pgpy::as_str(v) {
        return format_spec(&s, spec);
    }
    if spec.is_empty() {
        return str_(v);
    }
    // datetime.__format__ / date.__format__: strftime(spec)
    match v {
        V::DateTime(d) => return Ok(super::libs::strftime(&d.wall, d.offset(), d.tz.and_then(|t| t.tzname(&d.wall, d.fold).ok().flatten()), spec)?),
        V::Date(d) => return Ok(super::libs::strftime(&d.and_hms_opt(0, 0, 0).unwrap(), None, None, spec)?),
        // time.__format__: strftime on 1900-01-01
        V::Time(t) => return Ok(super::libs::strftime(&chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap().and_time(*t), None, None, spec)?),
        _ => {}
    }
    if let V::Enum(e, i) = v {
        return match e.kind {
            EnumKind::Plain | EnumKind::Str => format_spec(&V::str(str_(v)?), spec),
            _ => format_spec(&e.value(*i), spec),
        };
    }
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let (mut fill, mut align) = (' ', None);
    if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^' | '=') {
        fill = chars[0];
        align = Some(chars[1]);
        i = 2;
    } else if !chars.is_empty() && matches!(chars[0], '<' | '>' | '^' | '=') {
        align = Some(chars[0]);
        i = 1;
    }
    let mut sign = '-';
    if i < chars.len() && matches!(chars[i], '+' | '-' | ' ') {
        sign = chars[i];
        i += 1;
    }
    let mut alt = false;
    if i < chars.len() && chars[i] == '#' {
        alt = true;
        i += 1;
    }
    if i < chars.len() && chars[i] == '0' {
        fill = '0';
        if align.is_none() {
            align = Some('=');
        }
        i += 1;
    }
    // CPython: a number past Py_ssize_t is a ValueError (it wrapped here, then panicked or aborted)
    let too_many = || Exc::value_error("Too many decimal digits in format string");
    let push_digit = |n: usize, c: char| n.checked_mul(10).and_then(|n| n.checked_add(c.to_digit(10).unwrap_or(0) as usize)).filter(|n| *n <= isize::MAX as usize);
    let mut width = 0usize;
    while i < chars.len() && chars[i].is_ascii_digit() {
        width = push_digit(width, chars[i]).ok_or_else(too_many)?;
        i += 1;
    }
    let mut thousands: Option<char> = None;
    if i < chars.len() && (chars[i] == ',' || chars[i] == '_') {
        thousands = Some(chars[i]);
        i += 1;
    }
    let mut prec: Option<usize> = None;
    // Python 3.14: a separator for the fractional part, after the precision (`.6_f`)
    let mut frac_sep: Option<char> = None;
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let mut p = 0;
        while i < chars.len() && chars[i].is_ascii_digit() {
            p = push_digit(p, chars[i]).ok_or_else(too_many)?;
            i += 1;
        }
        prec = Some(p);
        if i < chars.len() && (chars[i] == ',' || chars[i] == '_') && super::python() >= (3, 14) {
            frac_sep = Some(chars[i]);
            i += 1;
        }
    }
    let invalid = || Exc::value_error(format!("Invalid format specifier '{spec}' for object of type '{}'", v.type_name()));
    if i + 1 < chars.len() {
        return Err(invalid());
    }
    let ty = chars.get(i).copied();
    if !matches!(v, V::Int(_) | V::Float(_) | V::Bool(_) | V::Str(_)) {
        return Err(Exc::type_error(format!("unsupported format string passed to {}.__format__", v.type_name())));
    }
    if let (Some(','), Some(c @ ('x' | 'X' | 'o' | 'b' | 's' | 'c' | 'n'))) = (thousands, ty) {
        return Err(Exc::value_error(format!("Cannot specify ',' with '{c}'.")));
    }
    if frac_sep.is_some() && !(matches!(v, V::Float(_) | V::Int(_) | V::Bool(_)) && matches!(ty, None | Some('f' | 'F' | 'e' | 'E' | 'g' | 'G' | '%'))) {
        return Err(invalid());
    }
    let numeric = matches!(v, V::Int(_) | V::Float(_) | V::Bool(_));
    // a float's precision: past INT_MAX "precision too big", else as many digits as memory holds (MemoryError)
    if let Some(p) = prec {
        if numeric && (matches!(ty, Some('f' | 'F' | 'e' | 'E' | 'g' | 'G' | '%')) || (ty.is_none() && matches!(v, V::Float(_)))) {
            if p > i32::MAX as usize {
                return Err(Exc::value_error("precision too big"));
            }
            repeat_room(1, p, "precision too big")?;
        }
    }
    let mut body = match (ty, v) {
        // CPython: "nan"/"inf" (upper case for F, E, G), the sign handled below; no '%' suffix scaling
        (Some(c @ ('f' | 'F' | 'e' | 'E' | 'g' | 'G' | '%')), V::Float(x)) if !x.is_finite() => {
            let t = if x.is_nan() { "nan" } else { "inf" };
            let t = if c.is_ascii_uppercase() { t.to_ascii_uppercase() } else { t.to_string() };
            if c == '%' { t + "%" } else { t }
        }
        (Some('d'), _) if int_of(v).is_some() => int_of(v).unwrap().abs().to_string(),
        (Some('f') | Some('F'), _) if num(v).is_some() => format!("{:.*}", prec.unwrap_or(6), num(v).unwrap().abs()),
        (Some('%'), _) if num(v).is_some() => format!("{:.*}%", prec.unwrap_or(6), num(v).unwrap().abs() * 100.0),
        (Some('x'), _) if int_of(v).is_some() => format!("{}{:x}", if alt { "0x" } else { "" }, int_of(v).unwrap().abs()),
        (Some('X'), _) if int_of(v).is_some() => format!("{}{:X}", if alt { "0X" } else { "" }, int_of(v).unwrap().abs()),
        (Some('o'), _) if int_of(v).is_some() => format!("{}{:o}", if alt { "0o" } else { "" }, int_of(v).unwrap().abs()),
        (Some('b'), _) if int_of(v).is_some() => format!("{}{:b}", if alt { "0b" } else { "" }, int_of(v).unwrap().abs()),
        (Some(c @ ('e' | 'E')), _) if num(v).is_some() => fmt_exp(num(v).unwrap().abs(), prec.unwrap_or(6), c == 'E'),
        (Some(c @ ('g' | 'G')), _) if num(v).is_some() => fmt_general(num(v).unwrap().abs(), prec.unwrap_or(6), c == 'G'),
        (Some('s') | None, V::Str(s)) => match prec {
            Some(p) => s.chars().take(p).collect(),
            None => s.to_string(),
        },
        (None, V::Float(x)) if prec.is_some() => fmt_general_none(x.abs(), prec.unwrap()),
        (None, V::Int(_) | V::Bool(_)) if prec.is_some() => return Err(Exc::value_error("Precision not allowed in integer format specifier")),
        (None, _) if numeric => {
            let s = str_(v)?;
            s.trim_start_matches('-').to_string()
        }
        (None, V::DateTime(_) | V::Date(_)) => str_(v)?,
        (Some(c), _) => return Err(Exc::value_error(format!("Unknown format code '{c}' for object of type '{}'", v.type_name()))),
        (None, _) => return Err(invalid()),
    };
    if let Some(sep) = frac_sep {
        // the digits right after the point, grouped by 3 from the point (`1.500,0`)
        if let Some(p) = body.find('.') {
            let digits: String = body[p + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
            let tail = body[p + 1 + digits.len()..].to_string();
            let mut grouped = String::new();
            for (k, c) in digits.chars().enumerate() {
                if k > 0 && k % 3 == 0 {
                    grouped.push(sep);
                }
                grouped.push(c);
            }
            body = format!("{}.{grouped}{tail}", &body[..p]);
        }
    }
    if let Some(sep) = thousands {
        // `_` groups hexadecimal, octal and binary digits by 4
        let n = if sep == '_' && matches!(ty, Some('x' | 'X' | 'o' | 'b')) { 4 } else { 3 };
        let skip = if alt && matches!(ty, Some('x' | 'X' | 'o' | 'b')) { 2 } else { 0 };
        let (prefix, body_) = body.split_at(skip);
        let end = body_.find(|c: char| if n == 4 { !c.is_ascii_hexdigit() } else { !c.is_ascii_digit() }).unwrap_or(body_.len());
        let (int_part, rest) = body_.split_at(end);
        let mut grouped = String::new();
        for (k, c) in int_part.chars().enumerate() {
            if k > 0 && (int_part.len() - k) % n == 0 {
                grouped.push(sep);
            }
            grouped.push(c);
        }
        body = format!("{prefix}{grouped}{rest}");
    }
    // a float's sign bit: format(-0.0, ".2f") == "-0.00" (NaN has no sign in CPython's formatting)
    let negative = match v {
        V::Float(x) => x.is_sign_negative() && !x.is_nan(),
        _ => num(v).map(|x| x < 0.0).unwrap_or(false),
    };
    let sign_s = if numeric {
        if negative {
            "-"
        } else if sign == '+' {
            "+"
        } else if sign == ' ' {
            " "
        } else {
            ""
        }
    } else {
        ""
    };
    let len = body.chars().count() + sign_s.len();
    if len >= width {
        return Ok(format!("{sign_s}{body}"));
    }
    let pad = width - len;
    repeat_room(fill.len_utf8(), pad, "Too many decimal digits in format string")?;
    let fills: String = std::iter::repeat_n(fill, pad).collect();
    Ok(match align.unwrap_or(if numeric { '>' } else { '<' }) {
        '<' => format!("{sign_s}{body}{fills}"),
        '^' => {
            let left: String = std::iter::repeat_n(fill, pad / 2).collect();
            let right: String = std::iter::repeat_n(fill, pad - pad / 2).collect();
            format!("{left}{sign_s}{body}{right}")
        }
        '=' => format!("{sign_s}{fills}{body}"),
        _ => format!("{fills}{sign_s}{body}"),
    })
}

/// `"fmt" % args` (printf-style, as used by logging).
pub fn percent_format(fmt: &str, args: &V) -> R<String> {
    // Python 3.15 reworded every error: the argument's number (or key), the `%`'s position, the counts
    let py315 = super::python() >= (3, 15);
    let tuple = matches!(args, V::Tuple(_));
    let items: Vec<V> = match args {
        V::Tuple(t) => t.to_vec(),
        other => vec![other.clone()],
    };
    let given = items.len();
    // a mapping on the right: `%(key)s` reads it, and unused values are no error (CPython's PyMapping_Check)
    let mapping = matches!(args, V::Dict(_));
    let mut used_key = false;
    let mut consumed = 0usize;
    let mut it = items.into_iter();
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '%' {
            out.push(c);
            i += 1;
            continue;
        }
        let pct = i;
        i += 1;
        if i < chars.len() && chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        let mut key: Option<String> = None;
        if i < chars.len() && chars[i] == '(' {
            // `%(name)`: nested parentheses count, as in CPython
            let mut depth = 1;
            let start = i + 1;
            i += 1;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            if depth > 0 {
                return Err(Exc::value_error(if py315 { format!("stray % or incomplete format key at position {pct}") } else { "incomplete format key".into() }));
            }
            key = Some(chars[start..i - 1].iter().collect());
        }
        let mut spec = String::new();
        while i < chars.len() && "-+ #0123456789.".contains(chars[i]) {
            spec.push(chars[i]);
            i += 1;
        }
        let Some(&conv) = chars.get(i) else {
            return Err(Exc::value_error(if py315 { format!("stray % at position {pct}") } else { "incomplete format".into() }));
        };
        if conv == '*' {
            return Err(Exc::type_error("py2axum: a `*` width or precision in %-formatting is not supported"));
        }
        if !"srdiuaxXoeEfFgGc".contains(conv) {
            return Err(Exc::value_error(if py315 {
                format!("unsupported format %{conv} at position {pct}")
            } else {
                format!("unsupported format character '{conv}' (0x{:x}) at index {i}", conv as u32)
            }));
        }
        let shown = conv;
        let conv = match conv {
            'u' => 'd',
            'F' => 'f',
            c => c,
        };
        i += 1;
        let arg = match &key {
            Some(k) => {
                if !mapping {
                    return Err(Exc::type_error(if py315 { format!("format requires a mapping, not {}", args.type_name()) } else { "format requires a mapping".into() }));
                }
                used_key = true;
                getitem(args, &V::str(k))?
            }
            None if mapping && used_key => {
                return Err(if py315 {
                    Exc::value_error(format!("format requires a parenthesised mapping key at position {pct}"))
                } else {
                    Exc::type_error("not enough arguments for format string")
                });
            }
            None => {
                consumed += 1;
                it.next().ok_or_else(|| {
                    Exc::type_error(if py315 { format!("not enough arguments for format string (got {given})") } else { "not enough arguments for format string".into() })
                })?
            }
        };
        // `format argument 2: ` (3.15) before the conversion's complaint
        let which = match &key {
            Some(k) => format!("format argument {}: ", repr(&V::str(k))?),
            None if tuple => format!("format argument {consumed}: "),
            None => "format argument: ".to_string(),
        };
        let needs = |what: &str, old: String| -> Exc {
            Exc::type_error(if py315 { format!("{which}%{shown} requires {what}, not {}", arg.type_name()) } else { old })
        };
        let left = spec.starts_with('-');
        let zero = spec.trim_start_matches('-').starts_with('0');
        // CPython: a width past Py_ssize_t is "width too big", a precision past INT_MAX "precision too big"
        // (they wrapped to 0 here); the text they make is checked against memory below
        let (ws, ps) = match spec.trim_start_matches(['-', '+', ' ', '#', '0']).split_once('.') {
            Some((w, p)) => (w, Some(p)),
            None => (spec.trim_start_matches(['-', '+', ' ', '#', '0']), None),
        };
        let digits_of = |t: &str, max: u64, what: &str| -> R<usize> {
            if t.is_empty() {
                return Ok(0);
            }
            // Python 3.15 adds the position of the `%`
            let msg = if py315 { format!("{what} too big at position {pct}") } else { format!("{what} too big") };
            t.parse::<u64>().ok().filter(|n| *n <= max).map(|n| n as usize).ok_or_else(|| Exc::value_error(msg))
        };
        let w = digits_of(ws, isize::MAX as u64, "width")?;
        let p = match ps {
            Some(t) => Some(digits_of(t, i32::MAX as u64, "precision")?),
            None => None,
        };
        if let (Some(p), 'f' | 'e' | 'E' | 'g' | 'G') = (p, conv) {
            repeat_room(1, p, "precision too big")?;
        }
        let body = match conv {
            's' => str_(&arg)?,
            'r' => repr(&arg)?,
            'd' | 'i' => match (int_of(&arg), num(&arg)) {
                (Some(x), _) => x.to_string(),
                (None, Some(f)) => (f.trunc() as i64).to_string(),
                _ => return Err(needs("a real number", format!("%{shown} format: a real number is required, not {}", arg.type_name()))),
            },
            'f' => format!("{:.*}", p.unwrap_or(6), num(&arg).ok_or_else(|| needs("a real number", format!("must be real number, not {}", arg.type_name())))?),
            'x' | 'X' | 'o' => {
                let n = int_of(&arg).ok_or_else(|| needs("an integer", format!("%{shown} format: an integer is required, not {}", arg.type_name())))?;
                let body = format_spec(&V::Int(n), &shown.to_string())?;
                if n < 0 && !body.starts_with('-') { format!("-{body}") } else { body }
            }
            'e' | 'E' | 'g' | 'G' => {
                // same presentation as format(): flags and width applied below
                let n = num(&arg).ok_or_else(|| needs("a real number", format!("must be real number, not {}", arg.type_name())))?;
                let body = format_spec(&V::Float(n), &match p { Some(p) => format!(".{p}{conv}"), None => conv.to_string() })?;
                if n < 0.0 && !body.starts_with('-') { format!("-{body}") } else { body }
            }
            'c' => match &arg {
                V::Str(s) if s.chars().count() == 1 => s.to_string(),
                V::Int(n) => char::from_u32(*n as u32).filter(|_| (0..=0x10FFFF).contains(n)).map(String::from)
                    .ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "%c arg not in range(0x110000)"))?,
                V::Str(s) => {
                    let n = s.chars().count();
                    return Err(Exc::type_error(if py315 {
                        format!("{which}%c requires an integer or a unicode character, not a string of length {n}")
                    } else if super::python() >= (3, 14) {
                        format!("%c requires an int or a unicode character, not a string of length {n}")
                    } else {
                        "%c requires int or char".to_string()
                    }));
                }
                o => return Err(needs("an integer or a unicode character", if super::python() >= (3, 14) {
                    format!("%c requires an int or a unicode character, not {}", o.type_name())
                } else {
                    "%c requires int or char".to_string()
                })),
            },
            _ => str_(&arg)?,
        };
        let body = if conv == 's' { match p { Some(p) => body.chars().take(p).collect(), None => body } } else { body };
        let n = body.chars().count();
        if n < w {
            let pad = w - n;
            repeat_room(1, pad, "width too big")?;
            if left {
                out.push_str(&body);
                out.push_str(&" ".repeat(pad));
            } else if zero && matches!(conv, 'd' | 'i' | 'f' | 'x') {
                if let Some(stripped) = body.strip_prefix('-') {
                    out.push('-');
                    out.push_str(&"0".repeat(pad));
                    out.push_str(stripped);
                } else {
                    out.push_str(&"0".repeat(pad));
                    out.push_str(&body);
                }
            } else {
                out.push_str(&" ".repeat(pad));
                out.push_str(&body);
            }
        } else {
            out.push_str(&body);
        }
    }
    if it.next().is_some() && !mapping && !used_key {
        return Err(Exc::type_error(if py315 {
            format!("not all arguments converted during string formatting (required {consumed}, got {given})")
        } else {
            "not all arguments converted during string formatting".into()
        }));
    }
    Ok(out)
}

// ---------------------------------------------------------------- containers

fn norm_index(i: i64, len: usize) -> Option<usize> {
    let j = if i < 0 { i + len as i64 } else { i };
    if j < 0 || j >= len as i64 { None } else { Some(j as usize) }
}

fn slice_bounds(len: usize, start: &V, stop: &V, step: &V) -> R<(Vec<usize>,)> {
    let step = match step {
        V::None => 1,
        s => int_of(s).ok_or_else(|| Exc::type_error("slice indices must be integers"))?,
    };
    if step == 0 {
        return Err(Exc::value_error("slice step cannot be zero"));
    }
    let len_i = len as i64;
    let clamp = |v: &V, default: i64| -> R<i64> {
        match v {
            V::None => Ok(default),
            x => {
                let mut i = int_of(x).ok_or_else(|| Exc::type_error("slice indices must be integers"))?;
                if i < 0 {
                    i += len_i;
                }
                Ok(if step > 0 { i.clamp(0, len_i) } else { i.clamp(-1, len_i - 1) })
            }
        }
    };
    let (s, e) = if step > 0 { (clamp(start, 0)?, clamp(stop, len_i)?) } else { (clamp(start, len_i - 1)?, clamp(stop, -1)?) };
    let mut idx = Vec::new();
    let mut i = s;
    while (step > 0 && i < e) || (step < 0 && i > e) {
        idx.push(i as usize);
        i += step;
    }
    Ok((idx,))
}

/// `lst[a:b] = iterable` (`lst[a:b:c] = ...` needs as many items as the slice)
pub fn setslice(v: &V, start: &V, stop: &V, step: &V, val: V) -> R<()> {
    if let Some(e) = super::proto::slice_refused(v, "__setitem__") {
        return Err(e);
    }
    let V::List(l) = v else {
        return Err(Exc::type_error(format!("'{}' object does not support slice assignment", v.type_name())));
    };
    let items = iter(&val)?;
    let mut list = l.lock();
    let len = list.len() as i64;
    if matches!(step, V::None) || int_of(step) == Some(1) {
        let bound = |x: &V, d: i64| -> R<i64> {
            match x {
                V::None => Ok(d),
                x => {
                    let mut i = int_of(x).ok_or_else(|| Exc::type_error("slice indices must be integers or None or have an __index__ method"))?;
                    if i < 0 {
                        i += len;
                    }
                    Ok(i.clamp(0, len))
                }
            }
        };
        let lo = bound(start, 0)?;
        let hi = bound(stop, len)?.max(lo);
        list.splice(lo as usize..hi as usize, items);
        return Ok(());
    }
    let (idx,) = slice_bounds(list.len(), start, stop, step)?;
    if idx.len() != items.len() {
        return Err(Exc::value_error(format!(
            "attempt to assign sequence of size {} to extended slice of size {}",
            items.len(),
            idx.len()
        )));
    }
    for (i, x) in idx.into_iter().zip(items) {
        list[i] = x;
    }
    Ok(())
}

pub fn getslice(v: &V, start: &V, stop: &V, step: &V) -> R {
    if let Some(e) = super::proto::slice_refused(v, "__getitem__") {
        return Err(e);
    }
    if let Some(t) = seq_tuple(v) {
        return getslice(&t, start, stop, step);
    }
    if let Some(s) = super::pgpy::as_str(v) {
        return getslice(&s, start, stop, step);
    }
    Ok(match v {
        V::Str(s) => {
            let cs: Vec<char> = s.chars().collect();
            let (idx,) = slice_bounds(cs.len(), start, stop, step)?;
            V::str(idx.into_iter().map(|i| cs[i]).collect::<String>())
        }
        V::List(l) => {
            let items = l.lock().clone();
            let (idx,) = slice_bounds(items.len(), start, stop, step)?;
            V::list(idx.into_iter().map(|i| items[i].clone()).collect())
        }
        V::Tuple(t) => {
            let (idx,) = slice_bounds(t.len(), start, stop, step)?;
            V::tuple(idx.into_iter().map(|i| t[i].clone()).collect())
        }
        V::Bytes(b) => {
            let (idx,) = slice_bounds(b.len(), start, stop, step)?;
            V::Bytes(Arc::from(idx.into_iter().map(|i| b[i]).collect::<Vec<u8>>()))
        }
        _ => return Err(Exc::type_error(format!("'{}' object is not subscriptable", v.type_name()))),
    })
}

// ---------------------------------------------------------------- collections.defaultdict

type DictCell = Mutex<IndexMap<Key, (V, V)>>;

/// defaultdicts are plain dicts registered here with their factory (a builtin type): only a missing key
/// and `repr` look it up. The `Weak` keeps the allocation, so an address is never reused while registered.
static DEFAULTS: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, (std::sync::Weak<DictCell>, &'static str)>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// `collections.defaultdict(factory, init)`; `init` is already a fresh dict.
pub fn defaultdict(factory: &'static str, init: V) -> R {
    let V::Dict(d) = &init else { return Err(Exc::type_error("py2axum: defaultdict initial value must be a dict")) };
    let mut reg = DEFAULTS.lock();
    if reg.len() >= 64 && reg.len().is_power_of_two() {
        reg.retain(|_, (w, _)| w.strong_count() > 0);
    }
    reg.insert(Arc::as_ptr(d) as usize, (Arc::downgrade(d), factory));
    Ok(init)
}

pub fn default_factory(d: &Arc<DictCell>) -> Option<&'static str> {
    let reg = DEFAULTS.lock();
    if reg.is_empty() {
        return None;
    }
    reg.get(&(Arc::as_ptr(d) as usize)).filter(|(w, _)| w.strong_count() > 0).map(|(_, f)| *f)
}

// ---------------------------------------------------------------- collections.Counter

/// a Counter is a dict registered with the factory "Counter": a missing key reads 0 (not stored)
pub fn is_counter(d: &Arc<DictCell>) -> bool {
    default_factory(d) == Some("Counter")
}

fn new_counter() -> R<Arc<DictCell>> {
    let d = Arc::new(Mutex::new(IndexMap::new()));
    defaultdict("Counter", V::Dict(d.clone()))?;
    Ok(d)
}

/// `Counter.update()` / `subtract()`: from a mapping its counts, from an iterable each element once
fn counter_add(d: &Arc<DictCell>, src: &V, kwargs: &[(String, V)], sign: i64) -> R<()> {
    let mut pairs: Vec<(V, V)> = Vec::new();
    match src {
        V::None => {}
        V::Dict(m) => pairs.extend(m.lock().values().cloned()),
        other => pairs.extend(iter(other)?.into_iter().map(|x| (x, V::Int(1)))),
    }
    pairs.extend(kwargs.iter().map(|(k, v)| (V::str(k), v.clone())));
    for (k, n) in pairs {
        let key = Key::dict_key(&k)?;
        let cur = d.lock().get(&key).map(|(_, v)| v.clone());
        let n = if sign < 0 { neg(&n)? } else { n };
        let v = match cur {
            Some(c) => add(&c, &n)?,
            // `self[elem] = count + self_get(elem, 0)`: an absent key gets 0 + count
            None => add(&V::Int(0), &n)?,
        };
        dict_put(&mut d.lock(), key, k, v);
    }
    Ok(())
}

/// `collections.Counter(iterable_or_mapping=None, /, **kwds)`
pub fn counter(args: &[V], kwargs: &[(String, V)]) -> R {
    if args.len() > 1 {
        return Err(Exc::type_error(format!("expected at most 1 argument, got {}", args.len())));
    }
    let d = new_counter()?;
    counter_add(&d, args.first().unwrap_or(&V::None), kwargs, 1)?;
    Ok(V::Dict(d))
}

fn most_common(d: &Arc<DictCell>) -> R<Vec<(V, V)>> {
    let mut items: Vec<(V, V)> = d.lock().values().cloned().collect();
    let mut err = None;
    // sorted(..., key=count, reverse=True): stable, ties keep the insertion order
    items.sort_by(|a, b| match cmp(&b.1, &a.1) {
        Ok(o) => o,
        Err(e) => {
            err.get_or_insert(e);
            std::cmp::Ordering::Equal
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(items),
    }
}

/// Counter's own methods (the others are dict's); None: not one of them
pub fn counter_method(d: &Arc<DictCell>, name: &str, args: &[V], kwargs: &[(String, V)]) -> Option<R> {
    if !matches!(name, "most_common" | "update" | "subtract" | "elements" | "total" | "copy") {
        return None;
    }
    Some((|| match name {
        "most_common" => {
            let items = most_common(d)?;
            let n = match args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "n").map(|(_, v)| v)) {
                None | Some(V::None) => items.len(),
                Some(V::Int(n)) => (*n).max(0) as usize,
                Some(o) => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
            };
            Ok(V::list(items.into_iter().take(n).map(|(k, v)| V::tuple(vec![k, v])).collect()))
        }
        "update" | "subtract" => {
            counter_add(d, args.first().unwrap_or(&V::None), kwargs, if name == "update" { 1 } else { -1 })?;
            Ok(V::None)
        }
        "elements" => {
            let mut out = Vec::new();
            for (k, v) in d.lock().values() {
                let n = match v {
                    V::Int(n) => *n,
                    V::Bool(b) => *b as i64,
                    o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
                };
                out.extend(std::iter::repeat_n(k.clone(), n.max(0) as usize));
            }
            Ok(V::list(out))
        }
        "total" => {
            let mut t = V::Int(0);
            for (_, v) in d.lock().values() {
                t = add(&t, v)?;
            }
            Ok(t)
        }
        "copy" => {
            let c = new_counter()?;
            *c.lock() = d.lock().clone();
            Ok(V::Dict(c))
        }
        _ => unreachable!(),
    })())
}

/// `+ - | &` between two Counters (positive counts kept, as collections.Counter)
pub fn counter_binop(a: &V, op: &str, b: &V) -> Option<R> {
    let (V::Dict(x), V::Dict(y)) = (a, b) else { return None };
    if !is_counter(x) {
        return None;
    }
    if !is_counter(y) {
        return Some(Err(Exc::type_error(format!("unsupported operand type(s) for {op}: 'Counter' and 'dict'"))));
    }
    Some((|| {
        let out = new_counter()?;
        let (xs, ys) = (x.lock().clone(), y.lock().clone());
        let zero = V::Int(0);
        let pos = |v: &V| -> R<bool> { Ok(cmp(v, &zero)? == std::cmp::Ordering::Greater) };
        for (k, (kv, c)) in &xs {
            let o = ys.get(k).map(|(_, v)| v.clone()).unwrap_or(V::Int(0));
            let n = match op {
                "+" => add(c, &o)?,
                "-" => sub(c, &o)?,
                "|" => if cmp(c, &o)? == std::cmp::Ordering::Less { o } else { c.clone() },
                _ => if cmp(c, &o)? == std::cmp::Ordering::Less { c.clone() } else { o },
            };
            if pos(&n)? {
                out.lock().insert(k.clone(), (kv.clone(), n));
            }
        }
        if op != "&" {
            for (k, (kv, c)) in &ys {
                if xs.contains_key(k) {
                    continue;
                }
                let n = if op == "-" { sub(&V::Int(0), c)? } else { c.clone() };
                if pos(&n)? {
                    out.lock().insert(k.clone(), (kv.clone(), n));
                }
            }
        }
        Ok(V::Dict(out))
    })())
}

fn default_value(factory: &str) -> V {
    match factory {
        "int" => V::Int(0),
        "float" => V::Float(0.0),
        "str" => V::str(""),
        "bool" => V::Bool(false),
        "list" => V::list(vec![]),
        "deque" => V::native(Native::Deque(Arc::new(super::deque::Deque { items: parking_lot::Mutex::new(Default::default()), maxlen: None }))),
        "tuple" => V::tuple(vec![]),
        "set" => V::Set(Arc::new(Mutex::new(IndexMap::new()))),
        _ => V::Dict(Arc::new(Mutex::new(IndexMap::new()))),
    }
}

pub fn getitem(v: &V, k: &V) -> R {
    if let Some(r) = super::proto::getitem(v, k) {
        return r;
    }
    if let Some(s) = super::pgpy::as_str(v) {
        return getitem(&s, k);
    }
    if let Some(t) = seq_tuple(v) {
        return getitem(&t, k);
    }
    if let V::Native(n) = v {
        match &**n {
            Native::CtCache(c) => return super::ctools::getitem(c, k),
            Native::JwkSet(s) => return super::jwks::set_getitem(s, k),
            Native::SaColumns(t) => return super::satable::getitem(t, k),
            Native::Mime(m) => return super::mail::mime_getitem(m, k),
            Native::EmailMsg(m) => return super::emailmsg::getitem(m, k),
            Native::Xl(x) => return super::xlsx::getitem(x, k),
            Native::Rl(x) => return super::rlbind::getitem(x, k),
            Native::DnsAnswer(a) => {
                let items = super::dnsres::answer_items(a);
                if a.rrset.is_none() {
                    return Err(Exc::new(&INDEX_ERROR, vec![]));
                }
                return match k {
                    V::Int(i) if *i >= 0 && (*i as usize) < items.len() => Ok(items[*i as usize].clone()),
                    V::Int(i) if *i >= 0 => Err(Exc::new(&STOP_ITERATION, vec![])),
                    V::Int(_) => Err(Exc::value_error("Indices for islice() must be None or an integer: 0 <= x <= sys.maxsize.")),
                    _ => Err(Exc::type_error("py2axum: Answer[...] takes an int index")),
                };
            }
            Native::Deque(d) => return super::deque::getitem(d, k),
            Native::Array(a) => return super::arr::getitem(a, k),
            Native::RespHeaders(r) => return super::resp::headers_getitem(&r.headers, k),
            Native::CellHeaders(c) => return super::resp::headers_getitem(&c.headers, k),
            Native::Headers(r) => {
                let key = str_(k)?;
                return r.header(&key).map(V::str).ok_or_else(|| Exc::new(&KEY_ERROR, vec![k.clone()]));
            }
            Native::UrlParts(u) => return getitem(&V::tuple(super::stdlib::url_parts_tuple(u)), k),
            Native::Record(_, f) => return getitem(&V::tuple(f.iter().map(|(_, x)| x.clone()).collect()), k),
            Native::Address(h, p) => {
                return match int_of(k) {
                    Some(0) | Some(-2) => Ok(V::str(h)),
                    Some(1) | Some(-1) => Ok(V::Int(*p as i64)),
                    _ => Err(Exc::msg(&INDEX_ERROR, "tuple index out of range")),
                }
            }
            _ => {}
        }
    }
    Ok(match v {
        V::List(l) => {
            let l = l.lock();
            let i = int_of(k).ok_or_else(|| Exc::type_error("list indices must be integers or slices"))?;
            norm_index(i, l.len()).map(|j| l[j].clone()).ok_or_else(|| Exc::msg(&INDEX_ERROR, "list index out of range"))?
        }
        V::Tuple(t) => {
            let i = int_of(k).ok_or_else(|| Exc::type_error("tuple indices must be integers or slices"))?;
            norm_index(i, t.len()).map(|j| t[j].clone()).ok_or_else(|| Exc::msg(&INDEX_ERROR, "tuple index out of range"))?
        }
        V::Bytes(b) => {
            let i = int_of(k).ok_or_else(|| Exc::type_error(format!("byte indices must be integers or slices, not {}", k.type_name())))?;
            norm_index(i, b.len()).map(|j| V::Int(b[j] as i64)).ok_or_else(|| Exc::msg(&INDEX_ERROR, "index out of range"))?
        }
        V::Str(s) => {
            let cs: Vec<char> = s.chars().collect();
            let i = int_of(k).ok_or_else(|| Exc::type_error("string indices must be integers"))?;
            norm_index(i, cs.len()).map(|j| V::str(cs[j].to_string())).ok_or_else(|| Exc::msg(&INDEX_ERROR, "string index out of range"))?
        }
        V::Dict(d) => {
            if let Some(e) = super::stripe::list_getitem(d, k) {
                return Err(e);
            }
            let key = Key::dict_key(k)?;
            let found = d.lock().get(&key).map(|(_, v)| v.clone());
            match found {
                Some(v) => v,
                None if super::stripe::missing_key(d, k).is_some() => return Err(super::stripe::missing_key(d, k).unwrap()),
                None => match default_factory(d) {
                    // Counter.__missing__: 0, not stored
                    Some("Counter") => V::Int(0),
                    // defaultdict.__missing__: the factory's value is stored, then returned
                    Some(f) => {
                        let v = default_value(f);
                        dict_put(&mut d.lock(), key, k.clone(), v.clone());
                        v
                    }
                    None => return Err(Exc::new(&KEY_ERROR, vec![k.clone()])),
                },
            }
        }
        V::Inst(i) => i.getitem(k)?,
        V::Class(c) => match c.kind {
            ClassKind::Enum(e) => {
                let n = str_(k)?;
                e.by_name(&n).ok_or_else(|| Exc::new(&KEY_ERROR, vec![k.clone()]))?
            }
            _ => return Err(Exc::type_error(format!("type '{}' is not subscriptable", c.name))),
        },
        V::Enum(e, i) if e.kind != EnumKind::Plain => getitem(&e.value(*i), k)?,
        V::Col(..) | V::Sql(_) => super::orm::sql_getitem(v, k)?,
        _ => return Err(Exc::type_error(format!("'{}' object is not subscriptable", v.type_name()))),
    })
}

pub fn setitem(v: &V, k: &V, val: V) -> R<()> {
    if let Some(r) = super::proto::setitem(v, k, &val) {
        return r;
    }
    if let V::Native(n) = v {
        match &**n {
            Native::CtCache(c) => return super::ctools::setitem(c, k, val),
            Native::Deque(d) => return super::deque::setitem(d, k, val),
            Native::Array(a) => return super::arr::setitem(a, k, val),
            Native::Mime(m) => return super::mail::mime_setitem(m, k, &val),
            Native::EmailMsg(m) => return super::emailmsg::setitem(m, k, &val),
            Native::Xl(x) => return super::xlsx::setitem(x, k, val),
            Native::JinjaFilters(j) => return super::mail::add_filter(j, k, val),
            Native::RespHeaders(r) => return super::resp::headers_setitem(&r.headers, k, &val),
            Native::CellHeaders(c) => return super::resp::headers_setitem(&c.headers, k, &val),
            _ => {}
        }
    }
    match v {
        V::List(l) => {
            let mut l = l.lock();
            let i = int_of(k).ok_or_else(|| Exc::type_error("list indices must be integers or slices"))?;
            let len = l.len();
            let j = norm_index(i, len).ok_or_else(|| Exc::msg(&INDEX_ERROR, "list assignment index out of range"))?;
            l[j] = val;
        }
        V::Dict(d) => {
            super::stripe::setitem_check(v, k, &val)?;
            dict_put(&mut d.lock(), Key::dict_key(k)?, k.clone(), val);
        }
        _ => return Err(Exc::type_error(format!("'{}' object does not support item assignment", v.type_name()))),
    }
    Ok(())
}

pub fn delitem(v: &V, k: &V) -> R<()> {
    if let Some(r) = super::proto::delitem(v, k) {
        return r;
    }
    if let V::Native(n) = v {
        if let Native::CtCache(c) = &**n {
            return super::ctools::delitem(c, k);
        }
        if let Native::RespHeaders(r) = &**n {
            return super::resp::headers_delitem(&r.headers, k);
        }
        if let Native::CellHeaders(c) = &**n {
            return super::resp::headers_delitem(&c.headers, k);
        }
        if let Native::EmailMsg(m) = &**n {
            return super::emailmsg::delitem(m, k);
        }
    }
    match v {
        V::Dict(d) => {
            if d.lock().shift_remove(&Key::dict_key(k)?).is_none() {
                return Err(Exc::new(&KEY_ERROR, vec![k.clone()]));
            }
        }
        V::List(l) => {
            let mut l = l.lock();
            let i = int_of(k).ok_or_else(|| Exc::type_error("list indices must be integers"))?;
            let len = l.len();
            let j = norm_index(i, len).ok_or_else(|| Exc::msg(&INDEX_ERROR, "list assignment index out of range"))?;
            l.remove(j);
        }
        _ => return Err(Exc::type_error(format!("'{}' object does not support item deletion", v.type_name()))),
    }
    Ok(())
}

/// Materialised iteration (a snapshot, like `list(x)`).
pub fn iter(v: &V) -> R<Vec<V>> {
    if let Some(r) = super::proto::iter_all(v) {
        return r;
    }
    if let Some(s) = super::pgpy::as_str(v) {
        return iter(&s);
    }
    if let Some(t) = seq_tuple(v) {
        return iter(&t);
    }
    if let V::Native(n) = v {
        if let Native::CtCache(c) = &**n {
            return Ok(super::ctools::iter(c));
        }
        if let Native::Xl(x) = &**n {
            return super::xlsx::iter(x);
        }
    }
    Ok(match v {
        V::List(l) => l.lock().clone(),
        V::Tuple(t) => t.to_vec(),
        V::Dict(d) => match super::stripe::list_data(d) {
            Some(data) => return iter(&data),
            None => d.lock().values().map(|(k, _)| k.clone()).collect(),
        },
        V::Set(s) | V::Frozen(s) => s.lock().values().cloned().collect(),
        V::Str(s) => s.chars().map(|c| V::str(c.to_string())).collect(),
        V::Bytes(b) => b.iter().map(|x| V::Int(*x as i64)).collect(),
        V::Result(r) => r.lock().take_rows()?,
        V::Native(n) if matches!(&**n, Native::EmailMsg(_)) => match &**n {
            Native::EmailMsg(m) => super::emailmsg::keys(m),
            _ => vec![],
        },
        V::Native(n) if matches!(&**n, Native::DnsAnswer(_)) => match &**n {
            Native::DnsAnswer(a) => super::dnsres::answer_items(a),
            _ => vec![],
        },
        V::Native(n) if matches!(&**n, Native::Deque(_)) => match &**n {
            Native::Deque(d) => super::deque::items(d),
            _ => vec![],
        },
        V::Native(n) if matches!(&**n, Native::Array(_)) => match &**n {
            Native::Array(a) => super::arr::items(a),
            _ => vec![],
        },
        V::Native(n) if matches!(&**n, Native::SaColumns(_)) => match &**n {
            Native::SaColumns(t) => super::satable::iter(t),
            _ => vec![],
        },
        // `for line in io.BytesIO(...)` (and a StreamingResponse over one): its lines from the position
        V::Native(n) if matches!(&**n, Native::BytesIO(_)) => match &**n {
            Native::BytesIO(b) => super::files::bytesio_lines(b),
            _ => vec![],
        },
        V::Native(n) if matches!(&**n, Native::File(_)) => match &**n {
            Native::File(f) => super::pathio::file_lines(f)?,
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::ValIter(_)) => match &**n {
            Native::ValIter(it) => it.drain()?,
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::CsvRows(..) | Native::StringIO(_) | Native::Iter(_)) => match &**n {
            Native::CsvRows(_, rows) => iter(rows)?,
            Native::Iter(q) => q.lock().drain(..).collect(),
            _ => super::stdlib::iter_lines(v).unwrap_or_default(),
        },
        // a Pydantic model iterates over its fields; a plain class or a dataclass is not iterable
        V::Inst(i) if i.desc.open || i.desc.dataclass => return Err(Exc::type_error(format!("'{}' object is not iterable", i.desc.name))),
        V::Inst(i) => i.iter_fields(),
        V::Native(n) if matches!(&**n, Native::CronGen(_)) => match &**n {
            Native::CronGen(g) => super::croniter::gen_iter(g)?,
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::Cron(_)) => {
            return Err(Exc::type_error("py2axum: iterating a croniter object is not supported (endless): call get_next() or next() on it"))
        }
        V::Native(n) if matches!(&**n, Native::UrlParts(_) | Native::Record(..) | Native::Headers(_) | Native::Address(..)) => match &**n {
            Native::Headers(r) => r.headers.iter().map(|(k, _)| V::str(k.to_lowercase())).collect(),
            Native::Address(h, p) => vec![V::str(h), V::Int(*p as i64)],
            Native::UrlParts(u) => super::stdlib::url_parts_tuple(u),
            Native::Record(_, f) => f.iter().map(|(_, x)| x.clone()).collect(),
            _ => unreachable!(),
        },
        V::Class(c) => match c.kind {
            ClassKind::Enum(e) => e.all(),
            _ => return Err(Exc::type_error(format!("'type' object is not iterable"))),
        },
        V::Enum(e, i) if e.kind != EnumKind::Plain => iter(&e.value(*i))?,
        _ => return Err(Exc::type_error(format!("'{}' object is not iterable", v.type_name()))),
    })
}

pub fn len(v: &V) -> R<usize> {
    if let Some(r) = super::proto::len(v) {
        return r;
    }
    if let Some(s) = super::pgpy::as_str(v) {
        return len(&s);
    }
    if let Some(t) = seq_tuple(v) {
        return len(&t);
    }
    if let V::Native(n) = v {
        match &**n {
            Native::CtCache(c) => return super::ctools::len(c),
            Native::Headers(r) => return Ok(r.headers.len()),
            Native::Address(..) => return Ok(2),
            _ => {}
        }
    }
    Ok(match v {
        V::List(l) => l.lock().len(),
        V::Tuple(t) => t.len(),
        V::Dict(d) => match super::stripe::list_data(d) {
            Some(data) => return len(&data),
            None => d.lock().len(),
        },
        V::Set(s) | V::Frozen(s) => s.lock().len(),
        V::Str(s) => s.chars().count(),
        V::Bytes(b) => b.len(),
        V::Class(c) => match c.kind {
            ClassKind::Enum(e) => e.members.len(),
            _ => return Err(Exc::type_error("object of type 'type' has no len()")),
        },
        V::Enum(e, i) if e.kind != EnumKind::Plain => len(&e.value(*i))?,
        V::Native(n) if matches!(&**n, Native::SaColumns(_)) => match &**n {
            Native::SaColumns(t) => t.cols.len(),
            _ => 0,
        },
        V::Native(n) if matches!(&**n, Native::Deque(_)) => match &**n {
            Native::Deque(d) => d.items.lock().len(),
            _ => 0,
        },
        V::Native(n) if matches!(&**n, Native::Array(_)) => match &**n {
            Native::Array(a) => a.items.lock().len(),
            _ => 0,
        },
        V::Native(n) if matches!(&**n, Native::EmailMsg(_)) => match &**n {
            Native::EmailMsg(m) => super::emailmsg::len(m),
            _ => 0,
        },
        V::Native(n) if matches!(&**n, Native::DnsAnswer(_)) => match &**n {
            Native::DnsAnswer(a) => a.rrset.as_ref().map(|s| s.len()).unwrap_or(0),
            _ => 0,
        },
        _ => return Err(Exc::type_error(format!("object of type '{}' has no len()", v.type_name()))),
    })
}

/// `*it` in a list display or comprehension (`list_msg`: "Value after * must be an iterable"), or in a set
/// comprehension or generator (iter()'s message)
pub fn iter_star(v: &V, list_msg: bool) -> R<Vec<V>> {
    iter(v).map_err(|e| {
        if list_msg && e.isinstance(&TYPE_ERROR) {
            Exc::type_error(format!("Value after * must be an iterable, not {}", v.type_name()))
        } else {
            e
        }
    })
}

/// `**m` in a dict display or comprehension: a dict's pairs; any other value is refused (CPython would call
/// `keys()` and `__getitem__` of a mapping-like object)
pub fn mapping_pairs(v: &V) -> R<Vec<(V, V)>> {
    match v {
        V::Dict(d) => Ok(d.lock().values().cloned().collect()),
        V::Obj(_) | V::Inst(_) | V::Native(_) => Err(Exc::type_error(format!("py2axum: ** of a '{}' (not a dict) is not supported", v.type_name()))),
        _ => Err(Exc::type_error(format!("'{}' object is not a mapping", v.type_name()))),
    }
}

/// Unpack `a, b = value` into exactly `n` items.
pub fn unpack(v: &V, n: usize) -> R<Vec<V>> {
    let items = iter(v)?;
    if items.len() != n {
        if items.len() > n {
            // 3.14 counts the items of a list, tuple or dict (an iterator, a str: still unknown)
            if super::python() >= (3, 14) && matches!(v, V::List(_) | V::Tuple(_) | V::Dict(_)) {
                return Err(Exc::value_error(format!("too many values to unpack (expected {n}, got {})", items.len())));
            }
            return Err(Exc::value_error(format!("too many values to unpack (expected {n})")));
        }
        return Err(Exc::value_error(format!("not enough values to unpack (expected {n}, got {})", items.len())));
    }
    Ok(items)
}

/// `a, *rest, z = v`: `before` items, the list of the middle ones, `after` items
pub fn unpack_star(v: &V, before: usize, after: usize) -> R<Vec<V>> {
    let items = iter(v)?;
    if items.len() < before + after {
        return Err(Exc::value_error(format!("not enough values to unpack (expected at least {}, got {})", before + after, items.len())));
    }
    let mut out: Vec<V> = items[..before].to_vec();
    out.push(V::list(items[before..items.len() - after].to_vec()));
    out.extend_from_slice(&items[items.len() - after..]);
    Ok(out)
}

/// A cell: a local variable a nested function rebinds (`nonlocal`), shared by both scopes (one-slot list).
pub fn cell_new() -> V {
    V::List(Arc::new(Mutex::new(vec![V::Unbound])))
}

/// A parameter held in a cell.
pub fn cell_of(v: V) -> V {
    V::List(Arc::new(Mutex::new(vec![v])))
}

/// The value in a cell; `free`: read from the nested function (CPython's NameError for a free variable).
pub fn cell_get(c: &V, name: &str, free: bool) -> R<V> {
    let V::List(l) = c else { unreachable!("py2axum cell") };
    match l.lock().first() {
        Some(V::Unbound) | None if free => Err(Exc::msg(&NAME_ERROR, format!("cannot access free variable '{name}' where it is not associated with a value in enclosing scope"))),
        Some(v) => bound(v, name),
        None => bound(&V::Unbound, name),
    }
}

/// The slot of a cell, assigned in place (`*cell_mut(&c) = v;`: the value is evaluated before the lock).
pub fn cell_mut(c: &V) -> parking_lot::MappedMutexGuard<'_, V> {
    let V::List(l) = c else { unreachable!("py2axum cell") };
    parking_lot::MutexGuard::map(l.lock(), |v| &mut v[0])
}

pub fn bound(v: &V, name: &str) -> R<V> {
    match v {
        V::Unbound => Err(Exc::msg(&UNBOUND_LOCAL_ERROR, format!("cannot access local variable '{name}' where it is not associated with a value"))),
        _ => Ok(v.clone()),
    }
}

pub fn dt_value(d: DateTime) -> V {
    V::DateTime(d)
}

/// `scheme://host` of a request as Starlette builds it from the scope (plain HTTP: the binary terminates no TLS)
pub fn request_origin(r: &super::web::ReqCell) -> String {
    format!("{}://{}", r.scheme, r.header("host").unwrap_or_else(|| "127.0.0.1".into()))
}


/// `timedelta.days`: whole days rounded down (`timedelta(hours=-1).days == -1`).
fn td_days(t: &Duration) -> i64 {
    match t.num_microseconds() {
        Some(us) => us.div_euclid(86_400_000_000),
        None => t.num_days() - i64::from(*t < Duration::days(t.num_days())),
    }
}

/// an aware datetime whose UTC offset changes with its fold (CPython's `pep495_eq_exception`)
fn fold_dependent(d: &DateTime) -> bool {
    match d.tz {
        Some(t) => t.offset_for_wall(&d.wall, 0) != t.offset_for_wall(&d.wall, 1),
        None => false,
    }
}

fn date_out_of_range() -> Exc {
    Exc::msg(&OVERFLOW_ERROR, "date value out of range")
}

/// CPython's dates stop at years 1 and 9999 (chrono's go much further).
fn date_plus_days(d: chrono::NaiveDate, days: i64) -> R<chrono::NaiveDate> {
    use chrono::Datelike;
    let r = Duration::try_days(days).and_then(|n| d.checked_add_signed(n)).ok_or_else(date_out_of_range)?;
    if (1..=9999).contains(&r.year()) { Ok(r) } else { Err(date_out_of_range()) }
}

fn dt_in_range(d: Option<DateTime>) -> R<DateTime> {
    use chrono::Datelike;
    match d {
        Some(d) if (1..=9999).contains(&d.wall.year()) => Ok(d),
        _ => Err(date_out_of_range()),
    }
}
