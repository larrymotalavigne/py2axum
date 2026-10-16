//! The protocol methods of project classes (`__iter__`, `__len__`, `__getitem__`, `__add__`...) called by the
//! operators and builtins, with CPython's rules: the left operand's method, then the reflected one of the right
//! operand (first when its class is a subclass of the left's), `NotImplemented` passing the turn, and the errors
//! CPython raises for a wrong result.
use super::ops::{call_dunder, dunder};
use super::v::*;

/// `NotImplemented`
pub fn not_implemented() -> V {
    static N: std::sync::LazyLock<V> = std::sync::LazyLock::new(|| V::native(Native::NotImplemented));
    N.clone()
}

/// A type's fully qualified name as CPython 3.15's messages write it (`%T`: the module, unless `builtins`)
fn qualified(v: &V) -> String {
    match class_of(v) {
        Some(c) => c.qualname.to_string(),
        None => v.type_name().to_string(),
    }
}

fn is_not_impl(v: &V) -> bool {
    matches!(v, V::Native(n) if matches!(&**n, Native::NotImplemented))
}

fn is_inst(v: &V) -> bool {
    matches!(v, V::Inst(_) | V::Obj(_))
}

fn class_of(v: &V) -> Option<&'static Class> {
    match v {
        V::Inst(i) => Some(i.desc.class),
        V::Obj(o) => Some(o.desc.class),
        _ => None,
    }
}

fn call(v: &V, name: &str, args: Vec<V>) -> Option<R> {
    dunder(v, name).map(|f| call_dunder(f, v, args, name))
}

/// `a OP b` when an operand is an instance with the operator's method: None when neither takes part
pub fn binop(a: &V, op: &str, b: &V) -> Option<R> {
    if !is_inst(a) && !is_inst(b) {
        return None;
    }
    let (name, rname) = names(op)?;
    let (lf, rf) = (dunder(a, name), dunder(b, rname));
    if lf.is_none() && rf.is_none() {
        return None;
    }
    // the right operand's reflected method first when its class is a strict subclass of the left one's
    let same = matches!((class_of(a), class_of(b)), (Some(x), Some(y)) if std::ptr::eq(x, y));
    let sub_first = matches!((class_of(a), class_of(b)), (Some(x), Some(y)) if !std::ptr::eq(x, y) && y.is_subclass(x));
    let mut order: Vec<(&V, &str, &V)> = Vec::new();
    if sub_first && rf.is_some() {
        order.push((b, rname, a));
    }
    if lf.is_some() {
        order.push((a, name, b));
    }
    if rf.is_some() && !sub_first && !same {
        order.push((b, rname, a));
    }
    for (recv, m, other) in order {
        match call(recv, m, vec![other.clone()])? {
            Ok(r) if is_not_impl(&r) => continue,
            r => return Some(r),
        }
    }
    Some(Err(Exc::type_error(seq_message(a, op, b)
        .unwrap_or_else(|| format!("unsupported operand type(s) for {op}: '{}' and '{}'", a.type_name(), b.type_name())))))
}

/// CPython's sequence slots answer last: `"x" + obj`, `[1] * obj`, `obj * "x"` fail with their own message
fn seq_message(a: &V, op: &str, b: &V) -> Option<String> {
    let seq = |v: &V| matches!(v, V::Str(_) | V::List(_) | V::Tuple(_) | V::Bytes(_));
    match op {
        "+" if matches!(a, V::Bytes(_)) => Some(format!("can't concat {} to bytes", b.type_name())),
        "+" if seq(a) => Some(format!("can only concatenate {} (not \"{}\") to {}", a.type_name(), b.type_name(), a.type_name())),
        "*" if seq(a) => Some(format!("can't multiply sequence by non-int of type '{}'", b.type_name())),
        "*" if seq(b) => Some(format!("can't multiply sequence by non-int of type '{}'", a.type_name())),
        _ => None,
    }
}

/// `a OP= b`: the in-place method when the left operand has it (its result, unless NotImplemented), else `a OP b`
pub fn inplace(a: &V, op: &str, b: &V) -> Option<R> {
    if !is_inst(a) && !is_inst(b) {
        return None;
    }
    let (name, _) = names(op)?;
    let iname = format!("__i{}", &name[2..]);
    if let Some(r) = call(a, &iname, vec![b.clone()]) {
        match r {
            Ok(v) if is_not_impl(&v) => {}
            r => return Some(r),
        }
    }
    binop(a, op, b)
}

