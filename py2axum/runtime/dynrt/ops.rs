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

pub fn truthy(v: &V) -> R<bool> {
    if let Some(t) = row_tuple(v) {
        return truthy(&t);
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
        V::Dict(d) => !d.lock().is_empty(),
        V::Set(s) => !s.lock().is_empty(),
        V::Delta(d) => !d.is_zero(),
        V::Enum(e, i) => match e.kind {
            EnumKind::Plain => true,
            _ => truthy(&e.value(*i))?,
        },
        V::Sql(_) | V::Col(..) => {
            return Err(Exc::type_error("Boolean value of this clause is not defined"))
        }
        _ => true,
    })
}

pub fn not(v: &V) -> R {
    Ok(V::Bool(!truthy(v)?))
}

// ---------------------------------------------------------------- numbers

fn num(v: &V) -> Option<f64> {
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
    Exc::type_error(format!(
        "unsupported operand type(s) for {}: '{}' and '{}'",
        op,
        a.type_name(),
        b.type_name()
    ))
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
        "+" => Ok(d::v(x.add(&y))),
        "-" => Ok(d::v(x.sub(&y))),
        "*" => Ok(d::v(x.mul(&y))),
        "/" => x.div(&y).map(d::v),
        "//" => x.divmod(&y).map(|(q, _)| d::v(q)),
        "%" => x.divmod(&y).map(|(_, r)| d::v(r)),
        _ => Err(binop_err(op, a, b)),
    })
}

