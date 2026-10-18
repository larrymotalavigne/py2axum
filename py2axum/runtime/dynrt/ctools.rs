//! cachetools 7.2: Cache, FIFOCache, LRUCache, TTLCache (and cachetools.func's unbounded TTL cache), the
//! `cached` decorator (key, lock, info) and cachetools.func's lru_cache / fifo_cache / ttl_cache, the
//! `keys.hashkey` / `methodkey` functions. Ported from cachetools/__init__.py, _cached.py, keys.py, func.py:
//! each operation calls the others as the Python methods do (an LRU's `popitem` reads through `pop`, which
//! touches the key; a TTL cache's timer is read once per outer operation, cachetools' `_Timer` nesting).
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;
use parking_lot::Mutex;

use super::methods;
use super::ops;
use super::v::*;
use super::{Cx, KwFn};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Cache,
    Fifo,
    Lru,
    Ttl,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Timer {
    Monotonic,
    Time,
}

pub struct CtCache {
    pub kind: Kind,
    pub name: &'static str,
    maxsize: V,
    maxf: f64,
    /// `getsizeof=len`
    sized: bool,
    ttl: V,
    ttlf: f64,
    timer: Timer,
    /// cachetools.func's unbounded TTL cache: `maxsize` reads None
    unbound: bool,
    st: Mutex<St>,
}

#[derive(Default)]
struct St {
    data: IndexMap<Key, (V, V)>,
    size: IndexMap<Key, i64>,
    currsize: i64,
    /// FIFO / LRU order
    order: IndexMap<Key, ()>,
    /// TTL: `__links` (moved to the end on each read) and the expiry list (insertion order of the last set)
    links: IndexMap<Key, f64>,
    expiry: IndexMap<Key, ()>,
    nesting: u32,
    time: f64,
}

fn key_error(k: &V) -> Exc {
    Exc::new(&KEY_ERROR, vec![k.clone()])
}

fn num(v: &V, what: &str) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Bool(b) => Ok(*b as i64 as f64),
        V::Float(f) => Ok(*f),
        o => Err(Exc::type_error(format!("'<' not supported between instances of '{}' and 'int' ({what})", o.type_name()))),
    }
}

fn clock(t: Timer) -> f64 {
    match t {
        Timer::Monotonic => {
            static T0: OnceLock<std::time::Instant> = OnceLock::new();
            T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() + 1000.0
        }
        Timer::Time => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0),
    }
}

impl CtCache {
    fn timer_call(&self, st: &St) -> f64 {
        if st.nesting == 0 { clock(self.timer) } else { st.time }
    }
    fn enter(&self, st: &mut St) -> f64 {
        if st.nesting == 0 {
            st.time = clock(self.timer);
        }
        st.nesting += 1;
        st.time
    }
    fn exit(&self, st: &mut St) {
        st.nesting -= 1;
    }
    /// `with self.timer as time:` around `f`
    fn with_timer<T>(&self, st: &mut St, f: impl FnOnce(&Self, &mut St, f64) -> R<T>) -> R<T> {
        let t = self.enter(st);
        let r = f(self, st, t);
        self.exit(st);
        r
    }

    fn getsizeof(&self, v: &V) -> R<i64> {
        if self.sized { Ok(ops::len(v)? as i64) } else { Ok(1) }
    }

    // ---- Cache's own methods (`Cache.__getitem__`...)