fn names(op: &str) -> Option<(&'static str, &'static str)> {
    Some(match op {
        "+" => ("__add__", "__radd__"),
        "-" => ("__sub__", "__rsub__"),
        "*" => ("__mul__", "__rmul__"),
        "/" => ("__truediv__", "__rtruediv__"),
        "//" => ("__floordiv__", "__rfloordiv__"),
        "%" => ("__mod__", "__rmod__"),
        "**" => ("__pow__", "__rpow__"),
        "&" => ("__and__", "__rand__"),
        "|" => ("__or__", "__ror__"),
        "^" => ("__xor__", "__rxor__"),
        "<<" => ("__lshift__", "__rlshift__"),
        ">>" => ("__rshift__", "__rrshift__"),
        "@" => ("__matmul__", "__rmatmul__"),
        _ => return None,
    })
}

/// `-a`, `+a`, `~a`, `abs(a)` on an instance with the method
pub fn unary(a: &V, name: &str) -> Option<R> {
    if !is_inst(a) {
        return None;
    }
    call(a, name, vec![])
}

/// `bool(a)`: `__bool__` (a bool, else TypeError), else `__len__`; None when the instance has neither
pub fn truthy(a: &V) -> Option<R<bool>> {
    if !is_inst(a) {
        return None;
    }
    if let Some(r) = call(a, "__bool__", vec![]) {
        return Some(r.and_then(|v| match v {
            V::Bool(b) => Ok(b),
            other => Err(Exc::type_error(if super::python() >= (3, 15) {
                format!("{}.__bool__() must return a bool, not {}", qualified(a), qualified(&other))
            } else {
                format!("__bool__ should return bool, returned {}", other.type_name())
            })),
        }));
    }
    dunder(a, "__len__")?;
    Some(len(a)?.map(|n| n > 0))
}

/// `len(a)`: `__len__`, an int (a bool is one) not below zero
pub fn len(a: &V) -> Option<R<usize>> {
    let r = call(a, "__len__", vec![])?;
    Some(r.and_then(|v| match v {
        V::Int(n) if n >= 0 => Ok(n as usize),
        V::Bool(b) => Ok(b as usize),
        V::Int(_) => Err(Exc::value_error("__len__() should return >= 0")),
        other => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", other.type_name()))),
    }))
}

/// `item in a`: `__contains__` (its result's truth), else iterating `__iter__` (or `__getitem__` from 0) until
/// an element is the item or equals it
pub fn contains(a: &V, item: &V) -> Option<R<bool>> {
    if !is_inst(a) {
        return None;
    }
    if let Some(r) = call(a, "__contains__", vec![item.clone()]) {
        return Some(r.and_then(|v| super::ops::truthy(&v)));
    }
    if dunder(a, "__iter__").is_none() && dunder(a, "__getitem__").is_none() {
        return None;
    }
    Some((|| {
        let mut it = LoopIter::start_sync(a)?;
        while let Some(x) = it.next()? {
            if super::ops::is(&x, item) || super::ops::eq_r(&x, item)? {
                return Ok(true);
            }
        }
        Ok(false)
    })())
}

/// `a[k]`, `a[k] = v`, `del a[k]` through the instance's methods
pub fn getitem(a: &V, k: &V) -> Option<R> {
    if !is_inst(a) {
        return None;
    }
    call(a, "__getitem__", vec![k.clone()])
}

pub fn setitem(a: &V, k: &V, v: &V) -> Option<R<()>> {
    if !is_inst(a) {
        return None;
    }
    call(a, "__setitem__", vec![k.clone(), v.clone()]).map(|r| r.map(|_| ()))
}

pub fn delitem(a: &V, k: &V) -> Option<R<()>> {
    if !is_inst(a) {
        return None;
    }
    call(a, "__delitem__", vec![k.clone()]).map(|r| r.map(|_| ()))
}

/// The elements of an instance that defines `__iter__` (or `__getitem__`), all of them (list(), sorted(), a
/// comprehension's source...): None for any other value
pub fn iter_all(a: &V) -> Option<R<Vec<V>>> {
    if !is_inst(a) || (dunder(a, "__iter__").is_none() && dunder(a, "__getitem__").is_none()) {
        return None;
    }
    Some((|| {
        let mut it = LoopIter::start_sync(a)?;
        let mut out = Vec::new();
        while let Some(x) = it.next()? {
            out.push(x);
        }
        Ok(out)
    })())
}