pub fn add(a: &V, b: &V) -> R {
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
        (V::DateTime(d), V::Delta(t)) | (V::Delta(t), V::DateTime(d)) => V::DateTime(d.add(*t)),
        (V::Date(d), V::Delta(t)) | (V::Delta(t), V::Date(d)) => V::Date(*d + Duration::days(t.num_days())),
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
    if let Some(r) = dec_op(a, b, "-") {
        return r;
    }
    if is_sql(a) || is_sql(b) {
        return orm::sql_binop(a, "-", b);
    }
    Ok(match (a, b) {
        (V::DateTime(d), V::Delta(t)) => V::DateTime(d.add(-*t)),
        (V::DateTime(x), V::DateTime(y)) => {
            if x.tz.is_some() != y.tz.is_some() {
                return Err(Exc::type_error("can't subtract offset-naive and offset-aware datetimes"));
            }
            V::Delta(x.utc() - y.utc())
        }
        (V::Date(x), V::Date(y)) => V::Delta(*x - *y),
        (V::Date(d), V::Delta(t)) => V::Date(*d - Duration::days(t.num_days())),
        (V::Delta(x), V::Delta(y)) => V::Delta(*x - *y),
        (V::Set(x), V::Set(y)) => {
            let y = y.lock();
            let m: IndexMap<Key, V> = x.lock().iter().filter(|(k, _)| !y.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            V::Set(Arc::new(Mutex::new(m)))
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

pub fn mul(a: &V, b: &V) -> R {
    if is_sql(a) || is_sql(b) {
        return orm::sql_binop(a, "*", b);
    }
    if let Some(r) = dec_op(a, b, "*") {
        return r;
    }
    Ok(match (a, b) {
        (V::Str(s), n) | (n, V::Str(s)) if int_of(n).is_some() => V::str(s.repeat(int_of(n).unwrap().max(0) as usize)),
        (V::Bytes(b), n) | (n, V::Bytes(b)) if int_of(n).is_some() => V::Bytes(Arc::from(b.repeat(int_of(n).unwrap().max(0) as usize))),
        (V::List(l), n) | (n, V::List(l)) if int_of(n).is_some() => {
            let src = l.lock().clone();
            let k = int_of(n).unwrap().max(0) as usize;
            V::list(src.iter().cloned().cycle().take(src.len() * k).collect())
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

pub fn truediv(a: &V, b: &V) -> R {
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
                return Err(zero_div("division by zero"));
            }
            V::Delta(Duration::microseconds((dt::micros(x) as f64 / d).round() as i64))
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => {
                if y == 0.0 {
                    return Err(zero_div("division by zero"));
                }
                V::Float(x / y)
            }
            _ => return Err(binop_err("/", a, b)),
        },
    })
}

pub fn floordiv(a: &V, b: &V) -> R {
    if let Some(r) = dec_op(a, b, "//") {
        return r;
    }
    Ok(match (a, b) {
        (V::Delta(x), V::Delta(y)) => {
            if y.is_zero() {
                return Err(zero_div("integer division or modulo by zero"));
            }
            V::Int(dt::micros(x).div_euclid(dt::micros(y)))
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => {
                if y == 0 {
                    return Err(zero_div("integer division or modulo by zero"));
                }
                V::Int(x.div_euclid(y) - if (x.rem_euclid(y) != 0) && (y < 0) { 1 } else { 0 })
            }
            _ => match (num(a), num(b)) {
                (Some(x), Some(y)) => {
                    if y == 0.0 {
                        return Err(zero_div("float floor division by zero"));
                    }
                    V::Float((x / y).floor())
                }
                _ => return Err(binop_err("//", a, b)),
            },
        },
    })
}

pub fn modulo(a: &V, b: &V) -> R {
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
                return Err(zero_div("integer division or modulo by zero"));
            }
            let r = x % y;
            Ok(V::Int(if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r }))
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => {
                if y == 0.0 {
                    return Err(zero_div("float modulo"));
                }
                let r = x % y;
                Ok(V::Float(if r != 0.0 && ((r < 0.0) != (y < 0.0)) { r + y } else { r }))
            }
            _ => Err(binop_err("%", a, b)),
        },
    }
}

pub fn neg(a: &V) -> R {
    Ok(match a {
        V::Decimal(d) => super::decimal::v(d.negate()),
        V::Bool(b) => V::Int(-(*b as i64)),
        V::Int(i) => V::Int(i.checked_neg().ok_or_else(overflow)?),
        V::Float(f) => V::Float(-f),
        V::Delta(d) => V::Delta(-*d),
        _ => return Err(Exc::type_error(format!("bad operand type for unary -: '{}'", a.type_name()))),
    })
}

pub fn pos(a: &V) -> R {
    Ok(match a {
        V::Bool(b) => V::Int(*b as i64),
        V::Int(_) | V::Float(_) | V::Delta(_) => a.clone(),
        V::Decimal(d) => super::decimal::v((**d).clone().fix()),
        _ => return Err(Exc::type_error(format!("bad operand type for unary +: '{}'", a.type_name()))),
    })
}

pub fn bitor(a: &V, b: &V) -> R {
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
        (V::Set(x), V::Set(y)) => {
            let mut m = x.lock().clone();
            for (k, v) in y.lock().iter() {
                m.entry(k.clone()).or_insert_with(|| v.clone());
            }
            V::Set(Arc::new(Mutex::new(m)))
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => V::Int(x | y),
            _ => return Err(binop_err("|", a, b)),
        },
    })
}

fn unsupported(op: &str, a: &V, b: &V) -> Exc {
    Exc::type_error(format!("unsupported operand type(s) for {op}: '{}' and '{}'", a.type_name(), b.type_name()))
}

/// `a ** b`: int ** non-negative int is an int (beyond 64 bits: OverflowError, py2axum has no big ints),
/// a negative exponent or a float gives a float
pub fn pow(a: &V, b: &V) -> R {
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
        return Err(Exc::msg(&ZERO_DIVISION_ERROR, "zero to a negative power"));
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

pub fn bitxor(a: &V, b: &V) -> R {
    match (a, b) {
        (V::Bool(x), V::Bool(y)) => Ok(V::Bool(x ^ y)),
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => Ok(V::Int(x ^ y)),
            _ => Err(unsupported("^", a, b)),
        },
    }
}

