//! SQLAlchemy 2.0 ORM semantics on sqlx: mapped objects, the AsyncSession unit of work
//! (identity map, autoflush, implicit transaction, flush order, rollback/expiry), the core
//! expression language (`select`, `update`, `delete`, `func.*`, column operators) and results.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{Column, Postgres, QueryBuilder, Row, TypeInfo};

use super::dt::{DateTime, Tz};
use super::ops;
use super::pyd;
use super::v::*;

// ---------------------------------------------------------------- descriptors

/// `sqlalchemy.Enum(PyEnum, values_callable=..., native_enum=..., name=...)`
pub struct EnumCol {
    pub desc: &'static EnumDesc,
    /// stores member values (values_callable) instead of member names
    pub by_value: bool,
    /// native Postgres enum type (needs a cast), None for VARCHAR storage
    pub pg_type: Option<&'static str>,
}

impl EnumCol {
    pub fn to_db(&self, v: &V) -> R<V> {
        match v {
            V::Enum(e, i) if std::ptr::eq(*e, self.desc) => {
                Ok(if self.by_value { e.value(*i) } else { V::str(e.member_name(*i)) })
            }
            V::None => Ok(V::None),
            other => {
                // a raw value/name is accepted if it denotes a member
                let m = if self.by_value { self.desc.by_value(other) } else { ops::str_(other).ok().and_then(|n| self.desc.by_name(&n)) }
                    .or_else(|| self.desc.by_value(other));
                match m {
                    Some(V::Enum(e, i)) => Ok(if self.by_value { e.value(i) } else { V::str(e.member_name(i)) }),
                    _ => Err(Exc::msg(&STATEMENT_ERROR, format!("'{}' is not among the defined enum values of {}", ops::str_(other).unwrap_or_default(), self.desc.name))),
                }
            }
        }
    }
    pub fn from_db(&self, v: V) -> V {
        if v.is_none() {
            return v;
        }
        let hit = if self.by_value { self.desc.by_value(&v) } else { v.as_str().and_then(|n| self.desc.by_name(n)) };
        hit.unwrap_or(v)
    }
}

#[derive(Clone, Copy)]
pub enum ColTy {
    Int,
    BigInt,
    SmallInt,
    Str,
    Bool,
    Float,
    /// `Numeric(asdecimal=False)`: NUMERIC in the database, float in Python
    NumFloat,
    /// `Numeric()`: NUMERIC in the database, decimal.Decimal in Python
    Numeric,
    Date,
    DateTime,
    DateTimeTz,
    Time,
    Json,
    /// `JSON(none_as_null=True)`: Python None is SQL NULL instead of JSON `null`
    JsonNull,
    Uuid,
    /// `LargeBinary`: BYTEA, bytes in Python
    Bytes,
    /// `ARRAY(String)`: TEXT[]/VARCHAR[], a list of str in Python
    StrArray,
    Enum(&'static EnumCol),
    /// a project `TypeDecorator`: its `impl` type, plus the compiled process_* methods
    Decorated(&'static TypeDec),
}

pub struct TypeDec {
    pub name: &'static str,
    /// the `impl` column type (what is bound and read)
    pub impl_ty: ColTy,
    pub bind: Option<pyd::MethodFn>,
    pub result: Option<pyd::MethodFn>,
}

impl ColTy {
    /// the type actually sent to and read from the database
    pub fn base(self) -> ColTy {
        match self {
            ColTy::Decorated(d) => d.impl_ty,
            t => t,
        }
    }
}

impl PartialEq for ColTy {
    fn eq(&self, other: &ColTy) -> bool {
        match (self, other) {
            (ColTy::Enum(a), ColTy::Enum(b)) => std::ptr::eq(*a, *b),
            (ColTy::Decorated(a), ColTy::Decorated(b)) => std::ptr::eq(*a, *b),
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

impl std::fmt::Debug for ColTy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ColTy::Int => "Int",
            ColTy::BigInt => "BigInt",
            ColTy::SmallInt => "SmallInt",
            ColTy::Str => "Str",
            ColTy::Bool => "Bool",
            ColTy::Float => "Float",
            ColTy::NumFloat => "NumFloat",
            ColTy::Numeric => "Numeric",
            ColTy::Date => "Date",
            ColTy::DateTime => "DateTime",
            ColTy::DateTimeTz => "DateTimeTz",
            ColTy::Time => "Time",
            ColTy::Json => "Json",
            ColTy::JsonNull => "JsonNull",
            ColTy::Uuid => "Uuid",
            ColTy::Bytes => "Bytes",
            ColTy::StrArray => "StrArray",
            ColTy::Enum(e) => return write!(f, "Enum({})", e.desc.name),
            ColTy::Decorated(d) => d.name,
        };
        f.write_str(name)
    }
}

pub enum ColDefault {
    None,
    /// Python-side `default=` literal, applied at INSERT when unset.
    Value(fn() -> V),
    /// Python-side `default=<callable>` (called per INSERT) or SQL expression (`func.now()`).
    Dyn(pyd::MethodFn),
}

impl ColDefault {
    /// The value of this default/onupdate, or None when there is none (`V::Sql` for SQL expressions).
    pub async fn eval(&self) -> R<Option<V>> {
        Ok(match self {
            ColDefault::None => None,
            ColDefault::Value(f) => Some(f()),
            ColDefault::Dyn(f) => Some(f(&super::root_cx(), V::None, vec![]).await?),
        })
    }
}

pub struct ColDesc {
    pub name: &'static str,
    pub ty: ColTy,
    pub nullable: bool,
    pub pk: bool,
    pub autoincrement: bool,
    pub default: ColDefault,
    pub server_default: bool,
    /// `onupdate=`: applied to every UPDATE (unit of work or `update()`) that does not set the column.
    pub onupdate: ColDefault,
    /// `deferred(Column(...))`: not part of what a query loads (SQLAlchemy leaves it out of the SELECT; the
    /// value is read here and dropped, so the column positions stay those of the table)
    pub deferred: bool,
}

pub struct ModelDesc {
    pub name: &'static str,
    pub class_qualname: &'static str,
    pub table: &'static str,
    pub class: &'static Class,
    pub cols: &'static [ColDesc],
    /// first primary key column (the only one unless the key is composite)
    pub pk: usize,
    /// every primary key column, in declaration order (SQLAlchemy's identity order)
    pub pks: &'static [usize],
    /// tables this one references
    pub fk_tables: &'static [&'static str],
    /// foreign keys: (column, referenced table, referenced column)
    pub fks: &'static [(usize, &'static str, &'static str)],
    /// topological rank over the foreign keys (INSERT order: referenced tables first)
    pub rank: usize,
    pub methods: &'static [(&'static str, bool, pyd::MethodFn)],
    /// the `@staticmethod`/`@classmethod`s, also read on the class (`Model.make()`, `cls.slug(x)`)
    pub class_methods: &'static [&'static str],
    pub rels: &'static [RelDesc],
    /// the methods that are `async def`s (a call not awaited is a coroutine)
    pub async_methods: &'static [&'static str],
}

impl ModelDesc {
    /// the identity of per-column values: the primary key, or a tuple of them when composite
    pub fn pk_of(&self, vals: &[V]) -> V {
        match self.pks {
            [i] => vals[*i].clone(),
            pks => V::tuple(pks.iter().map(|i| vals[*i].clone()).collect()),
        }
    }

    pub fn is_pk(&self, i: usize) -> bool {
        self.pks.contains(&i)
    }

    /// ` WHERE t.a = $n AND t.b = $m` for an identity
    fn where_pk(&self, r: &mut Rend, pk: &V) {
        let parts: Vec<V> = match (self.pks.len(), pk) {
            (1, _) => vec![pk.clone()],
            (_, V::Tuple(t)) => t.to_vec(),
            _ => vec![pk.clone()],
        };
        for (k, (i, v)) in self.pks.iter().zip(parts).enumerate() {
            r.sql += if k == 0 { " WHERE " } else { " AND " };
            r.sql += &format!("{}.{} = ", self.table, self.cols[*i].name);
            r.bind(v, Some(self.cols[*i].ty));
        }
    }

    /// `session.get()`'s identity argument: a scalar, a tuple/list in key order, or a dict by attribute
    fn get_ident(&self, pk: &V) -> R<V> {
        let parts: Vec<V> = match pk {
            V::Dict(d) if d.lock().len() != self.pks.len() => vec![V::None; d.lock().len()],
            V::Dict(d) => {
                let d = d.lock();
                let mut out = Vec::new();
                for i in self.pks {
                    match d.get(&Key::Str(Arc::from(self.cols[*i].name))) {
                        Some((_, v)) => out.push(v.clone()),
                        None => {
                            return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                                "Incorrect names of values in identifier to formulate primary key for session.get(); primary key attribute names are {} (synonym names are also accepted)",
                                self.pks.iter().map(|i| format!("'{}'", self.cols[*i].name)).collect::<Vec<_>>().join(","))))
                        }
                    }
                }
                out
            }
            V::Tuple(t) => t.to_vec(),
            V::List(l) => l.lock().clone(),
            other => vec![other.clone()],
        };
        if parts.len() != self.pks.len() {
            return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                "Incorrect number of values in identifier to formulate primary key for session.get(); primary key columns are {}",
                self.pks.iter().map(|i| format!("'{}.{}'", self.table, self.cols[*i].name)).collect::<Vec<_>>().join(","))));
        }
        Ok(match self.pks.len() {
            1 => parts.into_iter().next().unwrap(),
            _ => V::tuple(parts),
        })
    }

    pub fn col_index(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.name == name)
    }
    pub fn rel_index(&self, name: &str) -> Option<usize> {
        self.rels.iter().position(|r| r.name == name)
    }
}

/// `relationship(lazy=...)` (joined/subquery/immediate load like selectin: same objects, same order).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lazy {
    Select,
    Selectin,
    NoLoad,
    Raise,
}

/// A foreign-key `relationship()` between two models. Many-to-one: `local` is this model's FK
/// column, `remote` the target column it references. One-to-many: `local` is this model's
/// referenced column, `remote` the target's FK column.
pub struct RelDesc {
    pub name: &'static str,
    pub target: &'static ModelDesc,
    pub m2o: bool,
    pub local: usize,
    pub remote: usize,
    pub uselist: bool,
    pub lazy: Lazy,
    pub back: Option<&'static str>,
    pub delete: bool,
    pub orphan: bool,
    pub passive_deletes: bool,
    /// `order_by=`: (target column, descending) — the collection's order when loaded
    pub order: &'static [(usize, bool)],
}

/// One loader option path: `selectinload(A.b).selectinload(B.c)` = [(Selectin, A, b), (Selectin, B, c)].
pub type LoadChain = Vec<(Lazy, &'static ModelDesc, usize)>;

// ---------------------------------------------------------------- mapped objects

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Transient,
    Pending,
    Persistent,
    Deleted,
}

pub struct ObjState {
    pub vals: Vec<V>,
    pub committed: Vec<V>,
    pub modified: Vec<bool>,
    pub status: Status,
    pub expired: bool,
    pub extra: Vec<(String, V)>,
    /// relationship values, None = not loaded
    pub rels: Vec<Option<V>>,
    /// members of a loaded collection as of the last load/flush (diffed at flush)
    pub rel_snap: Vec<Option<Vec<V>>>,
    /// many-to-one assigned since the last flush
    pub rel_set: Vec<bool>,
    /// foreign key copies resolved at flush: (my column, source object or None for NULL, its column)
    pub links: Vec<(usize, Option<Arc<ObjCell>>, usize)>,
}

impl ObjState {
    fn unload_rels(&mut self) {
        for r in self.rels.iter_mut() {
            *r = None;
        }
        for r in self.rel_snap.iter_mut() {
            *r = None;
        }
    }
}

pub struct ObjCell {
    pub desc: &'static ModelDesc,
    pub st: Mutex<ObjState>,
    sess: Mutex<Weak<tokio::sync::Mutex<SessInner>>>,
}

fn members(v: &V) -> Vec<V> {
    match v {
        V::List(l) => l.lock().clone(),
        V::None => vec![],
        other => vec![other.clone()],
    }
}

fn same(a: &V, b: &V) -> bool {
    matches!((a, b), (V::Obj(x), V::Obj(y)) if Arc::ptr_eq(x, y))
}

impl ObjCell {
    pub fn transient(desc: &'static ModelDesc) -> Arc<ObjCell> {
        let n = desc.cols.len();
        Arc::new(ObjCell {
            desc,
            st: Mutex::new(ObjState {
                vals: vec![V::Unbound; n],
                committed: vec![V::Unbound; n],
                modified: vec![false; n],
                status: Status::Transient,
                expired: false,
                extra: Vec::new(),
                rels: vec![None; desc.rels.len()],
                rel_snap: vec![None; desc.rels.len()],
                rel_set: vec![false; desc.rels.len()],
                links: Vec::new(),
            }),
            sess: Mutex::new(Weak::new()),
        })
    }

    /// Reading a relationship: loaded value, or what SQLAlchemy's lazy loader does without IO
    /// (empty for transient/pending objects and `noload`, identity-map hit for a many-to-one,
    /// None for a NULL foreign key); anything needing SQL is MissingGreenlet in an async session.
    fn rel_get(&self, st: &mut ObjState, ri: usize) -> R {
        let rel = &self.desc.rels[ri];
        if let Some(v) = &st.rels[ri] {
            return Ok(v.clone());
        }
        let fresh = st.status == Status::Transient || st.status == Status::Pending;
        if fresh || rel.lazy == Lazy::NoLoad {
            if rel.uselist {
                let v = V::list(vec![]);
                st.rels[ri] = Some(v.clone());
                if st.rel_snap[ri].is_none() && !fresh {
                    st.rel_snap[ri] = Some(vec![]);
                }
                return Ok(v);
            }
            return Ok(V::None);
        }
        if rel.lazy == Lazy::Raise {
            return Err(Exc::msg(
                &INVALID_REQUEST_ERROR,
                format!("'{}.{}' is not available due to lazy='raise'", self.desc.name, rel.name),
            ));
        }
        if rel.m2o {
            let fk = st.vals[rel.local].clone();
            if fk.is_none() {
                return Ok(V::None);
            }
            if !matches!(fk, V::Unbound) && rel.target.pks == [rel.remote] {
                if let Some(sess) = self.sess.lock().upgrade() {
                    if let Ok(s) = sess.try_lock() {
                        if let Some(o) = s.identity.get(&ident(rel.target, &fk)?) {
                            let o = &o;
                            // a self-referential row pointing at itself: its state is the one locked here
                            let ok = if std::ptr::eq(Arc::as_ptr(o), self) {
                                !st.expired && st.status == Status::Persistent
                            } else {
                                let ost = o.st.lock();
                                !ost.expired && ost.status == Status::Persistent
                            };
                            if ok {
                                let v = V::Obj(o.clone());
                                st.rels[ri] = Some(v.clone());
                                return Ok(v);
                            }
                        }
                    }
                }
            }
        }
        Err(Exc::msg(
            &MISSING_GREENLET,
            format!(
                "greenlet_spawn has not been called; can't call await_only() here. Was IO attempted in an unexpected place? \
                 (lazy load of {}.{}: use selectinload() or lazy=\"selectin\")",
                self.desc.name, rel.name
            ),
        ))
    }

