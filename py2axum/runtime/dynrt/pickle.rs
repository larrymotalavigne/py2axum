//! `pickle.dumps` / `pickle.loads` in CPython's format, so that a value written by the binary is read by
//! the project's Python processes and the other way round (a Redis cache shared by both).
//!
//! Written: protocol 4/5 (the project's Python default), the opcodes CPython's pickler emits — memo
//! (MEMOIZE/BINGET), STACK_GLOBAL, REDUCE for datetime/date/time/timedelta/timezone/Decimal/enums,
//! NEWOBJ + BUILD for UUID, project classes (`__dict__` or `(None, slots)` state), dataclasses and Pydantic
//! models (`__dict__`, `__pydantic_fields_set__`...). A mapped (ORM) object is refused.
//! Read: every opcode of protocols 2 to 5 except out-of-band buffers and persistent ids; globals limited to
//! the types above and the project's classes (anything else raises like a missing module would).
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use chrono::{Datelike, Timelike};
use super::dt::{DateTime, Tz};
use super::ops;
use super::pyd;
use super::v::*;

static CLASSES: OnceLock<Vec<&'static Class>> = OnceLock::new();

/// The project's classes, found by `module.qualname` when unpickling (called once at startup).
pub fn register(classes: &[&'static Class]) {
    let _ = CLASSES.set(classes.to_vec());
}

fn class_by_name(module: &str, name: &str) -> Option<&'static Class> {
    let q = format!("{module}.{name}");
    CLASSES.get().and_then(|cs| cs.iter().find(|c| c.qualname == q).copied())
}

fn split_qual(q: &str) -> (&str, &str) {
    q.rsplit_once('.').unwrap_or(("__main__", q))
}

fn perr(m: impl Into<String>) -> Exc {
    Exc::msg(&PICKLING_ERROR, m.into())
}

fn uerr(m: impl Into<String>) -> Exc {
    Exc::msg(&UNPICKLING_ERROR, m.into())
}

// ---------------------------------------------------------------- writer

#[derive(Hash, PartialEq, Eq)]
enum MemoKey {
    Str(Arc<str>),
    /// an object by identity (kept alive in `W::keep` so that its address is not reused)
    Ptr(usize),
    /// a value memoized without identity (its position in the output)
    Pos(usize),
    Global(String),
}

struct W {
    out: Vec<u8>,
    memo: HashMap<MemoKey, u32>,
    keep: Vec<V>,
}

impl W {
    fn memoize(&mut self, k: MemoKey) {
        let n = self.memo.len() as u32;
        self.memo.insert(k, n);
        self.out.push(0x94); // MEMOIZE
    }

    fn get(&mut self, k: &MemoKey) -> bool {
        match self.memo.get(k) {
            Some(&n) => {
                if n < 256 {
                    self.out.extend([b'h', n as u8]);
                } else {
                    self.out.push(b'j');
                    self.out.extend(n.to_le_bytes());
                }
                true
            }
            None => false,
        }
    }

    fn int(&mut self, i: i128) {
        if (0..256).contains(&i) {
            self.out.extend([b'K', i as u8]);
        } else if (0..65536).contains(&i) {
            self.out.push(b'M');
            self.out.extend((i as u16).to_le_bytes());
        } else if i >= i32::MIN as i128 && i <= i32::MAX as i128 {
            self.out.push(b'J');
            self.out.extend((i as i32).to_le_bytes());
        } else {
            // LONG1: little-endian two's complement, shortest
            let bytes = i.to_le_bytes();
            let mut n = 16;
            while n > 1 && ((bytes[n - 1] == 0 && bytes[n - 2] & 0x80 == 0) || (bytes[n - 1] == 0xff && bytes[n - 2] & 0x80 != 0)) {
                n -= 1;
            }
            self.out.extend([0x8a, n as u8]);
            self.out.extend(&bytes[..n]);
        }
    }

    fn str(&mut self, s: &Arc<str>) {
        let k = MemoKey::Str(s.clone());
        if self.get(&k) {
            return;
        }
        let b = s.as_bytes();
        if b.len() < 256 {
            self.out.extend([0x8c, b.len() as u8]);
        } else if b.len() <= u32::MAX as usize {
            self.out.push(b'X');
            self.out.extend((b.len() as u32).to_le_bytes());
        } else {
            self.out.push(0x8d);
            self.out.extend((b.len() as u64).to_le_bytes());
        }
        self.out.extend(b);
        self.memoize(k);
    }