    fn c_getitem(&self, st: &mut St, k: &V) -> R {
        match st.data.get(&Key::dict_key(k)?) {
            Some((_, v)) => Ok(v.clone()),
            None => Err(key_error(k)),
        }
    }
    fn c_contains(&self, st: &St, k: &V) -> R<bool> {
        Ok(st.data.contains_key(&Key::dict_key(k)?))
    }
    fn c_setitem(&self, st: &mut St, k: &V, v: V) -> R<()> {
        let size = self.getsizeof(&v)?;
        if size < 0 {
            return Err(Exc::value_error("value size must be non-negative"));
        }
        if size as f64 > self.maxf {
            return Err(Exc::value_error("value too large"));
        }
        let key = Key::dict_key(k)?;
        let mut diff;
        if !st.data.contains_key(&key) {
            diff = size;
            while (st.currsize + diff) as f64 > self.maxf {
                self.popitem(st)?;
            }
        } else {
            diff = size - st.size.get(&key).copied().unwrap_or(1);
            while (st.currsize + diff) as f64 > self.maxf {
                self.popitem(st)?;
                if !st.data.contains_key(&key) {
                    diff = size;
                }
            }
        }
        match st.data.get_mut(&key) {
            Some(slot) => slot.1 = v,
            None => {
                st.data.insert(key.clone(), (k.clone(), v));
            }
        }
        if self.sized {
            st.size.insert(key, size);
        }
        st.currsize += diff;
        Ok(())
    }
    fn c_delitem(&self, st: &mut St, k: &V) -> R<()> {
        let key = Key::dict_key(k)?;
        let size = if self.sized {
            st.size.shift_remove(&key).ok_or_else(|| key_error(k))?
        } else {
            1
        };
        if st.data.shift_remove(&key).is_none() {
            return Err(key_error(k));
        }
        st.currsize -= size;
        Ok(())
    }
    fn c_clear(&self, st: &mut St) {
        st.data.clear();
        st.size.clear();
        st.currsize = 0;
    }
    fn c_get(&self, st: &mut St, k: &V, default: V) -> R {
        if self.contains_in(st, k)? { self.getitem_in(st, k) } else { Ok(default) }
    }
    fn c_pop(&self, st: &mut St, k: &V, default: Option<V>) -> R {
        if self.contains_in(st, k)? {
            let v = self.getitem_in(st, k)?;
            self.delitem_in(st, k)?;
            Ok(v)
        } else {
            default.ok_or_else(|| key_error(k))
        }
    }
    fn c_setdefault(&self, st: &mut St, k: &V, default: V) -> R {
        if self.contains_in(st, k)? {
            self.getitem_in(st, k)
        } else {
            self.setitem_in(st, k, default.clone())?;
            Ok(default)
        }
    }
    fn c_repr(&self, st: &St) -> R<String> {
        let d = V::dict_from(st.data.values().cloned().collect())?;
        let maxsize = if self.unbound { V::Float(f64::INFINITY) } else { self.maxsize.clone() };
        Ok(format!("{}({}, maxsize={}, currsize={})", self.name, ops::repr(&d)?, ops::repr(&maxsize)?, st.currsize))
    }

    // ---- the subclasses' methods (Python's dispatch on type(self))

    fn touch(st: &mut St, key: Key) {
        match st.order.get_index_of(&key) {
            Some(i) => {
                let n = st.order.len();
                st.order.move_index(i, n - 1);
            }
            None => {
                st.order.insert(key, ());
            }
        }
    }

    fn getlink(st: &mut St, key: &Key) -> Option<f64> {
        let i = st.links.get_index_of(key)?;
        let n = st.links.len();
        st.links.move_index(i, n - 1);
        st.links.get(key).copied()
    }

    fn getitem_in(&self, st: &mut St, k: &V) -> R {
        match self.kind {
            Kind::Cache | Kind::Fifo => self.c_getitem(st, k),
            Kind::Lru => {
                let v = self.c_getitem(st, k)?;
                if self.c_contains(st, k)? {
                    Self::touch(st, Key::dict_key(k)?);
                }
                Ok(v)
            }
            Kind::Ttl => {
                let key = Key::dict_key(k)?;
                let expired = match Self::getlink(st, &key) {
                    None => false,
                    Some(exp) => !(self.timer_call(st) < exp),
                };
                if expired { Err(key_error(k)) } else { self.c_getitem(st, k) }
            }
        }
    }

    fn contains_in(&self, st: &St, k: &V) -> R<bool> {
        match self.kind {
            Kind::Ttl => match st.links.get(&Key::dict_key(k)?) {
                None => Ok(false),
                Some(exp) => Ok(self.timer_call(st) < *exp),
            },
            _ => self.c_contains(st, k),
        }
    }

    fn setitem_in(&self, st: &mut St, k: &V, v: V) -> R<()> {
        match self.kind {
            Kind::Cache => self.c_setitem(st, k, v),
            Kind::Fifo => {
                self.c_setitem(st, k, v)?;
                let key = Key::dict_key(k)?;
                Self::touch(st, key);
                Ok(())
            }
            Kind::Lru => {
                self.c_setitem(st, k, v)?;
                Self::touch(st, Key::dict_key(k)?);
                Ok(())
            }
            Kind::Ttl => {
                let ttl = self.ttlf;
                let expires = self.with_timer(st, |c, st, time| {
                    let expires = time + ttl;
                    c.expire_in(st, Some(time))?;
                    c.c_setitem(st, k, v)?;
                    Ok(expires)
                })?;
                let key = Key::dict_key(k)?;
                if Self::getlink(st, &key).is_none() {
                    st.links.insert(key.clone(), expires);
                } else {
                    st.expiry.shift_remove(&key);
                    st.links.insert(key.clone(), expires);
                }
                st.expiry.insert(key, ());
                Ok(())
            }
        }
    }

    fn delitem_in(&self, st: &mut St, k: &V) -> R<()> {
        self.c_delitem(st, k)?;
        let key = Key::dict_key(k)?;
        match self.kind {
            Kind::Cache => {}
            Kind::Fifo | Kind::Lru => {
                st.order.shift_remove(&key).ok_or_else(|| key_error(k))?;
            }
            Kind::Ttl => {
                let exp = st.links.shift_remove(&key).ok_or_else(|| key_error(k))?;
                st.expiry.shift_remove(&key);
                if !(self.timer_call(st) < exp) {
                    return Err(key_error(k));
                }
            }
        }
        Ok(())
    }