    fn rel_set(self: &Arc<Self>, ri: usize, v: V) -> R<()> {
        let rel = &self.desc.rels[ri];
        if rel.uselist && !matches!(v, V::List(_)) {
            return Err(Exc::type_error(format!("{}.{} needs a list", self.desc.name, rel.name)));
        }
        if !rel.uselist && !matches!(v, V::Obj(_) | V::None) {
            return Err(Exc::type_error(format!("{}.{} needs a {} or None", self.desc.name, rel.name, rel.target.name)));
        }
        let old = {
            let mut st = self.st.lock();
            let old = st.rels[ri].replace(v.clone());
            st.rel_set[ri] = true;
            old
        };
        // back_populates: keep the other side in step when it is loaded
        if let (Some(back), true) = (rel.back, rel.m2o) {
            let me = V::Obj(self.clone());
            if let Some(bi) = rel.target.rel_index(back) {
                if let Some(V::Obj(prev)) = &old {
                    if let Some(V::List(l)) = &prev.st.lock().rels[bi] {
                        l.lock().retain(|x| !same(x, &me));
                    }
                }
                if let V::Obj(new) = &v {
                    let mut nst = new.st.lock();
                    match &nst.rels[bi] {
                        Some(V::List(l)) => {
                            let mut l = l.lock();
                            if !l.iter().any(|x| same(x, &me)) {
                                l.push(me.clone());
                            }
                        }
                        _ if !rel.target.rels[bi].uselist => nst.rels[bi] = Some(me.clone()),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }


    /// `obj.__dict__`: SQLAlchemy's instance dict (`_sa_instance_state`, then the loaded attributes); a
    /// snapshot, writing into it does not change the object
    pub fn instance_dict(&self) -> R {
        let st = self.st.lock();
        let mut items = vec![(V::str("_sa_instance_state"), V::native(Native::Namespace("InstanceState")))];
        for (i, c) in self.desc.cols.iter().enumerate() {
            if !matches!(st.vals[i], V::Unbound) {
                items.push((V::str(c.name), st.vals[i].clone()));
            }
        }
        for (ri, r) in self.desc.rels.iter().enumerate() {
            if let Some(v) = &st.rels[ri] {
                items.push((V::str(r.name), v.clone()));
            } else if r.lazy == Lazy::NoLoad && st.status == Status::Persistent {
                // lazy="noload" populates the attribute (empty) when the row is loaded
                items.push((V::str(r.name), if r.uselist { V::list(vec![]) } else { V::None }));
            }
        }
        for (k, v) in st.extra.iter() {
            items.push((V::str(k), v.clone()));
        }
        V::dict_from(items)
    }

    pub fn get_attr_sync(&self, name: &str) -> R {
        if name == "__dict__" {
            return self.instance_dict();
        }
        let mut st = self.st.lock();
        if let Some(ri) = self.desc.rel_index(name) {
            return self.rel_get(&mut st, ri);
        }
        if let Some(i) = self.desc.col_index(name) {
            return match &st.vals[i] {
                V::Unbound => {
                    if st.status == Status::Transient || st.status == Status::Pending {
                        Ok(V::None)
                    } else {
                        Err(Exc::msg(
                            &MISSING_GREENLET,
                            format!(
                                "attribute '{}' of {} is not loaded (expired): an async session cannot lazy-load it, call `await session.refresh(obj)`",
                                name, self.desc.name
                            ),
                        ))
                    }
                }
                v => Ok(v.clone()),
            };
        }
        if let Some((_, v)) = st.extra.iter().find(|(k, _)| k == name) {
            return Ok(v.clone());
        }
        Err(Exc::attr_error(format!("'{}' object has no attribute '{}'", self.desc.name, name)))
    }

    /// Reading an attribute: in a synchronous `Session`, what needs SQL (an expired column, a
    /// relationship not loaded yet) is loaded like SQLAlchemy's lazy loader instead of MissingGreenlet.
    pub async fn get_attr(self: &Arc<Self>, name: &str) -> R {
        match self.get_attr_sync(name) {
            Err(e) if e.isinstance(&MISSING_GREENLET) => {
                let Some(sess) = self.sess.lock().upgrade() else { return Err(e) };
                if !sess.lock().await.sync {
                    return Err(e);
                }
                Session(sess).lazy_load(self, name).await?;
                self.get_attr_sync(name)
            }
            r => r,
        }
    }

    /// A modified persistent object is held strongly by its session until the next flush.
    fn mark_dirty(self: &Arc<Self>) {
        if self.st.lock().status != Status::Persistent {
            return;
        }
        if let Some(sess) = self.sess.lock().upgrade() {
            if let Ok(mut s) = sess.try_lock() {
                if !s.strong.iter().any(|x| Arc::ptr_eq(x, self)) {
                    s.strong.push(self.clone());
                }
            }
        }
    }

    pub fn set_attr(self: &Arc<Self>, name: &str, v: V) -> R<()> {
        if let Some(ri) = self.desc.rel_index(name) {
            self.mark_dirty();
            return self.rel_set(ri, v);
        }
        if self.desc.col_index(name).is_some() {
            self.mark_dirty();
        }
        let mut st = self.st.lock();
        if let Some(i) = self.desc.col_index(name) {
            st.vals[i] = v;
            st.modified[i] = true;
            return Ok(());
        }
        if let Some(slot) = st.extra.iter_mut().find(|(k, _)| k == name) {
            slot.1 = v;
        } else {
            st.extra.push((name.to_string(), v));
        }
        Ok(())
    }

    fn pk(&self) -> V {
        self.desc.pk_of(&self.st.lock().vals)
    }

    fn load_row(&self, row: &PgRow, offset: usize, tz: Tz) -> R<()> {
        let mut vals = Vec::with_capacity(self.desc.cols.len());
        for i in 0..self.desc.cols.len() {
            vals.push(if self.desc.cols[i].deferred { V::Unbound } else { map_col(&self.desc.cols[i], decode(row, offset + i, tz)?) });
        }
        let mut st = self.st.lock();
        st.committed = vals.clone();
        st.vals = vals;
        st.modified = vec![false; self.desc.cols.len()];
        st.status = Status::Persistent;
        st.expired = false;
        Ok(())
    }
}

/// `Model(**kwargs)`: SQLAlchemy's default constructor sets the given attributes.
pub fn construct(desc: &'static ModelDesc, kwargs: Vec<(String, V)>) -> R {
    let obj = ObjCell::transient(desc);
    for (k, v) in kwargs {
        if desc.col_index(&k).is_none() && desc.rel_index(&k).is_none() {
            return Err(Exc::type_error(format!("'{}' is an invalid keyword argument for {}", k, desc.name)));
        }
        obj.set_attr(&k, v)?;
    }
    Ok(V::Obj(obj))
}

// ---------------------------------------------------------------- SQL expressions

#[derive(Clone)]
pub enum SelCol {
    Entity(&'static ModelDesc),
    /// `select(aliased(M))`
    AEntity(Arc<Alias>),
    Expr(Sql),
}

/// `aliased(Model)`: the model under another name in the FROM list (`properties AS properties_1`,
/// numbered per statement like SQLAlchemy's anonymous aliases)
pub struct Alias {
    pub model: &'static ModelDesc,
    pub id: usize,
}

static ALIAS_IDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// a string in `order_by`/`group_by`: a label (or column name) of the columns clause, as SQLAlchemy
/// resolves it; anything else is its CompileError
fn label_ref(s: &Select, a: &V, clause: &str) -> R<Sql> {
    let in_cols = |name: &str| s.cols.iter().any(|c| matches!(c, SelCol::Expr(Sql::Label(_, l)) if l == name));
    // a labelled expression: GROUP BY repeats the expression; ORDER BY names the label when the columns
    // clause has it (SQLAlchemy's compiler), else the expression; `desc("name")` is a label reference
    let resolve = |e: &Sql| -> R<Sql> {
        Ok(match e {
            Sql::Label(_, l) if clause == "ORDER BY" && in_cols(l) => Sql::Text(l.clone()),
            Sql::Label(x, _) => (**x).clone(),
            Sql::LabelRef(n) => label_ref(s, &V::str(n.as_str()), clause)?,
            other => other.clone(),
        })
    };
    if let V::Sql(x) = a {
        return match &**x {
            Sql::Order(e, d, nl) => Ok(Sql::Order(Box::new(resolve(e)?), *d, *nl)),
            e => resolve(e),
        };
    }
    let V::Str(name) = a else { return Ok(to_sql(a, None)) };
    let known = s.cols.iter().any(|c| match c {
        SelCol::Expr(Sql::Label(_, l)) => l == &**name,
        SelCol::Expr(Sql::Col(m, i)) => m.cols[*i].name == &**name,
        _ => false,
    });
    if !known {
        // raised when the statement is compiled, like SQLAlchemy
        return Ok(Sql::BadLabel(format!(
            "Can't resolve label reference for {clause} / GROUP BY / DISTINCT etc. Textual SQL expression '{name}' should be explicitly declared as text('{name}')"
        )));
    }
    Ok(Sql::Text(name.to_string()))
}

/// `join(Target)` without ON: the single foreign-key path between Target and an entity already in the
/// statement (SQLAlchemy's rule, with its errors when there is none or more than one)
fn infer_onclause(s: &Select, target: &'static ModelDesc) -> R<Sql> {
    let mut lefts: Vec<&'static ModelDesc> = Vec::new();
    let mut push = |m: &'static ModelDesc| {
        if !lefts.iter().any(|x| std::ptr::eq(*x, m)) {
            lefts.push(m)
        }
    };
    for c in &s.cols {
        match c {
            SelCol::Entity(m) => push(m),
            SelCol::Expr(e) => {
                let mut ts = Vec::new();
                tables_in(e, &mut ts);
                ts.into_iter().for_each(&mut push);
            }
            SelCol::AEntity(_) => {}
        }
    }
    s.from.iter().for_each(|m| push(m));
    s.joins.iter().filter(|j| j.3.is_none()).for_each(|j| push(j.0));
    let paths = |left: &'static ModelDesc| -> Vec<Sql> {
        let mut out = Vec::new();
        for (ci, t, c) in target.fks {
            if *t == left.table {
                if let Some(li) = left.col_index(c) {
                    out.push(Sql::Bin("=", Box::new(Sql::Col(left, li)), Box::new(Sql::Col(target, *ci))));
                }
            }
        }
        for (ci, t, c) in left.fks {
            if *t == target.table && !std::ptr::eq(left, target) {
                if let Some(ti) = target.col_index(c) {
                    out.push(Sql::Bin("=", Box::new(Sql::Col(left, *ci)), Box::new(Sql::Col(target, ti))));
                }
            }
        }
        out
    };
    let found: Vec<(&'static ModelDesc, Vec<Sql>)> = lefts.iter().map(|l| (*l, paths(l))).filter(|(_, p)| !p.is_empty()).collect();
    match found.len() {
        0 => Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
            "Don't know how to join to <Mapper at 0x0; {}>. Please use the .select_from() method to establish an explicit left side, as well as providing an explicit ON clause if not present already to help resolve the ambiguity.",
            target.name
        ))),
        1 => {
            let (l, mut p) = found.into_iter().next().unwrap();
            if p.len() > 1 {
                return Err(Exc::msg(&ARGUMENT_ERROR, format!(
                    "Can't determine join between '{}' and '{}'; tables have more than one foreign key constraint relationship between them. Please specify the 'onclause' of this join explicitly.",
                    l.table, target.table
                )));
            }
            Ok(p.remove(0))
        }
        _ => Err(Exc::msg(&INVALID_REQUEST_ERROR, "Can't determine which FROM clause to join from, there are multiple FROMS which can join to this entity. Please use the .select_from() method to establish an explicit left side, as well as providing an explicit ON clause if not present already to help resolve the ambiguity.")),
    }
}

/// `sqlalchemy.orm.aliased(Model)`
pub fn aliased(v: &V) -> R {
    let model = model_of(v)?;
    Ok(sql(Sql::Alias(Arc::new(Alias { model, id: ALIAS_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }))))
}

#[derive(Clone, Default)]
pub struct Select {
    pub cols: Vec<SelCol>,
    pub from: Vec<&'static ModelDesc>,
    pub joins: Vec<(&'static ModelDesc, Option<Sql>, bool, Option<Arc<Alias>>)>,
    pub wheres: Vec<Sql>,
    pub order: Vec<Sql>,
    pub group: Vec<Sql>,
    pub having: Vec<Sql>,
    pub limit: Option<Sql>,
    pub offset: Option<Sql>,
    pub distinct: bool,
    /// `distinct(*cols)`: PostgreSQL DISTINCT ON
    pub distinct_on: Vec<Sql>,
    /// `with_for_update(...)`: the locking clause, e.g. " FOR UPDATE SKIP LOCKED"
    pub for_update: Option<String>,
    pub loads: Vec<LoadChain>,
    /// subqueries in the FROM list (`select_from(sq)` or referenced through `sq.c`)
    pub from_subs: Vec<Arc<Subq>>,
    /// `join(sq, on)`: (rendered before `joins[pos]`, the subquery, ON, outer)
    pub join_subs: Vec<(usize, Arc<Subq>, Sql, bool)>,
}

#[derive(Clone)]
pub struct Ins {
    pub m: &'static ModelDesc,
    /// one entry per VALUES row: (column, value)
    pub rows: Vec<Vec<(usize, Sql)>>,
    pub conflict: Option<Conflict>,
    pub returning: Vec<SelCol>,
    /// `sqlalchemy.dialects.postgresql.insert` (ON CONFLICT, `excluded`)
    pub pg: bool,
}

#[derive(Clone)]
pub enum Conflict {
    /// the conflict target, rendered: ` (a, b)`, ` ON CONSTRAINT name` or empty
    Nothing(String),
    Update { target: String, set: Vec<(usize, Sql)>, wheres: Vec<Sql> },
}

#[derive(Clone)]
pub enum Sql {
    Col(&'static ModelDesc, usize),
    /// `x.in_(select(...))`
    InSelect(Box<Sql>, Box<Select>, bool),
    /// an unresolvable string label in ORDER BY / GROUP BY: CompileError when rendered
    BadLabel(String),
    /// `desc("name")` / `asc("name")`: a label of the columns clause, resolved by order_by/group_by
    LabelRef(String),
    /// a column of an `aliased()` model
    ACol(Arc<Alias>, usize),
    /// the `aliased()` entity itself
    Alias(Arc<Alias>),
    /// a bound value, with its identity: the same BindParameter rendered twice is one parameter
    /// (`func.date_trunc("hour", col)` built once, in the columns and in GROUP BY), 0 = none
    Param(V, Option<ColTy>, u64),
    Null,
    Bin(&'static str, Box<Sql>, Box<Sql>),
    Bool(&'static str, Vec<Sql>),
    Not(Box<Sql>),
    IsNull(Box<Sql>, bool),
    In(Box<Sql>, Vec<Sql>, bool),
    Like(Box<Sql>, &'static str, Box<Sql>, bool),
    Func(String, Vec<Sql>),
    Order(Box<Sql>, bool, Option<&'static str>),
    Label(Box<Sql>, String),
    Distinct(Box<Sql>),
    Exists(Box<Select>),
    Scalar(Box<Select>),
    Select(Box<Select>),
    /// (model, where, sets, synchronize the loaded objects)
    Update(&'static ModelDesc, Vec<Sql>, Vec<(usize, Sql)>, bool, Vec<SelCol>),
    /// `insert(Model)` (core, or the postgresql dialect's with ON CONFLICT)
    Insert(Box<Ins>),
    /// `stmt.excluded` of a postgresql insert, and its columns
    Excluded(&'static ModelDesc),
    ExCol(&'static ModelDesc, usize),
    /// `case((cond, value), ..., else_=value)`
    Case(Vec<(Sql, Sql)>, Option<Box<Sql>>),
    /// (model, WHERE, synchronize_session: objects matched in the session are marked deleted)
    Delete(&'static ModelDesc, Vec<Sql>, bool),
    Text(String),
    /// `text("... :name ...").bindparams(name=value)`: the text cut at its bound parameters
    TextBound(Vec<Sql>),
    /// `Model.relationship` (class attribute): loader options and `join()` only
    Rel(&'static ModelDesc, usize),
    /// `selectinload(...)`, `noload(...)`, ... (a `.options()` argument)
    Load(LoadChain),
    /// `select(...).subquery()` (used in a FROM, its columns through `.c`)
    Subquery(Arc<Subq>),
    /// `sq.c` (the columns namespace of a subquery)
    SubColumns(Arc<Subq>),
    /// `sq.c.name`
    SubCol(Arc<Subq>, String),
    /// aggregate `FILTER (WHERE ...)`
    Filter(Box<Sql>, Box<Sql>),
    /// `cast(expr, type)`
    Cast(Box<Sql>, &'static str),
    /// `extract(field, expr)`
    Extract(String, Box<Sql>),
}

pub struct Subq {
    pub sel: Select,
    pub name: String,
}

static ANON: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// The loader chains of an `options=[...]` argument.
pub fn load_chains(v: &V) -> R<Vec<LoadChain>> {
    let mut out = Vec::new();
    for o in ops::iter(v)? {
        match &o {
            V::Sql(x) => match &**x {
                Sql::Load(c) => out.push(c.clone()),
                _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
            },
            _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
        }
    }
    Ok(out)
}

/// `selectinload(A.b)` / `joinedload` / `subqueryload` (loaded like selectin), `noload`, `lazyload`.
pub fn loader(kind: &str, attr: &V) -> R {
    loader_chain(vec![], kind, attr)
}

fn loader_chain(mut chain: LoadChain, kind: &str, attr: &V) -> R {
    let lazy = match kind {
        "selectinload" | "joinedload" | "subqueryload" | "immediateload" => Lazy::Selectin,
        "noload" => Lazy::NoLoad,
        "lazyload" => Lazy::Select,
        _ => return Err(Exc::attr_error(format!("'Load' object has no attribute '{kind}'"))),
    };
    match attr {
        V::Sql(x) => match &**x {
            Sql::Rel(m, ri) => {
                if let Some((_, pm, pi)) = chain.last() {
                    if !std::ptr::eq(pm.rels[*pi].target, *m) {
                        return Err(Exc::msg(&ARGUMENT_ERROR, format!("{}.{} does not link from the previous path element", m.name, m.rels[*ri].name)));
                    }
                }
                chain.push((lazy, m, *ri));
                Ok(sql(Sql::Load(chain)))
            }
            _ => Err(Exc::msg(&ARGUMENT_ERROR, "a loader option needs a relationship attribute")),
        },
        _ => Err(Exc::msg(&ARGUMENT_ERROR, "a loader option needs a relationship attribute (strings are not supported)")),
    }
}

fn to_sql(v: &V, hint: Option<ColTy>) -> Sql {
    match v {
        V::Col(m, i) => Sql::Col(m, *i),
        V::Sql(s) => (**s).clone(),
        V::Native(n) if matches!(&**n, Native::Query(..)) => match &**n {
            Native::Query(_, sel) => to_sql(sel, hint),
            _ => unreachable!(),
        },
        // a value for a JSON column: None is JSON null (SQLAlchemy's JSON type, none_as_null=False)
        V::None if hint == Some(ColTy::Json) => Sql::Param(V::None, hint, bind_id()),
        V::None => Sql::Null,
        other => Sql::Param(other.clone(), hint, bind_id()),
    }
}

/// a new BindParameter's identity
fn bind_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn hint_of(s: &Sql) -> Option<ColTy> {
    match s {
        Sql::Col(m, i) => Some(m.cols[*i].ty),
        Sql::ACol(a, i) => Some(a.model.cols[*i].ty),
        Sql::ExCol(m, i) => Some(m.cols[*i].ty),
        Sql::Case(whens, els) => whens.iter().find_map(|(_, v)| hint_of(v)).or_else(|| els.as_deref().and_then(hint_of)),
        Sql::Label(e, _) | Sql::Order(e, _, _) | Sql::Filter(e, _) => hint_of(e),
        Sql::Func(name, args) if matches!(name.as_str(), "max" | "min" | "coalesce" | "sum") => args.first().and_then(hint_of),
        Sql::Func(name, _) if name == "count" => Some(ColTy::BigInt),
        // arithmetic keeps the numeric type (SQLAlchemy's type affinity): a float type wins, then Numeric
        Sql::Bin(op, a, b) if matches!(*op, "+" | "-" | "*" | "/") => {
            let (x, y) = (hint_of(a), hint_of(b));
            [ColTy::NumFloat, ColTy::Float, ColTy::Numeric].into_iter().find(|t| x == Some(*t) || y == Some(*t)).or(x).or(y)
        }
        _ => None,
    }
}

/// A database error; with PY2AXUM_SQL_DEBUG set, the statement is logged with it.
fn sql_error(e: sqlx::Error, sql: &str) -> Exc {
    if std::env::var_os("PY2AXUM_SQL_DEBUG").is_some() {
        eprintln!("DEBUG:py2axum:SQL failed: {e}\n  {sql}");
    }
    Exc::from(e)
}

/// `sqlalchemy.text("...")`
pub fn text(v: &V) -> R {
    Ok(sql(Sql::Text(ops::str_(v)?)))
}

/// TextClause's `:name` parameters (`(?<![:\w\\]):(\w+)(?!:)`, `\:` for a literal colon) -> `$n`
fn text_binds(q: &str, params: Option<&V>) -> R<(String, Vec<V>)> {
    static P: OnceLock<fancy_regex::Regex> = OnceLock::new();
    let re = P.get_or_init(|| fancy_regex::Regex::new(r"(?<![:\w\\]):(\w+)(?!:)").unwrap());
    let mut names: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut last = 0;
    for m in re.captures_iter(q) {
        let m = m.map_err(|e| Exc::runtime(e.to_string()))?;
        let whole = m.get(0).unwrap();
        let name = m.get(1).unwrap().as_str().to_string();
        out += &q[last..whole.start()];
        let i = match names.iter().position(|n| *n == name) {
            Some(i) => i,
            None => {
                names.push(name);
                names.len() - 1
            }
        };
        out += &format!("\u{0}{}\u{0}", i);
        last = whole.end();
    }
    out += &q[last..];
    let out = out.replace("\\:", ":");
    let mut vals = Vec::new();
    for n in &names {
        let v = match params {
            Some(V::Dict(d)) => d.lock().get(&Key::Str(Arc::from(n.as_str()))).map(|(_, v)| v.clone()),
            _ => None,
        };
        vals.push(v.ok_or_else(|| Exc::msg(&STATEMENT_ERROR, format!("(sqlalchemy.exc.InvalidRequestError) A value is required for bind parameter '{n}'")))?);
    }
    Ok((out, vals))
}

/// `text(q).bindparams(name=value, ...)`: each `:name` given becomes a bound value, the others stay as
/// written (SQLAlchemy then asks for them at execution)
fn text_bindparams(q: &str, kwargs: &[(String, V)]) -> R {
    static P: OnceLock<fancy_regex::Regex> = OnceLock::new();
    let re = P.get_or_init(|| fancy_regex::Regex::new(r"(?<![:\w\\]):(\w+)(?!:)").unwrap());
    let mut parts = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut last = 0;
    for m in re.captures_iter(q) {
        let m = m.map_err(|e| Exc::runtime(e.to_string()))?;
        let (whole, name) = (m.get(0).unwrap(), m.get(1).unwrap().as_str());
        if let Some((_, v)) = kwargs.iter().find(|(k, _)| k == name) {
            parts.push(Sql::Text(q[last..whole.start()].to_string()));
            parts.push(Sql::Param(v.clone(), None, 0));
            last = whole.end();
            seen.push(name);
        }
    }
    parts.push(Sql::Text(q[last..].to_string()));
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !seen.contains(&k.as_str())) {
        return Err(Exc::msg(&ARGUMENT_ERROR, format!("This text() construct doesn't define a bound parameter named '{k}'")));
    }
    Ok(sql(Sql::TextBound(parts)))
}

/// `sqlalchemy.desc(x)` / `asc(x)`: a string is a label reference (resolved by `order_by`)
pub fn order_fn(a: &V, desc: bool) -> R {
    match a {
        V::Str(n) => Ok(sql(Sql::Order(Box::new(Sql::LabelRef(n.to_string())), desc, None))),
        _ => sql_method(a, if desc { "desc" } else { "asc" }, vec![], vec![]),
    }
}

/// `sqlalchemy.tuple_(a, b, ...)`
pub fn tuple_(args: Vec<V>) -> R {
    Ok(sql(Sql::Func(String::new(), args.iter().map(|a| to_sql(a, None)).collect())))
}

pub fn sql(s: Sql) -> V {
    V::Sql(Arc::new(s))
}

pub fn sql_cmp(a: &V, op: &'static str, b: &V) -> R {
    let la = to_sql(a, None);
    let lb = to_sql(b, hint_of(&la));
    let la = if let Sql::Param(v, None, id) = la { Sql::Param(v, hint_of(&lb), id) } else { la };
    Ok(sql(match (op, &la, &lb) {
        ("=", _, Sql::Null) => Sql::IsNull(Box::new(la), false),
        ("!=", _, Sql::Null) => Sql::IsNull(Box::new(la), true),
        ("=", Sql::Null, _) => Sql::IsNull(Box::new(lb), false),
        ("!=", Sql::Null, _) => Sql::IsNull(Box::new(lb), true),
        _ => Sql::Bin(if op == "!=" { "<>" } else { op }, Box::new(la), Box::new(lb)),
    }))
}

pub fn sql_binop(a: &V, op: &'static str, b: &V) -> R {
    let la = to_sql(a, None);
    let lb = to_sql(b, hint_of(&la));
    Ok(sql(Sql::Bin(op, Box::new(la), Box::new(lb))))
}

/// `a / b` in SQL (SQLAlchemy 2.0 true division): integer by integer is `a / CAST(b AS NUMERIC)`
pub fn sql_div(a: &V, b: &V) -> R {
    let la = to_sql(a, None);
    let lb = to_sql(b, hint_of(&la));
    let int = |e: &Sql, v: &V| matches!(v, V::Int(_)) || matches!(hint_of(e), Some(ColTy::Int | ColTy::BigInt | ColTy::SmallInt));
    let rhs = if int(&la, a) && int(&lb, b) { Sql::Cast(Box::new(lb), "NUMERIC") } else { lb };
    Ok(sql(Sql::Bin("/", Box::new(la), Box::new(rhs))))
}

pub fn sql_bool(a: &V, op: &'static str, b: &V) -> R {
    Ok(sql(Sql::Bool(op, vec![to_sql(a, None), to_sql(b, None)])))
}

pub fn sql_not(a: &V) -> R {
    Ok(sql(Sql::Not(Box::new(to_sql(a, None)))))
}

pub fn sql_text(v: &V) -> R<String> {
    let (text, _) = render_expr_standalone(&to_sql(v, None))?;
    Ok(text)
}

fn expect_sql(v: &V) -> R<Sql> {
    match v {
        V::Col(..) | V::Sql(_) => Ok(to_sql(v, None)),
        V::Native(n) if matches!(&**n, Native::Query(..)) => Ok(to_sql(v, None)),
        other => Err(Exc::type_error(format!("expected a SQL expression, got {}", other.type_name()))),
    }
}

/// `and_(*conds)` / `or_(*conds)`
pub fn and_or(op: &'static str, items: Vec<V>) -> R {
    Ok(sql(Sql::Bool(op, items.iter().map(|v| to_sql(v, None)).collect())))
}

pub fn not_(v: &V) -> R {
    sql_not(v)
}

/// `func.<name>(*args)`
pub fn func(name: &str, args: Vec<V>) -> R {
    if name == "extract" && args.len() == 2 {
        // func.extract(field, expr) is SQLAlchemy's Extract construct
        return extract(&args[0], &args[1]);
    }
    Ok(sql(Sql::Func(name.to_string(), args.iter().map(|a| to_sql(a, None)).collect())))
}

/// `select(*entities_or_columns)`
pub fn select(items: Vec<V>) -> R {
    let mut s = Select::default();
    for it in items {
        match it {
            V::Class(c) => match c.kind {
                ClassKind::Model(m) => s.cols.push(SelCol::Entity(m)),
                _ => return Err(Exc::type_error(format!("cannot select {}", c.name))),
            },
            V::Sql(x) if matches!(&*x, Sql::Alias(_)) => {
                let Sql::Alias(a) = &*x else { unreachable!() };
                s.cols.push(SelCol::AEntity(a.clone()));
            }
            other => s.cols.push(SelCol::Expr(expect_sql(&other)?)),
        }
    }
    Ok(sql(Sql::Select(Box::new(s))))
}

fn model_of(v: &V) -> R<&'static ModelDesc> {
    match v {
        V::Class(c) => match c.kind {
            ClassKind::Model(m) => Ok(m),
            _ => Err(Exc::type_error(format!("{} is not a mapped class", c.name))),
        },
        other => Err(Exc::type_error(format!("expected a mapped class, got {}", other.type_name()))),
    }
}

/// `async_sessionmaker(bind=engine, expire_on_commit=, autoflush=, autocommit=False, class_=AsyncSession)`
/// called at run time (the process pool: one database per binary)
pub fn sessionmaker(args: &[V], kwargs: &[(String, V)], sync: bool) -> R {
    let (mut expire, mut autoflush) = (true, true);
    if args.len() > 1 {
        return Err(Exc::type_error("py2axum: async_sessionmaker() takes the engine and keyword options"));
    }
    for (k, v) in kwargs {
        match k.as_str() {
            "bind" => {}
            "expire_on_commit" => expire = ops::truthy(v)?,
            "autoflush" => autoflush = ops::truthy(v)?,
            "autocommit" if !ops::truthy(v)? => {}
            "class_" => {}
            other => return Err(Exc::type_error(format!("py2axum: async_sessionmaker({other}=) is not supported"))),
        }
    }
    Ok(V::native(Native::Maker(expire, autoflush, sync)))
}

/// `insert(Model)`; `pg`: the postgresql dialect's (on_conflict_do_update/nothing)
pub fn insert(m: &V, pg: bool) -> R {
    Ok(sql(Sql::Insert(Box::new(Ins { m: model_of(m)?, rows: vec![], conflict: None, returning: vec![], pg }))))
}

/// `case((cond, value), ..., else_=value)` / `case({key: value}, value=expr, else_=...)`
pub fn case(args: &[V], kwargs: &[(String, V)]) -> R {
    let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "else_" | "value")) {
        return Err(Exc::type_error(format!("case() got an unexpected keyword argument '{k}'")));
    }
    let mut whens = Vec::new();
    match (args, kw("value")) {
        ([V::Dict(d)], Some(value)) => {
            let base = to_sql(&value, None);
            for (_, (k, v)) in d.lock().iter() {
                whens.push((Sql::Bin("=", Box::new(base.clone()), Box::new(to_sql(k, hint_of(&base)))), to_sql(v, None)));
            }
        }
        (_, None) => {
            for a in args {
                let pair = match a {
                    V::Tuple(t) if t.len() == 2 => t.clone(),
                    _ => return Err(Exc::msg(&ARGUMENT_ERROR, "py2axum: case() takes (condition, value) tuples")),
                };
                whens.push((to_sql(&pair[0], None), to_sql(&pair[1], None)));
            }
        }
        _ => return Err(Exc::type_error("py2axum: case(value=) takes a dict of whens")),
    }
    if whens.is_empty() {
        return Err(Exc::msg(&ARGUMENT_ERROR, "case() requires at least one WHEN"));
    }
    Ok(sql(Sql::Case(whens, kw("else_").map(|e| Box::new(to_sql(&e, None))))))
}

/// `literal(value)`: a bound parameter
pub fn literal(v: &V) -> R {
    Ok(sql(to_sql(v, None)))
}

pub fn update(m: &V) -> R {
    Ok(sql(Sql::Update(model_of(m)?, vec![], vec![], true, vec![])))
}

pub fn delete(m: &V) -> R {
    Ok(sql(Sql::Delete(model_of(m)?, vec![], true)))
}

pub fn exists(v: &V) -> R {
    match v {
        V::Sql(s) => match &**s {
            Sql::Select(sel) => Ok(sql(Sql::Exists(sel.clone()))),
            _ => Err(Exc::type_error("exists() needs a select()")),
        },
        _ => Err(Exc::type_error("exists() needs a select()")),
    }
}

/// Methods on columns, expressions and statements.
pub fn sql_method(recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let base = expect_sql(recv)?;
    // a Query given as an argument (`col.in_(session.query(...))`) is its select()
    let args: Vec<V> = args
        .into_iter()
        .map(|a| match &a {
            V::Native(n) => match &**n {
                Native::Query(_, sel) => sel.clone(),
                _ => a,
            },
            _ => a,
        })
        .collect();
    if let Sql::Load(c) = &base {
        let a = args.first().cloned().ok_or_else(|| Exc::type_error(format!("{name}() takes one argument")))?;
        return loader_chain(c.clone(), name, &a);
    }
    #[allow(clippy::single_match)]
    match name {
        "bindparams" => {
            let Sql::Text(q) = &base else {
                return Err(Exc::type_error("py2axum: bindparams() is supported once, on a text() construct"));
            };
            if !args.is_empty() {
                return Err(Exc::type_error("py2axum: bindparams() takes keyword values (bindparam() objects are not supported)"));
            }
            return text_bindparams(q, &kwargs);
        }
        _ => {}
    }
    let one = |args: &Vec<V>| -> R<V> {
        args.first().cloned().ok_or_else(|| Exc::type_error(format!("{name}() takes one argument")))
    };
    if let Sql::Rel(m, ri) = &base {
        match name {
            "has" | "any" => return rel_exists(m, *ri, name, &args, &kwargs),
            _ => {}
        }
    }
    // keyword arguments: only those handled below, never ignored
    let allowed: &[&str] = match name {
        "filter_by" | "values" => &["*"],
        "join" | "outerjoin" | "join_from" => &["isouter", "full"],
        "with_for_update" => &["skip_locked", "nowait", "read", "key_share", "of"],
        "execution_options" => &["synchronize_session"],
        "on_conflict_do_update" => &["index_elements", "set_", "where", "constraint"],
        "on_conflict_do_nothing" => &["index_elements", "constraint"],
        _ => &[],
    };
    if allowed != ["*"] {
        if let Some((k, _)) = kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
            return Err(Exc::type_error(format!("py2axum: {name}({k}=) is not supported")));
        }
    }
    let kw = |k: &str| kwargs.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    if let Sql::Select(sel) = &base {
        let mut s = (**sel).clone();
        match name {
            "where" | "filter" => {
                for a in &args {
                    s.wheres.push(to_sql(a, None));
                }
            }
            "filter_by" => {
                // SQLAlchemy filters on the last joined entity, else the first selected one
                let ent = s.joins.last().map(|j| j.0).or_else(|| s.cols.iter().find_map(|c| if let SelCol::Entity(m) = c { Some(*m) } else { None }));
                let m = ent.ok_or_else(|| Exc::type_error("filter_by() needs an entity"))?;
                for (k, v) in &kwargs {
                    let i = m.col_index(k).ok_or_else(|| Exc::attr_error(format!("{} has no column {k}", m.name)))?;
                    if let V::Sql(x) = sql_cmp(&V::Col(m, i), "=", v)? {
                        s.wheres.push((*x).clone());
                    }
                }
            }
            "order_by" => {
                for a in &args {
                    if !a.is_none() {
                        s.order.push(label_ref(&s, a, "ORDER BY")?);
                    }
                }
            }
            "group_by" => {
                for a in &args {
                    s.group.push(label_ref(&s, a, "GROUP BY")?);
                }
            }
            "having" => {
                for a in &args {
                    s.having.push(to_sql(a, None));
                }
            }
            "limit" => s.limit = Some(to_sql(&one(&args)?, Some(ColTy::BigInt))),
            "offset" => s.offset = Some(to_sql(&one(&args)?, Some(ColTy::BigInt))),
            "select_from" => {
                for a in &args {
                    if let V::Sql(x) = a {
                        if let Sql::Subquery(q) = &**x {
                            s.from_subs.push(q.clone());
                            continue;
                        }
                    }
                    s.from.push(model_of(a)?);
                }
            }
            "join" | "outerjoin" | "join_from" => {
                if kw("full").map(|v| ops::truthy(&v).unwrap_or(false)).unwrap_or(false) {
                    return Err(Exc::type_error("py2axum: FULL OUTER JOIN is not supported"));
                }
                let outer = name == "outerjoin" || kw("isouter").map(|v| ops::truthy(&v).unwrap_or(false)).unwrap_or(false);
                // join_from(left, right, on): the left side is already in the FROM list
                let args = if name == "join_from" {
                    if args.len() < 2 {
                        return Err(Exc::type_error("join_from() takes the left and right entities"));
                    }
                    let left = model_of(&args[0])?;
                    if !s.from.iter().any(|m| std::ptr::eq(*m, left)) {
                        s.from.push(left);
                    }
                    args[1..].to_vec()
                } else {
                    args.clone()
                };
                let a = one(&args)?;
                if let V::Sql(x) = &a {
                    if let Sql::Rel(m, ri) = &**x {
                        // join(A.rel): ON the relationship's foreign key
                        let rel = &m.rels[*ri];
                        let on = Sql::Bin("=", Box::new(Sql::Col(m, rel.local)), Box::new(Sql::Col(rel.target, rel.remote)));
                        s.joins.push((rel.target, Some(on), outer, None));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                let on = args.get(1).map(|o| to_sql(o, None));
                if let V::Sql(x) = &a {
                    if let Sql::Subquery(q) = &**x {
                        let on = on.ok_or_else(|| Exc::type_error("py2axum: join(subquery) needs an explicit ON clause"))?;
                        s.join_subs.push((s.joins.len(), q.clone(), on, outer));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                if let V::Sql(x) = &a {
                    if let Sql::Alias(al) = &**x {
                        s.joins.push((al.model, on, outer, Some(al.clone())));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                let target = model_of(&a)?;
                let on = match on {
                    Some(on) => Some(on),
                    None => Some(infer_onclause(&s, target)?),
                };
                s.joins.push((target, on, outer, None));
            }
            "distinct" => {
                if args.is_empty() {
                    s.distinct = true;
                } else {
                    s.distinct_on = args.iter().map(|a| to_sql(a, None)).collect();
                }
            }
            "with_for_update" => {
                if kw("of").is_some() {
                    return Err(Exc::type_error("py2axum: with_for_update(of=) is not supported"));
                }
                let t = |k: &str| kw(k).map(|v| ops::truthy(&v).unwrap_or(false)).unwrap_or(false);
                let mut c = match (t("read"), t("key_share")) {
                    (true, true) => " FOR KEY SHARE",
                    (true, false) => " FOR SHARE",
                    (false, true) => " FOR NO KEY UPDATE",
                    (false, false) => " FOR UPDATE",
                }
                .to_string();
                if t("nowait") {
                    c += " NOWAIT";
                } else if t("skip_locked") {
                    c += " SKIP LOCKED";
                }
                s.for_update = Some(c);
            }
            "options" => {
                for a in &args {
                    match a {
                        V::Sql(x) => match &**x {
                            Sql::Load(c) => s.loads.push(c.clone()),
                            _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
                        },
                        _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
                    }
                }
            }
            "execution_options" => return Err(Exc::type_error("py2axum: execution_options() on a SELECT is not supported")),
            "scalar_subquery" => return Ok(sql(Sql::Scalar(Box::new(s)))),
            "with_only_columns" => {
                let mut cols = Vec::new();
                for a in &args {
                    match a {
                        V::Class(c) => match c.kind {
                            ClassKind::Model(m) => cols.push(SelCol::Entity(m)),
                            _ => return Err(Exc::type_error(format!("cannot select {}", c.name))),
                        },
                        other => cols.push(SelCol::Expr(expect_sql(other)?)),
                    }
                }
                // the FROM list is kept: SQLAlchemy keeps the original froms (maintain_column_froms=False
                // still correlates on the former entities)
                for c in &s.cols {
                    if let SelCol::Entity(m) = c {
                        if !s.from.iter().any(|x| std::ptr::eq(*x, *m)) {
                            s.from.push(m);
                        }
                    }
                }
                s.cols = cols;
                return Ok(sql(Sql::Select(Box::new(s))));
            }
            "subquery" | "alias" => {
                let name = match args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "name").map(|(_, v)| v)) {
                    Some(n) if !n.is_none() => ops::str_(n)?,
                    _ => format!("anon_{}", ANON.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
                };
                return Ok(sql(Sql::Subquery(Arc::new(Subq { sel: s, name }))));
            }
            "exists" => return Ok(sql(Sql::Exists(Box::new(s)))),
            _ => return Err(Exc::attr_error(format!("'Select' object has no attribute '{name}'"))),
        }
        return Ok(sql(Sql::Select(Box::new(s))));
    }
    if let Sql::Insert(ins) = &base {
        let mut ins = ins.clone();
        let m = ins.m;
        let col_of = |k: &V| -> R<usize> {
            match k {
                V::Str(n) => m.col_index(n).ok_or_else(|| Exc::msg(&COMPILE_ERROR, format!("Unconsumed column names: {n}"))),
                V::Col(cm, ci) if std::ptr::eq(*cm, m) => Ok(*ci),
                _ => Err(Exc::type_error("py2axum: insert keys must be column names or columns")),
            }
        };
        let row_of = |d: &V| -> R<Vec<(usize, Sql)>> {
            let V::Dict(d) = d else { return Err(Exc::type_error("py2axum: insert().values() takes dicts or keywords")) };
            let mut row = Vec::new();
            for (_, (k, v)) in d.lock().iter() {
                let i = col_of(k)?;
                row.retain(|(j, _): &(usize, Sql)| *j != i);
                row.push((i, to_sql(v, Some(m.cols[i].ty))));
            }
            Ok(row)
        };
        let names_of = |v: &V| -> R<Vec<String>> {
            ops::iter(v)?.iter().map(|x| match x {
                V::Str(s) => Ok(s.to_string()),
                V::Col(cm, ci) => Ok(cm.cols[*ci].name.to_string()),
                _ => Err(Exc::type_error("py2axum: index_elements are column names or columns")),
            }).collect()
        };
        let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        match name {
            "values" => {
                if !kwargs.is_empty() {
                    let mut row = Vec::new();
                    for (k, v) in &kwargs {
                        let i = col_of(&V::str(k))?;
                        row.push((i, to_sql(v, Some(m.cols[i].ty))));
                    }
                    ins.rows = vec![row];
                }
                for a in &args {
                    match a {
                        V::List(l) => {
                            ins.rows = l.lock().iter().map(&row_of).collect::<R<Vec<_>>>()?;
                        }
                        d => ins.rows = vec![row_of(d)?],
                    }
                }
            }
            "on_conflict_do_update" | "on_conflict_do_nothing" if ins.pg => {
                let cols = kw("index_elements").map(|v| names_of(&v)).transpose()?.unwrap_or_default();
                let target = match kw("constraint") {
                    Some(V::None) | None => if cols.is_empty() { String::new() } else { format!(" ({})", cols.join(", ")) },
                    Some(_) if !cols.is_empty() => {
                        return Err(Exc::value_error("'constraint' and 'index_elements' are mutually exclusive"));
                    }
                    // a name SQLAlchemy renders as is (one it would quote is not supported)
                    Some(V::Str(c)) if c.chars().next().is_some_and(|f| f.is_ascii_lowercase() || f == '_')
                        && c.chars().all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_') => format!(" ON CONSTRAINT {c}"),
                    Some(_) => return Err(Exc::type_error(format!("py2axum: {name}(constraint=) takes a lowercase constraint name"))),
                };
                if name == "on_conflict_do_nothing" {
                    ins.conflict = Some(Conflict::Nothing(target));
                } else {
                    if target.is_empty() {
                        return Err(Exc::msg(&ARGUMENT_ERROR, "Either constraint or index_elements, but not both, must be specified unless DO NOTHING"));
                    }
                    let set = match kw("set_") {
                        Some(d @ V::Dict(_)) => row_of(&d)?,
                        _ => return Err(Exc::value_error("set parameter dictionary must not be empty")),
                    };
                    let wheres = kw("where").map(|w| vec![to_sql(&w, None)]).unwrap_or_default();
                    ins.conflict = Some(Conflict::Update { target, set, wheres });
                }
            }
            "returning" => {
                for a in &args {
                    ins.returning.push(match a {
                        V::Class(c) => match c.kind {
                            ClassKind::Model(mm) => SelCol::Entity(mm),
                            _ => return Err(Exc::type_error("py2axum: returning() takes mapped classes or columns")),
                        },
                        other => SelCol::Expr(to_sql(other, None)),
                    });
                }
            }
            _ => return Err(Exc::attr_error(format!("'Insert' object has no attribute '{name}'"))),
        }
        return Ok(sql(Sql::Insert(ins)));
    }
    if let Sql::Update(m, w, sets, sync, ret) = &base {
        let (mut w, mut sets, mut sync, mut ret) = (w.clone(), sets.clone(), *sync, ret.clone());
        match name {
            "where" | "filter" => w.extend(args.iter().map(|a| to_sql(a, None))),
            "values" => {
                // values(col=v) or values({"col" | Model.col: v})
                let mut pairs: Vec<(usize, V)> = Vec::new();
                for (k, v) in &kwargs {
                    let i = m.col_index(k).ok_or_else(|| Exc::attr_error(format!("{} has no column {k}", m.name)))?;
                    pairs.push((i, v.clone()));
                }
                for a in &args {
                    let V::Dict(d) = a else { return Err(Exc::type_error("py2axum: update().values() takes keywords or a dict")) };
                    for (_, (k, v)) in d.lock().iter() {
                        let i = match k {
                            V::Str(n) => m.col_index(n).ok_or_else(|| Exc::attr_error(format!("{} has no column {n}", m.name)))?,
                            V::Col(cm, ci) if std::ptr::eq(*cm, *m) => *ci,
                            _ => return Err(Exc::type_error("py2axum: update().values() keys must be column names or columns")),
                        };
                        pairs.push((i, v.clone()));
                    }
                }
                for (i, v) in pairs {
                    sets.retain(|(j, _)| *j != i);
                    sets.push((i, to_sql(&v, Some(m.cols[i].ty))));
                }
            }
            "execution_options" => match kw("synchronize_session") {
                Some(V::Bool(false)) => sync = false,
                Some(V::Str(x)) if matches!(&*x, "auto" | "evaluate" | "fetch") => {}
                _ => return Err(Exc::type_error("py2axum: execution_options() supports synchronize_session only")),
            },
            "returning" => {
                for a in &args {
                    ret.push(match a {
                        V::Class(c) => match c.kind {
                            ClassKind::Model(mm) => SelCol::Entity(mm),
                            _ => return Err(Exc::type_error("py2axum: returning() takes mapped classes or columns")),
                        },
                        other => SelCol::Expr(to_sql(other, None)),
                    });
                }
            }
            _ => return Err(Exc::attr_error(format!("'Update' object has no attribute '{name}'"))),
        }
        return Ok(sql(Sql::Update(m, w, sets, sync, ret)));
    }
    if let Sql::Delete(m, w, sync) = &base {
        let (mut w, mut sync) = (w.clone(), *sync);
        match name {
            "where" | "filter" => w.extend(args.iter().map(|a| to_sql(a, None))),
            "execution_options" => match kw("synchronize_session") {
                Some(V::Bool(false)) => sync = false,
                Some(V::Str(x)) if matches!(&*x, "auto" | "evaluate" | "fetch") => {}
                _ => return Err(Exc::type_error("py2axum: execution_options() on a DELETE supports synchronize_session=False/'auto'/'evaluate'/'fetch' only")),
            },
            _ => return Err(Exc::attr_error(format!("'Delete' object has no attribute '{name}'"))),
        }
        return Ok(sql(Sql::Delete(m, w, sync)));
    }
    let hint = hint_of(&base);
    let b = Box::new(base.clone());
    Ok(sql(match name {
        "is_" | "is_not" | "isnot" => {
            let a = one(&args)?;
            let neg = name != "is_";
            match a {
                V::None => Sql::IsNull(b, neg),
                // PostgreSQL takes no parameter after IS: SQLAlchemy renders the literal
                V::Bool(x) => Sql::Bin(if neg { "IS NOT" } else { "IS" }, b, Box::new(Sql::Text(if x { "true" } else { "false" }.into()))),
                other => Sql::Bin(if neg { "IS NOT" } else { "IS" }, b, Box::new(to_sql(&other, hint))),
            }
        }
        "in_" | "not_in" | "notin_" if matches!(&base, Sql::Func(n, _) if n.is_empty()) => {
            // tuple_(a, b).in_([(x, y), ...]): each element bound with its column's type, but without the
            // psycopg dialect's `::INTEGER` cast (SQLAlchemy renders none in a tuple IN): an int out of the
            // column's range matches nothing instead of failing
            let Sql::Func(_, cols) = &base else { unreachable!() };
            let hint_of = |c: &Sql| match hint_of(c) {
                Some(ColTy::Int | ColTy::SmallInt) => None,
                h => h,
            };
            let mut items = Vec::new();
            for t in ops::iter(&one(&args)?)? {
                let vals = ops::iter(&t)?;
                if vals.len() != cols.len() {
                    return Err(Exc::msg(&ARGUMENT_ERROR, format!("tuple_ of {} elements compared with a tuple of {}", cols.len(), vals.len())));
                }
                items.push(Sql::Func(String::new(), vals.iter().zip(cols.iter()).map(|(v, c)| to_sql(v, hint_of(c))).collect()));
            }
            Sql::In(b, items, name != "in_")
        }
        "in_" | "not_in" | "notin_" if matches!(args.first(), Some(V::Sql(x)) if matches!(&**x, Sql::Select(_) | Sql::Scalar(_))) => {
            let sel = match &args[0] {
                V::Sql(x) => match &**x {
                    Sql::Select(q) | Sql::Scalar(q) => q.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            };
            Sql::InSelect(b, sel, name != "in_")
        }
        "in_" | "not_in" | "notin_" => {
            let items = ops::iter(&one(&args)?)?.iter().map(|x| to_sql(x, hint)).collect();
            Sql::In(b, items, name != "in_")
        }
        "is_distinct_from" | "isnot_distinct_from" | "is_not_distinct_from" => {
            let op = if name == "is_distinct_from" { "IS DISTINCT FROM" } else { "IS NOT DISTINCT FROM" };
            Sql::Bin(op, b, Box::new(to_sql(&one(&args)?, hint)))
        }
        "like" | "ilike" | "not_like" | "not_ilike" => {
            let op = if name.contains("ilike") { "ILIKE" } else { "LIKE" };
            Sql::Like(b, op, Box::new(to_sql(&one(&args)?, Some(ColTy::Str))), name.starts_with("not"))
        }
        // ARRAY operators (PostgreSQL)
        "contains" | "contained_by" | "overlap" if hint == Some(ColTy::StrArray) => {
            let op = match name {
                "contains" => "@>",
                "contained_by" => "<@",
                _ => "&&",
            };
            Sql::Bin(op, b, Box::new(to_sql(&one(&args)?, hint)))
        }
        "any" if hint == Some(ColTy::StrArray) => {
            Sql::Bin("=", Box::new(to_sql(&one(&args)?, Some(ColTy::Str))), Box::new(Sql::Func("ANY".into(), vec![*b])))
        }
        // `col.op("?|")(value)`: a custom binary operator, the value typed like the column (SQLAlchemy)
        "op" => {
            let op = ops::str_(&one(&args)?)?;
            let op = super::types::intern(&op);
            let left = *b;
            return Ok(V::native(Native::Func(Arc::new(move |_cx, a: Vec<V>| {
                let left = left.clone();
                Box::pin(async move {
                    let [x] = a.as_slice() else { return Err(Exc::type_error("op(...)() takes one argument")) };
                    Ok(sql(Sql::Bin(op, Box::new(left.clone()), Box::new(to_sql(x, hint_of(&left))))))
                })
            }))));
        }
        "startswith" | "endswith" | "contains" => {
            if !matches!(hint, None | Some(ColTy::Str) | Some(ColTy::Enum(_))) {
                return Err(Exc::type_error(format!("py2axum: {name}() on a {hint:?} column is not supported (only string LIKE)")));
            }
            let p = to_sql(&one(&args)?, Some(ColTy::Str));
            let pat = match name {
                "startswith" => Sql::Bin("||", Box::new(p), Box::new(Sql::Text("'%'".into()))),
                "endswith" => Sql::Bin("||", Box::new(Sql::Text("'%'".into())), Box::new(p)),
                _ => Sql::Bin("||", Box::new(Sql::Bin("||", Box::new(Sql::Text("'%'".into())), Box::new(p))), Box::new(Sql::Text("'%'".into()))),
            };
            Sql::Like(b, "LIKE", Box::new(pat), false)
        }
        "between" => {
            if args.len() != 2 {
                return Err(Exc::type_error("between() takes two arguments"));
            }
            Sql::Bool(
                "AND",
                vec![
                    Sql::Bin(">=", b.clone(), Box::new(to_sql(&args[0], hint))),
                    Sql::Bin("<=", b, Box::new(to_sql(&args[1], hint))),
                ],
            )
        }
        "desc" => Sql::Order(b, true, None),
        "asc" => Sql::Order(b, false, None),
        "nulls_last" | "nullslast" => match base {
            Sql::Order(e, d, _) => Sql::Order(e, d, Some("NULLS LAST")),
            _ => Sql::Order(b, false, Some("NULLS LAST")),
        },
        "nulls_first" | "nullsfirst" => match base {
            Sql::Order(e, d, _) => Sql::Order(e, d, Some("NULLS FIRST")),
            _ => Sql::Order(b, false, Some("NULLS FIRST")),
        },
        "label" => Sql::Label(b, ops::str_(&one(&args)?)?),
        "filter" if matches!(*b, Sql::Func(..)) => {
            let conds: Vec<Sql> = args.iter().map(|a| to_sql(a, None)).collect();
            let cond = if conds.len() == 1 { conds.into_iter().next().unwrap() } else { Sql::Bool("AND", conds) };
            Sql::Filter(b, Box::new(cond))
        }
        "distinct" => Sql::Distinct(b),
        _ => return Err(Exc::attr_error(format!("SQL expression has no method '{name}'"))),
    }))
}

// ---------------------------------------------------------------- rendering

enum Bind {
    V(V, Option<ColTy>),
}

struct Rend {
    sql: String,
    binds: Vec<Bind>,
    /// aliases named so far in this statement: (alias id, name)
    aliases: Vec<(usize, String)>,
    /// shared parameters rendered so far: (bind identity, placeholder text)
    shared: Vec<(u64, String)>,
}

impl Rend {
    fn alias_name(&mut self, a: &Alias) -> String {
        if let Some((_, n)) = self.aliases.iter().find(|(id, _)| *id == a.id) {
            return n.clone();
        }
        let k = self.aliases.iter().filter(|(_, n)| n.rsplit_once('_').map(|x| x.0) == Some(a.model.table)).count() + 1;
        let n = format!("{}_{k}", a.model.table);
        self.aliases.push((a.id, n.clone()));
        n
    }
}

fn push_alias(out: &mut Vec<Arc<Alias>>, a: &Arc<Alias>) {
    if !out.iter().any(|x| x.id == a.id) {
        out.push(a.clone());
    }
}

fn aliases_in(e: &Sql, out: &mut Vec<Arc<Alias>>) {
    match e {
        Sql::ACol(a, _) => push_alias(out, a),
        Sql::Bin(_, a, b) | Sql::Like(a, _, b, _) | Sql::Filter(a, b) => {
            aliases_in(a, out);
            aliases_in(b, out);
        }
        Sql::Bool(_, items) | Sql::Func(_, items) => items.iter().for_each(|i| aliases_in(i, out)),
        Sql::Case(whens, els) => {
            for (c, v) in whens {
                aliases_in(c, out);
                aliases_in(v, out);
            }
            if let Some(e) = els {
                aliases_in(e, out);
            }
        }
        Sql::In(a, items, _) => {
            aliases_in(a, out);
            items.iter().for_each(|i| aliases_in(i, out));
        }
        Sql::Not(a) | Sql::IsNull(a, _) | Sql::Order(a, _, _) | Sql::Label(a, _) | Sql::Distinct(a) | Sql::Cast(a, _) | Sql::Extract(_, a) | Sql::InSelect(a, _, _) => aliases_in(a, out),
        _ => {}
    }
}

impl Rend {
    fn bind(&mut self, v: V, ty: Option<ColTy>) {
        if let Some(ColTy::Decorated(_)) = ty {
            // process_bind_param runs just before execution (async), see Session::args
            self.binds.push(Bind::V(v, ty));
            self.sql += &format!("${}", self.binds.len());
            return;
        }
        let (v, cast) = match ty {
            Some(ColTy::Enum(ec)) => (ec.to_db(&v).unwrap_or(v), ec.pg_type),
            _ => (v, None),
        };
        let v = match v {
            V::Enum(e, i) if e.kind != EnumKind::Plain => e.value(i),
            other => other,
        };
        // a number's wire type depends on its value (int or float, i32 or i64): name it in the SQL so
        // that each type gets its own prepared statement (sqlx caches them by SQL text, with the
        // parameter types of the first execution)
        let num = match (&v, ty) {
            (V::Int(i), Some(ColTy::Int)) if i32::try_from(*i).is_ok() => Some("int4"),
            (V::Int(i), Some(ColTy::SmallInt)) if i16::try_from(*i).is_ok() => Some("int2"),
            // SQLAlchemy's psycopg dialect renders `::INTEGER` / `::SMALLINT`: out of range, PostgreSQL refuses
            // the cast (22003 integer out of range, a DataError) even in a WHERE that would match nothing
            (V::Int(_), Some(ColTy::Int)) => Some("int8::int4"),
            (V::Int(_), Some(ColTy::SmallInt)) => Some("int8::int2"),
            (V::Int(_), Some(ColTy::Float) | Some(ColTy::NumFloat)) => Some("float8"),
            (V::Int(_), Some(ColTy::Numeric)) | (V::Decimal(_), _) | (V::None, Some(ColTy::Numeric)) => Some("numeric"),
            (V::Int(_), Some(ColTy::BigInt)) => Some("int8"),
            // an untyped int: psycopg's dumper picks the smallest type (int2, int4, int8) and PostgreSQL
            // casts it implicitly where needed (`round(x, 1)` has no bigint overload)
            (V::Int(i), None) if i16::try_from(*i).is_ok() => Some("int2"),
            (V::Int(i), None) if i32::try_from(*i).is_ok() => Some("int4"),
            (V::Int(_), _) => Some("int8"),
            (V::Float(_), _) => Some("float8"),
            // psycopg sends a list as text[]: SQLAlchemy casts it to the column's array type
            (V::List(_) | V::Tuple(_), Some(ColTy::StrArray)) => Some("VARCHAR[]"),
            // a UUID (or a str bound to a Uuid column, which psycopg sends untyped) travels as text
            (_, Some(ColTy::Uuid)) => Some("uuid"),
            (V::Native(n), _) if matches!(&**n, Native::Uuid(_)) => Some("uuid"),
            _ => None,
        };
        // an int beyond the column's integer type: SQLAlchemy+psycopg cast the parameter to it
        // (`%(p)s::INTEGER`), so PostgreSQL answers "integer out of range" (DataError) instead of a
        // comparison that matches nothing
        let narrow = match (&v, ty) {
            (V::Int(i), Some(ColTy::Int)) if i32::try_from(*i).is_err() => Some("INTEGER"),
            (V::Int(i), Some(ColTy::SmallInt)) if i16::try_from(*i).is_err() => Some("SMALLINT"),
            _ => None,
        };
        let cast = cast.or(narrow);
        self.binds.push(Bind::V(v, ty.map(|t| if let ColTy::Enum(_) = t { ColTy::Str } else { t })));
        match (cast, num) {
            (Some(t), Some(n)) => self.sql += &format!("CAST(${}::{} AS {})", self.binds.len(), n, t),
            (Some(t), _) => self.sql += &format!("CAST(${} AS {})", self.binds.len(), t),
            (None, Some(n)) => self.sql += &format!("${}::{}", self.binds.len(), n),
            (None, None) => self.sql += &format!("${}", self.binds.len()),
        }
    }
}

fn render(r: &mut Rend, e: &Sql) -> R<()> {
    match e {
        Sql::Col(m, i) => r.sql += &format!("{}.{}", m.table, m.cols[*i].name),
        Sql::InSelect(a, q, neg) => {
            render(r, a)?;
            r.sql += if *neg { " NOT IN (" } else { " IN (" };
            render_select(r, q)?;
            r.sql.push(')');
        }
        Sql::ACol(a, i) => {
            let n = r.alias_name(a);
            r.sql += &format!("{n}.{}", a.model.cols[*i].name);
        }
        Sql::Alias(_) => return Err(Exc::msg(&ARGUMENT_ERROR, "py2axum: an aliased() entity used as an SQL expression")),
        Sql::BadLabel(m) => return Err(Exc::msg(&COMPILE_ERROR, m.clone())),
        Sql::LabelRef(n) => {
            return Err(Exc::msg(&COMPILE_ERROR, format!(
                "Can't resolve label reference for ORDER BY / GROUP BY / DISTINCT etc. Textual SQL expression '{n}' should be explicitly declared as text('{n}')"
            )))
        }
        Sql::Param(v, ty, id) => {
            if let Some((_, text)) = r.shared.iter().find(|(i, _)| *i == *id && *id != 0) {
                r.sql += &text.clone();
            } else {
                let at = r.sql.len();
                r.bind(v.clone(), *ty);
                if *id != 0 {
                    let text = r.sql[at..].to_string();
                    r.shared.push((*id, text));
                }
            }
        }
        Sql::Null => r.sql += "NULL",
        Sql::Text(t) => r.sql += t,
        Sql::TextBound(parts) => {
            for p in parts {
                render(r, p)?;
            }
        }
        Sql::Rel(m, ri) => {
            return Err(Exc::msg(&ARGUMENT_ERROR, format!("{}.{}: relationship comparisons are not supported", m.name, m.rels[*ri].name)))
        }
        Sql::Load(_) => return Err(Exc::msg(&ARGUMENT_ERROR, "a loader option is not a SQL expression")),
        Sql::Bin(op, a, b) => {
            render(r, a)?;
            r.sql += &format!(" {op} ");
            render(r, b)?;
        }
        Sql::Bool(op, items) => {
            if items.len() > 1 {
                r.sql.push('(');
            }
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    r.sql += &format!(" {op} ");
                }
                render(r, it)?;
            }
            if items.len() > 1 {
                r.sql.push(')');
            }
        }
        Sql::Not(a) => {
            r.sql += "NOT (";
            render(r, a)?;
            r.sql.push(')');
        }
        Sql::IsNull(a, neg) => {
            render(r, a)?;
            r.sql += if *neg { " IS NOT NULL" } else { " IS NULL" };
        }
        Sql::In(a, items, neg) => {
            if items.is_empty() {
                r.sql += if *neg { "1 = 1" } else { "1 != 1" };
                return Ok(());
            }
            render(r, a)?;
            r.sql += if *neg { " NOT IN (" } else { " IN (" };
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    r.sql += ", ";
                }
                render(r, it)?;
            }
            r.sql.push(')');
        }
        Sql::Like(a, op, p, neg) => {
            render(r, a)?;
            r.sql += &format!(" {}{} ", if *neg { "NOT " } else { "" }, op);
            render(r, p)?;
        }
        Sql::Func(name, args) => {
            r.sql += name;
            r.sql.push('(');
            if args.is_empty() && name == "count" {
                r.sql.push('*');
            }
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    r.sql += ", ";
                }
                render(r, a)?;
            }
            r.sql.push(')');
        }
        Sql::Order(a, desc, nulls) => {
            render(r, a)?;
            r.sql += if *desc { " DESC" } else { " ASC" };
            if let Some(n) = nulls {
                r.sql += &format!(" {n}");
            }
        }
        Sql::Label(a, name) => {
            render(r, a)?;
            r.sql += &format!(" AS {name}");
        }
        Sql::Distinct(a) => {
            r.sql += "DISTINCT ";
            render(r, a)?;
        }
        Sql::Exists(s) => {
            r.sql += "EXISTS (";
            render_select(r, s)?;
            r.sql.push(')');
        }
        Sql::Filter(a, cond) => {
            render(r, a)?;
            r.sql += " FILTER (WHERE ";
            render(r, cond)?;
            r.sql.push(')');
        }
        Sql::Extract(f, a) => {
            r.sql += &format!("EXTRACT({f} FROM ");
            render(r, a)?;
            r.sql.push(')');
        }
        Sql::Cast(a, t) => {
            r.sql += "CAST(";
            render(r, a)?;
            r.sql += &format!(" AS {t})");
        }
        Sql::SubCol(q, name) => r.sql += &format!("{}.{}", q.name, name),
        Sql::Subquery(q) => {
            r.sql.push('(');
            render_select(r, &q.sel)?;
            r.sql += &format!(") AS {}", q.name);
        }
        Sql::SubColumns(_) => return Err(Exc::type_error("py2axum: a subquery's .c is not an expression")),
        Sql::Scalar(s) => {
            r.sql.push('(');
            render_select(r, s)?;
            r.sql.push(')');
        }
        Sql::Select(s) => render_select(r, s)?,
        Sql::Update(m, w, sets, _, ret) => {
            r.sql += &format!("UPDATE {} SET ", m.table);
            for (i, (c, v)) in sets.iter().enumerate() {
                if i > 0 {
                    r.sql += ", ";
                }
                r.sql += &format!("{}=", m.cols[*c].name);
                render(r, v)?;
            }
            render_where(r, w)?;
            render_returning(r, ret)?;
        }
        Sql::Delete(m, w, _) => {
            r.sql += &format!("DELETE FROM {}", m.table);
            render_where(r, w)?;
        }
        Sql::Insert(ins) => render_insert(r, ins)?,
        Sql::Excluded(_) => return Err(Exc::msg(&COMPILE_ERROR, "py2axum: `excluded` is only usable through its columns")),
        Sql::ExCol(m, i) => r.sql += &format!("excluded.{}", m.cols[*i].name),
        Sql::Case(whens, els) => {
            r.sql += "CASE";
            for (c, v) in whens {
                r.sql += " WHEN ";
                render(r, c)?;
                r.sql += " THEN ";
                render(r, v)?;
            }
            if let Some(e) = els {
                r.sql += " ELSE ";
                render(r, e)?;
            }
            r.sql += " END";
        }
    }
    Ok(())
}

/// a value for column `c` of an INSERT / ON CONFLICT SET (JSON columns as in the unit of work)
fn render_col_value(r: &mut Rend, c: &ColDesc, v: &Sql) -> R<()> {
    match v {
        Sql::Param(x, _, _) if c.ty == ColTy::JsonNull && x.is_none() => r.bind(V::None, Some(ColTy::JsonNull)),
        Sql::Param(x, _, _) if matches!(c.ty, ColTy::Json | ColTy::JsonNull) => {
            r.binds.push(json_param(x)?);
            r.sql += &format!("CAST(${} AS JSON)", r.binds.len());
        }
        Sql::Null if c.ty == ColTy::Json => {
            r.binds.push(json_param(&V::None)?);
            r.sql += &format!("CAST(${} AS JSON)", r.binds.len());
        }
        other => render(r, other)?,
    }
    Ok(())
}

fn render_insert(r: &mut Rend, ins: &Ins) -> R<()> {
    let m = ins.m;
    let mut cols: Vec<usize> = Vec::new();
    for row in &ins.rows {
        for (i, _) in row {
            if !cols.contains(i) {
                cols.push(*i);
            }
        }
    }
    if cols.is_empty() {
        r.sql += &format!("INSERT INTO {} DEFAULT VALUES", m.table);
    } else {
        r.sql += &format!("INSERT INTO {} ({}) VALUES ", m.table, cols.iter().map(|i| m.cols[*i].name).collect::<Vec<_>>().join(", "));
        for (k, row) in ins.rows.iter().enumerate() {
            if k > 0 {
                r.sql += ", ";
            }
            r.sql.push('(');
            for (j, i) in cols.iter().enumerate() {
                if j > 0 {
                    r.sql += ", ";
                }
                match row.iter().find(|(c, _)| c == i) {
                    Some((_, v)) => render_col_value(r, &m.cols[*i], v)?,
                    None => r.sql += "DEFAULT",
                }
            }
            r.sql.push(')');
        }
    }
    match &ins.conflict {
        None => {}
        Some(Conflict::Nothing(t)) => r.sql += &format!(" ON CONFLICT{t} DO NOTHING"),
        Some(Conflict::Update { target, set, wheres }) => {
            r.sql += &format!(" ON CONFLICT{target} DO UPDATE SET ");
            for (k, (i, v)) in set.iter().enumerate() {
                if k > 0 {
                    r.sql += ", ";
                }
                r.sql += &format!("{} = ", m.cols[*i].name);
                render_col_value(r, &m.cols[*i], v)?;
            }
            render_where(r, wheres)?;
        }
    }
    render_returning(r, &ins.returning)
}

fn render_returning(r: &mut Rend, returning: &[SelCol]) -> R<()> {
    if !returning.is_empty() {
        r.sql += " RETURNING ";
        for (k, c) in returning.iter().enumerate() {
            if k > 0 {
                r.sql += ", ";
            }
            match c {
                SelCol::Entity(e) => r.sql += &e.cols.iter().map(|c| format!("{}.{}", e.table, c.name)).collect::<Vec<_>>().join(", "),
                SelCol::Expr(e) => render(r, e)?,
                SelCol::AEntity(_) => return Err(Exc::type_error("py2axum: returning(aliased(...)) is not supported")),
            }
        }
    }
    Ok(())
}

fn render_where(r: &mut Rend, w: &[Sql]) -> R<()> {
    for (i, c) in w.iter().enumerate() {
        r.sql += if i == 0 { " WHERE " } else { " AND " };
        render(r, c)?;
    }
    Ok(())
}

/// subqueries an expression reads through `sq.c.x` (they join the FROM list once)
fn subs_in(e: &Sql, out: &mut Vec<Arc<Subq>>) {
    match e {
        Sql::SubCol(q, _) => {
            if !out.iter().any(|x| Arc::ptr_eq(x, q)) {
                out.push(q.clone())
            }
        }
        Sql::Bin(_, a, b) | Sql::Like(a, _, b, _) | Sql::Filter(a, b) => {
            subs_in(a, out);
            subs_in(b, out);
        }
        Sql::Bool(_, items) | Sql::Func(_, items) => items.iter().for_each(|i| subs_in(i, out)),
        Sql::Case(whens, els) => {
            for (c, v) in whens {
                subs_in(c, out);
                subs_in(v, out);
            }
            if let Some(e) = els {
                subs_in(e, out);
            }
        }
        Sql::In(a, items, _) => {
            subs_in(a, out);
            items.iter().for_each(|i| subs_in(i, out));
        }
        Sql::Not(a) | Sql::IsNull(a, _) | Sql::Order(a, _, _) | Sql::Label(a, _) | Sql::Distinct(a) | Sql::Cast(a, _) | Sql::Extract(_, a) | Sql::InSelect(a, _, _) => subs_in(a, out),
        _ => {}
    }
}

fn tables_in(e: &Sql, out: &mut Vec<&'static ModelDesc>) {
    match e {
        Sql::Col(m, _) => {
            if !out.iter().any(|x| std::ptr::eq(*x, *m)) {
                out.push(m)
            }
        }
        Sql::Bin(_, a, b) | Sql::Like(a, _, b, _) => {
            tables_in(a, out);
            tables_in(b, out);
        }
        Sql::Bool(_, items) | Sql::Func(_, items) => items.iter().for_each(|i| tables_in(i, out)),
        Sql::Case(whens, els) => {
            for (c, v) in whens {
                tables_in(c, out);
                tables_in(v, out);
            }
            if let Some(e) = els {
                tables_in(e, out);
            }
        }
        Sql::In(a, items, _) => {
            tables_in(a, out);
            items.iter().for_each(|i| tables_in(i, out));
        }
        Sql::Not(a) | Sql::IsNull(a, _) | Sql::Order(a, _, _) | Sql::Label(a, _) | Sql::Distinct(a) | Sql::Cast(a, _) | Sql::Extract(_, a) | Sql::InSelect(a, _, _) => tables_in(a, out),
        Sql::Filter(a, b) => {
            tables_in(a, out);
            tables_in(b, out);
        }
        _ => {}
    }
}

fn render_select(r: &mut Rend, s: &Select) -> R<()> {
    r.sql += "SELECT ";
    if !s.distinct_on.is_empty() {
        r.sql += "DISTINCT ON (";
        for (i, e) in s.distinct_on.iter().enumerate() {
            if i > 0 {
                r.sql += ", ";
            }
            render(r, e)?;
        }
        r.sql += ") ";
    } else if s.distinct {
        r.sql += "DISTINCT ";
    }
    let mut froms: Vec<&'static ModelDesc> = Vec::new();
    for (i, c) in s.cols.iter().enumerate() {
        if i > 0 {
            r.sql += ", ";
        }
        match c {
            SelCol::Entity(m) => {
                r.sql += &m.cols.iter().map(|c| format!("{}.{}", m.table, c.name)).collect::<Vec<_>>().join(", ");
                if !froms.iter().any(|x| std::ptr::eq(*x, *m)) {
                    froms.push(m);
                }
            }
            SelCol::AEntity(a) => {
                let n = r.alias_name(a);
                r.sql += &a.model.cols.iter().map(|c| format!("{n}.{}", c.name)).collect::<Vec<_>>().join(", ");
            }
            SelCol::Expr(e) => {
                render(r, e)?;
                tables_in(e, &mut froms);
            }
        }
    }
    let mut from = s.from.clone();
    for f in froms {
        if !from.iter().any(|x| std::ptr::eq(*x, f)) && !s.joins.iter().any(|(j, _, _, a)| a.is_none() && std::ptr::eq(*j, f)) {
            from.push(f);
        }
    }
    if from.is_empty() {
        for w in &s.wheres {
            tables_in(w, &mut from);
        }
    }
    let mut subs: Vec<Arc<Subq>> = s.from_subs.clone();
    for c in &s.cols {
        if let SelCol::Expr(e) = c {
            subs_in(e, &mut subs);
        }
    }
    for w in &s.wheres {
        subs_in(w, &mut subs);
    }
    for (_, on, _, _) in &s.joins {
        if let Some(on) = on {
            subs_in(on, &mut subs);
        }
    }
    for (_, _, on, _) in &s.join_subs {
        subs_in(on, &mut subs);
    }
    // a joined subquery is not in the FROM list
    subs.retain(|q| !s.join_subs.iter().any(|(_, j, _, _)| Arc::ptr_eq(j, q)));
    // aliases used and not joined: in the FROM list
    let mut alias_from: Vec<Arc<Alias>> = Vec::new();
    for c in &s.cols {
        match c {
            SelCol::AEntity(a) => push_alias(&mut alias_from, a),
            SelCol::Expr(e) => aliases_in(e, &mut alias_from),
            _ => {}
        }
    }
    for w in &s.wheres {
        aliases_in(w, &mut alias_from);
    }
    alias_from.retain(|a| !s.joins.iter().any(|(_, _, _, j)| j.as_ref().is_some_and(|j| j.id == a.id)));
    if !from.is_empty() || !subs.is_empty() || !alias_from.is_empty() {
        r.sql += " FROM ";
        let mut items: Vec<String> = from.iter().map(|m| m.table.to_string()).collect();
        for a in &alias_from {
            let n = r.alias_name(a);
            items.push(format!("{} AS {n}", a.model.table));
        }
        r.sql += &items.join(", ");
        let from = items;
        for (i, q) in subs.iter().enumerate() {
            if i > 0 || !from.is_empty() {
                r.sql += ", ";
            }
            r.sql.push('(');
            render_select(r, &q.sel)?;
            r.sql += &format!(") AS {}", q.name);
        }
    }
    let render_subs = |r: &mut Rend, at: usize| -> R<()> {
        for (_, q, on, outer) in s.join_subs.iter().filter(|j| j.0 == at) {
            r.sql += if *outer { " LEFT OUTER JOIN (" } else { " JOIN (" };
            render_select(r, &q.sel)?;
            r.sql += &format!(") AS {} ON ", q.name);
            render(r, on)?;
        }
        Ok(())
    };
    for (i, (m, on, outer, al)) in s.joins.iter().enumerate() {
        render_subs(r, i)?;
        r.sql += if *outer { " LEFT OUTER JOIN " } else { " JOIN " };
        r.sql += m.table;
        if let Some(a) = al {
            let n = r.alias_name(a);
            r.sql += &format!(" AS {n}");
        }
        if let Some(on) = on {
            r.sql += " ON ";
            render(r, on)?;
        }
    }
    render_subs(r, s.joins.len())?;
    render_where(r, &s.wheres)?;
    if !s.group.is_empty() {
        r.sql += " GROUP BY ";
        for (i, g) in s.group.iter().enumerate() {
            if i > 0 {
                r.sql += ", ";
            }
            render(r, g)?;
        }
    }
    for (i, h) in s.having.iter().enumerate() {
        r.sql += if i == 0 { " HAVING " } else { " AND " };
        render(r, h)?;
    }
    if !s.order.is_empty() {
        r.sql += " ORDER BY ";
        for (i, o) in s.order.iter().enumerate() {
            if i > 0 {
                r.sql += ", ";
            }
            render(r, o)?;
        }
    }
    if let Some(l) = &s.limit {
        r.sql += " LIMIT ";
        render(r, l)?;
    }
    if let Some(o) = &s.offset {
        r.sql += " OFFSET ";
        render(r, o)?;
    }
    if let Some(c) = &s.for_update {
        r.sql += c;
    }
    Ok(())
}

fn render_expr_standalone(e: &Sql) -> R<(String, usize)> {
    let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
    render(&mut r, e)?;
    Ok((r.sql, r.binds.len()))
}

fn push_bind(args: &mut PgArguments, b: &Bind) -> R<()> {
    use sqlx::Arguments;
    let Bind::V(v, ty) = b;
    let ty = &ty.map(ColTy::base);
    let res = match v {
        V::None => match ty.unwrap_or(ColTy::Str) {
            ColTy::Int => args.add(None::<i32>),
            ColTy::BigInt => args.add(None::<i64>),
            ColTy::SmallInt => args.add(None::<i16>),
            ColTy::Bool => args.add(None::<bool>),
            ColTy::Float | ColTy::NumFloat => args.add(None::<f64>),
            ColTy::Numeric => args.add(None::<String>),
            ColTy::Date => args.add(None::<chrono::NaiveDate>),
            ColTy::DateTime => args.add(None::<chrono::NaiveDateTime>),
            ColTy::DateTimeTz => args.add(None::<chrono::DateTime<chrono::Utc>>),
            ColTy::Time => args.add(None::<chrono::NaiveTime>),
            // SQLAlchemy's JSON type stores None as JSON null unless none_as_null=True
            ColTy::Json => args.add(Some(sqlx::types::Json(serde_json::Value::Null))),
            ColTy::JsonNull => args.add(None::<sqlx::types::Json<serde_json::Value>>),
            ColTy::Uuid | ColTy::Str | ColTy::Enum(_) | ColTy::Decorated(_) => args.add(None::<String>),
            ColTy::Bytes => args.add(None::<Vec<u8>>),
            ColTy::StrArray => args.add(None::<Vec<String>>),
        },
        V::Bytes(b) => args.add(b.to_vec()),
        V::List(_) | V::Tuple(_) if matches!(ty, Some(ColTy::StrArray)) => {
            let mut items: Vec<Option<String>> = Vec::new();
            for x in ops::iter(v)? {
                items.push(match x {
                    V::Str(s) => Some(s.to_string()),
                    V::None => None,
                    other => return Err(Exc::type_error(format!("py2axum: an ARRAY(String) element must be a str, got {}", other.type_name()))),
                });
            }
            args.add(items)
        }
        V::Bool(x) => args.add(*x),
        V::Int(i) => match ty {
            // same wire type as a Decimal for `$n::numeric` (sqlx caches the parameter types by SQL text)
            Some(ColTy::Numeric) => args.add(i.to_string()),
            None if i16::try_from(*i).is_ok() => args.add(*i as i16),
            None if i32::try_from(*i).is_ok() => args.add(*i as i32),
            Some(ColTy::Int) if i32::try_from(*i).is_ok() => args.add(*i as i32),
            Some(ColTy::SmallInt) if i16::try_from(*i).is_ok() => args.add(*i as i16),
            Some(ColTy::Float) | Some(ColTy::NumFloat) => args.add(*i as f64),
            _ => args.add(*i),
        },
        V::Float(f) => args.add(*f),
        // sent as text, cast by the SQL (`$n::numeric`)
        V::Decimal(d) => args.add(d.to_string()),
        V::Str(s) => args.add(s.to_string()),
        V::Native(n) if matches!(&**n, Native::Uuid(_)) => args.add(ops::str_(v)?),
        V::Date(d) => args.add(*d),
        V::Time(t) => args.add(*t),
        V::DateTime(d) => match d.tz {
            Some(_) => args.add(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(d.utc(), chrono::Utc)),
            None => args.add(d.wall),
        },
        V::Dict(_) | V::List(_) | V::Tuple(_) if matches!(ty, Some(ColTy::Json) | Some(ColTy::JsonNull) | None) => {
            let text = pyd::to_json(v, &pyd::DUMPS, false)?;
            let val: serde_json::Value = serde_json::from_str(&text).map_err(|e| Exc::value_error(e.to_string()))?;
            args.add(sqlx::types::Json(val))
        }
        other => return Err(Exc::type_error(format!("cannot bind a value of type {} as a SQL parameter", other.type_name()))),
    };
    res.map_err(|e| Exc::type_error(format!("bind: {e}")))
}

/// JSON columns: a Python value (`None` included) stored as JSON text, like SQLAlchemy's JSON type.
fn json_param(v: &V) -> R<Bind> {
    let text = pyd::to_json(v, &pyd::DUMPS, false)?;
    let val: serde_json::Value = serde_json::from_str(&text).map_err(|e| Exc::value_error(e.to_string()))?;
    Ok(Bind::V(V::Str(Arc::from(val.to_string())), Some(ColTy::Str)))
}

// ---------------------------------------------------------------- decoding

/// PostgreSQL NUMERIC (binary format: ndigits, weight, sign, dscale, base-10000 digits) as its
/// decimal text, e.g. "-12.30"; None for SQL NULL.
fn numeric_text(row: &PgRow, i: usize) -> R<Option<String>> {
    use sqlx::ValueRef;
    let raw = row.try_get_raw(i).map_err(|e| Exc::runtime(e.to_string()))?;
    if raw.is_null() {
        return Ok(None);
    }
    let b = <&[u8] as sqlx::Decode<Postgres>>::decode(raw).map_err(|e| Exc::runtime(e.to_string()))?;
    let u = |k: usize| u16::from_be_bytes([b[k], b[k + 1]]);
    let (ndigits, weight, sign, dscale) = (u(0) as usize, u(2) as i16 as i32, u(4), u(6) as usize);
    match sign {
        0xC000 => return Ok(Some("NaN".into())),
        0xD000 => return Ok(Some("Infinity".into())),
        0xF000 => return Ok(Some("-Infinity".into())),
        _ => {}
    }
    let digit = |k: i32| if k >= 0 && (k as usize) < ndigits { u(8 + 2 * k as usize) } else { 0 };
    let mut int = String::new();
    for k in 0..=weight.max(0) {
        if weight < 0 {
            break;
        }
        int += &if k == 0 { digit(k).to_string() } else { format!("{:04}", digit(k)) };
    }
    if int.is_empty() {
        int.push('0');
    }
    let mut frac = String::new();
    let mut k = weight + 1;
    while frac.len() < dscale {
        frac += &format!("{:04}", digit(k));
        k += 1;
    }
    frac.truncate(dscale);
    let neg = sign == 0x4000;
    Ok(Some(format!("{}{}{}{}", if neg { "-" } else { "" }, int, if dscale > 0 { "." } else { "" }, frac)))
}

fn decode(row: &PgRow, i: usize, tz: Tz) -> R<V> {
    let ty = row.column(i).type_info().name().to_string();
    let e = |e: sqlx::Error| Exc::runtime(format!("decode column {i} ({ty}): {e}"));
    Ok(match ty.as_str() {
        // decimal.Decimal, as psycopg returns it (Numeric(asdecimal=False) columns: float, see map_col)
        "NUMERIC" => match numeric_text(row, i)? {
            Some(t) => super::decimal::v(super::decimal::Dec::parse(&t)?),
            None => V::None,
        },
        "INT4" => row.try_get::<Option<i32>, _>(i).map_err(e)?.map(|x| V::Int(x as i64)).unwrap_or(V::None),
        "INT8" => row.try_get::<Option<i64>, _>(i).map_err(e)?.map(V::Int).unwrap_or(V::None),
        "INT2" => row.try_get::<Option<i16>, _>(i).map_err(e)?.map(|x| V::Int(x as i64)).unwrap_or(V::None),
        "BOOL" => row.try_get::<Option<bool>, _>(i).map_err(e)?.map(V::Bool).unwrap_or(V::None),
        "FLOAT8" => row.try_get::<Option<f64>, _>(i).map_err(e)?.map(V::Float).unwrap_or(V::None),
        "FLOAT4" => row.try_get::<Option<f32>, _>(i).map_err(e)?.map(|x| V::Float(x as f64)).unwrap_or(V::None),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => row.try_get::<Option<String>, _>(i).map_err(e)?.map(V::str).unwrap_or(V::None),
        "DATE" => row.try_get::<Option<chrono::NaiveDate>, _>(i).map_err(e)?.map(V::Date).unwrap_or(V::None),
        "TIME" => row.try_get::<Option<chrono::NaiveTime>, _>(i).map_err(e)?.map(V::Time).unwrap_or(V::None),
        "TIMESTAMPTZ" => row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(i)
            .map_err(e)?
            .map(|d| V::DateTime(DateTime::from_utc(d.naive_utc(), tz)))
            .unwrap_or(V::None),
        "TIMESTAMP" => row
            .try_get::<Option<chrono::NaiveDateTime>, _>(i)
            .map_err(e)?
            .map(|d| V::DateTime(DateTime::naive(d)))
            .unwrap_or(V::None),
        "JSON" | "JSONB" => match row.try_get::<Option<sqlx::types::JsonValue>, _>(i).map_err(e)? {
            Some(v) => pyd::from_serde(&v),
            None => V::None,
        },
        "BYTEA" => row.try_get::<Option<Vec<u8>>, _>(i).map_err(e)?.map(|b| V::Bytes(Arc::from(b))).unwrap_or(V::None),
        "TEXT[]" | "VARCHAR[]" | "BPCHAR[]" => match row.try_get::<Option<Vec<Option<String>>>, _>(i).map_err(e)? {
            Some(items) => V::list(items.into_iter().map(|x| x.map(V::str).unwrap_or(V::None)).collect()),
            None => V::None,
        },
        "UUID" => row.try_get::<Option<sqlx::types::Uuid>, _>(i).map_err(e)?.map(|u| V::native(Native::Uuid(u.as_u128()))).unwrap_or(V::None),
        "INTERVAL" => {
            let iv = row.try_get::<Option<sqlx::postgres::types::PgInterval>, _>(i).map_err(e)?;
            match iv {
                Some(iv) => V::Delta(chrono::TimeDelta::microseconds(iv.microseconds + (iv.days as i64) * 86_400_000_000 + (iv.months as i64) * 30 * 86_400_000_000)),
                None => V::None,
            }
        }
        _ => match row.try_get_unchecked::<Option<String>, _>(i) {
            Ok(v) => v.map(V::str).unwrap_or(V::None),
            Err(_) => return Err(Exc::runtime(format!("py2axum: unsupported Postgres column type {ty}"))),
        },
    })
}

fn map_col(c: &ColDesc, v: V) -> V {
    match c.ty {
        ColTy::Enum(ec) => ec.from_db(v),
        ColTy::NumFloat => match v {
            V::Decimal(d) => V::Float(d.to_f64()),
            o => o,
        },
        _ => v,
    }
}

// ---------------------------------------------------------------- results

pub enum OutCol {
    Entity(&'static ModelDesc),
    Value,
}

pub struct QResult {
    pub rows: Option<Vec<Vec<V>>>,
    pub width: usize,
    pub scalars: bool,
    pub rowcount: i64,
    /// column keys of the rows (`row.total`), as SQLAlchemy names them
    pub names: Option<Arc<Vec<Arc<str>>>>,
}

/// SQLAlchemy's key for a selected column: its label, column or function name, the entity's class name
fn col_key(c: &SelCol) -> Arc<str> {
    Arc::from(match c {
        SelCol::Entity(m) => m.name,
        SelCol::AEntity(a) => a.model.name,
        SelCol::Expr(e) => match e {
            Sql::Label(_, l) => l.as_str(),
            Sql::Col(m, i) => m.cols[*i].name,
            Sql::ACol(a, i) => a.model.cols[*i].name,
            Sql::Func(n, _) => n.as_str(),
            Sql::SubCol(_, n) => n.as_str(),
            _ => "",
        },
    })
}

impl QResult {
    fn row_value(&self, r: Vec<V>) -> V {
        if self.scalars {
            r.into_iter().next().unwrap_or(V::None)
        } else if let Some(names) = &self.names {
            V::native(Native::Row(names.clone(), Arc::new(r)))
        } else {
            V::tuple(r)
        }
    }
    pub fn take_rows(&mut self) -> R<Vec<V>> {
        let rows = self.rows.take().ok_or_else(|| Exc::runtime("This result object is closed."))?;
        Ok(rows.into_iter().map(|r| self.row_value(r)).collect())
    }
}

pub fn result_method(res: &Arc<Mutex<QResult>>, name: &str) -> R {
    let mut q = res.lock();
    match name {
        "scalars" => {
            let rows = q.rows.take();
            Ok(V::Result(Arc::new(Mutex::new(QResult { rows, width: 1, scalars: true, rowcount: q.rowcount, names: None }))))
        }
        "all" | "fetchall" => Ok(V::list(q.take_rows()?)),
        // RowMapping objects: rows as dicts keyed like `row._mapping`
        "mappings" => {
            let names = q.names.clone();
            let rows = q.rows.take().unwrap_or_default();
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let items = r.into_iter().enumerate().map(|(i, x)| {
                    let k = names.as_ref().and_then(|n| n.get(i).cloned()).unwrap_or_else(|| Arc::from(i.to_string()));
                    (V::Str(k), x)
                });
                out.push(vec![V::dict_from(items.collect())?]);
            }
            Ok(V::Result(Arc::new(Mutex::new(QResult { rows: Some(out), width: 1, scalars: true, rowcount: q.rowcount, names: None }))))
        }
        "first" => Ok(q.take_rows()?.into_iter().next().unwrap_or(V::None)),
        "one" | "scalar_one" | "one_or_none" | "scalar_one_or_none" => {
            if name.starts_with("scalar_") {
                q.scalars = true;
            }
            let rows = q.take_rows()?;
            match (rows.len(), name.ends_with("or_none")) {
                (0, true) => Ok(V::None),
                (0, false) => Err(Exc::msg(&NO_RESULT_FOUND, "No row was found when one was required")),
                (1, _) => Ok(rows.into_iter().next().unwrap()),
                _ => Err(Exc::msg(&MULTIPLE_RESULTS_FOUND, "Multiple rows were found when exactly one was required")),
            }
        }
        "scalar" => {
            q.scalars = true;
            Ok(q.take_rows()?.into_iter().next().unwrap_or(V::None))
        }
        "unique" => {
            // rows (or scalars) deduplicated, objects by identity, first occurrence kept
            if let Some(rows) = q.rows.take() {
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let key = if q.scalars { vec![Key::of(&r[0])?] } else { r.iter().map(Key::of).collect::<R<Vec<_>>>()? };
                    if seen.insert(key) {
                        out.push(r);
                    }
                }
                q.rows = Some(out);
            }
            Ok(V::Result(res.clone()))
        }
        _ => Err(Exc::attr_error(format!("'Result' object has no attribute '{name}'"))),
    }
}

pub fn result_attr(res: &Arc<Mutex<QResult>>, name: &str) -> R {
    match name {
        "rowcount" => Ok(V::Int(res.lock().rowcount)),
        _ => Err(Exc::attr_error(format!("'Result' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- session

static DB_TZ: OnceLock<Tz> = OnceLock::new();

pub fn db_tz() -> Tz {
    *DB_TZ.get().unwrap_or(&Tz::Utc)
}

static ASYNCPG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `DATABASE_URL` as SQLAlchemy reads it: `postgresql+asyncpg://` selects asyncpg, whose timestamptz
/// codec returns `datetime.timezone.utc` instances whatever the session TimeZone (psycopg returns them
/// in the session's zone).
pub fn set_driver(url: &str) {
    ASYNCPG.store(url.starts_with("postgresql+asyncpg:"), std::sync::atomic::Ordering::Relaxed);
}

pub fn asyncpg() -> bool {
    ASYNCPG.load(std::sync::atomic::Ordering::Relaxed)
}

/// The zone a decoded timestamptz carries in Python (see `set_driver`).
pub fn row_tz() -> Tz {
    if ASYNCPG.load(std::sync::atomic::Ordering::Relaxed) { Tz::Utc } else { db_tz() }
}

/// The TimeZone a psycopg connection would get (server/db/role default): sqlx forces UTC at
/// connect, so the pool applies this one on every connection.
pub async fn discover_db_tz(pool: &sqlx::PgPool) -> String {
    if let Ok(v) = std::env::var("PY2AXUM_DB_TIMEZONE") {
        return v;
    }
    let role_db: Option<String> = sqlx::query_scalar(
        "SELECT split_part(c, '=', 2) FROM pg_db_role_setting s, unnest(s.setconfig) c \
         WHERE lower(split_part(c, '=', 1)) = 'timezone' \
         AND s.setdatabase IN (0, (SELECT oid FROM pg_database WHERE datname = current_database())) \
         AND s.setrole IN (0, (SELECT oid FROM pg_roles WHERE rolname = current_user)) \
         ORDER BY (s.setrole <> 0) DESC, (s.setdatabase <> 0) DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some(v) = role_db {
        return v;
    }
    let file: Option<String> = sqlx::query_scalar(
        "SELECT setting FROM pg_file_settings WHERE name = 'timezone' AND error IS NULL ORDER BY seqno DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    file.unwrap_or_else(|| "UTC".to_string())
}

static DB_TZ_NAME: OnceLock<String> = OnceLock::new();

pub fn set_db_tz(name: &str) {
    let tz = Tz::zone(name).unwrap_or(Tz::Utc);
    let _ = DB_TZ.set(tz);
    let _ = DB_TZ_NAME.set(name.to_string());
}

pub fn db_tz_name() -> Option<String> {
    DB_TZ_NAME.get().cloned()
}

/// Same discovery as `discover_db_tz`, on a single (just opened) connection.
pub async fn discover_db_tz_conn(conn: &mut sqlx::PgConnection) -> String {
    if let Ok(v) = std::env::var("PY2AXUM_DB_TIMEZONE") {
        return v;
    }
    let role_db: Option<String> = sqlx::query_scalar(
        "SELECT split_part(c, '=', 2) FROM pg_db_role_setting s, unnest(s.setconfig) c \
         WHERE lower(split_part(c, '=', 1)) = 'timezone' \
         AND s.setdatabase IN (0, (SELECT oid FROM pg_database WHERE datname = current_database())) \
         AND s.setrole IN (0, (SELECT oid FROM pg_roles WHERE rolname = current_user)) \
         ORDER BY (s.setrole <> 0) DESC, (s.setdatabase <> 0) DESC LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .ok()
    .flatten();
    if let Some(v) = role_db {
        return v;
    }
    let file: Option<String> = sqlx::query_scalar(
        "SELECT setting FROM pg_file_settings WHERE name = 'timezone' AND error IS NULL ORDER BY seqno DESC LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .ok()
    .flatten();
    file.unwrap_or_else(|| "UTC".to_string())
}

struct SessInner {
    pool: sqlx::PgPool,
    tx: Option<sqlx::Transaction<'static, Postgres>>,
    identity: IdMap,
    /// modified persistent objects: held strongly until flushed (SQLAlchemy's `_strong_obj`)
    strong: Vec<Arc<ObjCell>>,
    new: Vec<Arc<ObjCell>>,
    deleted: Vec<Arc<ObjCell>>,
    expire_on_commit: bool,
    /// a synchronous `Session`: reading an unloaded attribute loads it (SQL) instead of MissingGreenlet
    sync: bool,
    autoflush: bool,
    me: Weak<tokio::sync::Mutex<SessInner>>,
    /// the request (TypeDecorator methods are compiled Python: they run with it)
    cx: std::sync::Weak<super::CxInner>,
    /// objects loaded whose TypeDecorator columns still hold the raw database value
    pending: Vec<Arc<ObjCell>>,
    /// `begin_nested()` savepoints, innermost last
    savepoints: Vec<SpFrame>,
    sp_seq: usize,
    /// the connection of `session.connection()` was closed: statements raise until rollback/close
    conn_closed: bool,
    /// `engine.begin()`: the transaction commits when the `async with` ends without an exception
    commit_on_exit: bool,
}

/// What a savepoint's rollback undoes in the session: objects INSERTed (expunged), UPDATEd (expired)
/// and DELETEd (back, expired) since it began.
struct SpFrame {
    name: String,
    inserted: Vec<Arc<ObjCell>>,
    updated: Vec<Arc<ObjCell>>,
    deleted: Vec<Arc<ObjCell>>,
}

/// SQLAlchemy's weak-referencing identity map: an object nothing references any more (CPython frees
/// it at once) is gone from the map, a later access loads it again. Modified, new and deleted
/// objects are held strongly by the session until flushed.
#[derive(Default)]
struct IdMap(HashMap<(usize, Key), Weak<ObjCell>>);

impl IdMap {
    fn get(&self, k: &(usize, Key)) -> Option<Arc<ObjCell>> {
        self.0.get(k).and_then(|w| w.upgrade())
    }
    fn insert(&mut self, k: (usize, Key), o: &Arc<ObjCell>) {
        if self.0.len() > 64 && self.0.len().is_power_of_two() {
            self.0.retain(|_, w| w.strong_count() > 0);
        }
        self.0.insert(k, Arc::downgrade(o));
    }
    fn remove(&mut self, k: &(usize, Key)) -> Option<Arc<ObjCell>> {
        self.0.remove(k).and_then(|w| w.upgrade())
    }
    fn entries(&self) -> Vec<((usize, Key), Arc<ObjCell>)> {
        self.0.iter().filter_map(|(k, w)| w.upgrade().map(|o| (k.clone(), o))).collect()
    }
    fn values(&self) -> Vec<Arc<ObjCell>> {
        self.0.values().filter_map(|w| w.upgrade()).collect()
    }
}

impl Drop for SessInner {
    /// Related objects point at each other (parent <-> children): break the cycles with the session.
    fn drop(&mut self) {
        for o in self.identity.values().iter().chain(self.new.iter()).chain(self.deleted.iter()) {
            let mut st = o.st.lock();
            st.unload_rels();
            st.links.clear();
        }
    }
}

type BoxR<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = R<T>> + Send + 'a>>;

/// `Base.metadata.create_all`, compiled at translation time by SQLAlchemy (py2axum/ddl.py)
pub struct Ddl {
    /// named enum types, in the order SQLAlchemy creates them: (name, CREATE TYPE)
    pub types: &'static [(&'static str, &'static str)],
    /// tables in dependency order
    pub tables: &'static [DdlTable],
}

pub struct DdlTable {
    pub name: &'static str,
    /// CREATE TABLE, then its CREATE INDEX and COMMENT ON statements
    pub stmts: &'static [&'static str],
    /// its foreign keys of a cycle: ALTER TABLE ... ADD CONSTRAINT, once every table is created
    pub alters: &'static [&'static str],
    /// the CREATE TABLE SQLAlchemy 2.1 renders for a server before PostgreSQL 18, when it differs (a computed
    /// column without `persisted=` is STORED there, VIRTUAL from 18 on)
    pub create_pre18: Option<&'static str>,
}

/// `await conn.run_sync(Base.metadata.create_all)`
pub async fn run_create_all(conn: &V, ddl: &'static Ddl) -> R {
    match conn {
        V::Session(s) => {
            s.create_all(ddl).await?;
            Ok(V::None)
        }
        other => Err(Exc::attr_error(format!("'{}' object has no attribute 'run_sync'", other.type_name()))),
    }
}

/// `AsyncSession`: one per request (FastAPI dependency cache), cheap to clone.
#[derive(Clone)]
pub struct Session(Arc<tokio::sync::Mutex<SessInner>>);

fn ident(m: &'static ModelDesc, pk: &V) -> R<(usize, Key)> {
    Ok((m as *const ModelDesc as usize, Key::of(pk)?))
}

impl Session {
    /// a `sessionmaker` session (plain methods), not an `AsyncSession` (unknown while it is busy: async)
    pub fn is_sync(&self) -> bool {
        self.0.try_lock().map(|s| s.sync).unwrap_or(false)
    }

    pub fn new(pool: sqlx::PgPool, expire_on_commit: bool, autoflush: bool, sync: bool, cx: std::sync::Weak<super::CxInner>) -> Session {
        Session(Arc::new_cyclic(|me| {
            tokio::sync::Mutex::new(SessInner {
                pool,
                tx: None,
                identity: IdMap::default(),
                strong: Vec::new(),
                new: Vec::new(),
                deleted: Vec::new(),
                expire_on_commit,
                sync,
                autoflush,
                me: me.clone(),
                cx,
                pending: Vec::new(),
                savepoints: Vec::new(),
                sp_seq: 0,
                conn_closed: false,
                commit_on_exit: false,
            })
        }))
    }

    pub fn add(&self, obj: &V) -> R<()> {
        let o = match obj {
            V::Obj(o) => o.clone(),
            other => return Err(Exc::type_error(format!("session.add() needs a mapped object, got {}", other.type_name()))),
        };
        let mut s = self.0.try_lock().map_err(|_| Exc::runtime("session busy"))?;
        let mut st = o.st.lock();
        if st.status == Status::Transient {
            st.status = Status::Pending;
            drop(st);
            *o.sess.lock() = s.me.clone();
            s.new.push(o);
        }
        Ok(())
    }

    pub async fn delete(&self, obj: &V) -> R<()> {
        if let V::Obj(o) = obj {
            let mut s = self.0.lock().await;
            Session::delete_inner(&mut s, o.clone()).await
        } else {
            Err(Exc::type_error("session.delete() needs a mapped object"))
        }
    }

    /// `session.delete(obj)` with the relationship cascades: one-to-many children are deleted
    /// (cascade "delete"), or their foreign key is set to NULL at flush; unloaded children are
    /// loaded first unless `passive_deletes=True`. Children are deleted before their parent.
    fn delete_inner(s: &mut SessInner, o: Arc<ObjCell>) -> BoxR<'_, ()> {
        Box::pin(async move {
            if o.st.lock().status == Status::Pending {
                s.new.retain(|x| !Arc::ptr_eq(x, &o));
                o.st.lock().status = Status::Transient;
                return Ok(());
            }
            if s.deleted.iter().any(|x| Arc::ptr_eq(x, &o)) {
                return Ok(());
            }
            for (ri, rel) in o.desc.rels.iter().enumerate() {
                if rel.m2o {
                    continue;
                }
                let loaded = o.st.lock().rels[ri].clone();
                let kids = match loaded {
                    Some(v) => members(&v),
                    None if !rel.passive_deletes => {
                        Session::load_rel(s, &[o.clone()], o.desc, ri).await?;
                        Session::decode_pending(s).await?;
                        o.st.lock().rels[ri].clone().map(|v| members(&v)).unwrap_or_default()
                    }
                    None => vec![],
                };
                for k in kids {
                    if let V::Obj(k) = k {
                        if rel.delete {
                            Session::delete_inner(s, k).await?;
                        } else {
                            k.st.lock().links.push((rel.remote, None, 0));
                            s.strong.push(k.clone());
                        }
                    }
                }
            }
            s.deleted.push(o);
            Ok(())
        })
    }

    /// Loader strategies after a query: `lazy="selectin"` relationships (unless the target model
    /// is already on the path, as SQLAlchemy does without join_depth) and the `.options()` chains.
    fn eager<'a>(s: &'a mut SessInner, objs: Vec<Arc<ObjCell>>, desc: &'static ModelDesc, opts: Vec<LoadChain>, path: Vec<usize>) -> BoxR<'a, ()> {
        Box::pin(async move {
            if objs.is_empty() {
                return Ok(());
            }
            for (ri, rel) in desc.rels.iter().enumerate() {
                let mine: Vec<&LoadChain> = opts.iter().filter(|c| std::ptr::eq(c[0].1, desc) && c[0].2 == ri).collect();
                let target = rel.target as *const ModelDesc as usize;
                let kind = match mine.first() {
                    Some(c) => c[0].0,
                    None if rel.lazy == Lazy::Selectin && !path.contains(&target) => Lazy::Selectin,
                    None => continue,
                };
                let sub: Vec<LoadChain> = mine.iter().filter(|c| c.len() > 1).map(|c| c[1..].to_vec()).collect();
                match kind {
                    Lazy::NoLoad => {
                        for o in &objs {
                            let mut st = o.st.lock();
                            if st.rels[ri].is_none() {
                                st.rels[ri] = Some(if rel.uselist { V::list(vec![]) } else { V::None });
                                st.rel_snap[ri] = Some(vec![]);
                            }
                        }
                    }
                    Lazy::Selectin => {
                        let loaded = Session::load_rel(s, &objs, desc, ri).await?;
                        let mut p2 = path.clone();
                        p2.push(target);
                        Session::eager(s, loaded, rel.target, sub, p2).await?;
                    }
                    _ => {}
                }
            }
            Ok(())
        })
    }

    /// selectin loading of one relationship for the objects where it is not loaded yet:
    /// `SELECT target WHERE target.col IN (...)`, many-to-one targets found in the identity map
    /// are not queried. Returns the related objects (distinct, in load order).
    async fn load_rel(s: &mut SessInner, objs: &[Arc<ObjCell>], desc: &'static ModelDesc, ri: usize) -> R<Vec<Arc<ObjCell>>> {
        let rel = &desc.rels[ri];
        let t = rel.target;
        let empty = || if rel.uselist { V::list(vec![]) } else { V::None };
        let mut owners: Vec<(Arc<ObjCell>, V)> = Vec::new();
        for o in objs {
            let mut st = o.st.lock();
            if st.rels[ri].is_some() || st.status != Status::Persistent {
                continue;
            }
            let k = st.vals[rel.local].clone();
            if matches!(k, V::Unbound) {
                continue;
            }
            if k.is_none() {
                st.rels[ri] = Some(empty());
                st.rel_snap[ri] = Some(vec![]);
                continue;
            }
            owners.push((o.clone(), k));
        }
        let mut keys: Vec<V> = Vec::new();
        let mut seen = HashSet::new();
        for (_, k) in &owners {
            if seen.insert(Key::of(k)?) {
                keys.push(k.clone());
            }
        }
        let mut found: HashMap<Key, Vec<Arc<ObjCell>>> = HashMap::new();
        let mut related: Vec<Arc<ObjCell>> = Vec::new();
        let mut query = Vec::new();
        for k in keys {
            let hit = if rel.m2o && t.pks == [rel.remote] {
                s.identity.get(&ident(t, &k)?).filter(|o| {
                    let st = o.st.lock();
                    !st.expired && st.status == Status::Persistent
                })
            } else {
                None
            };
            match hit {
                Some(o) => {
                    found.insert(Key::of(&k)?, vec![o.clone()]);
                    related.push(o.clone());
                }
                None => query.push(k),
            }
        }
        if !query.is_empty() {
            let ty = t.cols[rel.remote].ty;
            let sel = Select {
                cols: vec![SelCol::Entity(t)],
                wheres: vec![Sql::In(Box::new(Sql::Col(t, rel.remote)), query.into_iter().map(|k| Sql::Param(k, Some(ty), 0)).collect(), false)],
                order: rel.order.iter().map(|&(i, desc)| Sql::Order(Box::new(Sql::Col(t, i)), desc, None)).collect(),
                ..Default::default()
            };
            let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
            render_select(&mut r, &sel)?;
            let rows = Session::run(s, &r).await?;
            for row in &rows {
                let o = Session::entity(s, t, row, 0)?;
                let k = Key::of(&o.st.lock().vals[rel.remote])?;
                found.entry(k).or_default().push(o.clone());
                if !related.iter().any(|x| Arc::ptr_eq(x, &o)) {
                    related.push(o);
                }
            }
        }
        for (o, k) in owners {
            let items = found.get(&Key::of(&k)?).cloned().unwrap_or_default();
            let v = if rel.uselist {
                V::list(items.iter().map(|x| V::Obj(x.clone())).collect())
            } else {
                items.first().map(|x| V::Obj(x.clone())).unwrap_or(V::None)
            };
            let mut st = o.st.lock();
            if !rel.m2o {
                st.rel_snap[ri] = Some(members(&v));
            }
            st.rels[ri] = Some(v);
        }
        Ok(related)
    }

    /// Relationship changes since the last flush, turned into foreign-key copies (`links`):
    /// many-to-one assignments, collection members added/removed (removed = NULL, or deleted with
    /// delete-orphan), and the save-update cascade (transient related objects become pending).
    fn sync_rels(s: &mut SessInner) -> R<()> {
        let mut queue: Vec<Arc<ObjCell>> = s.new.iter().cloned().chain(s.identity.values()).collect();
        let mut i = 0;
        while i < queue.len() {
            let o = queue[i].clone();
            i += 1;
            for (ri, rel) in o.desc.rels.iter().enumerate() {
                let (cur, snap, set) = {
                    let st = o.st.lock();
                    (st.rels[ri].clone(), st.rel_snap[ri].clone(), st.rel_set[ri])
                };
                let Some(cur) = cur else { continue };
                let now = members(&cur);
                for m in &now {
                    if let V::Obj(x) = m {
                        let mut xs = x.st.lock();
                        if xs.status == Status::Transient {
                            xs.status = Status::Pending;
                            drop(xs);
                            *x.sess.lock() = s.me.clone();
                            s.new.push(x.clone());
                            queue.push(x.clone());
                        }
                    }
                }
                if rel.m2o {
                    if set {
                        let src = match &cur {
                            V::Obj(x) => Some(x.clone()),
                            _ => None,
                        };
                        if src.as_ref().is_some_and(|x| Arc::ptr_eq(x, &o)) {
                            // the unit of work cannot order a row after itself (no post_update)
                            let at = format!("<{} at 0x{:x}>", o.desc.name, Arc::as_ptr(&o) as usize);
                            return Err(Exc::msg(&CIRCULAR_DEPENDENCY_ERROR, format!(
                                "Circular dependency detected. (SaveUpdateState({at}), ProcessState(ManyToOneDP({}.{}), {at}, delete=False))",
                                o.desc.name, rel.name)));
                        }
                        let mut st = o.st.lock();
                        st.links.push((rel.local, src, rel.remote));
                        st.rel_set[ri] = false;
                    }
                    continue;
                }
                let before = snap.unwrap_or_default();
                for m in &now {
                    if let V::Obj(x) = m {
                        if !before.iter().any(|b| same(b, m)) {
                            x.st.lock().links.push((rel.remote, Some(o.clone()), rel.local));
                            s.strong.push(x.clone());
                        }
                    }
                }
                for b in &before {
                    if let V::Obj(x) = b {
                        if now.iter().any(|m| same(b, m)) {
                            continue;
                        }
                        if rel.orphan {
                            let status = x.st.lock().status;
                            if status == Status::Pending {
                                x.st.lock().status = Status::Transient;
                                s.new.retain(|y| !Arc::ptr_eq(y, x));
                            } else if status == Status::Persistent && !s.deleted.iter().any(|y| Arc::ptr_eq(y, x)) {
                                s.deleted.push(x.clone());
                            }
                        } else {
                            x.st.lock().links.push((rel.remote, None, 0));
                            s.strong.push(x.clone());
                        }
                    }
                }
                let mut st = o.st.lock();
                st.rel_snap[ri] = Some(now);
                st.rel_set[ri] = false;
            }
        }
        Ok(())
    }

    /// Resolve the pending foreign-key copies of an object (its sources are flushed first).
    fn apply_links(o: &Arc<ObjCell>) {
        let links = std::mem::take(&mut o.st.lock().links);
        for (col, src, scol) in links {
            let v = match src {
                Some(x) => x.st.lock().vals[scol].clone(),
                None => V::None,
            };
            let mut st = o.st.lock();
            st.vals[col] = v;
            st.modified[col] = true;
        }
    }

    async fn begin(s: &mut SessInner) -> R<()> {
        if s.conn_closed {
            return Err(Exc::msg(&RESOURCE_CLOSED_ERROR, "This Connection is closed"));
        }
        if s.tx.is_none() {
            s.tx = Some(s.pool.begin().await?);
        }
        Ok(())
    }

    fn cx(s: &SessInner) -> R<super::Cx> {
        s.cx.upgrade().ok_or_else(|| Exc::runtime("py2axum: session used after its request ended"))
    }

    /// The bind parameters, `TypeDecorator.process_bind_param` applied (value, dialect=None).
    async fn args(s: &SessInner, r: &Rend) -> R<PgArguments> {
        let mut args = PgArguments::default();
        for b in &r.binds {
            match b {
                Bind::V(v, Some(ColTy::Decorated(d))) => {
                    let v = match d.bind {
                        Some(f) => f(&Session::cx(s)?, V::None, vec![v.clone(), V::None]).await?,
                        None => v.clone(),
                    };
                    push_bind(&mut args, &Bind::V(v, Some(d.impl_ty)))?;
                }
                b => push_bind(&mut args, b)?,
            }
        }
        Ok(args)
    }

    /// `process_result_value` of the decorated columns of the objects loaded since the last call.
    async fn decode_pending(s: &mut SessInner) -> R<()> {
        let pending = std::mem::take(&mut s.pending);
        for o in pending {
            for (i, c) in o.desc.cols.iter().enumerate() {
                if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = c.ty {
                    let raw = o.st.lock().vals[i].clone();
                    let v = f(&Session::cx(s)?, V::None, vec![raw, V::None]).await?;
                    let mut st = o.st.lock();
                    st.vals[i] = v.clone();
                    st.committed[i] = v;
                }
            }
        }
        Ok(())
    }

    fn loaded(s: &mut SessInner, o: &Arc<ObjCell>) {
        if o.desc.cols.iter().any(|c| matches!(c.ty, ColTy::Decorated(TypeDec { result: Some(_), .. }))) {
            s.pending.push(o.clone());
        }
    }

    async fn run(s: &mut SessInner, r: &Rend) -> R<Vec<PgRow>> {
        Session::begin(s).await?;
        let args = Session::args(s, r).await?;
        let tx = s.tx.as_mut().unwrap();
        sqlx::query_with(&r.sql, args).fetch_all(&mut **tx).await.map_err(|e| sql_error(e, &r.sql))
    }

    async fn run_exec(s: &mut SessInner, r: &Rend) -> R<u64> {
        Session::begin(s).await?;
        let args = Session::args(s, r).await?;
        let tx = s.tx.as_mut().unwrap();
        Ok(sqlx::query_with(&r.sql, args).execute(&mut **tx).await.map_err(|e| sql_error(e, &r.sql))?.rows_affected())
    }

    async fn flush_inner(s: &mut SessInner) -> R<()> {
        Session::sync_rels(s)?;
        // INSERTs, referenced tables first
        let mut new = std::mem::take(&mut s.new);
        new.sort_by_key(|o| o.desc.rank);
        for o in new {
            Session::apply_links(&o);
            let desc = o.desc;
            let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
            let mut cols = Vec::new();
            let mut dyn_defaults = Vec::new();
            // columns whose value the database generates: fetched by RETURNING (eager_defaults="auto")
            let mut fetch: Vec<usize> = desc.pks.to_vec();
            {
                let st = o.st.lock();
                for (i, c) in desc.cols.iter().enumerate() {
                    if matches!(st.vals[i], V::Unbound) {
                        if let ColDefault::Dyn(f) = c.default {
                            dyn_defaults.push((i, f));
                        }
                    }
                }
            }
            for (i, f) in dyn_defaults {
                let root = super::root_cx();
                let v = f(&root, V::None, vec![]).await?;
                if matches!(v, V::Sql(_)) && !fetch.contains(&i) {
                    fetch.push(i);
                }
                o.st.lock().vals[i] = v;
            }
            {
                let mut st = o.st.lock();
                for (i, c) in desc.cols.iter().enumerate() {
                    if matches!(st.vals[i], V::Unbound) {
                        if let ColDefault::Value(f) = c.default {
                            st.vals[i] = f();
                            if matches!(st.vals[i], V::Sql(_)) && !fetch.contains(&i) {
                                fetch.push(i); // a SQL expression default (`default=func.now()`)
                            }
                        }
                    }
                    if matches!(st.vals[i], V::Unbound) {
                        if c.server_default && !fetch.contains(&i) {
                            fetch.push(i);
                        }
                        continue;
                    }
                    cols.push(i);
                }
            }
            let vals = o.st.lock().vals.clone();
            r.sql = format!(
                "INSERT INTO {} ({}) VALUES (",
                desc.table,
                cols.iter().map(|i| desc.cols[*i].name).collect::<Vec<_>>().join(", ")
            );
            if cols.is_empty() {
                r.sql = format!("INSERT INTO {} DEFAULT VALUES", desc.table);
            } else {
                for (k, i) in cols.iter().enumerate() {
                    if k > 0 {
                        r.sql += ", ";
                    }
                    let c = &desc.cols[*i];
                    if let V::Sql(e) = &vals[*i] {
                        render(&mut r, e)?;
                    } else if c.ty == ColTy::JsonNull && vals[*i].is_none() {
                        r.bind(V::None, Some(ColTy::JsonNull));
                    } else if matches!(c.ty, ColTy::Json | ColTy::JsonNull) {
                        r.binds.push(json_param(&vals[*i])?);
                        r.sql += &format!("CAST(${} AS JSON)", r.binds.len());
                    } else {
                        r.bind(vals[*i].clone(), Some(c.ty));
                    }
                }
                r.sql.push(')');
            }
            r.sql += &format!(" RETURNING {}", fetch.iter().map(|i| desc.cols[*i].name).collect::<Vec<_>>().join(", "));
            if let Some(f) = s.savepoints.last_mut() {
                f.inserted.push(o.clone());
            }
            let rows = Session::run(s, &r).await?;
            let mut got = Vec::with_capacity(fetch.len());
            for (k, i) in fetch.iter().enumerate() {
                let mut v = map_col(&desc.cols[*i], decode(&rows[0], k, row_tz())?);
                if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = desc.cols[*i].ty {
                    v = f(&super::root_cx(), V::None, vec![v, V::None]).await?;
                }
                got.push(v);
            }
            let pk;
            {
                let mut st = o.st.lock();
                for (i, v) in fetch.iter().zip(got) {
                    st.vals[*i] = v;
                }
                pk = desc.pk_of(&st.vals);
                for (i, v) in st.vals.iter_mut().enumerate() {
                    if matches!(v, V::Sql(_)) {
                        *v = V::Unbound; // SQL-side default: expired until refreshed
                    } else if matches!(v, V::Unbound) && !desc.cols[i].server_default {
                        *v = V::None; // never set, no default: SQLAlchemy reads None without loading
                    }
                }
                st.committed = st.vals.clone();
                st.modified = vec![false; desc.cols.len()];
                st.status = Status::Persistent;
            }
            s.identity.insert(ident(desc, &pk)?, &o);
        }
        // UPDATEs of modified persistent objects
        let objs: Vec<Arc<ObjCell>> = s.identity.values();
        for o in objs {
            Session::apply_links(&o);
            let desc = o.desc;
            let (changes, pk) = {
                let st = o.st.lock();
                if st.status != Status::Persistent {
                    continue;
                }
                let mut ch = Vec::new();
                for i in 0..desc.cols.len() {
                    if st.modified[i] && !ops::eq_bool(&st.vals[i], &st.committed[i]) {
                        ch.push((i, st.vals[i].clone()));
                    }
                }
                (ch, desc.pk_of(&st.committed))
            };
            if changes.is_empty() {
                o.st.lock().modified = vec![false; desc.cols.len()];
                continue;
            }
            let mut r = Rend { sql: format!("UPDATE {} SET ", desc.table), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
            let mut first = true;
            for (i, v) in &changes {
                if !first {
                    r.sql += ", ";
                }
                first = false;
                let c = &desc.cols[*i];
                r.sql += &format!("{}=", c.name);
                if c.ty == ColTy::JsonNull && v.is_none() {
                    r.bind(V::None, Some(ColTy::JsonNull));
                } else if matches!(c.ty, ColTy::Json | ColTy::JsonNull) {
                    r.binds.push(json_param(v)?);
                    r.sql += &format!("CAST(${} AS JSON)", r.binds.len());
                } else {
                    r.bind(v.clone(), Some(c.ty));
                }
            }
            let mut expire = Vec::new();
            let mut prefetched = Vec::new();
            for (i, c) in desc.cols.iter().enumerate() {
                if changes.iter().any(|(j, _)| *j == i) {
                    continue;
                }
                match c.onupdate.eval().await? {
                    None => {}
                    Some(V::Sql(e)) => {
                        r.sql += &format!(", {}=", c.name);
                        render(&mut r, &e)?;
                        expire.push(i);
                    }
                    Some(v) => {
                        r.sql += &format!(", {}=", c.name);
                        r.bind(v.clone(), Some(c.ty));
                        prefetched.push((i, v));
                    }
                }
            }
            desc.where_pk(&mut r, &pk);
            Session::run_exec(s, &r).await?;
            if let Some(f) = s.savepoints.last_mut() {
                f.updated.push(o.clone());
            }
            let mut st = o.st.lock();
            for (i, v) in prefetched {
                st.vals[i] = v;
            }
            for i in expire {
                st.vals[i] = V::Unbound;
                st.expired = true;
            }
            st.committed = st.vals.clone();
            st.modified = vec![false; desc.cols.len()];
        }
        // DELETEs
        for o in std::mem::take(&mut s.deleted) {
            let desc = o.desc;
            let pk = o.pk();
            let mut r = Rend { sql: format!("DELETE FROM {}", desc.table), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
            desc.where_pk(&mut r, &pk);
            Session::run_exec(s, &r).await?;
            s.identity.remove(&ident(desc, &pk)?);
            o.st.lock().status = Status::Deleted;
            if let Some(f) = s.savepoints.last_mut() {
                f.deleted.push(o.clone());
            }
        }
        s.strong.clear();
        Ok(())
    }

    /// `await session.begin_nested()`: flush, then SAVEPOINT
    pub async fn begin_nested(&self) -> R<V> {
        let mut s = self.0.lock().await;
        Session::flush_inner(&mut s).await?;
        Session::begin(&mut s).await?;
        s.sp_seq += 1;
        let name = format!("sa_savepoint_{}", s.sp_seq);
        let tx = s.tx.as_mut().unwrap();
        sqlx::query(&format!("SAVEPOINT {name}")).execute(&mut **tx).await?;
        s.savepoints.push(SpFrame { name: name.clone(), inserted: vec![], updated: vec![], deleted: vec![] });
        Ok(V::native(super::v::Native::Savepoint(self.clone(), name)))
    }

    /// `savepoint.commit()` (flush, RELEASE) / `savepoint.rollback()` (ROLLBACK TO, session state undone)
    pub async fn end_savepoint(&self, name: &str, commit: bool) -> R<()> {
        let mut s = self.0.lock().await;
        let Some(i) = s.savepoints.iter().position(|f| f.name == name) else {
            return Err(Exc::msg(&INVALID_REQUEST_ERROR, "This nested transaction is inactive"));
        };
        if commit {
            Session::flush_inner(&mut s).await?;
            let tx = s.tx.as_mut().ok_or_else(|| Exc::msg(&INVALID_REQUEST_ERROR, "This nested transaction is inactive"))?;
            sqlx::query(&format!("RELEASE SAVEPOINT {name}")).execute(&mut **tx).await?;
            let frames: Vec<SpFrame> = s.savepoints.drain(i..).collect();
            if let Some(parent) = s.savepoints.last_mut() {
                for f in frames {
                    parent.inserted.extend(f.inserted);
                    parent.updated.extend(f.updated);
                    parent.deleted.extend(f.deleted);
                }
            }
            return Ok(());
        }
        let tx = s.tx.as_mut().ok_or_else(|| Exc::msg(&INVALID_REQUEST_ERROR, "This nested transaction is inactive"))?;
        sqlx::query(&format!("ROLLBACK TO SAVEPOINT {name}")).execute(&mut **tx).await?;
        let frames: Vec<SpFrame> = s.savepoints.drain(i..).collect();
        // pending objects added inside the savepoint are expunged with it
        for o in std::mem::take(&mut s.new) {
            o.st.lock().status = Status::Transient;
        }
        for f in frames {
            for o in f.inserted {
                let pk = o.pk();
                let mut st = o.st.lock();
                st.status = Status::Transient;
                if o.desc.pks.len() == 1 {
                    st.vals[o.desc.pk] = V::None;
                }
                drop(st);
                if !matches!(pk, V::None | V::Unbound) {
                    s.identity.remove(&ident(o.desc, &pk)?);
                }
            }
            for o in f.updated.into_iter().chain(f.deleted) {
                let pk = o.pk();
                {
                    let mut st = o.st.lock();
                    st.status = Status::Persistent;
                    for c in 0..o.desc.cols.len() {
                        if !o.desc.is_pk(c) {
                            st.vals[c] = V::Unbound;
                        }
                    }
                    st.expired = true;
                    st.modified = vec![false; o.desc.cols.len()];
                }
                s.identity.insert(ident(o.desc, &pk)?, &o);
            }
        }
        Ok(())
    }

    /// `engine.begin()`: a connection whose transaction commits at the end of its `async with`
    pub fn begin_block(self) -> Session {
        if let Ok(mut s) = self.0.try_lock() {
            s.commit_on_exit = true;
        }
        self
    }

    pub fn commits_on_exit(&self) -> bool {
        self.0.try_lock().map(|s| s.commit_on_exit).unwrap_or(false)
    }

    /// `metadata.create_all(conn)` (checkfirst, as SQLAlchemy runs it): every named enum type absent from
    /// `pg_type` is created (its table may exist: `MetaData.before_create`), then each table absent from
    /// `pg_class`, with its indexes and comments, then the foreign keys of cycles of the tables created.
    /// Same catalog queries as SQLAlchemy's PostgreSQL dialect.
    /// (Statements unprepared but through `query`: `raw_sql`'s future is not `Send` for every lifetime here.)
    pub async fn create_all(&self, ddl: &'static Ddl) -> R<()> {
        const HAS_TABLES: &str = "SELECT c.relname::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n \
            ON n.oid = c.relnamespace WHERE c.relname::text = ANY($1) AND c.relkind = ANY(ARRAY['r', 'p', 'f', 'v', 'm']) \
            AND pg_catalog.pg_table_is_visible(c.oid) AND n.nspname != 'pg_catalog'";
        const HAS_TYPE: &str = "SELECT t.typname::text FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n \
            ON n.oid = t.typnamespace WHERE t.typname::text = $1 AND pg_catalog.pg_type_is_visible(t.oid) \
            AND n.nspname != 'pg_catalog'";
        let mut s = self.0.lock().await;
        Session::begin(&mut s).await?;
        let tx = s.tx.as_mut().unwrap();
        let names: Vec<String> = ddl.tables.iter().map(|t| t.name.to_string()).collect();
        let existing: Vec<String> =
            sqlx::query_scalar(HAS_TABLES).bind(names).fetch_all(&mut **tx).await.map_err(|e| sql_error(e, HAS_TABLES))?;
        for &(name, sql) in ddl.types {
            let found: Option<String> =
                sqlx::query_scalar(HAS_TYPE).bind(name).fetch_optional(&mut **tx).await.map_err(|e| sql_error(e, HAS_TYPE))?;
            if found.is_none() {
                sqlx::query(sql).persistent(false).execute(&mut **tx).await.map_err(|e| sql_error(e, sql))?;
            }
        }
        let created: Vec<&DdlTable> = ddl.tables.iter().filter(|t| !existing.iter().any(|e| e == t.name)).collect();
        let pre18 = if created.iter().any(|t| t.create_pre18.is_some()) {
            const VERSION: &str = "SELECT current_setting('server_version_num')";
            let v: String = sqlx::query_scalar(VERSION).fetch_one(&mut **tx).await.map_err(|e| sql_error(e, VERSION))?;
            v.parse::<u32>().unwrap_or(0) < 180000
        } else {
            false
        };
        for t in &created {
            for (i, &sql) in t.stmts.iter().enumerate() {
                let sql = match t.create_pre18 {
                    Some(pre) if i == 0 && pre18 => pre,
                    _ => sql,
                };
                sqlx::query(sql).persistent(false).execute(&mut **tx).await.map_err(|e| sql_error(e, sql))?;
            }
        }
        for t in &created {
            for &sql in t.alters {
                sqlx::query(sql).persistent(false).execute(&mut **tx).await.map_err(|e| sql_error(e, sql))?;
            }
        }
        Ok(())
    }

    pub async fn flush(&self) -> R<()> {
        let mut s = self.0.lock().await;
        Session::flush_inner(&mut s).await
    }

    /// `await session.connection()`: begins the session's transaction
    pub async fn connection(&self) -> R<V> {
        let mut s = self.0.lock().await;
        Session::begin(&mut s).await?;
        Ok(V::native(Native::SessConn(self.clone())))
    }

    /// `session.close()` / `aclose()`: only the state of a closed `connection()` is reset here (the
    /// request's dependency ends the session itself)
    pub async fn close(&self) -> R<()> {
        self.0.lock().await.conn_closed = false;
        Ok(())
    }

    /// `await (await session.connection()).close()`: the connection goes back to the pool (its
    /// transaction rolled back); the session raises ResourceClosedError until rollback() or close()
    pub async fn close_connection(&self) -> R<()> {
        let mut s = self.0.lock().await;
        if let Some(tx) = s.tx.take() {
            let _ = tx.rollback().await;
            s.conn_closed = true;
        }
        Ok(())
    }

    pub async fn commit(&self) -> R<()> {
        let mut s = self.0.lock().await;
        if s.conn_closed {
            return Err(Exc::msg(&INVALID_REQUEST_ERROR, "This transaction is inactive"));
        }
        Session::flush_inner(&mut s).await?;
        if let Some(tx) = s.tx.take() {
            tx.commit().await?;
        }
        if s.expire_on_commit {
            for o in s.identity.values() {
                let mut st = o.st.lock();
                for i in 0..o.desc.cols.len() {
                    if !o.desc.is_pk(i) {
                        st.vals[i] = V::Unbound;
                    }
                }
                st.unload_rels();
                st.expired = true;
            }
        }
        Ok(())
    }

    pub async fn rollback(&self) -> R<()> {
        let mut s = self.0.lock().await;
        s.conn_closed = false;
        if let Some(tx) = s.tx.take() {
            let _ = tx.rollback().await;
        }
        for o in std::mem::take(&mut s.new) {
            o.st.lock().status = Status::Transient;
        }
        let mut keep = IdMap::default();
        for (k, o) in s.identity.entries() {
            let mut st = o.st.lock();
            if st.committed.iter().all(|v| matches!(v, V::Unbound)) {
                continue;
            }
            st.expired = true;
            drop(st);
            keep.insert(k, &o);
        }
        s.identity = keep;
        s.strong.clear();
        // objects INSERTed during the rolled back transaction are transient again
        let gone: Vec<(usize, Key)> = s
            .identity
            .entries()
            .into_iter()
            .filter(|(_, o)| o.st.lock().status != Status::Persistent)
            .map(|(k, _)| k)
            .collect();
        for k in gone {
            s.identity.remove(&k);
        }
        for o in s.identity.values() {
            let mut st = o.st.lock();
            for i in 0..o.desc.cols.len() {
                if !o.desc.is_pk(i) {
                    st.vals[i] = V::Unbound;
                }
            }
            st.unload_rels();
            st.rel_set.iter_mut().for_each(|x| *x = false);
            st.links.clear();
            st.modified = vec![false; o.desc.cols.len()];
        }
        s.deleted.clear();
        Ok(())
    }

    /// `await session.refresh(obj, ["col", "rel", ...])`: only those attributes are reloaded.
    pub async fn refresh_attrs(&self, obj: &V, names: Vec<String>) -> R<()> {
        let o = match obj {
            V::Obj(o) => o.clone(),
            _ => return Err(Exc::type_error("session.refresh() needs a mapped object")),
        };
        let desc = o.desc;
        let (mut cols, mut rels) = (Vec::new(), Vec::new());
        for n in &names {
            if let Some(i) = desc.col_index(n) {
                cols.push(i);
            } else if let Some(i) = desc.rel_index(n) {
                rels.push(i);
            } else {
                return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("No such attribute '{n}' on {}", desc.name)));
            }
        }
        let mut s = self.0.lock().await;
        if !cols.is_empty() {
            let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
            render_select(&mut r, &Select { cols: vec![SelCol::Entity(desc)], ..Default::default() })?;
            desc.where_pk(&mut r, &o.pk());
            let rows = Session::run(&mut s, &r).await?;
            let row = rows.first().ok_or_else(|| Exc::msg(&SQLALCHEMY_ERROR, format!("Could not refresh instance '{}'", desc.name)))?;
            let mut decoded = Vec::new();
            for &i in &cols {
                let mut v = map_col(&desc.cols[i], decode(row, i, row_tz())?);
                if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = desc.cols[i].ty {
                    v = f(&Session::cx(&s)?, V::None, vec![v, V::None]).await?;
                }
                decoded.push(v);
            }
            let mut st = o.st.lock();
            for (i, v) in cols.into_iter().zip(decoded) {
                st.vals[i] = v.clone();
                st.committed[i] = v;
                st.modified[i] = false;
            }
        }
        for ri in rels {
            {
                let mut st = o.st.lock();
                st.rels[ri] = None;
                st.rel_snap[ri] = None;
            }
            Session::load_rel(&mut s, &[o.clone()], desc, ri).await?;
        }
        Session::decode_pending(&mut s).await
    }

    /// A synchronous session's lazy loader (autoflush first, as SQLAlchemy's loaders do): the expired
    /// columns in one SELECT by primary key (ObjectDeletedError when the row is gone), then the
    /// relationship `name` if that is what was read.
    async fn lazy_load(&self, o: &Arc<ObjCell>, name: &str) -> R<()> {
        let mut s = self.0.lock().await;
        if s.autoflush {
            Session::flush_inner(&mut s).await?;
        }
        let desc = o.desc;
        let expired: Vec<usize> = {
            let st = o.st.lock();
            if st.status != Status::Persistent {
                vec![]
            } else {
                (0..desc.cols.len()).filter(|&i| matches!(st.vals[i], V::Unbound)).collect()
            }
        };
        if !expired.is_empty() {
            let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
            render_select(&mut r, &Select { cols: vec![SelCol::Entity(desc)], ..Default::default() })?;
            desc.where_pk(&mut r, &o.pk());
            let rows = Session::run(&mut s, &r).await?;
            let Some(row) = rows.first() else {
                return Err(Exc::msg(
                    &OBJECT_DELETED_ERROR,
                    format!("Instance '<{} at 0x{:x}>' has been deleted, or its row is otherwise not present.", desc.name, Arc::as_ptr(o) as usize),
                ));
            };
            let mut decoded = Vec::new();
            for &i in &expired {
                let mut v = map_col(&desc.cols[i], decode(row, i, row_tz())?);
                if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = desc.cols[i].ty {
                    v = f(&Session::cx(&s)?, V::None, vec![v, V::None]).await?;
                }
                decoded.push(v);
            }
            let mut st = o.st.lock();
            for (i, v) in expired.into_iter().zip(decoded) {
                st.vals[i] = v.clone();
                st.committed[i] = v;
                st.modified[i] = false;
            }
            st.expired = false;
        }
        if let Some(ri) = desc.rel_index(name) {
            if o.st.lock().rels[ri].is_none() {
                Session::load_rel(&mut s, &[o.clone()], desc, ri).await?;
            }
        }
        Session::decode_pending(&mut s).await
    }

    pub async fn refresh(&self, obj: &V) -> R<()> {
        let o = match obj {
            V::Obj(o) => o.clone(),
            _ => return Err(Exc::type_error("session.refresh() needs a mapped object")),
        };
        let mut s = self.0.lock().await;
        let desc = o.desc;
        let pk = o.pk();
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
        render_select(&mut r, &Select { cols: vec![SelCol::Entity(desc)], wheres: vec![], ..Default::default() })?;
        desc.where_pk(&mut r, &pk);
        let rows = Session::run(&mut s, &r).await?;
        match rows.first() {
            Some(row) => {
                o.load_row(row, 0, row_tz())?;
                Session::loaded(&mut s, &o);
                // relationships: eager ones reloaded; lazy ones already loaded stay as they are
                // (SQLAlchemy's refresh leaves them, even loaded through a selectinload() option)
                {
                    let mut st = o.st.lock();
                    for (i, rd) in desc.rels.iter().enumerate() {
                        if matches!(rd.lazy, Lazy::Selectin) {
                            st.rels[i] = None;
                            st.rel_snap[i] = None;
                        }
                    }
                }
                Session::eager(&mut s, vec![o.clone()], desc, vec![], vec![desc as *const ModelDesc as usize]).await?;
                Session::decode_pending(&mut s).await
            }
            None => Err(Exc::msg(&SQLALCHEMY_ERROR, format!("Could not refresh instance '{}'", desc.name))),
        }
    }

    /// `await session.get(Model, pk)`
    pub async fn get(&self, model: &V, pk: &V) -> R {
        self.get_with(model, pk, vec![]).await
    }

    /// `await session.get(Model, pk, options=[selectinload(...)])` (options apply when it loads)
    pub async fn get_with(&self, model: &V, pk: &V, opts: Vec<LoadChain>) -> R {
        let desc = model_of(model)?;
        if pk.is_none() && desc.pks.len() == 1 {
            return Ok(V::None);
        }
        let pk = &desc.get_ident(pk)?;
        if matches!(pk, V::Tuple(t) if t.iter().all(|v| v.is_none())) {
            return Ok(V::None);
        }
        let mut s = self.0.lock().await;
        if let Some(o) = s.identity.get(&ident(desc, pk)?) {
            let (status, expired) = {
                let st = o.st.lock();
                (st.status, st.expired)
            };
            if status == Status::Deleted {
                return Ok(V::None);
            }
            if !expired {
                return Ok(V::Obj(o));
            }
        }
        if s.autoflush {
            Session::flush_inner(&mut s).await?;
        }
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
        render_select(&mut r, &Select { cols: vec![SelCol::Entity(desc)], ..Default::default() })?;
        desc.where_pk(&mut r, pk);
        let rows = Session::run(&mut s, &r).await?;
        match rows.first() {
            Some(row) => {
                let o = Session::entity(&mut s, desc, row, 0)?;
                Session::eager(&mut s, vec![o.clone()], desc, opts, vec![desc as *const ModelDesc as usize]).await?;
                Session::decode_pending(&mut s).await?;
                Ok(V::Obj(o))
            }
            None => Ok(V::None),
        }
    }

    /// an entity column of a result row: None when its key is NULL (the missing side of an outer join)
    fn entity_v(s: &mut SessInner, desc: &'static ModelDesc, row: &PgRow, off: usize) -> R<V> {
        for i in desc.pks {
            if !decode(row, off + i, row_tz())?.is_none() {
                return Ok(V::Obj(Session::entity(s, desc, row, off)?));
            }
        }
        Ok(V::None)
    }

    /// an object of the session whose row a statement returned: reloaded from it
    fn refresh_from_row(s: &mut SessInner, desc: &'static ModelDesc, row: &PgRow, off: usize) -> R<()> {
        let mut vals = vec![V::None; desc.cols.len()];
        for i in desc.pks {
            vals[*i] = decode(row, off + i, db_tz())?;
        }
        let key = ident(desc, &desc.pk_of(&vals))?;
        if let Some(o) = s.identity.get(&key) {
            o.load_row(row, off, db_tz())?;
        }
        Ok(())
    }

    fn entity(s: &mut SessInner, desc: &'static ModelDesc, row: &PgRow, off: usize) -> R<Arc<ObjCell>> {
        let mut vals = vec![V::None; desc.cols.len()];
        for i in desc.pks {
            vals[*i] = decode(row, off + i, row_tz())?;
        }
        let key = ident(desc, &desc.pk_of(&vals))?;
        if let Some(o) = s.identity.get(&key) {
            if o.st.lock().expired {
                o.load_row(row, off, row_tz())?;
                Session::loaded(s, &o);
            }
            return Ok(o);
        }
        let o = ObjCell::transient(desc);
        o.load_row(row, off, row_tz())?;
        Session::loaded(s, &o);
        *o.sess.lock() = s.me.clone();
        s.identity.insert(key, &o);
        Ok(o)
    }

    /// decoded rows of a SELECT (or of a RETURNING clause): entities through the identity map, eager
    /// relationships, TypeDecorator results
    async fn select_result(s: &mut SessInner, sel: &Select, rows: Vec<PgRow>) -> R {
        let s = &mut *s;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut vals = Vec::new();
            let mut off = 0;
            for c in &sel.cols {
                match c {
                    SelCol::Entity(m) => {
                        vals.push(Session::entity_v(s, m, row, off)?);
                        off += m.cols.len();
                    }
                    SelCol::AEntity(a) => {
                        vals.push(Session::entity_v(s, a.model, row, off)?);
                        off += a.model.cols.len();
                    }
                    SelCol::Expr(e) => {
                        let v = decode(row, off, row_tz())?;
                        // an expression typed by a Numeric(asdecimal=False) column is read as float
                        let v = match (v, hint_of(e)) {
                            (V::Decimal(d), Some(ColTy::NumFloat)) => V::Float(d.to_f64()),
                            (v, _) => v,
                        };
                        vals.push(match e {
                            Sql::Col(m, ci) => map_col(&m.cols[*ci], v),
                            Sql::ACol(a, ci) => map_col(&a.model.cols[*ci], v),
                            _ => v,
                        });
                        off += 1;
                    }
                }
            }
            out.push(vals);
        }
        for (k, c) in sel.cols.iter().enumerate() {
            if let SelCol::Entity(m) = c {
                if m.rels.is_empty() {
                    continue;
                }
                let mut objs: Vec<Arc<ObjCell>> = Vec::new();
                for row in &out {
                    if let V::Obj(o) = &row[k] {
                        if !objs.iter().any(|x| Arc::ptr_eq(x, o)) {
                            objs.push(o.clone());
                        }
                    }
                }
                Session::eager(s, objs, *m, sel.loads.clone(), vec![*m as *const ModelDesc as usize]).await?;
            }
        }
        let n = out.len() as i64;
        Session::decode_pending(s).await?;
        // selected columns of a TypeDecorator: process_result_value too
        for (k, c) in sel.cols.iter().enumerate() {
            if let SelCol::Expr(Sql::Col(m, ci)) = c {
                if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = m.cols[*ci].ty {
                    for row in out.iter_mut() {
                        row[k] = f(&Session::cx(s)?, V::None, vec![row[k].clone(), V::None]).await?;
                    }
                }
            }
        }
        let names = Some(Arc::new(sel.cols.iter().map(col_key).collect::<Vec<_>>()));
        Ok(V::Result(Arc::new(Mutex::new(QResult { rows: Some(out), width: sel.cols.len(), scalars: false, rowcount: n, names }))))
    }

    /// `await session.execute(stmt)`
    pub async fn execute(&self, stmt: &V) -> R {
        self.execute_params(stmt, None).await
    }

    /// `session.execute(stmt, params)`: bind parameters for a `text()` statement
    pub async fn execute_params(&self, stmt: &V, params: Option<&V>) -> R {
        if let (V::Sql(x), Some(p)) = (stmt, params) {
            if !matches!(&**x, Sql::Text(_)) && !p.is_none() {
                return Err(Exc::type_error("py2axum: execute(statement, params) is only supported for text()"));
            }
        }
        if let V::Sql(x) = stmt {
            if let Sql::Text(q) = &**x {
                let (sql_text, binds) = text_binds(q, params)?;
                let mut s = self.0.lock().await;
                if s.autoflush {
                    Session::flush_inner(&mut s).await?;
                }
                // each value rendered by the usual binder (`$n` with its type), then put in place
                let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
                let mut marks = Vec::new();
                for v in binds {
                    let start = r.sql.len();
                    r.bind(v, None);
                    marks.push(r.sql[start..].to_string());
                }
                let mut final_sql = sql_text;
                for (i, m) in marks.iter().enumerate() {
                    final_sql = final_sql.replace(&format!("\u{0}{i}\u{0}"), m);
                }
                r.sql = final_sql;
                let rows = Session::run(&mut s, &r).await?;
                let width = rows.first().map(|r| r.len()).unwrap_or(0);
                let out = rows.iter().map(|row| (0..row.len()).map(|i| decode(row, i, row_tz())).collect::<R<Vec<_>>>()).collect::<R<Vec<_>>>()?;
                let n = out.len() as i64;
                let names = rows.first().map(|r| Arc::new(r.columns().iter().map(|c| Arc::from(c.name())).collect::<Vec<Arc<str>>>()));
                return Ok(V::Result(Arc::new(Mutex::new(QResult { rows: Some(out), width, scalars: false, rowcount: n, names }))));
            }
        }
        let mut st = match stmt {
            V::Sql(s) => (**s).clone(),
            other => return Err(Exc::type_error(format!("session.execute() needs a statement, got {}", other.type_name()))),
        };
        // Core `update()` applies the columns' `onupdate=` it does not set; the ORM expires them on
        // the loaded objects it synchronizes
        let mut implicit = Vec::new();
        if let Sql::Update(m, _, sets, _, _) = &mut st {
            for (i, c) in m.cols.iter().enumerate() {
                if !sets.iter().any(|(j, _)| *j == i) {
                    if let Some(v) = c.onupdate.eval().await? {
                        sets.push((i, to_sql(&v, Some(c.ty))));
                        implicit.push(i);
                    }
                }
            }
        }
        // Core `insert()` applies the Python-side column defaults to the columns a row does not set
        if let Sql::Insert(ins) = &mut st {
            let m = ins.m;
            let present: Vec<usize> = ins.rows.iter().flat_map(|r| r.iter().map(|(i, _)| *i)).collect();
            if ins.rows.is_empty() {
                ins.rows.push(vec![]);
            }
            let multi = ins.rows.len() > 1;
            for row in ins.rows.iter_mut() {
                for (i, c) in m.cols.iter().enumerate() {
                    if row.iter().any(|(j, _)| *j == i) {
                        continue;
                    }
                    if present.contains(&i) && multi && matches!(c.default, ColDefault::None) {
                        // a row of a multi-VALUES insert without a key the others have
                        return Err(Exc::msg(&COMPILE_ERROR, format!(
                            "INSERT value for column {}.{} is explicitly rendered as a boundparameter in the VALUES clause; a Python-side value or SQL expression is required",
                            m.table, c.name)));
                    }
                    let v = match c.default {
                        ColDefault::Value(f) => Some(f()),
                        ColDefault::Dyn(f) => Some(f(&super::root_cx(), V::None, vec![]).await?),
                        _ => None,
                    };
                    if let Some(v) = v {
                        row.push((i, to_sql(&v, Some(c.ty))));
                    }
                }
            }
        }
        let mut s = self.0.lock().await;
        if s.autoflush {
            Session::flush_inner(&mut s).await?;
        }
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new() };
        match &st {
            Sql::Insert(ins) => {
                render(&mut r, &st)?;
                if ins.returning.is_empty() {
                    // an ORM-enabled INSERT (target: a mapped class) reports the rows inserted over asyncpg
                    // (its command tag), -1 over psycopg (measured, SQLAlchemy 2.0.44)
                    let n = Session::run_exec(&mut s, &r).await? as i64;
                    let rowcount = if asyncpg() { n } else { -1 };
                    return Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount, names: None }))));
                }
                let rows = Session::run(&mut s, &r).await?;
                let sel = Select { cols: ins.returning.clone(), ..Default::default() };
                Session::select_result(&mut s, &sel, rows).await
            }
            Sql::Select(sel) => {
                render_select(&mut r, sel)?;
                let rows = Session::run(&mut s, &r).await?;
                Session::select_result(&mut s, sel, rows).await
            }
            Sql::Update(m, w, sets, sync, ret) if !ret.is_empty() => {
                // UPDATE ... RETURNING: the rows as a SELECT gives them; returned entities already in the
                // session are refreshed with them (SQLAlchemy populates the existing objects)
                let _ = (m, w, sets, sync);
                render(&mut r, &st)?;
                let rows = Session::run(&mut s, &r).await?;
                let sel = Select { cols: ret.clone(), ..Default::default() };
                for row in &rows {
                    let mut off = 0;
                    for c in ret {
                        match c {
                            SelCol::Entity(e) => {
                                Session::refresh_from_row(&mut s, e, row, off)?;
                                off += e.cols.len();
                            }
                            _ => off += 1,
                        }
                    }
                }
                Session::select_result(&mut s, &sel, rows).await
            }
            Sql::Update(m, w, sets, sync, _) => {
                render(&mut r, &st)?;
                let n = Session::run_exec(&mut s, &r).await? as i64;
                if !*sync {
                    return Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount: n, names: None }))));
                }
                // synchronize_session="auto" ("evaluate"): apply the new values to loaded objects
                for o in s.identity.values() {
                    if std::ptr::eq(o.desc, *m) && evaluates_true(&o, w) {
                        let mut ost = o.st.lock();
                        for (i, v) in sets {
                            if implicit.contains(i) {
                                ost.vals[*i] = V::Unbound;
                                ost.expired = true;
                            } else if let Sql::Param(val, _, _) = v {
                                ost.vals[*i] = val.clone();
                                ost.committed[*i] = val.clone();
                            } else if let Sql::Null = v {
                                ost.vals[*i] = V::None;
                                ost.committed[*i] = V::None;
                            } else {
                                ost.vals[*i] = V::Unbound;
                                ost.expired = true;
                            }
                        }
                    }
                }
                Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount: n, names: None }))))
            }
            Sql::Delete(m, w, sync) => {
                render(&mut r, &st)?;
                let n = Session::run_exec(&mut s, &r).await? as i64;
                let gone: Vec<(usize, Key)> = s
                    .identity
                    .entries()
                    .into_iter()
                    .filter(|(_, o)| *sync && std::ptr::eq(o.desc, *m) && evaluates_true(o, w))
                    .map(|(k, _)| k)
                    .collect();
                for k in gone {
                    if let Some(o) = s.identity.remove(&k) {
                        o.st.lock().status = Status::Deleted;
                    }
                }
                Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount: n, names: None }))))
            }
            _ => Err(Exc::type_error("session.execute() needs a select(), update(), delete() or text()")),
        }
    }

    pub async fn scalar(&self, stmt: &V) -> R {
        match self.execute(stmt).await? {
            V::Result(res) => result_method(&res, "scalar"),
            _ => Ok(V::None),
        }
    }

    pub async fn scalars(&self, stmt: &V) -> R {
        match self.execute(stmt).await? {
            V::Result(res) => result_method(&res, "scalars"),
            _ => Ok(V::None),
        }
    }
}