    fn bytes(&mut self, b: &[u8]) {
        if b.len() < 256 {
            self.out.extend([b'C', b.len() as u8]);
        } else {
            self.out.push(b'B');
            self.out.extend((b.len() as u32).to_le_bytes());
        }
        self.out.extend(b);
        self.memoize(MemoKey::Pos(self.out.len()));
    }

    fn global(&mut self, module: &str, name: &str) {
        let k = MemoKey::Global(format!("{module} {name}"));
        if self.get(&k) {
            return;
        }
        self.str(&Arc::from(module));
        self.str(&Arc::from(name));
        self.out.push(0x93); // STACK_GLOBAL
        self.memoize(k);
    }

    /// `global(*args)` rebuilt by REDUCE
    fn reduce(&mut self, module: &str, name: &str, args: &[V]) -> R<()> {
        self.global(module, name);
        self.tuple(args)?;
        self.out.push(b'R');
        self.memoize(MemoKey::Pos(self.out.len()));
        Ok(())
    }

    fn tuple(&mut self, items: &[V]) -> R<()> {
        if items.is_empty() {
            self.out.push(b')');
            return Ok(());
        }
        if items.len() > 3 {
            self.out.push(b'(');
        }
        for x in items {
            self.val(x)?;
        }
        self.out.push(match items.len() {
            1 => 0x85,
            2 => 0x86,
            3 => 0x87,
            _ => b't',
        });
        self.memoize(MemoKey::Pos(self.out.len()));
        Ok(())
    }

    /// `cls.__new__(cls)` then `obj.__setstate__(state)` / slots update
    fn newobj(&mut self, module: &str, name: &str, key: Option<usize>, state: &V) -> R<()> {
        self.global(module, name);
        self.out.extend([b')', 0x81]);
        match key {
            Some(p) => self.memoize(MemoKey::Ptr(p)),
            None => self.memoize(MemoKey::Pos(self.out.len())),
        }
        self.val(state)?;
        self.out.push(b'b');
        Ok(())
    }

    fn items_batched(&mut self, items: &[V], one: u8, many: u8) -> R<()> {
        for chunk in items.chunks(1000) {
            if chunk.len() == 1 {
                self.val(&chunk[0])?;
                self.out.push(one);
            } else {
                self.out.push(b'(');
                for x in chunk {
                    self.val(x)?;
                }
                self.out.push(many);
            }
        }
        Ok(())
    }