    fn popitem(&self, st: &mut St) -> R<V> {
        let empty = || Exc::new(&KEY_ERROR, vec![V::str(format!("{} is empty", self.name))]);
        match self.kind {
            Kind::Cache => {
                // MutableMapping.popitem: next(iter(self)), self[key], del self[key]
                let Some((_, (k, _))) = st.data.first() else { return Err(Exc::new(&KEY_ERROR, vec![])) };
                let k = k.clone();
                let v = self.getitem_in(st, &k)?;
                self.delitem_in(st, &k)?;
                Ok(V::tuple(vec![k, v]))
            }
            Kind::Fifo | Kind::Lru => {
                let Some((key, _)) = st.order.first() else { return Err(empty()) };
                let k = st.data.get(key).map(|(k, _)| k.clone()).ok_or_else(empty)?;
                let v = self.c_pop(st, &k, None)?;
                Ok(V::tuple(vec![k, v]))
            }
            Kind::Ttl => self.with_timer(st, |c, st, time| {
                c.expire_in(st, Some(time))?;
                let Some((key, _)) = st.links.first() else { return Err(empty()) };
                let k = st.data.get(key).map(|(k, _)| k.clone()).ok_or_else(empty)?;
                let v = c.with_timer(st, |c, st, _| c.c_pop(st, &k, None))?;
                Ok(V::tuple(vec![k, v]))
            }),
        }
    }

    /// `TTLCache.expire(time)`: the expired (key, value) pairs, in expiry order
    fn expire_in(&self, st: &mut St, time: Option<f64>) -> R<Vec<V>> {
        let time = match time {
            Some(t) => t,
            None => self.timer_call(st),
        };
        let mut out = Vec::new();
        loop {
            let Some((key, _)) = st.expiry.first() else { break };
            let key = key.clone();
            let exp = st.links.get(&key).copied().unwrap_or(f64::INFINITY);
            if time < exp {
                break;
            }
            let k = st.data.get(&key).map(|(k, _)| k.clone()).unwrap_or(V::None);
            let v = self.c_getitem(st, &k)?;
            out.push(V::tuple(vec![k.clone(), v]));
            self.c_delitem(st, &k)?;
            st.links.shift_remove(&key);
            st.expiry.shift_remove(&key);
        }
        Ok(out)
    }

    fn len_in(&self, st: &mut St) -> R<usize> {
        if self.kind == Kind::Ttl {
            return self.with_timer(st, |c, st, time| {
                c.expire_in(st, Some(time))?;
                Ok(st.data.len())
            });
        }
        Ok(st.data.len())
    }

    fn keys_in(&self, st: &mut St) -> Vec<V> {
        match self.kind {
            Kind::Ttl => {
                let mut out = Vec::new();
                let keys: Vec<Key> = st.expiry.keys().cloned().collect();
                for key in keys {
                    let t = self.enter(st);
                    if let Some(exp) = st.links.get(&key) {
                        if t < *exp {
                            if let Some((k, _)) = st.data.get(&key) {
                                out.push(k.clone());
                            }
                        }
                    }
                    self.exit(st);
                }
                out
            }
            _ => st.data.values().map(|(k, _)| k.clone()).collect(),
        }
    }
}

// ------------------------------------------------------------------------------------------------ construction

fn bind(qual: &str, args: &[V], kwargs: &[(String, V)], params: &[(&str, bool)]) -> R<Vec<Option<V>>> {
    let p: Vec<(&str, u8, bool)> = params.iter().map(|(n, d)| (*n, super::P_POS, *d)).collect();
    super::bind_params(qual, 1, args.to_vec(), kwargs.to_vec(), &p)
}

// the transpiler checks the timer, getsizeof and key arguments and passes the supported library functions as
// these markers (libmap: `cachetools` constructors)
fn timer_of(v: &Option<V>) -> R<Timer> {
    match v.as_ref().and_then(|v| v.as_str().map(String::from)).as_deref() {
        None if v.is_none() => Ok(Timer::Monotonic),
        Some("\u{0}timer:monotonic") => Ok(Timer::Monotonic),
        Some("\u{0}timer:time") => Ok(Timer::Time),
        _ => Err(Exc::type_error("py2axum: a cachetools timer other than time.monotonic or time.time is not supported")),
    }
}

fn sized_of(v: &Option<V>) -> R<bool> {
    match v {
        None | Some(V::None) => Ok(false),
        Some(V::Str(s)) if &**s == "\u{0}getsizeof:len" => Ok(true),
        Some(_) => Err(Exc::type_error("py2axum: a cachetools getsizeof other than len is not supported")),
    }
}