/// "evaluate" synchronisation: does this loaded object match the WHERE clauses?
fn evaluates_true(o: &ObjCell, w: &[Sql]) -> bool {
    fn ev(o: &ObjCell, e: &Sql) -> Option<bool> {
        let st = o.st.lock();
        let val = |s: &Sql| -> Option<V> {
            match s {
                Sql::Col(m, i) if std::ptr::eq(*m, o.desc) => Some(st.vals[*i].clone()),
                Sql::Param(v, _, _) => Some(v.clone()),
                Sql::Null => Some(V::None),
                _ => None,
            }
        };
        match e {
            Sql::Bin("=", a, b) => Some(ops::eq_bool(&val(a)?, &val(b)?)),
            Sql::Bin("<>", a, b) => Some(!ops::eq_bool(&val(a)?, &val(b)?)),
            Sql::IsNull(a, neg) => Some(val(a)?.is_none() != *neg),
            Sql::In(a, items, neg) => {
                let x = val(a)?;
                let hit = items.iter().any(|i| val(i).map(|y| ops::eq_bool(&x, &y)).unwrap_or(false));
                Some(hit != *neg)
            }
            Sql::Bool("AND", items) => {
                drop(st);
                let mut all = true;
                for i in items {
                    all &= ev(o, i)?;
                }
                Some(all)
            }
            _ => None,
        }
    }
    w.iter().all(|c| ev(o, c).unwrap_or(false))
}