    fn val(&mut self, v: &V) -> R<()> {
        if let Some(t) = ops::row_tuple(v) {
            return self.val(&t);
        }
        match v {
            V::None => self.out.push(b'N'),
            V::Bool(b) => self.out.push(if *b { 0x88 } else { 0x89 }),
            V::Int(i) => self.int(*i as i128),
            V::Float(f) => {
                self.out.push(b'G');
                self.out.extend(f.to_be_bytes());
            }
            V::Str(s) => self.str(s),
            V::Bytes(b) => self.bytes(b),
            V::List(l) => {
                self.keep.push(v.clone());
                let k = MemoKey::Ptr(Arc::as_ptr(l) as *const () as usize);
                if self.get(&k) {
                    return Ok(());
                }
                self.out.push(b']');
                self.memoize(k);
                let items = l.lock().clone();
                self.items_batched(&items, b'a', b'e')?;
            }
            V::Tuple(t) => self.tuple(t)?,
            V::Dict(d) => {
                self.keep.push(v.clone());
                let k = MemoKey::Ptr(Arc::as_ptr(d) as *const () as usize);
                if self.get(&k) {
                    return Ok(());
                }
                self.out.push(b'}');
                self.memoize(k);
                let items: Vec<(V, V)> = d.lock().values().cloned().collect();
                for chunk in items.chunks(1000) {
                    if chunk.len() == 1 {
                        self.val(&chunk[0].0)?;
                        self.val(&chunk[0].1)?;
                        self.out.push(b's');
                    } else {
                        self.out.push(b'(');
                        for (a, b) in chunk {
                            self.val(a)?;
                            self.val(b)?;
                        }
                        self.out.push(b'u');
                    }
                }
            }
            V::Set(s) => {
                self.keep.push(v.clone());
                let k = MemoKey::Ptr(Arc::as_ptr(s) as *const () as usize);
                if self.get(&k) {
                    return Ok(());
                }
                self.out.push(0x8f);
                self.memoize(k);
                let items: Vec<V> = s.lock().values().cloned().collect();
                for chunk in items.chunks(1000) {
                    self.out.push(b'(');
                    for x in chunk {
                        self.val(x)?;
                    }
                    self.out.push(0x90);
                }
            }
            V::DateTime(d) => {
                let mut st = date_bytes(d.wall.date()).to_vec();
                st.extend(time_bytes(d.wall.time(), d.fold));
                match &d.tz {
                    None => self.reduce("datetime", "datetime", &[V::Bytes(Arc::from(st.as_slice()))])?,
                    Some(tz) => {
                        self.global("datetime", "datetime");
                        self.bytes(&st);
                        self.tz(tz)?;
                        self.out.push(0x86);
                        self.memoize(MemoKey::Pos(self.out.len()));
                        self.out.push(b'R');
                        self.memoize(MemoKey::Pos(self.out.len()));
                    }
                }
            }
            V::Date(d) => self.reduce("datetime", "date", &[V::Bytes(Arc::from(&date_bytes(*d)[..]))])?,
            V::Time(t) => self.reduce("datetime", "time", &[V::Bytes(Arc::from(time_bytes(*t, 0).as_slice()))])?,
            V::Delta(d) => {
                let us = super::dt::micros(d);
                let (days, rest) = (us.div_euclid(86_400_000_000), us.rem_euclid(86_400_000_000));
                self.reduce("datetime", "timedelta", &[V::Int(days), V::Int(rest / 1_000_000), V::Int(rest % 1_000_000)])?
            }
            V::Tz(tz) => self.tz(tz)?,
            V::Decimal(d) => self.reduce("decimal", "Decimal", &[V::str(d.to_string())])?,
            V::Enum(e, i) => {
                let (m, n) = split_qual(e.class.qualname);
                self.reduce(m, n, &[e.value(*i)])?
            }
            V::Native(n) => match &**n {
                Native::Uuid(u) => {
                    self.global("uuid", "UUID");
                    self.out.extend([b')', 0x81]);
                    self.memoize(MemoKey::Pos(self.out.len()));
                    self.out.push(b'}');
                    self.memoize(MemoKey::Pos(self.out.len()));
                    self.str(&Arc::from("int"));
                    self.int(*u as i128);
                    if *u > i128::MAX as u128 {
                        return Err(perr("py2axum: UUID beyond 127 bits"));
                    }
                    self.out.extend([b's', b'b']);
                }
                _ => return Err(Exc::type_error(format!("cannot pickle '{}' object", v.type_name()))),
            },
            V::Inst(i) => {
                self.keep.push(v.clone());
                let key = Arc::as_ptr(i) as *const () as usize;
                if self.get(&MemoKey::Ptr(key)) {
                    return Ok(());
                }
                let (m, n) = split_qual(i.desc.class.qualname);
                let vals = i.vals.lock().clone();
                let fields: Vec<(V, V)> = i.desc.fields.iter().zip(vals.iter()).map(|(f, x)| (V::str(f.name), x.clone())).collect();
                let state = if i.desc.open {
                    let extra: Vec<(V, V)> = i.extra.lock().iter().map(|(k, x)| (V::str(k), x.clone())).collect();
                    if i.desc.slots.is_empty() {
                        V::dict_from(extra)?
                    } else {
                        let slots: Vec<(V, V)> =
                            i.desc.slots.iter().filter_map(|s| extra.iter().find(|(k, _)| matches!(k, V::Str(x) if &**x == *s)).cloned()).collect();
                        V::tuple(vec![V::None, V::dict_from(slots)?])
                    }
                } else if i.desc.dataclass {
                    V::dict_from(fields)?
                } else {
                    // pydantic.BaseModel.__getstate__
                    let set = i.set.lock().clone();
                    let names: Vec<V> = i.desc.fields.iter().zip(set.iter()).filter(|(_, s)| **s).map(|(f, _)| V::str(f.name)).collect();
                    let extra = if i.desc.extra == pyd::Extra::Allow {
                        V::dict_from(i.extra.lock().iter().map(|(k, x)| (V::str(k), x.clone())).collect())?
                    } else {
                        V::None
                    };
                    V::dict_from(vec![
                        (V::str("__dict__"), V::dict_from(fields)?),
                        (V::str("__pydantic_extra__"), extra),
                        (V::str("__pydantic_fields_set__"), super::methods::b_set(&[V::list(names)])?),
                        (V::str("__pydantic_private__"), V::None),
                    ])?
                };
                self.newobj(m, n, Some(key), &state)?;
            }
            V::Obj(o) => return Err(perr(format!("py2axum: pickling the mapped object {} is not supported", o.desc.name))),
            other => return Err(Exc::type_error(format!("cannot pickle '{}' object", other.type_name()))),
        }
        Ok(())
    }

