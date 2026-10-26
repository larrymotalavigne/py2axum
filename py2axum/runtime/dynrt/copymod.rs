//! `copy.copy` / `copy.deepcopy` (CPython's `copy` module) over the runtime's values.
//!
//! Immutable values are returned as is (`_copy_immutable`, `_deepcopy_atomic`); a tuple whose items deep-copy to
//! the same objects is returned itself. Lists, dicts (a defaultdict or Counter stays one), sets and deques are
//! rebuilt. A Pydantic model follows `BaseModel.__copy__`/`__deepcopy__` (`__dict__`, `__pydantic_extra__`,
//! `__pydantic_fields_set__`); a dataclass or plain class instance `copyreg.__reduce_ex__(4)` (`cls.__new__` then
//! its `__dict__`, shallow or deep). `deepcopy` keeps a memo by identity: shared and cyclic references are copied
//! once. A class defining `__copy__`, `__deepcopy__`, `__reduce__`... is refused at translation
//! (`Project.copies_objects`). A mapped (ORM) object, an exception and the runtime's handles (requests,
//! clients, sessions...) are refused at run time.
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use super::ops;
use super::pyd;
use super::v::*;

/// values `copy` returns as they are: immutable (CPython copies some through `__reduce_ex__`, an equal object)
fn immutable(v: &V) -> bool {
    match v {
        V::None | V::Bool(_) | V::Int(_) | V::Float(_) | V::Str(_) | V::Bytes(_) | V::Tuple(_) | V::Frozen(_) | V::DateTime(_)
        | V::Date(_) | V::Time(_) | V::Delta(_) | V::Tz(_) | V::Class(_) | V::Enum(..) | V::Decimal(_) | V::Col(..) => true,
        V::Native(n) => matches!(
            &**n,
            Native::Uuid(_) | Native::Path(_) | Native::IpAddr(_) | Native::IpNet(..) | Native::Complex(..) | Native::Fraction(_)
                | Native::Type(_) | Native::ExtType(_) | Native::Func(_) | Native::Builtin(..) | Native::PyFn(_) | Native::Secret(_)
                | Native::HttpUrl(_) | Native::PydUrl(..) | Native::YarlUrl(_) | Native::RelDelta(_) | Native::Pattern(_)
                | Native::Bound(..) | Native::Property(_) | Native::NotImplemented
        ),
        _ => false,
    }
}

fn refused(what: &str, v: &V) -> Exc {
    match v {
        V::Obj(o) => Exc::type_error(format!("py2axum: copy.{what} of the mapped object {} is not supported", o.desc.name)),
        _ => Exc::type_error(format!("py2axum: copy.{what} of a '{}' object is not supported", v.type_name())),
    }
}

fn new_dict(src: &Arc<Mutex<indexmap::IndexMap<Key, (V, V)>>>, items: indexmap::IndexMap<Key, (V, V)>) -> R {
    let d = V::Dict(Arc::new(Mutex::new(items)));
    match ops::default_factory(src) {
        Some(f) => ops::defaultdict(f, d),
        None => Ok(d),
    }
}

fn inst(i: &pyd::Inst, vals: Vec<V>, extra: indexmap::IndexMap<String, V>) -> V {
    V::Inst(Arc::new(pyd::Inst {
        desc: i.desc,
        vals: pyd::Slots::new(vals),
        set: Mutex::new(i.set.lock().clone()),
        extra: pyd::Slots::new(extra),
        raw: i.raw,
    }))
}

/// `copy.copy(x)`
pub fn copy(v: &V) -> R {
    if immutable(v) {
        return Ok(v.clone());
    }
    match v {
        V::List(l) => Ok(V::list(l.lock().clone())),
        V::Dict(d) => {
            let items = d.lock().clone();
            new_dict(d, items)
        }
        V::Set(s) => Ok(V::Set(Arc::new(Mutex::new(s.lock().clone())))),
        V::Inst(i) => Ok(inst(i, i.vals.read().clone(), i.extra.read().clone())),
        V::Native(n) => match &**n {
            Native::Deque(d) => Ok(V::native(Native::Deque(Arc::new(super::deque::Deque {
                items: Mutex::new(d.items.lock().clone()),
                maxlen: d.maxlen,
            })))),
            _ => Err(refused("copy()", v)),
        },
        _ => Err(refused("copy()", v)),
    }
}

/// `copy.deepcopy(x)`
pub fn deepcopy(v: &V) -> R {
    Deep { memo: HashMap::new(), keep: Vec::new() }.copy(v)
}

struct Deep {
    memo: HashMap<usize, V>,
    /// the originals stay alive while their addresses key the memo (CPython's `_keep_alive`)
    keep: Vec<V>,
}