/// `Model.rel.has(criterion, **kw)` (many-to-one) / `.any(...)` (one-to-many): a correlated EXISTS
fn rel_exists(m: &'static ModelDesc, ri: usize, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let rel = &m.rels[ri];
    if (name == "has") == rel.uselist {
        return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("'{}()' not implemented for {} relationships", name, if rel.uselist { "collection" } else { "scalar" })));
    }
    let t = rel.target;
    let mut wheres = vec![Sql::Bin("=", Box::new(Sql::Col(t, rel.remote)), Box::new(Sql::Col(m, rel.local)))];
    for a in args {
        wheres.push(to_sql(a, None));
    }
    for (k, v) in kwargs {
        let i = t.col_index(k).ok_or_else(|| Exc::attr_error(format!("{} has no column {k}", t.name)))?;
        if let V::Sql(x) = sql_cmp(&V::Col(t, i), "=", v)? {
            wheres.push((*x).clone());
        }
    }
    let sel = Select { cols: vec![SelCol::Expr(Sql::Text("1".into()))], from: vec![t], wheres, ..Default::default() };
    Ok(sql(Sql::Exists(Box::new(sel))))
}

/// `sq.c` / `sq.c.name`
pub fn sql_attr(v: &V, name: &str) -> Option<R> {
    let V::Sql(x) = v else { return None };
    match (&**x, name) {
        (Sql::Subquery(q), "c" | "columns") => Some(Ok(sql(Sql::SubColumns(q.clone())))),
        (Sql::SubColumns(q), n) => {
            let known = q.sel.cols.iter().any(|c| match c {
                SelCol::Expr(Sql::Label(_, l)) => l == n,
                SelCol::Expr(Sql::Col(m, i)) => m.cols[*i].name == n,
                SelCol::Entity(m) => m.col_index(n).is_some(),
                SelCol::AEntity(a) => a.model.col_index(n).is_some(),
                _ => false,
            });
            Some(if known { Ok(sql(Sql::SubCol(q.clone(), n.to_string()))) } else { Err(Exc::attr_error(format!("{n}"))) })
        }
        (Sql::Insert(ins), "excluded") if ins.pg => Some(Ok(sql(Sql::Excluded(ins.m)))),
        (Sql::Excluded(m), n) => Some(match m.col_index(n) {
            Some(i) => Ok(sql(Sql::ExCol(m, i))),
            None => Err(Exc::attr_error(format!("{n}"))),
        }),
        (Sql::Alias(a), n) => Some(match a.model.col_index(n) {
            Some(i) => Ok(sql(Sql::ACol(a.clone(), i))),
            None => Err(Exc::attr_error(format!("type object 'AliasedClass' has no attribute '{n}' (py2axum: only columns of an aliased() model)"))),
        }),
        _ => None,
    }
}