    fn tz(&mut self, tz: &Tz) -> R<()> {
        match tz {
            Tz::Utc => self.reduce("datetime", "timezone", &[V::Delta(chrono::TimeDelta::zero())]),
            Tz::Fixed(s) => self.reduce("datetime", "timezone", &[V::Delta(chrono::TimeDelta::seconds(*s as i64))]),
            Tz::Zone(z) => self.reduce("zoneinfo", "ZoneInfo._unpickle", &[V::str(z.name()), V::Int(1)]),
        }
    }
}

fn date_bytes(d: chrono::NaiveDate) -> [u8; 4] {
    let y = d.year() as u16;
    [(y >> 8) as u8, (y & 0xff) as u8, d.month() as u8, d.day() as u8]
}

fn time_bytes(t: chrono::NaiveTime, fold: u8) -> Vec<u8> {
    let us = t.nanosecond() / 1000;
    vec![t.hour() as u8 | if fold != 0 { 0x80 } else { 0 }, t.minute() as u8, t.second() as u8, (us >> 16) as u8, (us >> 8) as u8, us as u8]
}

/// `pickle.dumps(obj, protocol=None)`
pub fn dumps(args: &[V], kwargs: &[(String, V)]) -> R {
    let [obj] = args else { return Err(Exc::type_error("py2axum: pickle.dumps(obj) takes one positional argument")) };
    let mut proto: u8 = if super::python() >= (3, 14) { 5 } else { 4 };
    for (k, v) in kwargs {
        match (k.as_str(), v) {
            ("protocol", V::None) => {}
            ("protocol", V::Int(p)) if (4..=5).contains(p) => proto = *p as u8,
            ("protocol", V::Int(-1)) => proto = 5,
            ("protocol", _) => return Err(perr("py2axum: pickle protocols 4 and 5 only")),
            _ => return Err(Exc::type_error(format!("py2axum: pickle.dumps({k}=) is not supported"))),
        }
    }
    let mut w = W { out: Vec::new(), memo: HashMap::new(), keep: Vec::new() };
    w.val(obj)?;
    w.out.push(b'.');
    // PROTO, then one frame (the unpickler accepts any frame size)
    let mut out = vec![0x80, proto];
    if w.out.len() >= 4 {
        out.push(0x95);
        out.extend((w.out.len() as u64).to_le_bytes());
    }
    out.extend(w.out);
    Ok(V::Bytes(Arc::from(out.as_slice())))
}

// ---------------------------------------------------------------- reader