pub fn lshift(a: &V, b: &V) -> R {
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
    match (int_of(a), int_of(b)) {
        (Some(_), Some(y)) if y < 0 => Err(Exc::value_error("negative shift count")),
        (Some(x), Some(y)) => Ok(V::Int(if y >= 64 { if x < 0 { -1 } else { 0 } } else { x >> y })),
        _ => Err(unsupported(">>", a, b)),
    }
}

pub fn bitand(a: &V, b: &V) -> R {
    if is_sql(a) || is_sql(b) {
        return orm::sql_bool(a, "AND", b);
    }
    Ok(match (a, b) {
        (V::Bool(x), V::Bool(y)) => V::Bool(*x & *y),
        (V::Set(x), V::Set(y)) => {
            let y = y.lock();
            let m: IndexMap<Key, V> = x.lock().iter().filter(|(k, _)| y.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            V::Set(Arc::new(Mutex::new(m)))
        }
        _ => match (int_of(a), int_of(b)) {
            (Some(x), Some(y)) => V::Int(x & y),
            _ => return Err(binop_err("&", a, b)),
        },
    })
}

pub fn invert(a: &V) -> R {
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
    if let Some(t) = row_tuple(a) {
        return eq_bool(&t, b);
    }
    if let Some(t) = row_tuple(b) {
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
        (V::Native(x), V::Native(y)) if matches!((&**x, &**y), (Native::Path(_), Native::Path(_)) | (Native::Uuid(_), Native::Uuid(_))) => {
            match (&**x, &**y) {
                (Native::Path(p), Native::Path(q)) => p == q,
                (Native::Uuid(p), Native::Uuid(q)) => p == q,
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
        (V::Set(x), V::Set(y)) => {
            let (x, y) = (x.lock(), y.lock());
            x.len() == y.len() && x.keys().all(|k| y.contains_key(k))
        }
        (V::DateTime(x), V::DateTime(y)) => {
            if x.tz.is_some() != y.tz.is_some() {
                false
            } else {
                x.utc() == y.utc()
            }
        }
        (V::Date(x), V::Date(y)) => x == y,
        (V::Time(x), V::Time(y)) => x == y,
        (V::Delta(x), V::Delta(y)) => x == y,
        (V::Tz(x), V::Tz(y)) => x == y,
        (V::Class(x), V::Class(y)) => std::ptr::eq(*x, *y),
        (V::Obj(x), V::Obj(y)) => Arc::ptr_eq(x, y),
        (V::Inst(x), V::Inst(y)) => Arc::ptr_eq(x, y) || x.equals(y),
        (V::Native(x), V::Native(y)) => Arc::ptr_eq(x, y),
        (V::Exc(x), V::Exc(y)) => Arc::ptr_eq(&x.0, &y.0),
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

pub fn eq(a: &V, b: &V) -> R {
    if is_sql(a) || is_sql(b) {
        return orm::sql_cmp(a, "=", b);
    }
    Ok(V::Bool(eq_r(a, b)?))
}

pub fn ne(a: &V, b: &V) -> R {
    if is_sql(a) || is_sql(b) {
        return orm::sql_cmp(a, "!=", b);
    }
    Ok(V::Bool(!eq_r(a, b)?))
}

pub fn cmp(a: &V, b: &V) -> R<std::cmp::Ordering> {
    if let Some(t) = row_tuple(a) {
        return cmp(&t, b);
    }
    if let Some(t) = row_tuple(b) {
        return cmp(a, &t);
    }
    use std::cmp::Ordering::*;
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
        (V::Tuple(x), V::Tuple(y)) => seq_cmp(x, y)?,
        (V::List(x), V::List(y)) => seq_cmp(&x.lock().clone(), &y.lock().clone())?,
        (V::DateTime(x), V::DateTime(y)) => {
            if x.tz.is_some() != y.tz.is_some() {
                return Err(Exc::type_error("can't compare offset-naive and offset-aware datetimes"));
            }
            x.utc().cmp(&y.utc())
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
            if is_sql(a) || is_sql(b) {
                return orm::sql_cmp(a, $sql, b);
            }
            Ok(V::Bool(matches!(cmp(a, b)?, $pat)))
        }
    };
}
cmp_op!(lt, "<", std::cmp::Ordering::Less);
cmp_op!(le, "<=", std::cmp::Ordering::Less | std::cmp::Ordering::Equal);
cmp_op!(gt, ">", std::cmp::Ordering::Greater);
cmp_op!(ge, ">=", std::cmp::Ordering::Greater | std::cmp::Ordering::Equal);

pub fn is(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::None, V::None) => true,
        (V::Bool(x), V::Bool(y)) => x == y,
        (V::Int(x), V::Int(y)) => x == y && (-5..=256).contains(x),
        (V::Str(x), V::Str(y)) => Arc::ptr_eq(x, y),
        (V::List(x), V::List(y)) => Arc::ptr_eq(x, y),
        (V::Dict(x), V::Dict(y)) => Arc::ptr_eq(x, y),
        (V::Obj(x), V::Obj(y)) => Arc::ptr_eq(x, y),
        (V::Inst(x), V::Inst(y)) => Arc::ptr_eq(x, y),
        (V::Class(x), V::Class(y)) => std::ptr::eq(*x, *y),
        (V::Native(x), V::Native(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

pub fn contains(container: &V, item: &V) -> R<bool> {
    if let Some(t) = row_tuple(container) {
        return contains(&t, item);
    }
    if let V::Enum(e, i) = container {
        return contains(&e.value(*i), item);
    }
    Ok(match container {
        V::Class(c) => match c.kind {
            ClassKind::Enum(e) => match item {
                V::Enum(d, _) => std::ptr::eq(*d, e),
                other => e.by_value(other).is_some(),
            },
            _ => return Err(Exc::type_error(format!("argument of type 'type' is not iterable"))),
        },
        V::Str(s) => match &unenum(item) {
            V::Str(i) => s.contains(&**i),
            _ => return Err(Exc::type_error("'in <string>' requires string as left operand")),
        },
        V::List(l) => l.lock().clone().iter().any(|x| eq_bool(x, item)),
        V::Tuple(t) => t.iter().any(|x| eq_bool(x, item)),
        V::Dict(d) => d.lock().contains_key(&Key::of(item)?),
        V::Set(s) => s.lock().contains_key(&Key::of(item)?),
        V::Native(n) => match &**n {
            Native::RespHeaders(r) => super::resp::headers_contains(&r.headers, item)?,
            Native::CellHeaders(c) => super::resp::headers_contains(&c.headers, item)?,
            Native::Headers(r) => match item {
                V::Str(k) => r.header(k).is_some(),
                _ => false,
            },
            _ => return Err(Exc::type_error(format!("argument of type '{}' is not iterable", container.type_name()))),
        },
        _ => return Err(Exc::type_error(format!("argument of type '{}' is not iterable", container.type_name()))),
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
            c => out.push(c),
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
        _ => return None,
    };
    methods.iter().find(|(n, prop, _)| *n == name && !prop).map(|(_, _, f)| *f)
}

/// Calls a project dunder method from synchronous code: such methods are `def`s, they never suspend
/// (a pending future means it awaited something, which CPython could not do either).
pub fn call_dunder(f: super::pyd::MethodFn, recv: &V, args: Vec<V>, name: &str) -> R {
    use std::future::Future;
    let cx = super::root_cx();
    let mut fut = f(&cx, recv.clone(), args);
    let waker = std::task::Waker::noop();
    match fut.as_mut().poll(&mut std::task::Context::from_waker(waker)) {
        std::task::Poll::Ready(r) => r,
        std::task::Poll::Pending => Err(Exc::runtime(format!("py2axum: {name}() of {} suspended (awaited I/O)", recv.type_name()))),
    }
}

fn dunder_text(v: &V, names: &[&str]) -> Option<R<String>> {
    for name in names {
        if let Some(f) = dunder(v, name) {
            return Some(call_dunder(f, v, vec![], name).and_then(|r| match r {
                V::Str(s) => Ok(s.to_string()),
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
    if let Some(t) = row_tuple(v) {
        return str_(&t);
    }
    if let Some(r) = dunder_text(v, &["__str__", "__repr__"]) {
        return r;
    }
    if let V::Decimal(d) = v {
        return Ok(d.to_string());
    }
    if let V::Native(n) = v {
        if let Native::PydUrl(_, u) = &**n {
            return Ok(u.as_str().to_string());
        }
        if let Native::YarlUrl(u) = &**n {
            return Ok(u.clone()); // yarl keeps the text it was given (no added `/`)
        }
    }
    Ok(match v {
        V::Str(s) => s.to_string(),
        V::DateTime(d) => d.isoformat(' ', "auto"),
        V::Date(d) => dt::date_iso(d),
        V::Time(t) => dt::time_iso(t),
        V::Delta(d) => dt::delta_str(d),
        V::Exc(e) => e.message(),
        V::Tz(t) => t.name(),
        V::Native(n) if matches!(&**n, Native::Path(_) | Native::Uuid(_)) => match &**n {
            Native::Path(p) => p.clone(),
            Native::Uuid(u) => super::pathio::uuid_str(*u),
            _ => unreachable!(),
        },
        V::Sql(_) | V::Col(..) => orm::sql_text(v)?,
        V::Enum(e, i) => match e.kind {
            EnumKind::StrEnum | EnumKind::IntEnum => str_(&e.value(*i))?,
            _ => format!("{}.{}", e.name, e.member_name(*i)),
        },
        _ => repr(v)?,
    })
}

pub fn repr(v: &V) -> R<String> {
    if let Some(t) = row_tuple(v) {
        return repr(&t);
    }
    if let Some(r) = dunder_text(v, &["__repr__"]) {
        return r;
    }
    if let V::Decimal(d) = v {
        return Ok(format!("Decimal('{d}')"));
    }
    if let V::Native(n) = v {
        if let Native::PydUrl(name, u) = &**n {
            return Ok(format!("{name}('{}')", u.as_str()));
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
            format!("{{{}}}", parts.join(", "))
        }
        V::Set(s) => {
            let items = s.lock().clone();
            if items.is_empty() {
                "set()".into()
            } else {
                format!("{{{}}}", items.values().map(repr).collect::<R<Vec<_>>>()?.join(", "))
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
        V::Class(c) => format!("<class '{}'>", c.qualname),
        V::Exc(e) => format!("{}({})", e.0.class.name, e.args().iter().map(repr).collect::<R<Vec<_>>>()?.join(", ")),
        V::Inst(i) => i.repr()?,
        V::Native(n) if matches!(&**n, Native::Path(_) | Native::Uuid(_) | Native::Upload(_)) => match &**n {
            Native::Path(p) => format!("PosixPath({})", str_repr(p)),
            Native::Uuid(u) => format!("UUID({})", str_repr(&super::pathio::uuid_str(*u))),
            Native::Upload(u) => super::files::repr_upload(u)?,
            _ => unreachable!(),
        },
        V::Obj(o) => format!("<{} object>", o.desc.class_qualname),
        V::Enum(e, i) => format!("<{}.{}: {}>", e.name, e.member_name(*i), repr(&e.value(*i))?),
        other => format!("<{}>", other.type_name()),
    })
}

// ---------------------------------------------------------------- formatting

/// `format(value, spec)` for the spec subset used in f-strings: [[fill]align][sign][0][width][.prec][type]
pub fn format_spec(v: &V, spec: &str) -> R<String> {
    if let V::Decimal(d) = v {
        return super::decimal::format(d, spec);
    }
    if spec.is_empty() {
        return str_(v);
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
    if i < chars.len() && chars[i] == '0' {
        fill = '0';
        if align.is_none() {
            align = Some('=');
        }
        i += 1;
    }
    let mut width = 0usize;
    while i < chars.len() && chars[i].is_ascii_digit() {
        width = width * 10 + chars[i].to_digit(10).unwrap() as usize;
        i += 1;
    }
    let mut thousands = false;
    if i < chars.len() && (chars[i] == ',' || chars[i] == '_') {
        thousands = true;
        i += 1;
    }
    let mut prec: Option<usize> = None;
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let mut p = 0;
        while i < chars.len() && chars[i].is_ascii_digit() {
            p = p * 10 + chars[i].to_digit(10).unwrap() as usize;
            i += 1;
        }
        prec = Some(p);
    }
    let ty = chars.get(i).copied();
    let numeric = matches!(v, V::Int(_) | V::Float(_) | V::Bool(_));
    let mut body = match (ty, v) {
        (Some('d'), _) if int_of(v).is_some() => int_of(v).unwrap().abs().to_string(),
        (Some('f') | Some('F'), _) if num(v).is_some() => format!("{:.*}", prec.unwrap_or(6), num(v).unwrap().abs()),
        (Some('%'), _) if num(v).is_some() => format!("{:.*}%", prec.unwrap_or(6), num(v).unwrap().abs() * 100.0),
        (Some('x'), _) if int_of(v).is_some() => format!("{:x}", int_of(v).unwrap().abs()),
        (Some('s') | None, V::Str(s)) => match prec {
            Some(p) => s.chars().take(p).collect(),
            None => s.to_string(),
        },
        (None, _) if numeric => {
            let s = str_(v)?;
            s.trim_start_matches('-').to_string()
        }
        (None, V::DateTime(_) | V::Date(_)) => str_(v)?,
        _ => return Err(Exc::value_error(format!("Unknown format code for object of type '{}'", v.type_name()))),
    };
    if thousands {
        let (int_part, rest) = match body.find('.') {
            Some(p) => (body[..p].to_string(), body[p..].to_string()),
            None => (body.clone(), String::new()),
        };
        let mut grouped = String::new();
        for (k, c) in int_part.chars().enumerate() {
            if k > 0 && (int_part.len() - k) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(c);
        }
        body = grouped + &rest;
    }
    let negative = num(v).map(|x| x < 0.0).unwrap_or(false);
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
    let items: Vec<V> = match args {
        V::Tuple(t) => t.to_vec(),
        other => vec![other.clone()],
    };
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
        i += 1;
        if i < chars.len() && chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        let mut spec = String::new();
        while i < chars.len() && "-+ #0123456789.".contains(chars[i]) {
            spec.push(chars[i]);
            i += 1;
        }
        let conv = chars.get(i).copied().unwrap_or('s');
        i += 1;
        let arg = it.next().ok_or_else(|| Exc::type_error("not enough arguments for format string"))?;
        let left = spec.starts_with('-');
        let zero = spec.trim_start_matches('-').starts_with('0');
        let (w, p) = match spec.trim_start_matches(['-', '+', ' ', '#', '0']).split_once('.') {
            Some((w, p)) => (w.parse::<usize>().unwrap_or(0), Some(p.parse::<usize>().unwrap_or(0))),
            None => (spec.trim_start_matches(['-', '+', ' ', '#', '0']).parse::<usize>().unwrap_or(0), None),
        };
        let body = match conv {
            's' => str_(&arg)?,
            'r' => repr(&arg)?,
            'd' | 'i' => match (int_of(&arg), num(&arg)) {
                (Some(x), _) => x.to_string(),
                (None, Some(f)) => (f.trunc() as i64).to_string(),
                _ => return Err(Exc::type_error(format!("%d format: a real number is required, not {}", arg.type_name()))),
            },
            'f' => format!("{:.*}", p.unwrap_or(6), num(&arg).ok_or_else(|| Exc::type_error("must be real number"))?),
            'x' => format!("{:x}", int_of(&arg).unwrap_or(0)),
            _ => str_(&arg)?,
        };
        let body = if conv == 's' { match p { Some(p) => body.chars().take(p).collect(), None => body } } else { body };
        let n = body.chars().count();
        if n < w {
            let pad = w - n;
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
    if let Some(t) = row_tuple(v) {
        return getslice(&t, start, stop, step);
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

pub fn getitem(v: &V, k: &V) -> R {
    if let Some(t) = row_tuple(v) {
        return getitem(&t, k);
    }
    if let V::Native(n) = v {
        match &**n {
            Native::Mime(m) => return super::mail::mime_getitem(m, k),
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
            let key = Key::of(k)?;
            match d.lock().get(&key) {
                Some((_, v)) => v.clone(),
                None => return Err(Exc::new(&KEY_ERROR, vec![k.clone()])),
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
        _ => return Err(Exc::type_error(format!("'{}' object is not subscriptable", v.type_name()))),
    })
}

pub fn setitem(v: &V, k: &V, val: V) -> R<()> {
    if let V::Native(n) = v {
        match &**n {
            Native::Mime(m) => return super::mail::mime_setitem(m, k, &val),
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
            d.lock().insert(Key::of(k)?, (k.clone(), val));
        }
        _ => return Err(Exc::type_error(format!("'{}' object does not support item assignment", v.type_name()))),
    }
    Ok(())
}

pub fn delitem(v: &V, k: &V) -> R<()> {
    if let V::Native(n) = v {
        if let Native::RespHeaders(r) = &**n {
            return super::resp::headers_delitem(&r.headers, k);
        }
        if let Native::CellHeaders(c) = &**n {
            return super::resp::headers_delitem(&c.headers, k);
        }
    }
    match v {
        V::Dict(d) => {
            if d.lock().shift_remove(&Key::of(k)?).is_none() {
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
    if let Some(t) = row_tuple(v) {
        return iter(&t);
    }
    Ok(match v {
        V::List(l) => l.lock().clone(),
        V::Tuple(t) => t.to_vec(),
        V::Dict(d) => d.lock().values().map(|(k, _)| k.clone()).collect(),
        V::Set(s) => s.lock().values().cloned().collect(),
        V::Str(s) => s.chars().map(|c| V::str(c.to_string())).collect(),
        V::Result(r) => r.lock().take_rows()?,
        V::Native(n) if matches!(&**n, Native::CsvRows(..) | Native::StringIO(_) | Native::Iter(_)) => match &**n {
            Native::CsvRows(_, rows) => iter(rows)?,
            Native::Iter(q) => q.lock().drain(..).collect(),
            _ => super::stdlib::iter_lines(v).unwrap_or_default(),
        },
        V::Inst(i) => i.iter_fields(),
        V::Native(n) if matches!(&**n, Native::UrlParts(_) | Native::Record(..)) => match &**n {
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
    if let Some(t) = row_tuple(v) {
        return len(&t);
    }
    Ok(match v {
        V::List(l) => l.lock().len(),
        V::Tuple(t) => t.len(),
        V::Dict(d) => d.lock().len(),
        V::Set(s) => s.lock().len(),
        V::Str(s) => s.chars().count(),
        V::Bytes(b) => b.len(),
        V::Class(c) => match c.kind {
            ClassKind::Enum(e) => e.members.len(),
            _ => return Err(Exc::type_error("object of type 'type' has no len()")),
        },
        V::Enum(e, i) if e.kind != EnumKind::Plain => len(&e.value(*i))?,
        _ => return Err(Exc::type_error(format!("object of type '{}' has no len()", v.type_name()))),
    })
}

/// Unpack `a, b = value` into exactly `n` items.
pub fn unpack(v: &V, n: usize) -> R<Vec<V>> {
    let items = iter(v)?;
    if items.len() != n {
        if items.len() > n {
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

pub fn bound(v: &V, name: &str) -> R<V> {
    match v {
        V::Unbound => Err(Exc::msg(&UNBOUND_LOCAL_ERROR, format!("cannot access local variable '{name}' where it is not associated with a value"))),
        _ => Ok(v.clone()),
    }
}

pub fn dt_value(d: DateTime) -> V {
    V::DateTime(d)
}