/// `extract("year", expr)`: PostgreSQL returns numeric, read as a Decimal like SQLAlchemy over psycopg
pub fn extract(field: &V, e: &V) -> R {
    let f = ops::str_(field)?.to_ascii_uppercase();
    if !f.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
        return Err(Exc::value_error(format!("bad extract field {f}")));
    }
    Ok(sql(Sql::Extract(f, Box::new(to_sql(e, None)))))
}

/// `cast(expr, Type)`
pub fn cast(e: &V, ty: &V) -> R {
    let name = match ty {
        V::Str(s) => s.to_string(),
        V::Native(n) => match &**n {
            Native::Type(t) => t.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let pg = match name.as_str() {
        "String" | "Text" | "Unicode" | "str" => "VARCHAR",
        "Integer" | "int" => "INTEGER",
        "BigInteger" => "BIGINT",
        "Float" | "float" => "FLOAT",
        "Date" => "DATE",
        "Boolean" => "BOOLEAN",
        _ => return Err(Exc::type_error(format!("py2axum: cast(..., {name}) is not supported"))),
    };
    Ok(sql(Sql::Cast(Box::new(to_sql(e, None)), pg)))
}

/// `sqlalchemy.orm.attributes.flag_modified(obj, "attr")`: the column is written at the next flush
pub fn flag_modified(obj: &V, name: &V) -> R {
    let V::Obj(o) = obj else { return Err(Exc::type_error("flag_modified() needs a mapped object")) };
    let n = ops::str_(name)?;
    let i = o.desc.col_index(&n).ok_or_else(|| Exc::msg(&INVALID_REQUEST_ERROR, format!("Attribute '{n}' is not a column of {}", o.desc.name)))?;
    let v = o.st.lock().vals[i].clone();
    o.set_attr(&n, v)?;
    o.st.lock().committed[i] = V::Unbound; // differs from anything: the flush writes it
    Ok(V::None)
}

// ---------------------------------------------------------------- legacy Query (session.query)

/// `session.query(*entities)`: a `select()` bound to its session. Building methods return a new Query;
/// the others execute it like SQLAlchemy 2.0's `Query._iter()` (a single entity gives the objects,
/// anything else rows), with autoflush as any execution.
pub fn query(sess: &Session, ents: Vec<V>) -> R {
    Ok(V::native(Native::Query(sess.clone(), select(ents)?)))
}

fn query_sel(sel: &V) -> R<&Select> {
    match sel {
        V::Sql(s) => match &**s {
            Sql::Select(x) => Ok(x),
            _ => Err(Exc::type_error("py2axum: a Query needs a select()")),
        },
        _ => Err(Exc::type_error("py2axum: a Query needs a select()")),
    }
}

/// the mapped class of `query(Model)` (one entity selected)
fn query_entity(sel: &V) -> R<Option<&'static ModelDesc>> {
    Ok(match query_sel(sel)?.cols.as_slice() {
        [SelCol::Entity(m)] => Some(*m),
        _ => None,
    })
}

async fn query_rows(sess: &Session, sel: &V) -> R<Vec<V>> {
    let single = query_entity(sel)?.is_some();
    let V::Result(res) = sess.execute(sel).await? else { return Err(Exc::type_error("py2axum: a Query returned no rows")) };
    let res = if single {
        match result_method(&res, "scalars")? {
            V::Result(r) => r,
            _ => unreachable!(),
        }
    } else {
        res
    };
    let rows = res.lock().take_rows()?;
    Ok(rows)
}

/// The Query's own target for `delete()`/`update()`: one entity, no joins, ordering or limits.
fn query_bulk_target(sel: &V, name: &str) -> R<(&'static ModelDesc, Vec<Sql>)> {
    let s = query_sel(sel)?;
    let m = query_entity(sel)?.ok_or_else(|| Exc::type_error(format!("py2axum: Query.{name}() needs query(Model)")))?;
    if !s.joins.is_empty() || !s.join_subs.is_empty() || !s.order.is_empty() || !s.group.is_empty() || s.limit.is_some() || s.offset.is_some() || s.distinct || !s.from_subs.is_empty() {
        return Err(Exc::type_error(format!("py2axum: Query.{name}() is supported on query(Model).filter(...) only")));
    }
    Ok((m, s.wheres.clone()))
}

fn sync_option(name: &str, args: &[V], kwargs: &[(String, V)], pos: usize) -> R<Option<V>> {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "synchronize_session") {
        return Err(Exc::type_error(format!("py2axum: Query.{name}({k}=) is not supported")));
    }
    if args.len() > pos + 1 {
        return Err(Exc::type_error(format!("py2axum: Query.{name}() takes at most {} positional arguments", pos + 1)));
    }
    Ok(kwargs.first().map(|(_, v)| v.clone()).or_else(|| args.get(pos).cloned()))
}