fn addr<T: ?Sized>(a: &Arc<T>) -> usize {
    Arc::as_ptr(a) as *const () as usize
}

impl Deep {
    fn seen(&self, k: usize) -> Option<V> {
        self.memo.get(&k).cloned()
    }

    fn remember(&mut self, k: usize, orig: &V, copy: &V) {
        self.memo.insert(k, copy.clone());
        self.keep.push(orig.clone());
    }

    fn copy(&mut self, v: &V) -> R {
        super::stack_guard()?;
        match v {
            V::Tuple(t) => {
                let k = addr(t);
                if let Some(c) = self.seen(k) {
                    return Ok(c);
                }
                let items = t.iter().map(|x| self.copy(x)).collect::<R<Vec<_>>>()?;
                // a cycle through the tuple copied it already (`_deepcopy_tuple`)
                if let Some(c) = self.seen(k) {
                    return Ok(c);
                }
                let same = items.iter().zip(t.iter()).all(|(a, b)| ops::is(a, b));
                let out = if same { v.clone() } else { V::tuple(items) };
                self.remember(k, v, &out);
                Ok(out)
            }
            V::Frozen(s) => {
                let k = addr(s);
                if let Some(c) = self.seen(k) {
                    return Ok(c);
                }
                let items: Vec<V> = s.lock().values().cloned().collect();
                let items = items.iter().map(|x| self.copy(x)).collect::<R<Vec<_>>>()?;
                let out = super::methods::b_frozenset(&[V::list(items)])?;
                self.remember(k, v, &out);
                Ok(out)
            }
            _ if immutable(v) => Ok(v.clone()),
            V::List(l) => {
                let k = addr(l);
                if let Some(c) = self.seen(k) {
                    return Ok(c);
                }
                let out = Arc::new(Mutex::new(Vec::new()));
                self.remember(k, v, &V::List(out.clone()));
                let items = l.lock().clone();
                for x in &items {
                    let c = self.copy(x)?;
                    out.lock().push(c);
                }
                Ok(V::List(out))
            }
            V::Dict(d) => {
                let k = addr(d);
                if let Some(c) = self.seen(k) {
                    return Ok(c);
                }
                let out = new_dict(d, indexmap::IndexMap::new())?;
                self.remember(k, v, &out);
                let items: Vec<(V, V)> = d.lock().values().cloned().collect();
                for (key, x) in items {
                    let (key, x) = (self.copy(&key)?, self.copy(&x)?);
                    ops::setitem(&out, &key, x)?;
                }
                Ok(out)
            }
            V::Set(s) => {
                let k = addr(s);
                if let Some(c) = self.seen(k) {
                    return Ok(c);
                }
                let items: Vec<V> = s.lock().values().cloned().collect();
                let items = items.iter().map(|x| self.copy(x)).collect::<R<Vec<_>>>()?;
                let out = super::methods::b_set(&[V::list(items)])?;
                self.remember(k, v, &out);
                Ok(out)
            }
            V::Inst(i) => {
                let k = addr(i);
                if let Some(c) = self.seen(k) {
                    return Ok(c);
                }
                let fresh = Arc::new(pyd::Inst {
                    desc: i.desc,
                    vals: pyd::Slots::new(Vec::new()),
                    set: Mutex::new(i.set.lock().clone()),
                    extra: pyd::Slots::new(indexmap::IndexMap::new()),
                    raw: i.raw,
                });
                let out = V::Inst(fresh.clone());
                self.remember(k, v, &out);
                let vals = i.vals.read().clone();
                let vals = vals.iter().map(|x| self.copy(x)).collect::<R<Vec<_>>>()?;
                let extra = i.extra.read().clone();
                let mut ex = indexmap::IndexMap::new();
                for (name, x) in extra {
                    ex.insert(name, self.copy(&x)?);
                }
                *fresh.vals.lock() = vals;
                *fresh.extra.lock() = ex;
                Ok(out)
            }
            V::Native(n) => match &**n {
                Native::Deque(d) => {
                    let k = addr(n);
                    if let Some(c) = self.seen(k) {
                        return Ok(c);
                    }
                    let items = d.items.lock().clone();
                    let items = items.iter().map(|x| self.copy(x)).collect::<R<_>>()?;
                    let out = V::native(Native::Deque(Arc::new(super::deque::Deque { items: Mutex::new(items), maxlen: d.maxlen })));
                    self.remember(k, v, &out);
                    Ok(out)
                }
                _ => Err(refused("deepcopy()", v)),
            },
            _ => Err(refused("deepcopy()", v)),
        }
    }
}
