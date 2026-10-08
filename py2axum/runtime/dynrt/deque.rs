//! `collections.deque`: a double-ended queue, optionally bounded (`maxlen`: appending on one end drops
//! from the other), with CPython's messages.
use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

use super::ops;
use super::v::*;

pub struct Deque {
    pub items: Mutex<VecDeque<V>>,
    pub maxlen: Option<usize>,
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// `deque(iterable=(), maxlen=None)`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    if args.len() > 2 {
        return Err(Exc::type_error(format!("deque() takes at most 2 arguments ({} given)", args.len())));
    }
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "iterable" && k != "maxlen") {
        return Err(Exc::type_error(format!("deque() got an unexpected keyword argument '{k}'")));
    }
    let maxlen = match args.get(1).or_else(|| kw(kwargs, "maxlen")) {
        None | Some(V::None) => None,
        Some(V::Int(n)) if *n < 0 => return Err(Exc::value_error("maxlen must be non-negative")),
        Some(V::Int(n)) => Some(*n as usize),
        Some(o) => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    };
    let d = Deque { items: Mutex::new(VecDeque::new()), maxlen };
    if let Some(it) = args.first().or_else(|| kw(kwargs, "iterable")) {
        for x in ops::iter(it)? {
            push(&d, x, false);
        }
    }
    Ok(V::native(Native::Deque(Arc::new(d))))
}

fn push(d: &Deque, x: V, left: bool) {
    let mut q = d.items.lock();
    if d.maxlen == Some(0) {
        return;
    }
    if d.maxlen.is_some_and(|m| q.len() >= m) {
        if left {
            q.pop_back();
        } else {
            q.pop_front();
        }
    }
    if left {
        q.push_front(x);
    } else {
        q.push_back(x);
    }
}

fn index(d: &Deque, k: &V) -> R<usize> {
    let n = d.items.lock().len() as i64;
    let i = match k {
        V::Int(i) => *i,
        V::Bool(b) => *b as i64,
        o => return Err(Exc::type_error(format!("sequence index must be integer, not '{}'", o.type_name()))),
    };
    let j = if i < 0 { i + n } else { i };
    if j < 0 || j >= n {
        return Err(Exc::msg(&INDEX_ERROR, "deque index out of range"));
    }
    Ok(j as usize)
}

pub fn getitem(d: &Deque, k: &V) -> R {
    let i = index(d, k)?;
    Ok(d.items.lock()[i].clone())
}

pub fn setitem(d: &Deque, k: &V, v: V) -> R<()> {
    let i = index(d, k)?;
    d.items.lock()[i] = v;
    Ok(())
}

pub fn items(d: &Deque) -> Vec<V> {
    d.items.lock().iter().cloned().collect()
}

pub fn repr(d: &Deque) -> R<String> {
    let parts = items(d).iter().map(ops::repr).collect::<R<Vec<_>>>()?;
    Ok(match d.maxlen {
        Some(m) => format!("deque([{}], maxlen={m})", parts.join(", ")),
        None => format!("deque([{}])", parts.join(", ")),
    })
}

fn not_found(what: &str) -> Exc {
    let (major, minor) = super::python();
    if (major, minor) >= (3, 14) {
        Exc::value_error(format!("deque.{what}(x): x not in deque"))
    } else {
        Exc::value_error("x not in deque")
    }
}

pub fn method(d: &Arc<Deque>, name: &str, args: &[V]) -> R {
    let one = || args.first().cloned().ok_or_else(|| Exc::type_error(format!("deque.{name}() takes exactly one argument (0 given)")));
    Ok(match name {
        "append" => {
            push(d, one()?, false);
            V::None
        }
        "appendleft" => {
            push(d, one()?, true);
            V::None
        }
        "extend" | "extendleft" => {
            for x in ops::iter(&one()?)? {
                push(d, x, name == "extendleft");
            }
            V::None
        }
        "pop" | "popleft" => {
            let mut q = d.items.lock();
            let x = if name == "pop" { q.pop_back() } else { q.pop_front() };
            x.ok_or_else(|| Exc::msg(&INDEX_ERROR, "pop from an empty deque"))?
        }
        "clear" => {
            d.items.lock().clear();
            V::None
        }
        "count" => {
            let x = one()?;
            V::Int(items(d).iter().filter(|y| ops::eq_bool(y, &x)).count() as i64)
        }
        "remove" => {
            let x = one()?;
            let pos = items(d).iter().position(|y| ops::eq_bool(y, &x)).ok_or_else(|| not_found("remove"))?;
            d.items.lock().remove(pos);
            V::None
        }
        "index" => {
            let x = one()?;
            V::Int(items(d).iter().position(|y| ops::eq_bool(y, &x)).ok_or_else(|| not_found("index"))? as i64)
        }
        "rotate" => {
            let n = match args.first() {
                None => 1,
                Some(V::Int(n)) => *n,
                Some(o) => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
            };
            let mut q = d.items.lock();
            let len = q.len() as i64;
            if len > 0 {
                let k = n.rem_euclid(len) as usize;
                q.rotate_right(k);
            }
            V::None
        }
        "reverse" => {
            d.items.lock().make_contiguous().reverse();
            V::None
        }
        "copy" => V::native(Native::Deque(Arc::new(Deque { items: Mutex::new(d.items.lock().clone()), maxlen: d.maxlen }))),
        _ => return Err(Exc::attr_error(format!("'collections.deque' object has no attribute '{name}'"))),
    })
}