#[derive(Clone)]
enum G {
    Datetime,
    Date,
    Time,
    Timedelta,
    Timezone,
    TzInfo,
    Decimal,
    Uuid,
    ZoneInfo,
    Set,
    Frozenset,
    Class(&'static Class),
}

#[derive(Clone)]
enum Item {
    V(V),
    Mark,
    G(G),
    /// an integer beyond 64 bits (a UUID's state)
    Big(i128),
    /// `UUID.__new__(UUID)` before its state
    NewUuid,
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> R<&[u8]> {
        if self.i + n > self.b.len() {
            return Err(uerr("pickle data was truncated"));
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> R<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> R<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn line(&mut self) -> R<String> {
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i] != b'\n' {
            self.i += 1;
        }
        let s = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        self.i += 1;
        Ok(s)
    }
}

fn utf8(b: &[u8]) -> R<V> {
    std::str::from_utf8(b).map(V::str).map_err(|_| uerr("invalid utf-8 in a pickled str"))
}

fn global(module: &str, name: &str) -> R<G> {
    Ok(match (module, name) {
        ("datetime", "datetime") => G::Datetime,
        ("datetime", "date") => G::Date,
        ("datetime", "time") => G::Time,
        ("datetime", "timedelta") => G::Timedelta,
        ("datetime", "timezone") => G::Timezone,
        ("pydantic_core._pydantic_core", "TzInfo") => G::TzInfo,
        ("decimal", "Decimal") => G::Decimal,
        ("uuid", "UUID") => G::Uuid,
        ("zoneinfo", "ZoneInfo._unpickle") | ("zoneinfo._zoneinfo", "ZoneInfo._unpickle") => G::ZoneInfo,
        ("builtins", "set") => G::Set,
        ("builtins", "frozenset") => G::Frozenset,
        _ => match class_by_name(module, name) {
            Some(c) => G::Class(c),
            None => return Err(Exc::attr_error(format!("py2axum: cannot unpickle the global {module}.{name}"))),
        },
    })
}

fn as_v(it: Item) -> R<V> {
    match it {
        Item::V(v) => Ok(v),
        // py2axum has no integers beyond 64 bits: an exact integral Decimal stands for them
        Item::Big(i) => super::decimal::new(&[V::str(i.to_string())], &[]),
        _ => Err(uerr("py2axum: unexpected pickle stack item")),
    }
}

fn int_of(v: &V) -> R<i64> {
    match v {
        V::Int(i) => Ok(*i),
        _ => Err(uerr("py2axum: an int was expected")),
    }
}

fn bytes_of(v: &V) -> R<Vec<u8>> {
    match v {
        V::Bytes(b) => Ok(b.to_vec()),
        V::Str(s) => Ok(s.chars().map(|c| c as u32 as u8).collect()), // latin-1 strings of protocol 2
        _ => Err(uerr("py2axum: bytes were expected")),
    }
}

fn tz_of(v: &V) -> R<Option<Tz>> {
    match v {
        V::None => Ok(None),
        V::Tz(t) => Ok(Some(*t)),
        _ => Err(uerr("py2axum: a tzinfo was expected")),
    }
}

fn time_of(b: &[u8]) -> R<(chrono::NaiveTime, u8)> {
    let us = ((b[3] as u32) << 16) | ((b[4] as u32) << 8) | b[5] as u32;
    let t = chrono::NaiveTime::from_hms_micro_opt((b[0] & 0x7f) as u32, b[1] as u32, b[2] as u32, us).ok_or_else(|| uerr("bad time state"))?;
    Ok((t, (b[0] >> 7) & 1))
}

fn date_of(b: &[u8]) -> R<chrono::NaiveDate> {
    chrono::NaiveDate::from_ymd_opt(((b[0] as i32) << 8) | b[1] as i32, b[2] as u32, b[3] as u32).ok_or_else(|| uerr("bad date state"))
}

fn reduce(g: G, args: Vec<V>) -> R<Item> {
    Ok(Item::V(match (g, args.as_slice()) {
        (G::Datetime, [st, rest @ ..]) => {
            let b = bytes_of(st)?;
            if b.len() != 10 {
                return Err(uerr("py2axum: datetime(year, ...) arguments are not supported in a pickle"));
            }
            let (t, fold) = time_of(&b[4..])?;
            let tz = match rest.first() {
                Some(z) => tz_of(z)?,
                None => None,
            };
            V::DateTime(DateTime { wall: date_of(&b[..4])?.and_time(t), tz, fold })
        }
        (G::Date, [st]) => V::Date(date_of(&bytes_of(st)?)?),
        (G::Time, [st, ..]) => V::Time(time_of(&bytes_of(st)?)?.0),
        (G::Timedelta, [d, s, us]) => V::Delta(chrono::TimeDelta::microseconds(int_of(d)? * 86_400_000_000 + int_of(s)? * 1_000_000 + int_of(us)?)),
        (G::Timezone, [V::Delta(d), ..]) => {
            let s = d.num_seconds() as i32;
            V::Tz(if s == 0 { Tz::Utc } else { Tz::Fixed(s) })
        }
        (G::TzInfo, [V::Int(s)]) => V::Tz(if *s == 0 { Tz::Utc } else { Tz::Fixed(*s as i32) }),
        (G::Decimal, [s]) => super::decimal::new(&[s.clone()], &[])?,
        (G::ZoneInfo, [V::Str(k), ..]) => V::Tz(Tz::Zone(k.parse().map_err(|_| uerr(format!("No time zone found with key {k}")))?)),
        (G::Set | G::Frozenset, [items]) => super::methods::b_set(&[items.clone()])?,
        (G::Set | G::Frozenset, []) => super::methods::b_set(&[])?,
        (G::Class(c), [value]) if matches!(c.kind, ClassKind::Enum(_)) => {
            let ClassKind::Enum(e) = c.kind else { unreachable!() };
            (0..e.members.len() as u16)
                .find(|i| ops::eq_bool(&e.value(*i), value))
                .map(|i| V::Enum(e, i))
                .ok_or_else(|| Exc::value_error(format!("{} is not a valid {}", ops::repr(value).unwrap_or_default(), c.name)))?
        }
        _ => return Err(uerr("py2axum: unsupported REDUCE in a pickle")),
    }))
}

fn newobj(g: G) -> R<Item> {
    match g {
        G::Uuid => Ok(Item::NewUuid),
        G::Class(c) => match c.kind {
            ClassKind::Schema(s) => Ok(Item::V(pyd::object_new_blank(s))),
            _ => Err(uerr(format!("py2axum: cannot unpickle a {} instance", c.name))),
        },
        _ => Err(uerr("py2axum: unsupported NEWOBJ in a pickle")),
    }
}

fn dict_items(v: &V) -> R<Vec<(V, V)>> {
    match v {
        V::Dict(d) => Ok(d.lock().values().cloned().collect()),
        V::None => Ok(vec![]),
        _ => Err(uerr("py2axum: a dict state was expected")),
    }
}

fn build(obj: Item, state: V) -> R<Item> {
    match obj {
        Item::NewUuid => {
            let items = dict_items(&state)?;
            let n = items.iter().find(|(k, _)| matches!(k, V::Str(s) if &**s == "int")).map(|(_, v)| v.clone());
            match n {
                Some(n @ (V::Int(_) | V::Decimal(_))) => Ok(Item::V(super::pathio::uuid_new(&[], &[("int".into(), n)])?)),
                _ => Err(uerr("py2axum: bad UUID state")),
            }
        }
        Item::V(V::Inst(i)) => {
            let d = i.desc;
            if d.open {
                // plain class: __dict__, or (dict, slots) for a class with __slots__
                let (dict, slots) = match &state {
                    V::Tuple(t) if t.len() == 2 => (t[0].clone(), t[1].clone()),
                    other => (other.clone(), V::None),
                };
                let mut extra = i.extra.lock();
                for (k, v) in dict_items(&dict)?.into_iter().chain(dict_items(&slots)?) {
                    extra.insert(ops::str_(&k)?, v);
                }
            } else {
                let items = dict_items(&state)?;
                let get = |n: &str| items.iter().find(|(k, _)| matches!(k, V::Str(s) if &**s == n)).map(|(_, v)| v.clone());
                let (fields, set) = if d.dataclass {
                    (state.clone(), None)
                } else {
                    (get("__dict__").unwrap_or(V::None), get("__pydantic_fields_set__"))
                };
                let fields = dict_items(&fields)?;
                let mut vals = Vec::with_capacity(d.fields.len());
                let mut flags = Vec::with_capacity(d.fields.len());
                for f in d.fields {
                    let v = fields.iter().find(|(k, _)| matches!(k, V::Str(s) if &**s == f.name)).map(|(_, v)| v.clone());
                    flags.push(match &set {
                        Some(s) => ops::contains(s, &V::str(f.name))?,
                        None => true,
                    });
                    vals.push(v.unwrap_or(V::None));
                }
                *i.vals.lock() = vals;
                *i.set.lock() = flags;
                if let Some(V::Dict(x)) = get("__pydantic_extra__") {
                    let mut extra = i.extra.lock();
                    for (_, (k, v)) in x.lock().iter() {
                        extra.insert(ops::str_(k)?, v.clone());
                    }
                }
            }
            Ok(Item::V(V::Inst(i)))
        }
        _ => Err(uerr("py2axum: unsupported BUILD in a pickle")),
    }
}

fn pop_mark(stack: &mut Vec<Item>) -> R<Vec<Item>> {
    let at = stack.iter().rposition(|x| matches!(x, Item::Mark)).ok_or_else(|| uerr("could not find MARK"))?;
    let items = stack.split_off(at + 1);
    stack.pop();
    Ok(items)
}

fn pop(stack: &mut Vec<Item>) -> R<Item> {
    stack.pop().ok_or_else(|| uerr("unpickling stack underflow"))
}

fn top_v(stack: &[Item]) -> R<V> {
    match stack.last() {
        Some(Item::V(v)) => Ok(v.clone()),
        _ => Err(uerr("py2axum: a container was expected on the stack")),
    }
}

/// `pickle.loads(data)`
pub fn loads(args: &[V], kwargs: &[(String, V)]) -> R {
    if !kwargs.is_empty() {
        return Err(Exc::type_error("py2axum: pickle.loads() options are not supported"));
    }
    let [data] = args else { return Err(Exc::type_error("py2axum: pickle.loads(data) takes one positional argument")) };
    let data = match data {
        V::Bytes(b) => b.clone(),
        other => return Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", other.type_name()))),
    };
    if data.is_empty() {
        return Err(Exc::msg(&EOF_ERROR, "Ran out of input"));
    }
    let mut r = Rd { b: &data, i: 0 };
    let mut stack: Vec<Item> = Vec::new();
    let mut memo: HashMap<u32, Item> = HashMap::new();
    loop {
        let op = r.u8()?;
        match op {
            0x80 => {
                let p = r.u8()?;
                if p > 5 {
                    return Err(Exc::value_error(format!("unsupported pickle protocol: {p}")));
                }
            }
            0x95 => {
                r.u64()?;
            }
            b'.' => return as_v(pop(&mut stack)?),
            b'(' => stack.push(Item::Mark),
            b'0' => {
                pop(&mut stack)?;
            }
            b'1' => {
                pop_mark(&mut stack)?;
            }
            b'2' => {
                let t = stack.last().cloned().ok_or_else(|| uerr("stack underflow"))?;
                stack.push(t);
            }
            b'N' => stack.push(Item::V(V::None)),
            0x88 => stack.push(Item::V(V::Bool(true))),
            0x89 => stack.push(Item::V(V::Bool(false))),
            b'K' => stack.push(Item::V(V::Int(r.u8()? as i64))),
            b'M' => stack.push(Item::V(V::Int(r.u16()? as i64))),
            b'J' => stack.push(Item::V(V::Int(r.u32()? as i32 as i64))),
            0x8a | 0x8b => {
                let n = if op == 0x8a { r.u8()? as usize } else { r.u32()? as usize };
                let b = r.take(n)?;
                if n > 16 {
                    return Err(uerr("py2axum: an integer beyond 128 bits"));
                }
                let mut buf = [if b.last().is_some_and(|x| x & 0x80 != 0) { 0xff } else { 0 }; 16];
                buf[..n].copy_from_slice(b);
                let i = i128::from_le_bytes(buf);
                stack.push(if i >= i64::MIN as i128 && i <= i64::MAX as i128 { Item::V(V::Int(i as i64)) } else { Item::Big(i) });
            }
            b'I' => {
                let l = r.line()?;
                stack.push(Item::V(match l.as_str() {
                    "00" => V::Bool(false),
                    "01" => V::Bool(true),
                    s => V::Int(s.parse().map_err(|_| uerr("bad INT"))?),
                }));
            }
            b'G' => stack.push(Item::V(V::Float(f64::from_be_bytes(r.take(8)?.try_into().unwrap())))),
            b'F' => stack.push(Item::V(V::Float(r.line()?.parse().map_err(|_| uerr("bad FLOAT"))?))),
            0x8c => {
                let n = r.u8()? as usize;
                stack.push(Item::V(utf8(r.take(n)?)?));
            }
            b'X' => {
                let n = r.u32()? as usize;
                stack.push(Item::V(utf8(r.take(n)?)?));
            }
            0x8d => {
                let n = r.u64()? as usize;
                stack.push(Item::V(utf8(r.take(n)?)?));
            }
            b'C' => {
                let n = r.u8()? as usize;
                stack.push(Item::V(V::Bytes(Arc::from(r.take(n)?))));
            }
            b'B' => {
                let n = r.u32()? as usize;
                stack.push(Item::V(V::Bytes(Arc::from(r.take(n)?))));
            }
            0x8e | 0x96 => {
                let n = r.u64()? as usize;
                stack.push(Item::V(V::Bytes(Arc::from(r.take(n)?))));
            }
            b'U' => {
                let n = r.u8()? as usize;
                stack.push(Item::V(V::str(r.take(n)?.iter().map(|&c| c as char).collect::<String>())));
            }
            b'T' => {
                let n = r.u32()? as usize;
                stack.push(Item::V(V::str(r.take(n)?.iter().map(|&c| c as char).collect::<String>())));
            }
            b']' => stack.push(Item::V(V::list(vec![]))),
            b'l' => {
                let items = pop_mark(&mut stack)?.into_iter().map(as_v).collect::<R<Vec<_>>>()?;
                stack.push(Item::V(V::list(items)));
            }
            b'a' | b'e' => {
                let items = if op == b'a' { vec![as_v(pop(&mut stack)?)?] } else { pop_mark(&mut stack)?.into_iter().map(as_v).collect::<R<Vec<_>>>()? };
                match top_v(&stack)? {
                    V::List(l) => l.lock().extend(items),
                    _ => return Err(uerr("py2axum: APPEND on a non-list")),
                }
            }
            b')' => stack.push(Item::V(V::tuple(vec![]))),
            0x85 | 0x86 | 0x87 => {
                let n = (op - 0x84) as usize;
                if stack.len() < n {
                    return Err(uerr("stack underflow"));
                }
                let items = stack.split_off(stack.len() - n).into_iter().map(as_v).collect::<R<Vec<_>>>()?;
                stack.push(Item::V(V::tuple(items)));
            }
            b't' => {
                let items = pop_mark(&mut stack)?.into_iter().map(as_v).collect::<R<Vec<_>>>()?;
                stack.push(Item::V(V::tuple(items)));
            }
            b'}' => stack.push(Item::V(V::dict_from(vec![])?)),
            b'd' => {
                let items = pop_mark(&mut stack)?.into_iter().map(as_v).collect::<R<Vec<_>>>()?;
                stack.push(Item::V(V::dict_from(items.chunks(2).map(|p| (p[0].clone(), p[1].clone())).collect())?));
            }
            b's' | b'u' => {
                let items = if op == b's' {
                    let v = as_v(pop(&mut stack)?)?;
                    let k = as_v(pop(&mut stack)?)?;
                    vec![k, v]
                } else {
                    pop_mark(&mut stack)?.into_iter().map(as_v).collect::<R<Vec<_>>>()?
                };
                match top_v(&stack)? {
                    V::Dict(d) => {
                        let mut d = d.lock();
                        for p in items.chunks(2) {
                            d.insert(Key::of(&p[0])?, (p[0].clone(), p[1].clone()));
                        }
                    }
                    _ => return Err(uerr("py2axum: SETITEM on a non-dict")),
                }
            }
            0x8f => stack.push(Item::V(super::methods::b_set(&[])?)),
            0x90 => {
                let items = pop_mark(&mut stack)?.into_iter().map(as_v).collect::<R<Vec<_>>>()?;
                match top_v(&stack)? {
                    V::Set(s) => {
                        let mut s = s.lock();
                        for x in items {
                            s.insert(Key::of(&x)?, x);
                        }
                    }
                    _ => return Err(uerr("py2axum: ADDITEMS on a non-set")),
                }
            }
            0x91 => {
                let items = pop_mark(&mut stack)?.into_iter().map(as_v).collect::<R<Vec<_>>>()?;
                stack.push(Item::V(super::methods::b_set(&[V::list(items)])?));
            }
            b'c' => {
                let m = r.line()?;
                let n = r.line()?;
                stack.push(Item::G(global(&m, &n)?));
            }
            0x93 => {
                let n = as_v(pop(&mut stack)?)?;
                let m = as_v(pop(&mut stack)?)?;
                stack.push(Item::G(global(&ops::str_(&m)?, &ops::str_(&n)?)?));
            }
            b'R' => {
                let args = as_v(pop(&mut stack)?)?;
                let Item::G(g) = pop(&mut stack)? else { return Err(uerr(format!("py2axum: REDUCE of a non-global (offset {})", r.i - 1))) };
                let args = match args {
                    V::Tuple(t) => t.to_vec(),
                    _ => return Err(uerr("py2axum: REDUCE arguments must be a tuple")),
                };
                stack.push(reduce(g, args)?);
            }
            0x81 => {
                pop(&mut stack)?; // args (always empty for the supported classes)
                let Item::G(g) = pop(&mut stack)? else { return Err(uerr("py2axum: NEWOBJ of a non-global")) };
                stack.push(newobj(g)?);
            }
            0x92 => {
                pop(&mut stack)?;
                pop(&mut stack)?;
                let Item::G(g) = pop(&mut stack)? else { return Err(uerr("py2axum: NEWOBJ_EX of a non-global")) };
                stack.push(newobj(g)?);
            }
            b'b' => {
                let state = as_v(pop(&mut stack)?)?;
                let obj = pop(&mut stack)?;
                let built = build(obj, state)?;
                // a memoized object rebuilt: refresh its memo entries
                for x in memo.values_mut() {
                    if matches!(x, Item::NewUuid) {
                        *x = built.clone();
                    }
                }
                stack.push(built);
            }
            0x94 => {
                let n = memo.len() as u32;
                memo.insert(n, stack.last().cloned().ok_or_else(|| uerr("stack underflow"))?);
            }
            b'p' => {
                let n: u32 = r.line()?.parse().map_err(|_| uerr("bad PUT"))?;
                memo.insert(n, stack.last().cloned().ok_or_else(|| uerr("stack underflow"))?);
            }
            b'q' => {
                let n = r.u8()? as u32;
                memo.insert(n, stack.last().cloned().ok_or_else(|| uerr("stack underflow"))?);
            }
            b'r' => {
                let n = r.u32()?;
                memo.insert(n, stack.last().cloned().ok_or_else(|| uerr("stack underflow"))?);
            }
            b'g' => {
                let n: u32 = r.line()?.parse().map_err(|_| uerr("bad GET"))?;
                stack.push(memo.get(&n).cloned().ok_or_else(|| uerr(format!("Memo value not found at index {n}")))?);
            }
            b'h' => {
                let n = r.u8()? as u32;
                stack.push(memo.get(&n).cloned().ok_or_else(|| uerr(format!("Memo value not found at index {n}")))?);
            }
            b'j' => {
                let n = r.u32()?;
                stack.push(memo.get(&n).cloned().ok_or_else(|| uerr(format!("Memo value not found at index {n}")))?);
            }
            other => return Err(uerr(format!("py2axum: unsupported pickle opcode 0x{other:02x}"))),
        }
    }
}