pub async fn query_method(sess: &Session, sel: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let wrap = |v: V| V::native(Native::Query(sess.clone(), v));
    let no_args = |args: &Vec<V>, kwargs: &Vec<(String, V)>| -> R<()> {
        if args.is_empty() && kwargs.is_empty() {
            Ok(())
        } else {
            Err(Exc::type_error(format!("py2axum: Query.{name}() takes no arguments")))
        }
    };
    match name {
        "filter" | "where" | "filter_by" | "order_by" | "group_by" | "having" | "join" | "outerjoin" | "options" | "limit" | "offset"
        | "distinct" | "select_from" | "with_for_update" => Ok(wrap(sql_method(sel, name, args, kwargs)?)),
        "with_entities" => Ok(wrap(sql_method(sel, "with_only_columns", args, kwargs)?)),
        "subquery" | "scalar_subquery" | "exists" | "label" => sql_method(sel, name, args, kwargs),
        "all" => {
            no_args(&args, &kwargs)?;
            Ok(V::list(query_rows(sess, sel).await?))
        }
        "first" => {
            no_args(&args, &kwargs)?;
            let one = sql_method(sel, "limit", vec![V::Int(1)], vec![])?;
            Ok(query_rows(sess, &one).await?.into_iter().next().unwrap_or(V::None))
        }
        "one" | "one_or_none" | "scalar" => {
            no_args(&args, &kwargs)?;
            let rows = query_rows(sess, sel).await?;
            match rows.len() {
                0 if name == "one" => Err(Exc::msg(&NO_RESULT_FOUND, "No row was found when one was required")),
                0 => Ok(V::None),
                1 => {
                    let r = rows.into_iter().next().unwrap();
                    // Query.scalar(): one(), then the first element of a row
                    if name == "scalar" && query_entity(sel)?.is_none() {
                        ops::getitem(&r, &V::Int(0))
                    } else {
                        Ok(r)
                    }
                }
                _ => Err(Exc::msg(&MULTIPLE_RESULTS_FOUND, "Multiple rows were found when exactly one was required")),
            }
        }
        "count" => {
            // SELECT count(*) FROM (<query>) AS anon_1
            no_args(&args, &kwargs)?;
            let sub = sql_method(sel, "subquery", vec![], vec![])?;
            let c = sql_method(&select(vec![func("count", vec![])?])?, "select_from", vec![sub], vec![])?;
            sess.scalar(&c).await
        }
        "get" => {
            let m = query_entity(sel)?.ok_or_else(|| Exc::type_error("py2axum: Query.get() needs query(Model)"))?;
            if args.len() != 1 || !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: Query.get() takes the primary key"));
            }
            sess.get(&V::Class(m.class), &args[0]).await
        }
        "delete" => {
            let (m, w) = query_bulk_target(sel, name)?;
            let sync = match sync_option(name, &args, &kwargs, 0)? {
                None => true,
                Some(V::Bool(false)) => false,
                Some(V::Str(x)) if matches!(&*x, "auto" | "evaluate" | "fetch") => true,
                Some(_) => return Err(Exc::type_error("py2axum: Query.delete(synchronize_session=) must be False, 'auto', 'evaluate' or 'fetch'")),
            };
            match sess.execute(&sql(Sql::Delete(m, w, sync))).await? {
                V::Result(r) => result_attr(&r, "rowcount"),
                other => Ok(other),
            }
        }
        "update" => {
            let (m, w) = query_bulk_target(sel, name)?;
            let values = args.first().cloned().ok_or_else(|| Exc::type_error("py2axum: Query.update() takes a dict of values"))?;
            let mut u = sql(Sql::Update(m, w, vec![], true, vec![]));
            if let Some(opt) = sync_option(name, &args, &kwargs, 1)? {
                u = sql_method(&u, "execution_options", vec![], vec![("synchronize_session".into(), opt)])?;
            }
            u = sql_method(&u, "values", vec![values], vec![])?;
            match sess.execute(&u).await? {
                V::Result(r) => result_attr(&r, "rowcount"),
                other => Ok(other),
            }
        }
        _ => Err(Exc::attr_error(format!("'Query' object has no attribute '{name}' (not supported by py2axum)"))),
    }
}