/// `next(a[, default])` on an instance with `__next__`
pub fn next(a: &V, default: Option<&V>) -> Option<R> {
    if !is_inst(a) {
        return None;
    }
    let r = call(a, "__next__", vec![])?;
    Some(match (r, default) {
        (Err(e), Some(d)) if e.isinstance(&STOP_ITERATION) => Ok(d.clone()),
        (r, _) => r,
    })
}

/// The iteration of a `for` loop: the elements of a builtin value, or an instance's iterator advanced one
/// element at a time (`__next__` runs between the loop's bodies, as in CPython)
pub enum LoopIter {
    Eager(std::vec::IntoIter<V>),
    Next(V),
    /// the old sequence protocol: `__getitem__(0)`, `__getitem__(1)`... until IndexError
    Index(V, i64),
}

impl LoopIter {
    pub async fn start(v: &V) -> R<LoopIter> {
        if is_inst(v) && (dunder(v, "__iter__").is_some() || dunder(v, "__getitem__").is_some()) {
            return Self::start_sync(v);
        }
        Ok(LoopIter::Eager(super::orm::iter(v).await?.into_iter()))
    }

    pub fn start_sync(v: &V) -> R<LoopIter> {
        if let Some(r) = call(v, "__iter__", vec![]) {
            let it = r?;
            if dunder(&it, "__next__").is_some() {
                return Ok(LoopIter::Next(it));
            }
            if is_inst(&it) {
                return Err(Exc::type_error(if super::python() >= (3, 15) {
                    format!("{}.__iter__() must return an iterator, not {}", qualified(v), qualified(&it))
                } else {
                    format!("iter() returned non-iterator of type '{}'", it.type_name())
                }));
            }
            // `return iter(self._items)`: the runtime's iter() of a builtin is a snapshot of its elements
            return Ok(LoopIter::Eager(super::ops::iter(&it)?.into_iter()));
        }
        if dunder(v, "__getitem__").is_some() {
            return Ok(LoopIter::Index(v.clone(), 0));
        }
        Ok(LoopIter::Eager(super::ops::iter(v)?.into_iter()))
    }

    pub fn next(&mut self) -> R<Option<V>> {
        match self {
            LoopIter::Eager(it) => Ok(it.next()),
            LoopIter::Next(it) => match call(it, "__next__", vec![]).unwrap_or_else(|| Err(Exc::type_error("'NoneType' object is not an iterator"))) {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.isinstance(&STOP_ITERATION) => Ok(None),
                Err(e) => Err(e),
            },
            LoopIter::Index(v, i) => match call(v, "__getitem__", vec![V::Int(*i)]).expect("checked") {
                Ok(x) => {
                    *i += 1;
                    Ok(Some(x))
                }
                Err(e) if e.isinstance(&INDEX_ERROR) || e.isinstance(&STOP_ITERATION) => Ok(None),
                Err(e) => Err(e),
            },
        }
    }
}

/// `a OP= b` (other than `+=`, `dynrt::iadd`): the in-place method of an instance, else `a OP b`
pub fn aug(a: &V, op: &str, b: &V, f: fn(&V, &V) -> R) -> R {
    inplace(a, op, b).unwrap_or_else(|| f(a, b))
}

/// `obj[a:b]` on an instance with the item method: CPython passes a `slice` object, which the runtime does not
/// have (refused when it runs, never read as something else)
pub fn slice_refused(v: &V, name: &str) -> Option<Exc> {
    if is_inst(v) && dunder(v, name).is_some() {
        return Some(Exc::type_error(format!("py2axum: slicing '{}' ({name} with a slice object) is not supported", v.type_name())));
    }
    None
}

/// `next(iter(obj))`: the first element of an instance's iteration only (an endless iterator ends there too)
pub fn first(a: &V) -> Option<R<Option<V>>> {
    if !is_inst(a) || (dunder(a, "__iter__").is_none() && dunder(a, "__getitem__").is_none()) {
        return None;
    }
    Some(LoopIter::start_sync(a).and_then(|mut it| it.next()))
}

/// `object()`
pub fn object_new(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() || !kwargs.is_empty() {
        return Err(Exc::type_error("object() takes no arguments"));
    }
    Ok(V::native(Native::Object))
}