fn make(kind: Kind, name: &'static str, maxsize: V, sized: bool, ttl: V, timer: Timer, unbound: bool) -> R {
    let maxf = num(&maxsize, "maxsize")?;
    if maxf < 0.0 {
        return Err(Exc::value_error("maxsize must be non-negative"));
    }
    let ttlf = if kind == Kind::Ttl { num(&ttl, "ttl")? } else { 0.0 };
    Ok(V::native(Native::CtCache(Arc::new(CtCache { kind, name, maxsize, maxf, sized, ttl, ttlf, timer, unbound, st: Mutex::new(St::default()) }))))
}

/// `Cache(maxsize, getsizeof=None)`, `FIFOCache(...)`, `LRUCache(...)`
pub fn cache_new(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let (k, name) = match kind {
        "FIFOCache" => (Kind::Fifo, "FIFOCache"),
        "LRUCache" => (Kind::Lru, "LRUCache"),
        _ => (Kind::Cache, "Cache"),
    };
    let s = bind(&format!("{name}.__init__()"), args, kwargs, &[("maxsize", false), ("getsizeof", true)])?;
    make(k, name, s[0].clone().unwrap(), sized_of(&s[1])?, V::None, Timer::Monotonic, false)
}

/// `TTLCache(maxsize, ttl, timer=time.monotonic, getsizeof=None)`
pub fn ttl_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = bind("TTLCache.__init__()", args, kwargs, &[("maxsize", false), ("ttl", false), ("timer", true), ("getsizeof", true)])?;
    make(Kind::Ttl, "TTLCache", s[0].clone().unwrap(), sized_of(&s[3])?, s[1].clone().unwrap(), timer_of(&s[2])?, false)
}

// ------------------------------------------------------------------------------------------------ the mapping protocol

pub fn getitem(c: &CtCache, k: &V) -> R {
    c.getitem_in(&mut c.st.lock(), k)
}

pub fn setitem(c: &CtCache, k: &V, v: V) -> R<()> {
    c.setitem_in(&mut c.st.lock(), k, v)
}

pub fn delitem(c: &CtCache, k: &V) -> R<()> {
    c.delitem_in(&mut c.st.lock(), k)
}

pub fn contains(c: &CtCache, k: &V) -> R<bool> {
    c.contains_in(&c.st.lock(), k)
}

pub fn len(c: &CtCache) -> R<usize> {
    c.len_in(&mut c.st.lock())
}

pub fn iter(c: &CtCache) -> Vec<V> {
    c.keys_in(&mut c.st.lock())
}

pub fn repr(c: &CtCache) -> R<String> {
    let mut st = c.st.lock();
    if c.kind == Kind::Ttl {
        return c.with_timer(&mut st, |c, st, time| {
            c.expire_in(st, Some(time))?;
            c.c_repr(st)
        });
    }
    c.c_repr(&st)
}