/// `for x in <iterable>`: a Query is executed (`Query.__iter__`), anything else iterated as usual.
pub async fn iter(v: &V) -> R<Vec<V>> {
    if let V::Native(n) = v {
        if let Native::Query(sess, sel) = &**n {
            return query_rows(sess, sel).await;
        }
    }
    ops::iter(v)
}

// ---------------------------------------------------------------- sqlalchemy.inspect

fn mapper_ns(desc: &'static ModelDesc) -> R {
    let cols = desc
        .cols
        .iter()
        .map(|c| super::libs::namespace(&[], &[("key".to_string(), V::str(c.name))]))
        .collect::<R<Vec<_>>>()?;
    super::libs::namespace(&[], &[("column_attrs".to_string(), V::list(cols))])
}

/// `sqlalchemy.inspect(x, raiseerr=False)`: a mapped object's state (`unloaded`: the attributes missing
/// from its `__dict__`, `mapper.column_attrs[i].key`), a mapped class's mapper (`column_attrs`), None
/// for anything else. Only those attributes: the others raise AttributeError.
pub fn inspect(v: &V) -> R {
    match v {
        V::Obj(o) => {
            let present = match o.instance_dict()? {
                V::Dict(d) => d.lock().values().map(|(k, _)| ops::str_(k)).collect::<R<Vec<_>>>()?,
                _ => vec![],
            };
            let mut m = indexmap::IndexMap::new();
            for name in o.desc.cols.iter().map(|c| c.name).chain(o.desc.rels.iter().map(|r| r.name)) {
                if !present.iter().any(|p| p == name) {
                    m.insert(Key::Str(name.into()), V::str(name));
                }
            }
            super::libs::namespace(
                &[],
                &[("unloaded".to_string(), V::Set(Arc::new(Mutex::new(m)))), ("mapper".to_string(), mapper_ns(o.desc)?)],
            )
        }
        V::Class(c) => match c.kind {
            super::v::ClassKind::Model(d) => mapper_ns(d),
            _ => Ok(V::None),
        },
        _ => Ok(V::None),
    }
}
