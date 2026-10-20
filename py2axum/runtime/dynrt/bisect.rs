//! `bisect`: bisect_left/right/bisect and insort_left/right/insort over any sequence, with `lo`, `hi` and `key`,
//! and CPython's messages (the C accelerator's: 3.15 says "lo cannot be negative").
use super::methods::{call_method, call_value};
use super::ops;
use super::Cx;
use super::v::*;

fn int_arg(v: &V) -> R<i64> {
    match v {
        V::Int(i) => Ok(*i),
        V::Bool(b) => Ok(*b as i64),
        o => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    }
}

/// `(a, x, lo=0, hi=None, *, key=None)`: the five parameters
fn params(name: &str, args: &[V], kwargs: &[(String, V)]) -> R<[Option<V>; 5]> {
    let mut out: [Option<V>; 5] = Default::default();
    let names = ["a", "x", "lo", "hi", "key"];
    if args.len() > 4 {
        return Err(Exc::type_error(format!("{name}() takes at most 4 positional arguments ({} given)", args.len())));
    }
    for (i, v) in args.iter().enumerate() {
        out[i] = Some(v.clone());
    }
    for (k, v) in kwargs {
        let i = names.iter().position(|n| n == k).ok_or_else(|| Exc::type_error(format!("{name}() got an unexpected keyword argument '{k}'")))?;
        if i < 4 && out[i].is_some() || (i == 4 && out[4].is_some()) {
            return Err(Exc::type_error(format!("argument for {name}() given by name ('{k}') and position ({})", i + 1)));
        }
        out[i] = Some(v.clone());
    }
    for (i, n) in names.iter().enumerate().take(2) {
        if out[i].is_none() {
            return Err(Exc::type_error(format!("{name}() missing required argument '{n}' (pos {})", i + 1)));
        }
    }
    Ok(out)
}

/// the insertion index; `keyed` x is compared as given (bisect) or already keyed (insort)
async fn search(cx: &Cx, left: bool, a: &V, x: &V, lo: &Option<V>, hi: &Option<V>, key: &Option<V>) -> R<i64> {
    let mut lo = match lo {
        Some(v) => int_arg(v)?,
        None => 0,
    };
    if lo < 0 {
        return Err(Exc::value_error(if super::python() >= (3, 15) { "lo cannot be negative" } else { "lo must be non-negative" }));
    }
    let mut hi = match hi {
        Some(V::None) | None => ops::len(a)? as i64,
        Some(v) => int_arg(v)?,
    };
    let key = key.as_ref().filter(|k| !k.is_none());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mut item = ops::getitem(a, &V::Int(mid))?;
        if let Some(k) = key {
            item = call_value(cx, k, vec![item], vec![]).await?;
        }
        let less = if left { ops::cmp(&item, x)? == std::cmp::Ordering::Less } else { ops::cmp(x, &item)? != std::cmp::Ordering::Less };
        // bisect_right: `x < item` goes left, so `not (x < item)` goes right
        if less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

pub async fn bisect(cx: &Cx, name: &str, left: bool, args: &[V], kwargs: &[(String, V)]) -> R {
    let p = params(name, args, kwargs)?;
    let (a, x) = (p[0].clone().unwrap(), p[1].clone().unwrap());
    Ok(V::Int(search(cx, left, &a, &x, &p[2], &p[3], &p[4]).await?))
}

pub async fn insort(cx: &Cx, name: &str, left: bool, args: &[V], kwargs: &[(String, V)]) -> R {
    let p = params(name, args, kwargs)?;
    let (a, x) = (p[0].clone().unwrap(), p[1].clone().unwrap());
    let probe = match p[4].as_ref().filter(|k| !k.is_none()) {
        Some(k) => call_value(cx, k, vec![x.clone()], vec![]).await?,
        None => x.clone(),
    };
    let i = search(cx, left, &a, &probe, &p[2], &p[3], &p[4]).await?;
    call_method(cx, &a, "insert", vec![V::Int(i), x], vec![]).await?;
    Ok(V::None)
}