/// `cache == other` (Mapping.__eq__: `dict(self.items()) == dict(other.items())`); None: not a mapping
pub fn eq(c: &CtCache, other: &V) -> R<Option<bool>> {
    let mine = V::dict_from(items(c)?)?;
    let theirs = match other {
        V::Dict(_) => other.clone(),
        V::Native(n) => match &**n {
            Native::CtCache(o) => V::dict_from(items(o)?)?,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    Ok(Some(ops::eq_bool(&mine, &theirs)))
}

/// `self.items()` iterated: each key read through `self[key]`
fn items(c: &CtCache) -> R<Vec<(V, V)>> {
    let keys = iter(c);
    let mut out = Vec::new();
    for k in keys {
        out.push((k.clone(), getitem(c, &k)?));
    }
    Ok(out)
}

pub fn attr(c: &CtCache, name: &str) -> Option<R> {
    let st = || c.st.lock();
    Some(Ok(match name {
        "maxsize" => {
            if c.unbound {
                V::None
            } else {
                c.maxsize.clone()
            }
        }
        "currsize" => {
            let mut s = st();
            if c.kind == Kind::Ttl {
                return Some(c.with_timer(&mut s, |c, st, time| {
                    c.expire_in(st, Some(time))?;
                    Ok(V::Int(st.currsize))
                }));
            }
            V::Int(s.currsize)
        }
        "ttl" if c.kind == Kind::Ttl => c.ttl.clone(),
        _ => return None,
    }))
}

/// the methods of a cache (Cache's, MutableMapping's mixins, TTLCache.expire)
pub fn method(c: &CtCache, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let q = |m: &str| format!("{}.{m}()", c.name);
    let mut st = c.st.lock();
    let timed = c.kind == Kind::Ttl;
    match name {
        "get" => {
            let s = bind(&q("get"), args, kwargs, &[("key", false), ("default", true)])?;
            let (k, d) = (s[0].clone().unwrap(), s[1].clone().unwrap_or(V::None));
            if timed { c.with_timer(&mut st, |c, st, _| c.c_get(st, &k, d)) } else { c.c_get(&mut st, &k, d) }
        }
        "pop" => {
            let s = bind(&q("pop"), args, kwargs, &[("key", false), ("default", true)])?;
            let (k, d) = (s[0].clone().unwrap(), s[1].clone());
            if timed { c.with_timer(&mut st, |c, st, _| c.c_pop(st, &k, d)) } else { c.c_pop(&mut st, &k, d) }
        }
        "setdefault" => {
            let s = bind(&q("setdefault"), args, kwargs, &[("key", false), ("default", true)])?;
            let (k, d) = (s[0].clone().unwrap(), s[1].clone().unwrap_or(V::None));
            if timed { c.with_timer(&mut st, |c, st, _| c.c_setdefault(st, &k, d)) } else { c.c_setdefault(&mut st, &k, d) }
        }
        "popitem" => {
            bind(&q("popitem"), args, kwargs, &[])?;
            c.popitem(&mut st)
        }
        "clear" => {
            bind(&q("clear"), args, kwargs, &[])?;
            c.c_clear(&mut st);
            st.order.clear();
            st.links.clear();
            st.expiry.clear();
            Ok(V::None)
        }
        "expire" if timed => {
            let s = bind(&q("expire"), args, kwargs, &[("time", true)])?;
            let t = match &s[0] {
                None | Some(V::None) => None,
                Some(v) => Some(num(v, "time")?),
            };
            Ok(V::list(c.expire_in(&mut st, t)?))
        }
        "getsizeof" => {
            let s = super::bind_params("getsizeof()", 0, args.to_vec(), kwargs.to_vec(), &[("value", super::P_POS, false)])?;
            Ok(V::Int(c.getsizeof(&s[0].clone().unwrap())?))
        }
        "keys" | "values" | "items" => {
            bind(&q(name), args, kwargs, &[])?;
            drop(st);
            Ok(V::list(match name {
                "keys" => iter(c),
                "values" => items(c)?.into_iter().map(|(_, v)| v).collect(),
                _ => items(c)?.into_iter().map(|(k, v)| V::tuple(vec![k, v])).collect(),
            }))
        }
        "update" => {
            drop(st);
            if args.len() > 1 {
                return Err(Exc::type_error(format!("update expected at most 1 argument, got {}", args.len())));
            }
            if let Some(o) = args.first() {
                let pairs: Vec<(V, V)> = match o {
                    V::Dict(d) => d.lock().values().cloned().collect(),
                    V::Native(n) if matches!(&**n, Native::CtCache(_)) => {
                        let Native::CtCache(x) = &**n else { unreachable!() };
                        items(x)?
                    }
                    other => ops::iter(other)?.into_iter().map(|p| {
                        let t = ops::iter(&p)?;
                        if t.len() != 2 {
                            return Err(Exc::value_error(format!("dictionary update sequence element has length {}; 2 is required", t.len())));
                        }
                        Ok((t[0].clone(), t[1].clone()))
                    }).collect::<R<Vec<_>>>()?,
                };
                for (k, v) in pairs {
                    setitem(c, &k, v)?;
                }
            }
            for (k, v) in kwargs {
                setitem(c, &V::str(k), v.clone())?;
            }
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", c.name))),
    }
}

// ------------------------------------------------------------------------------------------------ keys

/// `cachetools.keys._HashedTuple`, the marker between positional and keyword arguments
fn kwmark() -> V {
    static M: OnceLock<V> = OnceLock::new();
    M.get_or_init(|| V::tuple(vec![V::native(Native::ExtType("cachetools.keys._HashedTuple"))])).clone()
}

/// `keys.hashkey(*args, **kwargs)`
pub fn hashkey(args: &[V], kwargs: &[(String, V)]) -> V {
    let mut out: Vec<V> = args.to_vec();
    if !kwargs.is_empty() {
        out.extend(ops::iter(&kwmark()).unwrap_or_default());
        let mut kw: Vec<&(String, V)> = kwargs.iter().collect();
        kw.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(kw.into_iter().map(|(k, v)| V::tuple(vec![V::str(k), v.clone()])));
    }
    V::tuple(out)
}

/// a hashkey's key, hashable (the key is a _HashedTuple: 3.14 names its type in the unhashable-key message)
fn hashed(k: V) -> R {
    if let Err(e) = Key::dict_key(&k) {
        let m = e.message().replacen("cannot use 'tuple'", "cannot use 'cachetools.keys._HashedTuple'", 1);
        return Err(Exc::type_error(m));
    }
    Ok(k)
}

pub fn hashkey_fn(args: &[V], kwargs: &[(String, V)]) -> R {
    Ok(hashkey(args, kwargs))
}

/// `keys.methodkey(self, *args, **kwargs)`
pub fn methodkey_fn(args: &[V], kwargs: &[(String, V)]) -> R {
    if args.is_empty() {
        return Err(Exc::type_error("methodkey() missing 1 required positional argument: 'self'"));
    }
    Ok(hashkey(&args[1..], kwargs))
}

// ------------------------------------------------------------------------------------------------ cached

pub struct Cfg {
    /// a cached function's cache, or (cachedmethod) the callable giving an instance's
    cache: V,
    key: Option<V>,
    lock: V,
    info: bool,
    /// cachetools.func: (maxsize, typed) for `cache_parameters()`
    params: Option<(V, V)>,
}

/// `cached(cache, key=hashkey, lock=None, condition=None, info=False)`: the decorator
pub fn cached(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("cached()", 0, args.to_vec(), kwargs.to_vec(), &[("cache", super::P_POS, false), ("key", super::P_POS, true),
        ("lock", super::P_POS, true), ("condition", super::P_POS, true), ("info", super::P_POS, true)])?;
    if s[3].as_ref().map(|c| !c.is_none()).unwrap_or(false) {
        return Err(Exc::type_error("py2axum: cachetools.cached(condition=) is not supported"));
    }
    let key = s[1].clone().filter(|k| !is_hashkey(k));
    let info = match &s[4] {
        Some(v) => ops::truthy(v)?,
        None => false,
    };
    let cfg = Cfg { cache: s[0].clone().unwrap(), key, lock: s[2].clone().unwrap_or(V::None), info, params: None };
    Ok(V::native(Native::CtDecorator(Arc::new(cfg))))
}

fn is_hashkey(v: &V) -> bool {
    matches!(v, V::Str(s) if &**s == "\u{0}key:hashkey")
}

/// `keys.hashkey` as a value (`wrapper.cache_key`)
pub fn hashkey_value() -> V {
    static F: OnceLock<V> = OnceLock::new();
    F.get_or_init(|| {
        let f: KwFn = Arc::new(|_cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| Box::pin(async move { Ok(hashkey(&args, &kwargs)) }));
        super::pyfn("cachetools.keys", "hashkey", None, false, f)
    }).clone()
}

/// cachetools.func's `lru_cache(maxsize=128, typed=False)`, `fifo_cache(...)`, `ttl_cache(maxsize=128, ttl=600,
/// timer=time.monotonic, typed=False)`: a decorator, or the wrapper when called on the function itself
pub fn func_cache(which: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let params: &[(&str, bool)] = if which == "ttl_cache" { &[("maxsize", true), ("ttl", true), ("timer", true), ("typed", true)] } else { &[("maxsize", true), ("typed", true)] };
    let s = super::bind_params(&format!("{which}()"), 0, args.to_vec(), kwargs.to_vec(), &params.iter().map(|(n, d)| (*n, super::P_POS, *d)).collect::<Vec<_>>())?;
    let typed = s[params.len() - 1].clone().unwrap_or(V::Bool(false));
    if ops::truthy(&typed)? {
        return Err(Exc::type_error("py2axum: cachetools.func caches with typed=True are not supported"));
    }
    let maxsize = s[0].clone().unwrap_or(V::Int(128));
    let callable_max = ops::truthy(&methods::b_callable(std::slice::from_ref(&maxsize))?)?;
    let size = if callable_max { V::Int(128) } else { maxsize.clone() };
    let cache = if which == "ttl_cache" {
        let ttl = s[1].clone().unwrap_or(V::Int(600));
        let timer = timer_of(&s[2])?;
        if size.is_none() {
            make(Kind::Ttl, "_UnboundTTLCache", V::Float(f64::INFINITY), false, ttl, timer, true)?
        } else {
            make(Kind::Ttl, "TTLCache", size.clone(), false, ttl, timer, false)?
        }
    } else if size.is_none() {
        V::empty_dict()
    } else {
        make(if which == "lru_cache" { Kind::Lru } else { Kind::Fifo }, if which == "lru_cache" { "LRUCache" } else { "FIFOCache" },
             size.clone(), false, V::None, Timer::Monotonic, false)?
    };
    // `_cache(cache, maxsize, typed)`: cached(cache, key, condition=Condition(), info=True); the condition only
    // serializes concurrent computations of one key
    let cfg = Arc::new(Cfg { cache, key: None, lock: V::None, info: true, params: Some((size, typed)) });
    if callable_max {
        return Ok(wrap(maxsize, cfg));
    }
    Ok(V::native(Native::CtDecorator(cfg)))
}

/// the decorator applied: cachetools' `_wrapper` (functools.update_wrapper attributes, `__wrapped__`)
pub fn decorate(cfg: &Arc<Cfg>, args: &[V]) -> R {
    match args {
        [f] => Ok(wrap(f.clone(), cfg.clone())),
        _ => Err(Exc::type_error(format!("decorator() takes 1 positional argument but {} were given", args.len()))),
    }
}

struct Counts {
    hits: i64,
    misses: i64,
}

fn wrap(f: V, cfg: Arc<Cfg>) -> V {
    let counts = Arc::new(Mutex::new(Counts { hits: 0, misses: 0 }));
    let (target, c2, k2) = (f.clone(), cfg.clone(), counts.clone());
    let call: KwFn = Arc::new(move |cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| {
        let (f, cfg, counts) = (target.clone(), c2.clone(), k2.clone());
        Box::pin(async move { run(cx, f, cfg, counts, args, kwargs).await })
    });
    let mut attrs: Vec<(String, V)> = match &f {
        V::Native(n) => match &**n {
            Native::PyFn(p) => p.attrs.lock().iter().filter(|(k, _)| k != "__wrapped__").cloned().collect(),
            _ => vec![],
        },
        _ => vec![],
    };
    let cache = cfg.cache.clone();
    let clear_counts = counts.clone();
    let info_now = cfg.info;
    let clear: KwFn = Arc::new(move |_cx: &Cx, _a: Vec<V>, _k: Vec<(String, V)>| {
        let (cache, counts) = (cache.clone(), clear_counts.clone());
        Box::pin(async move {
            clear_cache(&cache)?;
            if info_now {
                let mut c = counts.lock();
                c.hits = 0;
                c.misses = 0;
            }
            Ok(V::None)
        })
    });
    let info_v = if cfg.info {
        let (cache, counts) = (cfg.cache.clone(), counts.clone());
        let f: KwFn = Arc::new(move |_cx: &Cx, _a: Vec<V>, _k: Vec<(String, V)>| {
            let (cache, counts) = (cache.clone(), counts.clone());
            Box::pin(async move { cache_info(&cache, &counts) })
        });
        super::pyfn("cachetools._cached", "_unlocked_info.<locals>.cache_info", None, false, f)
    } else {
        V::None
    };
    attrs.push(("cache_clear".into(), super::pyfn("cachetools._cached", "_unlocked.<locals>.<lambda>", None, false, clear)));
    attrs.push(("cache_info".into(), info_v));
    if let Some((maxsize, typed)) = &cfg.params {
        let d = V::dict_from(vec![(V::str("maxsize"), maxsize.clone()), (V::str("typed"), typed.clone())]).unwrap_or(V::None);
        let p: KwFn = Arc::new(move |_cx: &Cx, _a: Vec<V>, _k: Vec<(String, V)>| {
            let d = d.clone();
            Box::pin(async move { Ok(V::dict_from(match &d { V::Dict(m) => m.lock().values().cloned().collect(), _ => vec![] })?) })
        });
        attrs.push(("cache_parameters".into(), super::pyfn("cachetools.func", "_cache.<locals>.decorator.<locals>.<lambda>", None, false, p)));
    }
    attrs.push(("cache".into(), cfg.cache.clone()));
    attrs.push(("cache_key".into(), cfg.key.clone().unwrap_or_else(hashkey_value)));
    attrs.push(("cache_lock".into(), cfg.lock.clone()));
    attrs.push(("cache_condition".into(), V::None));
    attrs.push(("__wrapped__".into(), f));
    V::native(Native::PyFn(PyFn { call, is_async: false, attrs: Mutex::new(attrs) }))
}

fn clear_cache(cache: &V) -> R<()> {
    match cache {
        V::Native(n) => match &**n {
            Native::CtCache(c) => {
                method(c, "clear", &[], &[])?;
                Ok(())
            }
            _ => Err(Exc::type_error("py2axum: this cache does not support clear()")),
        },
        V::Dict(d) => {
            d.lock().clear();
            Ok(())
        }
        V::None => Ok(()),
        o => Err(Exc::attr_error(format!("'{}' object has no attribute 'clear'", o.type_name()))),
    }
}

fn cache_info(cache: &V, counts: &Mutex<Counts>) -> R {
    let (h, m) = {
        let c = counts.lock();
        (c.hits, c.misses)
    };
    let (maxsize, currsize) = match cache {
        V::Native(n) if matches!(&**n, Native::CtCache(_)) => {
            let Native::CtCache(c) = &**n else { unreachable!() };
            (attr(c, "maxsize").unwrap()?, attr(c, "currsize").unwrap()?)
        }
        V::Dict(d) => (V::None, V::Int(d.lock().len() as i64)),
        V::None => (V::Int(0), V::Int(0)),
        _ => (V::Int(0), V::Int(0)),
    };
    Ok(V::native(Native::Record("CacheInfo", Arc::new(vec![("hits", V::Int(h)), ("misses", V::Int(m)), ("maxsize", maxsize), ("currsize", currsize)]))))
}

fn get_from(cache: &V, k: &V) -> R<Option<V>> {
    let r = match cache {
        V::Native(n) => match &**n {
            Native::CtCache(c) => getitem(c, k),
            _ => return Err(Exc::type_error("py2axum: a cachetools cache other than a cachetools cache or a dict is not supported")),
        },
        V::Dict(_) => ops::getitem(cache, k),
        _ => return Err(Exc::type_error("py2axum: a cachetools cache other than a cachetools cache or a dict is not supported")),
    };
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.isinstance(&KEY_ERROR) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `cache[k] = v` (locked: `cache.setdefault(k, v)`), a ValueError (too large) ignored
fn store(cache: &V, k: &V, v: V, locked: bool) -> R<V> {
    let r = match cache {
        V::Native(n) => match &**n {
            Native::CtCache(c) if locked => method(c, "setdefault", &[k.clone(), v.clone()], &[]),
            Native::CtCache(c) => setitem(c, k, v.clone()).map(|_| v.clone()),
            _ => unreachable!(),
        },
        V::Dict(d) if locked => {
            let key = Key::dict_key(k)?;
            let mut m = d.lock();
            match m.get(&key) {
                Some((_, x)) => Ok(x.clone()),
                None => {
                    m.insert(key, (k.clone(), v.clone()));
                    Ok(v.clone())
                }
            }
        }
        _ => ops::setitem(cache, k, v.clone()).map(|_| v.clone()),
    };
    match r {
        Ok(x) => Ok(x),
        Err(e) if e.isinstance(&VALUE_ERROR) => Ok(v),
        Err(e) => Err(e),
    }
}

async fn run(cx: &Cx, f: V, cfg: Arc<Cfg>, counts: Arc<Mutex<Counts>>, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if cfg.cache.is_none() {
        if cfg.info {
            counts.lock().misses += 1;
        }
        return methods::call_value(cx, &f, args, kwargs).await;
    }
    let k = match &cfg.key {
        None => hashed(hashkey(&args, &kwargs))?,
        Some(key) => methods::call_value(cx, key, args.clone(), kwargs.clone()).await?,
    };
    if let Some(v) = get_from(&cfg.cache, &k)? {
        if cfg.info {
            counts.lock().hits += 1;
        }
        return Ok(v);
    }
    if cfg.info {
        counts.lock().misses += 1;
    }
    let v = methods::call_value(cx, &f, args, kwargs).await?;
    let locked = !cfg.lock.is_none() || cfg.params.is_some();
    let out = store(&cfg.cache, &k, v.clone(), locked && cfg.params.is_none())?;
    Ok(if locked && cfg.params.is_none() { out } else { v })
}

/// `cachedmethod(cache, key=methodkey, lock=None, condition=None, info=False)`: its configuration, applied by the
/// method's generated wrapper (`method_call`)
pub fn cachedmethod(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = super::bind_params("cachedmethod()", 0, args.to_vec(), kwargs.to_vec(), &[("cache", super::P_POS, false),
        ("key", super::P_POS, true), ("lock", super::P_POS, true), ("condition", super::P_POS, true), ("info", super::P_POS, true)])?;
    if s[3].as_ref().map(|c| !c.is_none()).unwrap_or(false) {
        return Err(Exc::type_error("py2axum: cachetools.cachedmethod(condition=) is not supported"));
    }
    if let Some(i) = &s[4] {
        if ops::truthy(i)? {
            return Err(Exc::type_error("py2axum: cachetools.cachedmethod(info=True) is not supported"));
        }
    }
    let key = s[1].clone().filter(|k| !matches!(k, V::Str(x) if &**x == "\u{0}key:methodkey"));
    Ok(V::native(Native::CtDecorator(Arc::new(Cfg { cache: s[0].clone().unwrap(), key, lock: s[2].clone().unwrap_or(V::None), info: false, params: None }))))
}

/// a @cachedmethod's call: `c = cache(self)`, `k = key(self, *args, **kwargs)`, `c[k]`, else the method and
/// `c[k] = v` (with a lock: `with lock(self): c.setdefault(k, v)`)
pub async fn method_call(cx: &Cx, deco: &V, slf: V, packed: Vec<V>, raw: super::pyd::MethodFn) -> R {
    let V::Native(n) = deco else { return Err(Exc::type_error("py2axum: not a cachedmethod")) };
    let Native::CtDecorator(cfg) = &**n else { return Err(Exc::type_error("py2axum: not a cachedmethod")) };
    let (args, kwargs) = super::unpack(packed.clone());
    let c = methods::call_value(cx, &cfg.cache, vec![slf.clone()], vec![]).await?;
    let k = match &cfg.key {
        None => hashed(hashkey(&args, &kwargs))?,
        Some(key) => {
            let mut a = vec![slf.clone()];
            a.extend(args.iter().cloned());
            methods::call_value(cx, key, a, kwargs.clone()).await?
        }
    };
    let locked = !cfg.lock.is_none();
    if locked {
        methods::call_value(cx, &cfg.lock, vec![slf.clone()], vec![]).await?;
    }
    if let Some(v) = get_from(&c, &k)? {
        return Ok(v);
    }
    let v = raw(cx, slf, packed).await?;
    let out = store(&c, &k, v.clone(), locked)?;
    Ok(if locked { out } else { v })
}

/// the attributes `cached` sets on its wrapper (the transpiler's catalogue of runtime names reads them here)
pub fn is_wrapper_attr(name: &str) -> bool {
    matches!(name, "cache_clear" | "cache_info" | "cache_parameters" | "cache_key" | "cache_lock" | "cache_condition" | "cache")
}
