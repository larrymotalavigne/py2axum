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
                    _ => Err(statement_error(&LOOKUP_ERROR, "builtins.LookupError", &format!("'{}' is not among the defined enum values of {}", ops::str_(other).unwrap_or_default(), self.desc.name), None)),
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
    /// the attribute (row keys, `session.get` dicts, `inspect`)
    pub name: &'static str,
    /// the SQL column name (`Column("metadata", JSON)` mapped to another attribute)
    pub sql: &'static str,
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
    /// `deferred_group=`: undeferred together (`undefer_group()`), loaded together when one is read
    pub group: Option<&'static str>,
    /// `deferred_raiseload=True`: reading it while unloaded raises InvalidRequestError instead of loading it
    pub raiseload: bool,
    /// a JSONB column: `contains`/`contained_by`/`has_key`/`has_any`/`has_all` are PostgreSQL's JSONB operators
    pub jsonb: bool,
    /// joined table inheritance: the table the column lives in, when it is not the model's (an inherited column)
    pub table: Option<&'static str>,
}

/// What a query does with a column of an entity it loads: load it, defer it (loaded when read), or leave
/// it to raise when read (`raiseload=True`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ColLoad {
    Load,
    Defer,
    Raise,
}

impl ColDesc {
    /// the mapping's own strategy (`deferred=`, `deferred_raiseload=`)
    pub fn default_load(&self) -> ColLoad {
        if !self.deferred {
            ColLoad::Load
        } else if self.raiseload {
            ColLoad::Raise
        } else {
            ColLoad::Defer
        }
    }
    /// the column's name in SQL (quoted when SQLAlchemy quotes it: `"end"`)
    pub fn q(&self) -> std::borrow::Cow<'static, str> {
        quote_ident(self.sql)
    }
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
    /// `sqlalchemy.ext.hybrid` attributes
    pub hybrids: &'static [HybridDesc],
    /// `__mapper_args__ = {"eager_defaults": True}`: an UPDATE also fetches (RETURNING) what its SQL `onupdate=` set
    pub eager_defaults: bool,
    /// single table inheritance: the mapped class this one extends (its columns come first, same positions)
    pub parent: Option<&'static ModelDesc>,
    /// `polymorphic_on`: the discriminator column of the hierarchy (a row's class is the one of its identity)
    pub poly_on: Option<usize>,
    /// `polymorphic_load="selectin"` / `"inline"`: the subclass's columns loaded with a query on its base
    pub poly_load: bool,
    /// `polymorphic_identity`
    pub identity: Option<&'static str>,
    /// every mapped subclass, at any depth
    pub subs: &'static [&'static ModelDesc],
    /// a Core `Table(...)` (no mapped class): selected as its columns, its columns read through `.c`
    pub core: bool,
    /// `composite()` attributes
    pub composites: &'static [CompositeDesc],
    /// joined table inheritance: (table, its key column, parent table, parent key column) from the root down; the
    /// class reads `root JOIN t1 ON ... JOIN t2 ON ...`
    pub joins: &'static [(&'static str, &'static str, &'static str, &'static str)],
}

/// A `composite(Cls, col_a, col_b, ...)`: on an instance the object `Cls(a, b)` built from its columns (kept until
/// they change), set by its values (a dataclass's fields in order, or `__composite_values__()`); on the class the
/// columns together (selected as one value, compared column by column, ANDed as SQLAlchemy's Comparator does).
pub struct CompositeDesc {
    pub name: &'static str,
    pub cols: &'static [usize],
    /// the class or callable building the value (evaluated once)
    pub ctor: super::v::ClsAttr,
    /// `Mapped[Cls | None]`: None when every column is NULL (`return_none_on`)
    pub none_on_null: bool,
}

impl ModelDesc {
    pub fn composite(&self, name: &str) -> Option<usize> {
        self.composites.iter().position(|c| c.name == name)
    }
}

/// the column values of a composite object: `__composite_values__()`, else a dataclass's fields in order
pub fn composite_values(v: &V, n: usize) -> R<Vec<V>> {
    if v.is_none() {
        return Ok(vec![V::None; n]);
    }
    if let Some(f) = ops::dunder(v, "__composite_values__") {
        return ops::iter(&ops::call_dunder(f, v, vec![], "__composite_values__")?);
    }
    match v {
        V::Inst(i) if i.desc.dataclass => Ok(i.vals.read().clone()),
        other => Err(Exc::type_error(format!(
            "py2axum: the value of a composite must be a dataclass or define __composite_values__(), not {}", other.type_name()))),
    }
}

/// `Cls(*values)` of a composite, from synchronous code (a dataclass's constructor does no I/O)
fn composite_build(c: &'static CompositeDesc, vals: Vec<V>) -> R {
    if c.none_on_null && vals.iter().all(|v| v.is_none()) {
        return Ok(V::None);
    }
    ops::block_on_sync(async move {
        let cx = super::root_cx();
        let f = (c.ctor)(&cx).await?;
        super::methods::call_value(&cx, &f, vals, vec![]).await
    }, c.name)
}

/// A `@hybrid_property` / `@hybrid_method`: on an instance its function (a property, or a method, in the method
/// table too); on the class its expression (or the same function given the class: SQL), its setter on assignment.
pub struct HybridDesc {
    pub name: &'static str,
    pub method: bool,
    pub get: pyd::MethodFn,
    pub expr: Option<pyd::MethodFn>,
    pub set: Option<pyd::MethodFn>,
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
    /// the INSERT order of the unit of work: a whole inheritance hierarchy is one group (rows in the order they
    /// were added, like SQLAlchemy's base mapper), after every table any of its classes references
    pub fn uow_rank(&'static self) -> usize {
        let root = self.root();
        root.subs.iter().map(|d| d.rank).chain(std::iter::once(root.rank)).max().unwrap_or(self.rank)
    }

    /// the base of its inheritance hierarchy (itself without inheritance)
    pub fn root(&'static self) -> &'static ModelDesc {
        let mut m = self;
        while let Some(p) = m.parent {
            m = p;
        }
        m
    }

    /// `SELECT <entity> WHERE <primary key> = ...` (`get()`, refresh, lazy load)
    fn pk_select(&'static self, pk: &V) -> Select {
        let wheres = self.pks.iter().zip(self.pk_parts(pk))
            .map(|(i, v)| Sql::Bin("=", Box::new(Sql::Col(self, *i)), Box::new(Sql::Param(v, Some(self.cols[*i].ty), 0))))
            .collect();
        Select { cols: vec![SelCol::Entity(self)], wheres, ..Default::default() }
    }

    fn pk_parts(&self, pk: &V) -> Vec<V> {
        match (self.pks.len(), pk) {
            (1, _) => vec![pk.clone()],
            (_, V::Tuple(t)) => t.to_vec(),
            _ => vec![pk.clone()],
        }
    }

    fn where_pk(&self, r: &mut Rend, pk: &V) {
        let parts = self.pk_parts(pk);
        for (k, (i, v)) in self.pks.iter().zip(parts).enumerate() {
            r.sql += if k == 0 { " WHERE " } else { " AND " };
            r.sql += &format!("{} = ", self.col_ref(*i));
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
    /// a column by its SQL name (foreign keys name the target table's column)
    pub fn sql_index(&self, sql: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.sql == sql)
    }
    /// the table's name in SQL (quoted like SQLAlchemy's PostgreSQL IdentifierPreparer: `"user"`)
    pub fn qt(&self) -> std::borrow::Cow<'static, str> {
        quote_ident(self.table)
    }

    /// `table.column` of a column (joined table inheritance: an inherited column is in its own table)
    pub fn col_ref(&self, i: usize) -> String {
        format!("{}.{}", quote_ident(self.cols[i].table.unwrap_or(self.table)), self.cols[i].q())
    }

    /// the tables a row of the class is read from (joined table inheritance: the root's, then each subclass's)
    pub fn tables(&self) -> Vec<&'static str> {
        match self.joins.first() {
            None => vec![self.table],
            Some(j) => std::iter::once(j.2).chain(self.joins.iter().map(|j| j.0)).collect(),
        }
    }

    /// the class in a FROM list: its table, or the join of its hierarchy's tables on their keys
    pub fn from_sql(&self) -> String {
        let mut out = self.tables()[0].to_string();
        out = quote_ident(&out).into_owned();
        for (t, c, pt, pc) in self.joins {
            out += &format!(" JOIN {} ON {}.{} = {}.{}", quote_ident(t), quote_ident(pt), quote_ident(pc), quote_ident(t), quote_ident(c));
        }
        out
    }
    pub fn rel_index(&self, name: &str) -> Option<usize> {
        self.rels.iter().position(|r| r.name == name)
    }
    pub fn hybrid(&self, name: &str) -> Option<&'static HybridDesc> {
        self.hybrids.iter().find(|h| h.name == name)
    }
}

/// `relationship(lazy=...)` (joined/subquery/immediate load like selectin: same objects, same order).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lazy {
    Select,
    Selectin,
    NoLoad,
    Raise,
    /// `undefer(Model.col)`: the last element of a loader chain is (Undefer, model, column index)
    Undefer,
    /// `defer(Model.col)` / `defer(Model.col, raiseload=True)`
    Defer,
    DeferRaise,
    /// `load_only(Model.col, ...)` (one element per column) / `load_only(..., raiseload=True)`
    Only,
    OnlyRaise,
    /// `defaultload(A.b)`: the relationship's own strategy, a path for the options after it
    Default,
    /// inheritance: load the columns of this subclass for the objects of it (`selectin_polymorphic()`,
    /// `with_polymorphic()`, `polymorphic_load`): (SubPoly, subclass, 0)
    SubPoly,
}

impl Lazy {
    /// a column option (the last element of its chain, its index is a column's)
    fn col(self) -> bool {
        matches!(self, Lazy::Undefer | Lazy::Defer | Lazy::DeferRaise | Lazy::Only | Lazy::OnlyRaise)
    }
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
    /// `secondary=`: the association table, its column referencing `local`, its column referencing `remote`
    pub secondary: Option<(&'static ModelDesc, usize, usize)>,
    /// `WriteOnlyMapped[...]` / `lazy="write_only"`: never loaded; the attribute is a `WriteOnlyCollection`
    pub write_only: bool,
    /// `cascade=` with `refresh-expire` ("all"): `session.expire(obj)` reaches the loaded related objects
    pub refresh_expire: bool,
}

/// A row of an association table to insert or delete at flush: (table, [(column, object, its column)] x2)
struct M2mOp {
    assoc: &'static ModelDesc,
    pairs: [(usize, Arc<ObjCell>, usize); 2],
    add: bool,
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
    /// the key expired by `session.expire(obj, ["id"])`: kept for the identity, read through a load
    pub pk_expired: bool,
    pub extra: Vec<(String, V)>,
    /// how each unloaded column loads when read while the object is not expired (an expired object's
    /// columns follow the mapping: `ColDesc::default_load`)
    pub defer: Vec<ColLoad>,
    /// relationship values, None = not loaded
    pub rels: Vec<Option<V>>,
    /// the composite objects read or set: (value, the column values it was built from)
    pub comps: Vec<Option<(V, Vec<V>)>>,
    /// the loader options under a relationship the query left to load lazily (`defaultload(A.b).load_only(...)`),
    /// applied when it loads
    pub rel_opts: Vec<Option<Arc<Vec<LoadChain>>>>,
    /// members of a loaded collection as of the last load/flush (diffed at flush)
    pub rel_snap: Vec<Option<Vec<V>>>,
    /// many-to-one assigned since the last flush
    pub rel_set: Vec<bool>,
    /// foreign key copies resolved at flush: (my column, source object or None for NULL, its column)
    pub links: Vec<(usize, Option<Arc<ObjCell>>, usize)>,
    /// columns an INSERT left unset (never given, no insert default): None when read, without IO, but out of
    /// SQLAlchemy's instance dict until set or loaded from a row
    pub absent: Vec<bool>,
    /// built from a row (not INSERTed by this process): `lazy="noload"` relationships are populated
    pub from_row: bool,
}

impl ObjCell {
    /// a persistent object out of any session (expunged): what is not loaded cannot be loaded
    fn detached(&self, st: &ObjState) -> bool {
        matches!(st.status, Status::Persistent | Status::Deleted) && self.sess.lock().strong_count() == 0
    }
}

/// an object expired as a whole: every column but the key unloaded, relationships unloaded, changes not
/// flushed dropped
fn expire_whole(o: &Arc<ObjCell>) {
    let mut st = o.st.lock();
    for i in 0..o.desc.cols.len() {
        if !o.desc.is_pk(i) {
            st.vals[i] = V::Unbound;
            st.absent[i] = false;
        }
        st.modified[i] = false;
    }
    st.unload_rels();
    st.rel_set.iter_mut().for_each(|x| *x = false);
    st.expired = true;
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
                pk_expired: false,
                extra: Vec::new(),
                defer: vec![ColLoad::Load; n],
                rels: vec![None; desc.rels.len()],
                rel_opts: vec![None; desc.rels.len()],
                comps: vec![None; desc.composites.len()],
                rel_snap: vec![None; desc.rels.len()],
                rel_set: vec![false; desc.rels.len()],
                links: Vec::new(),
                absent: vec![false; n],
                from_row: false,
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
            if rel.uselist && !fresh && !st.from_row {
                // noload on an object INSERTed here: an empty list each read, never set in the instance dict
                return Ok(V::list(vec![]));
            }
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
                st.rels[ri] = Some(V::None); // the lazy loader's None is set in the instance dict
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
        if self.detached(st) {
            return Err(Exc::msg(&DETACHED_INSTANCE_ERROR, format!(
                "Parent instance <{} at 0x{:x}> is not bound to a Session; lazy load operation of attribute '{}' cannot proceed \
                 (Background on this error at: https://sqlalche.me/e/21/bhk3)", self.desc.name, self as *const Self as usize, rel.name)));
        }
        // the lazy load's SELECT would need IO: SQLAlchemy's StatementError, caused by MissingGreenlet
        Err(greenlet_error())
    }

    fn rel_set(self: &Arc<Self>, ri: usize, v: V) -> R<()> {
        let rel = &self.desc.rels[ri];
        if rel.write_only {
            // a write-only collection is only given whole to a new object (`Model(coll=[...])`): its pending members
            let V::List(l) = &v else {
                return Err(Exc::type_error(format!("{}.{} needs a list", self.desc.name, rel.name)));
            };
            let mut st = self.st.lock();
            if !matches!(st.status, Status::Transient | Status::Pending) {
                return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                    "Collection \"{}.{}\" does not support implicit iteration; collection replacement operations can't be used",
                    self.desc.name, rel.name)));
            }
            if st.rels[ri].is_some() {
                return Err(Exc::type_error(format!(
                    "py2axum: assigning the write-only collection {}.{} of an object already loaded or filled is not supported \
                     (use add_all()/remove())", self.desc.name, rel.name)));
            }
            let items = l.lock().clone();
            wo_check(rel, &items)?;
            st.rels[ri] = Some(V::list(items));
            return Ok(());
        }
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
                        // the collection of a transient or pending object: SQLAlchemy initializes it (empty, nothing
                        // to load) and appends; a persistent one's unloaded collection only queues the append
                        None if matches!(nst.status, Status::Transient | Status::Pending) => nst.rels[bi] = Some(V::list(vec![me.clone()])),
                        _ => {}
                    }
                }
            }
        }
        // a collection assigned whole (`Parent(children=[...])`, `p.children = [...]`): each member's many-to-one
        // follows at once, like SQLAlchemy's backref events (an append to the list is only seen at flush)
        if let (Some(back), true, None) = (rel.back, rel.uselist, rel.secondary) {
            if let Some(bi) = rel.target.rel_index(back).filter(|bi| rel.target.rels[*bi].m2o) {
                let me = V::Obj(self.clone());
                let now = members(&v);
                for m in &now {
                    if let V::Obj(x) = m {
                        let cur = x.st.lock().rels[bi].clone();
                        if !matches!(&cur, Some(c) if same(c, &me)) {
                            x.rel_set(bi, me.clone())?;
                        }
                    }
                }
                for m in old.as_ref().map(members).unwrap_or_default() {
                    if let V::Obj(x) = &m {
                        if !now.iter().any(|y| same(y, &m)) && matches!(&x.st.lock().rels[bi], Some(c) if same(c, &me)) {
                            x.rel_set(bi, V::None)?;
                        }
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
        let key_expired = self.key_expired(&st);
        for (i, c) in self.desc.cols.iter().enumerate() {
            if !matches!(st.vals[i], V::Unbound) && !st.absent[i] && !(key_expired && self.desc.is_pk(i)) {
                items.push((V::str(c.name), st.vals[i].clone()));
            }
        }
        for (ci, c) in self.desc.composites.iter().enumerate() {
            if let Some((v, _)) = &st.comps[ci] {
                if c.cols.iter().all(|i| !matches!(st.vals[*i], V::Unbound)) {
                    items.push((V::str(c.name), v.clone()));
                }
            }
        }
        for (ri, r) in self.desc.rels.iter().enumerate() {
            if r.write_only {
                continue;
            }
            if let Some(v) = &st.rels[ri] {
                items.push((V::str(r.name), v.clone()));
            } else if r.lazy == Lazy::NoLoad && st.status == Status::Persistent && st.from_row {
                // lazy="noload" populates the attribute (empty) when the row is loaded (not after an INSERT)
                items.push((V::str(r.name), if r.uselist { V::list(vec![]) } else { V::None }));
            }
        }
        for (k, v) in st.extra.iter() {
            items.push((V::str(k), v.clone()));
        }
        V::dict_from(items)
    }

    /// the key is expired: the object expired as a whole (commit, expire_all, expire) or `expire(obj, ["id"])`.
    /// SQLAlchemy's dict no longer has it and reading it loads the row; the value is kept here for the identity map
    fn key_expired(&self, st: &ObjState) -> bool {
        st.status == Status::Persistent
            && (st.pk_expired || (st.expired && (0..self.desc.cols.len()).all(|i| self.desc.is_pk(i) || matches!(st.vals[i], V::Unbound))))
    }

    /// how the unloaded column `i` loads when read
    fn unloaded(&self, st: &ObjState, i: usize) -> ColLoad {
        if st.expired {
            self.desc.cols[i].default_load()
        } else {
            st.defer[i]
        }
    }

    pub fn get_attr_sync(self: &Arc<Self>, name: &str) -> R {
        if name == "__dict__" {
            return self.instance_dict();
        }
        if let Some(ri) = self.desc.rel_index(name) {
            if self.desc.rels[ri].write_only {
                return Ok(V::native(Native::WriteOnly(self.clone(), ri)));
            }
        }
        let mut st = self.st.lock();
        if let Some(ri) = self.desc.rel_index(name) {
            return self.rel_get(&mut st, ri);
        }
        if let Some(ci) = self.desc.composite(name) {
            let c = &self.desc.composites[ci];
            if let Some(i) = c.cols.iter().copied().find(|i| matches!(st.vals[*i], V::Unbound)) {
                if !matches!(st.status, Status::Transient | Status::Pending) {
                    drop(st);
                    // what reading that column raises (a lazy load in a synchronous session, then read again)
                    return Err(match self.get_attr_sync(self.desc.cols[i].name) {
                        Err(e) => e,
                        Ok(_) => Exc::runtime("py2axum: a composite's column is not loaded"),
                    });
                }
            }
            let vals: Vec<V> = c.cols.iter().map(|i| if matches!(st.vals[*i], V::Unbound) { V::None } else { st.vals[*i].clone() }).collect();
            if let Some((v, snap)) = &st.comps[ci] {
                if snap.len() == vals.len() && snap.iter().zip(&vals).all(|(a, b)| ops::eq(a, b).and_then(|x| ops::truthy(&x)).unwrap_or(false)) {
                    return Ok(v.clone());
                }
            }
            drop(st);
            let v = composite_build(c, vals.clone())?;
            self.st.lock().comps[ci] = Some((v.clone(), vals));
            return Ok(v);
        }
        if let Some(i) = self.desc.col_index(name) {
            // a key expired on its own reads like an unloaded column
            let cur = if self.desc.is_pk(i) && self.key_expired(&st) { &V::Unbound } else { &st.vals[i] };
            return match cur {
                V::Unbound => {
                    if st.status == Status::Transient || st.status == Status::Pending {
                        Ok(V::None)
                    } else if self.unloaded(&st, i) == ColLoad::Raise {
                        Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("'{}.{}' is not available due to raiseload=True", self.desc.name, name)))
                    } else if self.detached(&st) {
                        Err(Exc::msg(&DETACHED_INSTANCE_ERROR, format!(
                            "Instance <{} at 0x{:x}> is not bound to a Session; attribute refresh operation cannot proceed \
                             (Background on this error at: https://sqlalche.me/e/21/bhk3)", self.desc.name, Arc::as_ptr(self) as usize)))
                    } else {
                        Err(greenlet_error())
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
            Err(e) if is_greenlet_error(&e) => {
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
        if let Some(ci) = self.desc.composite(name) {
            // the composite's columns set from its values; the object itself is what a read returns
            let c = &self.desc.composites[ci];
            let vals = composite_values(&v, c.cols.len())?;
            if vals.len() != c.cols.len() {
                return Err(Exc::value_error(format!("py2axum: composite {} takes {} values, got {}", c.name, c.cols.len(), vals.len())));
            }
            for (i, x) in c.cols.iter().zip(vals.iter()) {
                self.set_attr(self.desc.cols[*i].name, x.clone())?;
            }
            self.st.lock().comps[ci] = Some((v, vals));
            return Ok(());
        }
        if let Some(ri) = self.desc.rel_index(name) {
            self.mark_dirty();
            return self.rel_set(ri, v);
        }
        if self.desc.col_index(name).is_some() {
            self.mark_dirty();
        }
        let mut st = self.st.lock();
        if let Some(i) = self.desc.col_index(name) {
            st.absent[i] = false;
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

    /// `plan`: what the statement's options do with each column (empty: the mapping's strategies)
    fn load_row(&self, row: &PgRow, offset: usize, tz: Tz, plan: &[ColLoad]) -> R<()> {
        self.load_row_from(row, offset, tz, plan, self.desc.cols.len())
    }

    /// `read`: the columns the row holds (those of the class queried); a subclass's own columns past them are
    /// not loaded (inheritance: read on access, like SQLAlchemy)
    fn load_row_from(&self, row: &PgRow, offset: usize, tz: Tz, plan: &[ColLoad], read: usize) -> R<()> {
        let mut vals = Vec::with_capacity(self.desc.cols.len());
        let mut defer = Vec::with_capacity(self.desc.cols.len());
        for i in 0..self.desc.cols.len() {
            let how = plan.get(i).copied().unwrap_or_else(|| self.desc.cols[i].default_load());
            vals.push(if i >= read || how != ColLoad::Load {
                V::Unbound
            } else {
                map_col(&self.desc.cols[i], decode(row, offset + i, tz)?)
            });
            defer.push(how);
        }
        let mut st = self.st.lock();
        st.defer = defer;
        st.comps.iter_mut().for_each(|c| *c = None);
        st.committed = vals.clone();
        st.vals = vals;
        st.absent = vec![false; self.desc.cols.len()];
        st.from_row = true;
        st.modified = vec![false; self.desc.cols.len()];
        st.status = Status::Persistent;
        st.expired = false;
        st.pk_expired = false;
        drop(st);
        self.build_composites()
    }

    /// the composites whose columns are all loaded, built now (SQLAlchemy's load and refresh handlers)
    fn build_composites(&self) -> R<()> {
        self.build_composites_of(None)
    }

    /// `only`: those composites (indexes), else all of them
    fn build_composites_of(&self, only: Option<&[usize]>) -> R<()> {
        for (ci, c) in self.desc.composites.iter().enumerate() {
            if only.is_some_and(|o| !o.contains(&ci)) {
                continue;
            }
            let vals: Option<Vec<V>> = {
                let st = self.st.lock();
                c.cols.iter().map(|i| if matches!(st.vals[*i], V::Unbound) { None } else { Some(st.vals[*i].clone()) }).collect()
            };
            if let Some(vals) = vals {
                let v = composite_build(c, vals.clone())?;
                self.st.lock().comps[ci] = Some((v, vals));
            }
        }
        Ok(())
    }
}

/// `Model(*args, **kwargs)` of a mapped class with its own `__init__`: run on a new transient object (the
/// discriminator of single table inheritance already set, as SQLAlchemy's instrumented constructor does)
pub async fn construct_init(cx: &super::Cx, desc: &'static ModelDesc, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let V::Obj(obj) = construct(desc, vec![])? else { unreachable!() };
    let Some((_, _, f)) = desc.methods.iter().find(|(n, _, _)| *n == "__init__") else {
        return construct(desc, kwargs);
    };
    f(cx, V::Obj(obj.clone()), super::pack(args, kwargs)).await?;
    Ok(V::Obj(obj))
}

/// `Model(**kwargs)`: SQLAlchemy's default constructor sets the given attributes.
pub fn construct(desc: &'static ModelDesc, kwargs: Vec<(String, V)>) -> R {
    let obj = ObjCell::transient(desc);
    if let (Some(p), Some(i)) = (desc.poly_on, desc.identity) {
        // SQLAlchemy sets the discriminator when the object is created
        let mut st = obj.st.lock();
        st.vals[p] = V::str(i);
        st.modified[p] = true;
    }
    for (k, v) in kwargs {
        if desc.col_index(&k).is_none() && desc.rel_index(&k).is_none() && desc.composite(&k).is_none() {
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
    /// `aliased(Model, name="x")`: its name in the FROM list
    pub name: Option<String>,
}

static ALIAS_IDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// a name the application gives (label, subquery alias, `index_elements` string) as SQLAlchemy's
/// IdentifierPreparer writes it: as is when it is a plain lowercase identifier and not a reserved word,
/// else between double quotes with any `"` doubled (never spliced raw into the SQL text)
pub fn quote_ident(name: &str) -> std::borrow::Cow<'_, str> {
    const RESERVED: &[&str] = &[
        "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric", "authorization", "binary",
        "both", "case", "cast", "check", "collate", "collation", "column", "concurrently", "constraint", "create",
        "cross", "current_catalog", "current_date", "current_role", "current_schema", "current_time",
        "current_timestamp", "current_user", "default", "deferrable", "desc", "distinct", "do", "else", "end",
        "except", "false", "fetch", "for", "foreign", "freeze", "from", "full", "grant", "group", "having", "ilike",
        "in", "initially", "inner", "intersect", "into", "is", "isnull", "join", "lateral", "leading", "left", "like",
        "limit", "localtime", "localtimestamp", "natural", "not", "notnull", "null", "offset", "on", "only", "or",
        "order", "outer", "overlaps", "placing", "primary", "references", "returning", "right", "select",
        "session_user", "similar", "some", "symmetric", "system_user", "table", "tablesample", "then", "to",
        "trailing", "true", "union", "unique", "user", "using", "variadic", "verbose", "when", "where", "window",
        "with",
    ];
    let plain = name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'$');
    if plain && !RESERVED.contains(&name) {
        std::borrow::Cow::Borrowed(name)
    } else {
        std::borrow::Cow::Owned(format!("\"{}\"", name.replace('"', "\"\"")))
    }
}

/// a string in `order_by`/`group_by`: a label (or column name) of the columns clause, as SQLAlchemy
/// resolves it; anything else is its CompileError
fn label_ref(s: &Select, a: &V, clause: &str) -> R<Sql> {
    let in_cols = |name: &str| s.cols.iter().any(|c| matches!(c, SelCol::Expr(Sql::Label(_, l)) if l == name));
    // a labelled expression: GROUP BY repeats the expression; ORDER BY names the label when the columns
    // clause has it (SQLAlchemy's compiler), else the expression; `desc("name")` is a label reference
    let resolve = |e: &Sql| -> R<Sql> {
        Ok(match e {
            Sql::Label(_, l) if clause == "ORDER BY" && in_cols(l) => Sql::Text(quote_ident(l).into_owned()),
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
        SelCol::Expr(Sql::Col(m, i)) => m.cols[*i].sql == &**name,
        _ => false,
    });
    if !known {
        // raised when the statement is compiled, like SQLAlchemy
        return Ok(Sql::BadLabel(format!(
            "Can't resolve label reference for {clause} / GROUP BY / DISTINCT etc. Textual SQL expression '{name}' should be explicitly declared as text('{name}')"
        )));
    }
    Ok(Sql::Text(quote_ident(name).into_owned()))
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
                if let Some(li) = left.sql_index(c) {
                    out.push(Sql::Bin("=", Box::new(Sql::Col(left, li)), Box::new(Sql::Col(target, *ci))));
                }
            }
        }
        for (ci, t, c) in left.fks {
            if *t == target.table && !std::ptr::eq(left, target) {
                if let Some(ti) = target.sql_index(c) {
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

/// One side of a join whose ON clause is inferred: a table, an alias of one, or a subquery (its columns that
/// are table columns, by their exported name).
enum Side {
    T(&'static ModelDesc),
    A(Arc<Alias>),
    Q(Arc<Subq>),
}

impl Side {
    fn of(v: &V) -> R<Side> {
        match v {
            V::Sql(x) => match &**x {
                Sql::Alias(a) => Ok(Side::A(a.clone())),
                Sql::Subquery(q) => Ok(Side::Q(q.clone())),
                _ => Ok(Side::T(model_of(v)?)),
            },
            _ => Ok(Side::T(model_of(v)?)),
        }
    }
    /// (model, column index, the column as this side exposes it)
    fn cols(&self) -> Vec<(&'static ModelDesc, usize, Sql)> {
        match self {
            Side::T(m) => (0..m.cols.len()).map(|i| (*m, i, Sql::Col(m, i))).collect(),
            Side::A(a) => (0..a.model.cols.len()).map(|i| (a.model, i, Sql::ACol(a.clone(), i))).collect(),
            Side::Q(q) => q.sel.cols.iter().filter_map(|c| match c {
                SelCol::Expr(Sql::Col(m, i)) => Some((*m, *i, Sql::SubCol(q.clone(), m.cols[*i].sql.to_string()))),
                _ => None,
            }).collect(),
        }
    }
    fn name(&self) -> String {
        match self {
            Side::T(m) => m.table.to_string(),
            Side::A(a) => a.model.table.to_string(),
            Side::Q(q) => q.name.clone(),
        }
    }
}

/// the ON clauses a foreign key gives between two sides (SQLAlchemy's join condition from the foreign keys)
fn fk_paths(left: &Side, right: &Side) -> Vec<Sql> {
    let mut out = Vec::new();
    let (lc, rc) = (left.cols(), right.cols());
    for (rm, ri, rs) in &rc {
        for (ci, t, c) in rm.fks {
            if ci == ri {
                for (lm, li, ls) in &lc {
                    if *t == lm.table && lm.cols[*li].sql == *c {
                        out.push(Sql::Bin("=", Box::new(ls.clone()), Box::new(rs.clone())));
                    }
                }
            }
        }
    }
    for (lm, li, ls) in &lc {
        for (ci, t, c) in lm.fks {
            if ci == li {
                for (rm, ri, rs) in &rc {
                    if *t == rm.table && rm.cols[*ri].sql == *c && !(std::ptr::eq(*lm, *rm) && matches!((left, right), (Side::T(_), Side::T(_)))) {
                        out.push(Sql::Bin("=", Box::new(ls.clone()), Box::new(rs.clone())));
                    }
                }
            }
        }
    }
    out
}

/// `join_from(left, right)` without ON: the foreign key between those two
fn infer_between(left: &Side, right: &Side) -> R<Sql> {
    let mut p = fk_paths(left, right);
    match p.len() {
        0 => Err(Exc::msg(&NO_FOREIGN_KEYS_ERROR, format!(
            "Can't find any foreign key relationships between '{}' and '{}'.", left.name(), right.name()))),
        1 => Ok(p.remove(0)),
        _ => Err(Exc::msg(&ARGUMENT_ERROR, format!(
            "Can't determine join between '{}' and '{}'; tables have more than one foreign key constraint relationship between them. Please specify the 'onclause' of this join explicitly.",
            left.name(), right.name()))),
    }
}

/// `sqlalchemy.orm.aliased(Model)`
pub fn aliased(v: &V) -> R {
    let model = model_of(v)?;
    Ok(sql(Sql::Alias(Arc::new(Alias { model, id: ALIAS_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed), name: None }))))
}

/// `aliased(Model, name="x")`
pub fn aliased_named(v: &V, name: &V) -> R {
    let model = model_of(v)?;
    let name = if name.is_none() { None } else { Some(ops::str_(name)?.to_string()) };
    Ok(sql(Sql::Alias(Arc::new(Alias { model, id: ALIAS_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed), name }))))
}

/// `with_parent(obj, Parent.rel)`: the criteria of the related rows of `obj` (its key bound, as SQLAlchemy renders
/// `:param_1 = child.parent_id`)
pub fn with_parent(obj: &V, rel: &V) -> R {
    let V::Sql(x) = rel else { return Err(Exc::msg(&ARGUMENT_ERROR, "with_parent() takes a relationship attribute")) };
    let Sql::Rel(m, ri) = &**x else { return Err(Exc::msg(&ARGUMENT_ERROR, "with_parent() takes a relationship attribute")) };
    let r = &m.rels[*ri];
    if r.secondary.is_some() {
        return Err(Exc::type_error("py2axum: with_parent() of a many-to-many relationship is not supported"));
    }
    let V::Obj(o) = obj else { return Err(Exc::msg(&ARGUMENT_ERROR, "with_parent() takes a mapped instance")) };
    if !std::ptr::eq(o.desc, *m) {
        return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("with_parent(): the instance is not a {}", m.name)));
    }
    let v = rel_value(o, r.local)?;
    Ok(sql(Sql::Bin("=", Box::new(Sql::Param(v, Some(m.cols[r.local].ty), 0)), Box::new(Sql::Col(r.target, r.remote)))))
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
    /// aliases put in the FROM list explicitly (`join_from(alias, ...)`)
    pub from_aliases: Vec<Arc<Alias>>,
    /// the joins (indexes into `joins`) that are FULL OUTER JOINs
    pub full_joins: Vec<usize>,
    /// `join(sq, on)`: (rendered before `joins[pos]`, the subquery, ON, outer)
    pub join_subs: Vec<(usize, Arc<Subq>, Sql, bool)>,
    /// `execution_options(populate_existing=True)`: entities already in the session take the rows' values
    pub populate_existing: bool,
    /// `with_polymorphic(Base, [Sub, ...])` entities: the base read with its subclasses' tables LEFT OUTER JOINed
    pub polys: Vec<(&'static ModelDesc, Vec<&'static ModelDesc>)>,
    /// `execution_options(autoflush=False)`: the session does not flush before it
    pub no_autoflush: bool,
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
    /// `execution_options(render_nulls=True)`: a bulk INSERT's None values are NULL (else the key is left out)
    pub render_nulls: bool,
    /// `WriteOnlyCollection.insert()`: `rows[0]` (the foreign key to the parent) goes into every row of a bulk INSERT
    pub fixed: bool,
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
    /// DELETE: (model, where, synchronize_session, RETURNING columns)
    Delete(&'static ModelDesc, Vec<Sql>, bool, Vec<SelCol>),
    Text(String),
    /// `text("... :name ...").bindparams(name=value)`: the text cut at its bound parameters
    TextBound(Vec<Sql>),
    /// `Model.relationship` (class attribute): loader options, `join()`, comparisons with an object
    Rel(&'static ModelDesc, usize),
    /// `aliased(Model).relationship`: `join()` only
    ARel(Arc<Alias>, usize),
    /// `selectinload(...)`, `noload(...)`, `undefer(...)` ... (a `.options()` argument)
    Load(LoadChain),
    /// `selectinload(A.b).options(...)`: the loader and its sub-options, each a chain from the root
    Loads(Vec<LoadChain>),
    /// `undefer_group("name")` / `undefer("*")` (None): resolved against the entities of its level
    UndeferGroup(Option<String>),
    /// `select(...).subquery()` (used in a FROM, its columns through `.c`)
    Subquery(Arc<Subq>),
    /// `sq.c` (the columns namespace of a subquery)
    SubColumns(Arc<Subq>),
    /// `table.c` (the columns namespace of a Core table)
    TableCols(&'static ModelDesc),
    /// `alias.c` (the columns namespace of an alias of a Core table)
    AliasCols(Arc<Alias>),
    /// `table.c["a", "b"]`: a sub-collection of columns (selected as its columns)
    Cols(Vec<Sql>),
    /// `A.rel.of_type(alias)` / `A.rel.and_(criteria)` (also from an alias): a join target with its ON clause parts;
    /// `of`: `A.rel.of_type(Subclass)` (inheritance: the join reads that subclass)
    RelJoin { src: Option<Arc<Alias>>, m: &'static ModelDesc, ri: usize, to: Option<Arc<Alias>>, extra: Vec<Sql>, of: Option<&'static ModelDesc>,
              poly: Vec<&'static ModelDesc> },
    /// `Model.composite`: its columns (selected as one value built by the composite's class)
    Composite(&'static ModelDesc, usize),
    /// `with_polymorphic(Base, [Sub, ...])` (or "*"): the base entity with its subclasses' columns
    WithPoly(&'static ModelDesc, Vec<&'static ModelDesc>),
    /// `bindparam("name", value=None)`: its value comes from the execution's parameters (`execute(stmt, {...})`,
    /// one execution per dict of a list), typed by the column it is compared with or assigned to
    BindParam(String, Option<V>, Option<ColTy>),
    /// `sq.c.name`
    SubCol(Arc<Subq>, String),
    /// aggregate `FILTER (WHERE ...)`
    Filter(Box<Sql>, Box<Sql>),
    /// `cast(expr, type)`
    Cast(Box<Sql>, &'static str),
    /// `extract(field, expr)`
    Extract(String, Box<Sql>),
    /// `json_col["k"]` / `json_col[2]` (an index of a JSON or JSONB column) and its typed accessors:
    /// None = the JSON element itself (`as_json()`), Some(type) = `CAST(col ->> key AS type)`
    JsonElem(Box<Sql>, V, Option<String>),
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
                Sql::Loads(cs) => out.extend(cs.iter().cloned()),
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

/// `sub` is `base` or one of its mapped subclasses
fn is_subclass(sub: &'static ModelDesc, base: &'static ModelDesc) -> bool {
    let mut m = Some(sub);
    while let Some(d) = m {
        if std::ptr::eq(d, base) {
            return true;
        }
        m = d.parent;
    }
    false
}

/// the classes of `with_polymorphic(base, classes)` / `selectin_polymorphic(base, classes)`: mapped subclasses of
/// `base` (or "*": all of them)
fn poly_classes(base: &'static ModelDesc, classes: &V, star: bool) -> R<Vec<&'static ModelDesc>> {
    if star && matches!(classes, V::Str(s) if &**s == "*") {
        return Ok(base.subs.to_vec());
    }
    let items = match classes {
        V::List(l) => l.lock().clone(),
        V::Tuple(t) => t.to_vec(),
        other => return Err(Exc::type_error(format!("py2axum: a list of mapped subclasses expected, not {}", other.type_name()))),
    };
    let mut out: Vec<&'static ModelDesc> = Vec::new();
    for c in items {
        let d = model_of(&c)?;
        if !is_subclass(d, base) {
            return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("{} is not a subclass of {}", d.name, base.name)));
        }
        if !std::ptr::eq(d, base) && !out.iter().any(|x| std::ptr::eq(*x, d)) {
            out.push(d);
        }
    }
    Ok(out)
}

/// `selectin_polymorphic(Base, [Sub, ...])`: the subclasses' columns loaded for the objects of them
pub fn selectin_polymorphic(base: &V, classes: &V) -> R {
    let b = model_of(base)?;
    Ok(sql(Sql::Loads(poly_classes(b, classes, false)?.into_iter().map(|d| vec![(Lazy::SubPoly, d, 0)]).collect())))
}

/// `with_polymorphic(Base, [Sub, ...] | "*")`
pub fn with_polymorphic(base: &V, classes: &V) -> R {
    let b = model_of(base)?;
    if b.subs.is_empty() || b.poly_on.is_none() {
        return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("{} has no mapped subclasses (inheritance)", b.name)));
    }
    Ok(sql(Sql::WithPoly(b, poly_classes(b, classes, true)?)))
}

/// What the column options of this level (`load_only`, `defer`, `undefer`) do with each column of `desc`,
/// over the mapping's strategies: `load_only()` loads the listed columns and the primary key, defers the
/// others (raise with `raiseload=True`); then `defer()` / `undefer()` in order.
fn col_plan(opts: &[LoadChain], desc: &'static ModelDesc) -> Vec<ColLoad> {
    let mut plan: Vec<ColLoad> = desc.cols.iter().map(|c| c.default_load()).collect();
    let mine: Vec<&LoadChain> = opts.iter().filter(|c| c.len() == 1 && c[0].0.col() && std::ptr::eq(c[0].1, desc)).collect();
    let only: Vec<&&LoadChain> = mine.iter().filter(|c| matches!(c[0].0, Lazy::Only | Lazy::OnlyRaise)).collect();
    if !only.is_empty() {
        let raise = only.iter().any(|c| c[0].0 == Lazy::OnlyRaise);
        for (i, p) in plan.iter_mut().enumerate() {
            *p = if desc.is_pk(i) || only.iter().any(|c| c[0].2 == i) {
                ColLoad::Load
            } else if raise {
                ColLoad::Raise
            } else {
                ColLoad::Defer
            };
        }
    }
    for c in mine {
        match c[0].0 {
            Lazy::Undefer => plan[c[0].2] = ColLoad::Load,
            Lazy::Defer => plan[c[0].2] = ColLoad::Defer,
            Lazy::DeferRaise => plan[c[0].2] = ColLoad::Raise,
            _ => {}
        }
    }
    plan
}

fn col_attr(attr: &V, what: &str) -> R<(&'static ModelDesc, usize)> {
    match attr {
        V::Col(m, i) => Ok((m, *i)),
        _ => Err(Exc::msg(&ARGUMENT_ERROR, format!("py2axum: {what}() takes mapped column attributes (strings are not supported)"))),
    }
}

/// `undefer(Model.col)`: the deferred column loaded with the entities of its level; `undefer("*")`: all of them
pub fn undefer(attr: &V) -> R {
    if let V::Str(s) = attr {
        if &**s == "*" {
            return Ok(sql(Sql::UndeferGroup(None)));
        }
    }
    let (m, i) = col_attr(attr, "undefer")?;
    Ok(sql(Sql::Load(vec![(Lazy::Undefer, m, i)])))
}

/// `undefer_group("name")`: the columns of that `deferred_group=` loaded with the entities of its level
pub fn undefer_group(name: &V) -> R {
    Ok(sql(Sql::UndeferGroup(Some(ops::str_(name)?.to_string()))))
}

/// `defer(Model.col, raiseload=False)`
pub fn defer(attr: &V, raiseload: &V) -> R {
    let (m, i) = col_attr(attr, "defer")?;
    if m.is_pk(i) {
        return Err(Exc::msg(&ARGUMENT_ERROR, format!("py2axum: defer() of the primary key column {}.{} is not supported", m.name, m.cols[i].name)));
    }
    Ok(sql(Sql::Load(vec![(if ops::truthy(raiseload)? { Lazy::DeferRaise } else { Lazy::Defer }, m, i)])))
}

/// `load_only(Model.a, Model.b, raiseload=False)`: one chain per column
pub fn load_only(attrs: Vec<V>, raiseload: &V) -> R {
    let kind = if ops::truthy(raiseload)? { Lazy::OnlyRaise } else { Lazy::Only };
    let mut out = Vec::new();
    for a in &attrs {
        let (m, i) = col_attr(a, "load_only")?;
        if let Some(c) = out.first().map(|c: &LoadChain| c[0].1) {
            if !std::ptr::eq(c, m) {
                return Err(Exc::msg(&ARGUMENT_ERROR, "py2axum: load_only() of columns of several entities is not supported"));
            }
        }
        out.push(vec![(kind, m, i)]);
    }
    if out.is_empty() {
        return Err(Exc::msg(&ARGUMENT_ERROR, "load_only() takes at least one column"));
    }
    Ok(sql(Sql::Loads(out)))
}

/// `undefer_group(...)` / `undefer("*")` for the entities of a level: one `undefer()` per column
fn group_chains(group: &Option<String>, m: &'static ModelDesc) -> Vec<LoadChain> {
    m.cols
        .iter()
        .enumerate()
        .filter(|(_, c)| c.deferred && group.as_deref().is_none_or(|g| c.group == Some(g)))
        .map(|(i, _)| vec![(Lazy::Undefer, m, i)])
        .collect()
}

/// `selectinload(A.b).options(selectinload(B.c), undefer(B.d))`: each option continues the chain
fn chain_options(chain: LoadChain, args: &[V]) -> R {
    let target = match chain.last() {
        Some((l, ..)) if l.col() => return Err(Exc::msg(&ARGUMENT_ERROR, "options() follows a relationship loader")),
        None => return Err(Exc::msg(&ARGUMENT_ERROR, "options() follows a relationship loader")),
        Some((_, m, ri)) => m.rels[*ri].target,
    };
    let mut out = vec![chain.clone()];
    for a in args {
        let subs = match a {
            V::Sql(x) => match &**x {
                Sql::Load(c) => vec![c.clone()],
                Sql::Loads(cs) => cs.clone(),
                Sql::UndeferGroup(g) => group_chains(g, target),
                _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
            },
            _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
        };
        for sub in subs {
            let (_, m, i) = sub[0];
            if !is_subclass(m, target) {
                let what = if sub[0].0 == Lazy::SubPoly { "*" } else if sub[0].0.col() { m.cols[i].name } else { m.rels[i].name };
                return Err(Exc::msg(&ARGUMENT_ERROR, format!("{}.{what} does not link from the previous path element", m.name)));
            }
            out.push(chain.iter().cloned().chain(sub).collect());
        }
    }
    Ok(sql(Sql::Loads(out)))
}

fn loader_chain(mut chain: LoadChain, kind: &str, attr: &V) -> R {
    if matches!(chain.last(), Some((l, ..)) if l.col()) {
        return Err(Exc::attr_error(format!("'Load' object has no attribute '{kind}'")));
    }
    let lazy = match kind {
        "selectinload" | "joinedload" | "subqueryload" | "immediateload" => Lazy::Selectin,
        "defaultload" => Lazy::Default,
        "noload" => Lazy::NoLoad,
        "lazyload" => Lazy::Select,
        _ => return Err(Exc::attr_error(format!("'Load' object has no attribute '{kind}'"))),
    };
    match attr {
        V::Sql(x) => match &**x {
            Sql::RelJoin { src: None, m, ri, to: None, extra, of: Some(_), poly } if extra.is_empty() && lazy == Lazy::Selectin => {
                // `selectinload(A.rel.of_type(with_polymorphic(...)))`: the related objects and their subclass columns
                if let Some((_, pm, pi)) = chain.last() {
                    if !is_subclass(*m, pm.rels[*pi].target) {
                        return Err(Exc::msg(&ARGUMENT_ERROR, format!("{}.{} does not link from the previous path element", m.name, m.rels[*ri].name)));
                    }
                }
                chain.push((lazy, m, *ri));
                let mut out = vec![chain.clone()];
                out.extend(poly.iter().map(|d| chain.iter().cloned().chain([(Lazy::SubPoly, *d, 0)]).collect()));
                Ok(sql(Sql::Loads(out)))
            }
            Sql::Rel(m, ri) => {
                if let Some((_, pm, pi)) = chain.last() {
                    if !is_subclass(*m, pm.rels[*pi].target) {
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

/// An operand: a label applies to the columns clause only (`x.label("n") > 3` is `x > 3`)
fn operand(v: &V, hint: Option<ColTy>) -> Sql {
    match to_sql(v, hint) {
        Sql::Label(e, _) => *e,
        other => other,
    }
}

/// A value of an INSERT row: a SELECT there is its scalar subquery (SQLAlchemy coerces it)
fn value_sql(v: &V, ty: ColTy) -> Sql {
    match to_sql(v, Some(ty)) {
        Sql::Select(sel) => Sql::Scalar(sel),
        other => other,
    }
}

fn to_sql(v: &V, hint: Option<ColTy>) -> Sql {
    match v {
        V::Col(m, i) => Sql::Col(m, *i),
        V::Sql(s) => match &**s {
            Sql::BindParam(n, d, None) if hint.is_some() => Sql::BindParam(n.clone(), d.clone(), hint),
            s => s.clone(),
        },
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

/// A JSONB column (not JSON: SQLAlchemy's JSON comparator has none of JSONB's operators).
fn is_jsonb(s: &Sql) -> bool {
    match s {
        Sql::Col(m, i) | Sql::ExCol(m, i) => m.cols[*i].jsonb,
        Sql::ACol(a, i) => a.model.cols[*i].jsonb,
        _ => false,
    }
}

fn hint_of(s: &Sql) -> Option<ColTy> {
    match s {
        Sql::BindParam(_, _, t) => *t,
        Sql::Col(m, i) => Some(m.cols[*i].ty),
        Sql::ACol(a, i) => Some(a.model.cols[*i].ty),
        Sql::ExCol(m, i) => Some(m.cols[*i].ty),
        Sql::Case(whens, els) => whens.iter().find_map(|(_, v)| hint_of(v)).or_else(|| els.as_deref().and_then(hint_of)),
        Sql::Label(e, _) | Sql::Order(e, _, _) | Sql::Filter(e, _) => hint_of(e),
        Sql::Func(name, args) if matches!(name.as_str(), "max" | "min" | "coalesce" | "sum") => args.first().and_then(hint_of),
        Sql::Func(name, _) if name == "count" => Some(ColTy::BigInt),
        Sql::Bin("||", _, _) => Some(ColTy::Str),
        Sql::JsonElem(_, _, Some(t)) => match t.as_str() {
            "BOOLEAN" => Some(ColTy::Bool),
            "VARCHAR" => Some(ColTy::Str),
            "INTEGER" => Some(ColTy::Int),
            "FLOAT" => Some(ColTy::Float),
            _ => Some(ColTy::Numeric),
        },
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
/// a `text()` statement with its parameters, rendered (`$n` per value, with its type)
fn text_rend(q: &str, params: Option<&V>) -> R<Rend> {
    let (sql_text, binds) = text_binds(q, params)?;
    let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
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
    Ok(r)
}

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
        vals.push(v.ok_or_else(|| bind_missing(format!("A value is required for bind parameter '{n}'")))?);
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

/// `col["k"]` / `col[2]` on a JSON or JSONB column (SQLAlchemy's JSON index; paths and other types refused)
pub fn sql_getitem(v: &V, k: &V) -> R {
    // `table.c["name"]`, `table.c["a", "b"]` (and an alias's)
    if let V::Sql(x) = v {
        let col = |n: &V| -> R<Sql> {
            let n = ops::str_(n)?;
            match &**x {
                Sql::TableCols(m) => m.col_index(&n).map(|i| Sql::Col(m, i)),
                Sql::AliasCols(a) => a.model.col_index(&n).map(|i| Sql::ACol(a.clone(), i)),
                _ => None,
            }
            .ok_or_else(|| Exc::new(&super::v::KEY_ERROR, vec![V::str(&n)]))
        };
        if matches!(&**x, Sql::TableCols(_) | Sql::AliasCols(_)) {
            return match k {
                V::Tuple(t) => Ok(sql(Sql::Cols(t.iter().map(col).collect::<R<Vec<_>>>()?))),
                V::Str(_) => Ok(sql(col(k)?)),
                other => Err(Exc::type_error(format!("py2axum: a column collection is indexed by name, not {}", other.type_name()))),
            };
        }
    }
    let base = to_sql(v, None);
    let json = match &base {
        Sql::Col(m, i) | Sql::ExCol(m, i) => matches!(m.cols[*i].ty, ColTy::Json | ColTy::JsonNull),
        Sql::ACol(a, i) => matches!(a.model.cols[*i].ty, ColTy::Json | ColTy::JsonNull),
        _ => false,
    };
    if !json {
        return Err(Exc::type_error("py2axum: only a JSON or JSONB column can be indexed in SQL (ARRAY and nested JSON indexes are not supported)"));
    }
    match k {
        V::Str(_) | V::Int(_) => Ok(sql(Sql::JsonElem(Box::new(base), k.clone(), None))),
        V::Tuple(_) | V::List(_) => Err(Exc::type_error("py2axum: a JSON path index (col[(\"a\", \"b\")], #>>) is not supported: index one key at a time is not equivalent either, keep this query in Python")),
        other => Err(Exc::type_error(format!("py2axum: a JSON index must be a str or an int, not {}", other.type_name()))),
    }
}

/// the typed accessors of a JSON index (`as_boolean()`, `as_string()`, ...): `CAST(col ->> key AS type)`
fn json_accessor(col: &Sql, key: &V, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let elem = |cast: Option<String>| Ok(sql(Sql::JsonElem(Box::new(col.clone()), key.clone(), cast)));
    let no_args = || -> R<()> {
        if args.is_empty() && kwargs.is_empty() {
            Ok(())
        } else {
            Err(Exc::type_error(format!("{name}() takes no arguments")))
        }
    };
    match name {
        "as_boolean" => no_args().and_then(|_| elem(Some("BOOLEAN".into()))),
        "as_string" => no_args().and_then(|_| elem(Some("VARCHAR".into()))),
        "as_integer" => no_args().and_then(|_| elem(Some("INTEGER".into()))),
        "as_float" => no_args().and_then(|_| elem(Some("FLOAT".into()))),
        "as_json" => no_args().and_then(|_| elem(None)),
        "as_numeric" => {
            // as_numeric(precision, scale, asdecimal=True): read back as a Decimal
            let get = |i: usize, n: &str| args.get(i).cloned().or_else(|| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()));
            if kwargs.iter().any(|(k, _)| !matches!(k.as_str(), "precision" | "scale" | "asdecimal")) || args.len() > 3 {
                return Err(Exc::type_error("as_numeric() takes precision, scale and asdecimal"));
            }
            if !matches!(get(2, "asdecimal"), None | Some(V::Bool(true))) {
                return Err(Exc::type_error("py2axum: as_numeric(asdecimal=False) is not supported"));
            }
            match (get(0, "precision"), get(1, "scale")) {
                (Some(V::Int(p)), Some(V::Int(s))) => elem(Some(format!("NUMERIC({p}, {s})"))),
                (None, _) | (_, None) => Err(Exc::type_error("as_numeric() missing required arguments: 'precision' and 'scale'")),
                _ => Err(Exc::type_error("py2axum: as_numeric() takes int precision and scale")),
            }
        }
        "label" => {
            let n = args.first().ok_or_else(|| Exc::type_error("label() takes one argument"))?;
            Ok(sql(Sql::Label(Box::new(Sql::JsonElem(Box::new(col.clone()), key.clone(), None)), ops::str_(n)?)))
        }
        _ => Err(Exc::type_error(format!(
            "py2axum: .{name}() on a JSON index is not supported (use as_string(), as_integer(), as_float(), as_boolean(), as_numeric() or as_json())"
        ))),
    }
}

/// a bare JSON index in a comparison or arithmetic: SQLAlchemy binds the other side as JSON, refused
fn no_bare_json(e: &Sql) -> R<()> {
    if matches!(e, Sql::JsonElem(_, _, None)) {
        return Err(Exc::type_error(
            "py2axum: comparing a JSON index itself is not supported: compare a typed accessor (col[\"k\"].as_string() == ...)",
        ));
    }
    Ok(())
}

pub fn sql_cmp(a: &V, op: &'static str, b: &V) -> R {
    if let V::Sql(x) = a {
        if let Sql::Rel(m, ri) = &**x {
            return rel_cmp(m, *ri, op, b);
        }
        if let Sql::Composite(m, ci) = &**x {
            // column by column, ANDed (SQLAlchemy's Composite.Comparator, `!=` included)
            let c = &m.composites[*ci];
            let vals = composite_values(b, c.cols.len())?;
            let mut parts = Vec::new();
            for (i, v) in c.cols.iter().zip(vals.iter()) {
                let V::Sql(p) = sql_cmp(&V::Col(m, *i), op, v)? else { unreachable!() };
                parts.push((*p).clone());
            }
            return Ok(sql(Sql::Bool("AND", parts)));
        }
    }
    if let V::Sql(x) = b {
        if let Sql::Rel(m, ri) = &**x {
            return rel_cmp(m, *ri, op, a);
        }
    }
    let la = operand(a, None);
    let lb = operand(b, hint_of(&la));
    no_bare_json(&la)?;
    no_bare_json(&lb)?;
    let la = if let Sql::Param(v, None, id) = la { Sql::Param(v, hint_of(&lb), id) } else { la };
    Ok(sql(match (op, &la, &lb) {
        ("=", _, Sql::Null) => Sql::IsNull(Box::new(la), false),
        ("!=", _, Sql::Null) => Sql::IsNull(Box::new(la), true),
        ("=", Sql::Null, _) => Sql::IsNull(Box::new(lb), false),
        ("!=", Sql::Null, _) => Sql::IsNull(Box::new(lb), true),
        _ => Sql::Bin(if op == "!=" { "<>" } else { op }, Box::new(la), Box::new(lb)),
    }))
}

/// The value of an object's column a relationship comparison binds (a pending object has none yet).
fn rel_value(o: &Arc<ObjCell>, i: usize) -> R<V> {
    match o.st.lock().vals[i].clone() {
        V::Unbound => Err(Exc::type_error("py2axum: comparing a relationship with an object whose key is not loaded (expired or pending) is not supported")),
        v => Ok(v),
    }
}

/// `Address.user == obj` / `!= obj` / `== None` (many-to-one): SQLAlchemy's criteria on the foreign key
fn rel_cmp(m: &'static ModelDesc, ri: usize, op: &'static str, other: &V) -> R {
    let rel = &m.rels[ri];
    if !rel.m2o || rel.secondary.is_some() {
        if matches!(op, "=" | "!=") && rel.uselist {
            return Err(Exc::msg(&INVALID_REQUEST_ERROR, "Can't compare a collection to an object or collection; use contains() to test for membership."));
        }
        return Err(Exc::type_error(format!("py2axum: comparing {}.{} (not a many-to-one relationship) is not supported", m.name, rel.name)));
    }
    let col = || Box::new(Sql::Col(m, rel.local));
    let ty = Some(m.cols[rel.local].ty);
    Ok(sql(match (op, other) {
        ("=", V::None) => Sql::IsNull(col(), false),
        ("!=", V::None) => Sql::IsNull(col(), true),
        ("=", V::Obj(o)) if std::ptr::eq(o.desc, rel.target) => Sql::Bin("=", Box::new(Sql::Param(rel_value(o, rel.remote)?, ty, 0)), col()),
        ("!=", V::Obj(o)) if std::ptr::eq(o.desc, rel.target) => Sql::Bool("OR", vec![
            Sql::Bin("<>", col(), Box::new(Sql::Param(rel_value(o, rel.remote)?, ty, 0))), Sql::IsNull(col(), false)]),
        ("=" | "!=", _) => return Err(Exc::msg(&ARGUMENT_ERROR, format!("Mapped instance expected for relationship comparison to object.   Classes, queries and other SQL elements are not accepted in this context; for comparison with a subquery, use {}.{}.has(**criteria).", m.name, rel.name))),
        _ => return Err(Exc::type_error(format!("py2axum: operator {op} on a relationship is not supported"))),
    }))
}

pub fn sql_binop(a: &V, op: &'static str, b: &V) -> R {
    let la = operand(a, None);
    let lb = operand(b, hint_of(&la));
    no_bare_json(&la)?;
    no_bare_json(&lb)?;
    // `+` with a string on either side is SQLAlchemy's concatenation (`func.upper(x) + " suffix"`, `col + "x"`)
    // (a string literal takes the other side's type: `int_col + "1"` stays an addition)
    let (ha, hb) = (hint_of(&la), hint_of(&lb));
    let str_ty = |h: Option<ColTy>| matches!(h, Some(ColTy::Str));
    let concat = str_ty(ha) || str_ty(hb) || (matches!(a, V::Str(_)) && hb.is_none()) || (matches!(b, V::Str(_)) && ha.is_none());
    let op = if op == "+" && concat { "||" } else { op };
    Ok(sql(Sql::Bin(op, Box::new(la), Box::new(lb))))
}

/// `a / b` in SQL (SQLAlchemy 2.0 true division): integer by integer is `a / CAST(b AS NUMERIC)`
pub fn sql_div(a: &V, b: &V) -> R {
    let la = operand(a, None);
    let lb = operand(b, hint_of(&la));
    let int = |e: &Sql, v: &V| matches!(v, V::Int(_)) || matches!(hint_of(e), Some(ColTy::Int | ColTy::BigInt | ColTy::SmallInt));
    let rhs = if int(&la, a) && int(&lb, b) { Sql::Cast(Box::new(lb), "NUMERIC") } else { lb };
    Ok(sql(Sql::Bin("/", Box::new(la), Box::new(rhs))))
}

pub fn sql_bool(a: &V, op: &'static str, b: &V) -> R {
    Ok(sql(Sql::Bool(op, vec![operand(a, None), operand(b, None)])))
}

pub fn sql_not(a: &V) -> R {
    Ok(sql(Sql::Not(Box::new(operand(a, None)))))
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
        select_item(&mut s, it)?;
    }
    Ok(sql(Sql::Select(Box::new(s))))
}

/// one item of a columns clause (`select(...)`, `add_columns(...)`)
fn select_item(s: &mut Select, it: V) -> R<()> {
    match it {
        V::Class(c) => match c.kind {
            ClassKind::Model(m) => s.cols.extend(sel_cols(m)),
            _ => return Err(Exc::type_error(format!("cannot select {}", c.name))),
        },
        V::Sql(x) if matches!(&*x, Sql::Alias(_)) => {
            let Sql::Alias(a) = &*x else { unreachable!() };
            if a.model.core {
                s.cols.extend((0..a.model.cols.len()).map(|i| SelCol::Expr(Sql::ACol(a.clone(), i))));
            } else {
                s.cols.push(SelCol::AEntity(a.clone()));
            }
        }
        V::Sql(x) if matches!(&*x, Sql::Cols(_)) => {
            let Sql::Cols(cs) = &*x else { unreachable!() };
            s.cols.extend(cs.iter().cloned().map(SelCol::Expr));
        }
        V::Sql(x) if matches!(&*x, Sql::WithPoly(..)) => {
            // the base entity; its subclasses' tables outer joined (their columns usable in the criteria), the
            // subclass columns of the objects loaded with them
            let Sql::WithPoly(b, subs) = &*x else { unreachable!() };
            s.cols.push(SelCol::Entity(b));
            s.polys.push((b, subs.clone()));
            s.loads.extend(subs.iter().map(|d| vec![(Lazy::SubPoly, *d, 0)]));
        }
        // `select(1)`: SQLAlchemy coerces a number to its literal text (`SELECT 1`, no bound parameter)
        V::Int(i) => s.cols.push(SelCol::Expr(Sql::Text(i.to_string()))),
        V::Float(f) if f.is_finite() => s.cols.push(SelCol::Expr(Sql::Text(ops::repr(&V::Float(f))?))),
        other => s.cols.push(SelCol::Expr(expect_sql(&other)?)),
    }
    Ok(())
}

/// what selecting a mapped class (its entity) or a Core table (its columns, in order) puts in the columns
fn sel_cols(m: &'static ModelDesc) -> Vec<SelCol> {
    if m.core {
        (0..m.cols.len()).map(|i| SelCol::Expr(Sql::Col(m, i))).collect()
    } else {
        vec![SelCol::Entity(m)]
    }
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
/// `Session(engine)` / `AsyncSession(engine)` (also `bind=`, `expire_on_commit=`, `autoflush=`): a session on the
/// process pool, closed (rolled back) when its `with` block ends
pub fn new_session(cx: &super::Cx, args: Vec<V>, kwargs: Vec<(String, V)>, sync: bool) -> R {
    let (mut expire, mut autoflush, mut bind) = (true, true, args.first().cloned());
    if args.len() > 1 {
        return Err(Exc::type_error("py2axum: Session() takes the engine and keyword options"));
    }
    for (k, v) in &kwargs {
        match k.as_str() {
            "bind" => bind = Some(v.clone()),
            "expire_on_commit" => expire = ops::truthy(v)?,
            "autoflush" => autoflush = ops::truthy(v)?,
            other => return Err(Exc::type_error(format!("py2axum: Session({other}=) is not supported"))),
        }
    }
    let cls = if sync { "Session" } else { "AsyncSession" };
    match bind {
        Some(V::Native(n)) if matches!(&*n, Native::Engine(s) if *s == sync) => {}
        Some(V::Native(n)) if matches!(&*n, Native::Engine(_)) => {
            return Err(Exc::msg(&ARGUMENT_ERROR, format!("{cls}() needs {} engine", if sync { "a create_engine()" } else { "a create_async_engine()" })));
        }
        Some(other) => return Err(Exc::type_error(format!("py2axum: {cls}({}) is not supported (an engine only)", other.type_name()))),
        None => return Err(Exc::type_error(format!("py2axum: {cls}() without an engine is not supported"))),
    }
    Ok(V::Session(Session::new(cx.app.pool.clone(), expire, autoflush, sync, Arc::downgrade(cx))))
}

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
            // class_=AsyncSession / Session (the default); a project subclass would be ignored: refused
            "class_" if !matches!(v, V::Class(_)) => {}
            other => return Err(Exc::type_error(format!("py2axum: async_sessionmaker({other}=) is not supported"))),
        }
    }
    Ok(V::native(Native::Maker(expire, autoflush, sync)))
}

/// `insert(Model)`; `pg`: the postgresql dialect's (on_conflict_do_update/nothing)
pub fn insert(m: &V, pg: bool) -> R {
    Ok(sql(Sql::Insert(Box::new(Ins { m: model_of(m)?, rows: vec![], conflict: None, returning: vec![], pg, render_nulls: false, fixed: false }))))
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
    Ok(sql(Sql::Delete(model_of(m)?, vec![], true, vec![])))
}

/// `exists()` (SELECT * FROM the tables of its WHERE, correlated) / `exists(select(...))`
pub fn exists(args: &[V]) -> R {
    let Some(v) = args.first() else {
        return Ok(sql(Sql::Exists(Box::new(Select { cols: vec![SelCol::Expr(Sql::Text("*".into()))], ..Default::default() }))));
    };
    if args.len() > 1 {
        return Err(Exc::type_error("py2axum: exists() takes one select() (or none)"));
    }
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
        if name == "options" {
            return chain_options(c.clone(), &args);
        }
        match name {
            "selectin_polymorphic" if args.len() == 1 && kwargs.is_empty() => {
                // `selectinload(A.rel).selectin_polymorphic([Sub, ...])`: the loaded objects' subclass columns
                let target = match c.last() {
                    Some((l, m, ri)) if !l.col() && *l != Lazy::SubPoly => m.rels[*ri].target,
                    _ => return Err(Exc::msg(&ARGUMENT_ERROR, "selectin_polymorphic() follows a relationship loader")),
                };
                let subs = poly_classes(target, &args[0], false)?;
                return Ok(sql(Sql::Loads(subs.into_iter().map(|d| c.iter().cloned().chain([(Lazy::SubPoly, d, 0)]).collect()).collect())));
            }
            _ => {}
        }
        // `selectinload(A.b).load_only(B.x)` ...: a column option continuing the chain
        let raiseload = kwargs.iter().find(|(k, _)| k == "raiseload").map_or(V::Bool(false), |(_, v)| v.clone());
        if kwargs.iter().any(|(k, _)| k != "raiseload") || (name == "undefer" && !kwargs.is_empty()) {
            return Err(Exc::type_error(format!("py2axum: {name}() keyword arguments are not supported here")));
        }
        match name {
            "load_only" => return chain_options(c.clone(), &[load_only(args, &raiseload)?]),
            "defer" if args.len() == 1 => return chain_options(c.clone(), &[defer(&args[0], &raiseload)?]),
            "undefer" if args.len() == 1 => return chain_options(c.clone(), &[undefer(&args[0])?]),
            "undefer_group" if args.len() == 1 => return chain_options(c.clone(), &[undefer_group(&args[0])?]),
            _ => {}
        }
        if !kwargs.is_empty() || args.len() != 1 {
            return Err(Exc::type_error(format!("{name}() takes one argument")));
        }
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
    // a second positional of a pattern operator (`like(pat, escape)`, `startswith(s, escape)`): refused, never dropped
    if matches!(name, "like" | "ilike" | "not_like" | "not_ilike" | "startswith" | "endswith" | "contains") && args.len() > 1 {
        return Err(Exc::type_error(format!("py2axum: {name}() with {} positional arguments is not supported", args.len())));
    }
    if let Sql::JsonElem(col, key, None) = &base {
        return json_accessor(col, key, name, &args, &kwargs);
    }
    // `A.rel.of_type(alias)`, `A.rel.and_(criteria)` (also on an alias's relationship, and chained)
    let rel_join = match name {
        "of_type" | "and_" if kwargs.is_empty() => true,
        _ => false,
    };
    if rel_join {
        let start = match &base {
            Sql::Rel(m, ri) => Some((None, *m, *ri, None, Vec::new(), None, Vec::new())),
            Sql::ARel(a, ri) => Some((Some(a.clone()), a.model, *ri, None, Vec::new(), None, Vec::new())),
            Sql::RelJoin { src, m, ri, to, extra, of, poly } => Some((src.clone(), *m, *ri, to.clone(), extra.clone(), *of, poly.clone())),
            _ => None,
        };
        if let Some(mut j) = start {
            if j.1.rels[j.2].secondary.is_some() {
                return Err(Exc::type_error(format!("py2axum: {name}() on a many-to-many relationship is not supported")));
            }
            if name == "of_type" {
                if let [V::Sql(w)] = &args[..] {
                    if let Sql::WithPoly(b, subs) = &**w {
                        // with_polymorphic(): the target with its subclasses' tables (LEFT OUTER JOIN)
                        if !std::ptr::eq(*b, j.1.rels[j.2].target) {
                            return Err(Exc::msg(&ARGUMENT_ERROR, format!("{} is not a {}", b.name, j.1.rels[j.2].target.name)));
                        }
                        j.5 = Some(*b);
                        j.6 = subs.clone();
                        return Ok(sql(Sql::RelJoin { src: j.0, m: j.1, ri: j.2, to: j.3, extra: j.4, of: j.5, poly: j.6 }));
                    }
                }
                if let [V::Class(c)] = &args[..] {
                    // inheritance: a mapped subclass of the relationship's target
                    let target = j.1.rels[j.2].target;
                    let sub = match c.kind {
                        super::v::ClassKind::Model(d) if std::ptr::eq(d, target) || target.subs.iter().any(|x| std::ptr::eq(*x, d)) => d,
                        _ => return Err(Exc::msg(&ARGUMENT_ERROR, format!("{} is not a subclass of {}", c.name, target.name))),
                    };
                    j.5 = Some(sub);
                    return Ok(sql(Sql::RelJoin { src: j.0, m: j.1, ri: j.2, to: j.3, extra: j.4, of: j.5, poly: j.6 }));
                }
                let [V::Sql(a)] = &args[..] else { return Err(Exc::type_error("py2axum: of_type() takes an aliased() entity")) };
                let Sql::Alias(al) = &**a else { return Err(Exc::type_error("py2axum: of_type() takes an aliased() entity")) };
                if !std::ptr::eq(al.model, j.1.rels[j.2].target) {
                    return Err(Exc::msg(&ARGUMENT_ERROR, format!("{} is not a {}", al.model.name, j.1.rels[j.2].target.name)));
                }
                j.3 = Some(al.clone());
            } else {
                j.4.extend(args.iter().map(|a| to_sql(a, None)));
            }
            return Ok(sql(Sql::RelJoin { src: j.0, m: j.1, ri: j.2, to: j.3, extra: j.4, of: j.5, poly: j.6 }));
        }
    }
    if let Sql::Rel(m, ri) = &base {
        match name {
            "has" | "any" => return rel_exists(m, *ri, name, &args, &kwargs),
            "contains" => {
                // `User.addresses.contains(address)` (one-to-many): the parent's column the child's key copies
                let rel = &m.rels[*ri];
                if rel.m2o || rel.secondary.is_some() || args.len() != 1 || !kwargs.is_empty() {
                    return Err(Exc::type_error(format!("py2axum: {}.{}.contains() is supported on a one-to-many relationship with one object", m.name, rel.name)));
                }
                let V::Obj(o) = &args[0] else {
                    return Err(Exc::msg(&ARGUMENT_ERROR, "contains() expects a mapped instance"));
                };
                return Ok(sql(Sql::Bin("=", Box::new(Sql::Col(m, rel.local)), Box::new(Sql::Param(rel_value(o, rel.remote)?, Some(m.cols[rel.local].ty), 0)))));
            }
            _ => {}
        }
    }
    // keyword arguments: only those handled below, never ignored
    let allowed: &[&str] = match name {
        "filter_by" | "values" => &["*"],
        "join" | "outerjoin" | "join_from" => &["isouter", "full"],
        "with_for_update" => &["skip_locked", "nowait", "read", "key_share", "of"],
        "execution_options" => &["synchronize_session", "populate_existing", "autoflush", "yield_per", "render_nulls"],
        "on_conflict_do_update" => &["index_elements", "set_", "where", "constraint"],
        "on_conflict_do_nothing" => &["index_elements", "constraint"],
        "regexp_match" => &["flags"],
        // an INSERT's RETURNING rows in the order of its parameters: what the per-row bulk INSERT gives
        "returning" => &["sort_by_parameter_order"],
        _ => &[],
    };
    if allowed != ["*"] {
        if let Some((k, _)) = kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
            return Err(Exc::type_error(format!("py2axum: {name}({k}=) is not supported")));
        }
    }
    let kw = |k: &str| kwargs.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    if let Sql::Exists(sel) = &base {
        // `exists().where(...)`: the criteria go to its SELECT
        if !matches!(name, "where" | "filter") {
            return Err(Exc::attr_error(format!("py2axum: Exists.{name}() is not supported (where only)")));
        }
        let mut s = (**sel).clone();
        for a in &args {
            s.wheres.push(to_sql(a, None));
        }
        return Ok(sql(Sql::Exists(Box::new(s))));
    }
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
                let ent = s.joins.last().map(|j| j.0).or_else(|| {
                    s.cols.iter().find_map(|c| match c {
                        SelCol::Entity(m) => Some(*m),
                        SelCol::Expr(e) => {
                            let mut ts = Vec::new();
                            tables_in(e, &mut ts);
                            ts.first().copied()
                        }
                        SelCol::AEntity(_) => None,
                    })
                }).or_else(|| s.from.first().copied());
                let m = ent.ok_or_else(|| Exc::type_error("filter_by() needs an entity"))?;
                for (k, v) in &kwargs {
                    // a column, a composite, or a hybrid property (its SQL expression)
                    let lhs = if let Some(i) = m.col_index(k) {
                        V::Col(m, i)
                    } else if let Some(ci) = m.composite(k) {
                        sql(Sql::Composite(m, ci))
                    } else if let Some(h) = m.hybrid(k).filter(|h| !h.method) {
                        let f = h.expr.unwrap_or(h.get);
                        ops::block_on_sync(async move { f(&super::root_cx(), V::Class(m.class), vec![]).await }, k)?
                    } else {
                        return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                            "None of the FROM clause entities have a property \"{k}\". Searched entities: Mapper[{}({})]", m.name, m.table)));
                    };
                    if let V::Sql(x) = sql_cmp(&lhs, "=", v)? {
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
                let full = kw("full").map(|v| ops::truthy(&v).unwrap_or(false)).unwrap_or(false);
                if full {
                    // FULL OUTER JOIN: rendered for the join added now
                    let n = s.joins.len();
                    let r = sql_method(&sql(Sql::Select(Box::new(s.clone()))), name, args.clone(),
                                       kwargs.iter().filter(|(k, _)| k != "full").cloned().collect())?;
                    let V::Sql(x) = &r else { unreachable!() };
                    let Sql::Select(mut s2) = (**x).clone() else { unreachable!() };
                    if s2.joins.len() != n + 1 || !s2.join_subs.iter().all(|j| j.0 <= n) || s2.join_subs.len() != s.join_subs.len() {
                        return Err(Exc::type_error("py2axum: FULL OUTER JOIN of this target is not supported"));
                    }
                    s2.joins[n].2 = true;
                    s2.full_joins.push(n);
                    return Ok(sql(Sql::Select(s2)));
                }
                let outer = name == "outerjoin" || kw("isouter").map(|v| ops::truthy(&v).unwrap_or(false)).unwrap_or(false);
                let mut left_side: Option<Side> = None;
                // join_from(left, right, on): the left side is already in the FROM list
                let args = if name == "join_from" {
                    if args.len() < 2 {
                        return Err(Exc::type_error("join_from() takes the left and right entities"));
                    }
                    left_side = Some(Side::of(&args[0])?);
                    match &args[0] {
                        V::Sql(x) if matches!(&**x, Sql::Alias(_)) => {
                            let Sql::Alias(al) = &**x else { unreachable!() };
                            push_alias(&mut s.from_aliases, al);
                        }
                        other => {
                            let left = model_of(other)?;
                            if !s.from.iter().any(|m| std::ptr::eq(*m, left)) {
                                s.from.push(left);
                            }
                        }
                    }
                    args[1..].to_vec()
                } else {
                    args.clone()
                };
                let a = args.first().cloned().ok_or_else(|| Exc::type_error(format!("{name}() takes one argument")))?;
                // join(Target, A.rel) / join(alias, A.rel): ON the relationship's foreign key, to that target
                if let (Some(V::Sql(on_rel)), 2) = (args.get(1), args.len()) {
                    if let Sql::Rel(m, ri) = &**on_rel {
                        let rel = &m.rels[*ri];
                        if rel.secondary.is_some() {
                            return Err(Exc::type_error("py2axum: join(target, many-to-many relationship) is not supported"));
                        }
                        let (target, alias) = match &a {
                            V::Sql(x) if matches!(&**x, Sql::Alias(_)) => {
                                let Sql::Alias(al) = &**x else { unreachable!() };
                                (al.model, Some(al.clone()))
                            }
                            other => (model_of(other)?, None),
                        };
                        if !std::ptr::eq(target, rel.target) {
                            return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("Join target {} does not correspond to the right side of join condition {}.{}", target.name, m.name, rel.name)));
                        }
                        let right = match &alias {
                            Some(al) => Sql::ACol(al.clone(), rel.remote),
                            None => Sql::Col(target, rel.remote),
                        };
                        let on = Sql::Bin("=", Box::new(Sql::Col(m, rel.local)), Box::new(right));
                        s.joins.push((target, Some(on), outer, alias));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                if args.len() > 2 {
                    return Err(Exc::type_error(format!("{name}() takes at most two arguments")));
                }
                if let V::Sql(x) = &a {
                    if let Sql::ARel(al, ri) = &**x {
                        // join(alias.rel): ON the alias's column
                        let rel = &al.model.rels[*ri];
                        if rel.secondary.is_some() {
                            return Err(Exc::type_error("py2axum: join() of an aliased many-to-many relationship is not supported"));
                        }
                        let on = Sql::Bin("=", Box::new(Sql::ACol(al.clone(), rel.local)), Box::new(Sql::Col(rel.target, rel.remote)));
                        s.joins.push((rel.target, Some(on), outer, None));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                if let V::Sql(x) = &a {
                    if let Sql::RelJoin { src, m, ri, to, extra, of, poly } = &**x {
                        // join(A.rel.of_type(alias)) / join(A.rel.and_(...)): ON the foreign key, then the criteria
                        let rel = &m.rels[*ri];
                        let target = of.unwrap_or(rel.target);
                        if of.is_some() && to.is_some() {
                            return Err(Exc::type_error("py2axum: of_type() of a subclass and of an alias together is not supported"));
                        }
                        let left = match src {
                            Some(al) => Sql::ACol(al.clone(), rel.local),
                            None => Sql::Col(m, rel.local),
                        };
                        let right = match to {
                            Some(al) => Sql::ACol(al.clone(), rel.remote),
                            None => Sql::Col(target, rel.remote),
                        };
                        let mut parts = vec![Sql::Bin("=", Box::new(left), Box::new(right))];
                        parts.extend(extra.iter().cloned());
                        let on = if parts.len() == 1 { parts.pop().unwrap() } else { Sql::Bool("AND", parts) };
                        if !poly.is_empty() {
                            s.polys.push((target, poly.clone()));
                            s.loads.extend(poly.iter().map(|d| vec![(Lazy::SubPoly, *d, 0)]));
                        }
                        s.joins.push((target, Some(on), outer, to.clone()));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                    if let Sql::Rel(m, ri) = &**x {
                        // join(A.rel): ON the relationship's foreign key
                        let rel = &m.rels[*ri];
                        // select(B).select_from(A).join(B.rel_to_A): the join starts from B (SQLAlchemy replaces the
                        // explicit FROM entity the relationship points to)
                        if s.from.iter().any(|f| std::ptr::eq(*f, rel.target)) && !s.from.iter().any(|f| std::ptr::eq(*f, *m))
                            && !s.joins.iter().any(|j| std::ptr::eq(j.0, *m)) {
                            s.from.retain(|f| !std::ptr::eq(*f, rel.target));
                            s.from.push(m);
                        }
                        if let Some((t, a, b)) = rel.secondary {
                            // many-to-many: through the association table
                            let on1 = Sql::Bin("=", Box::new(Sql::Col(m, rel.local)), Box::new(Sql::Col(t, a)));
                            let on2 = Sql::Bin("=", Box::new(Sql::Col(rel.target, rel.remote)), Box::new(Sql::Col(t, b)));
                            s.joins.push((t, Some(on1), outer, None));
                            s.joins.push((rel.target, Some(on2), outer, None));
                            return Ok(sql(Sql::Select(Box::new(s))));
                        }
                        let on = Sql::Bin("=", Box::new(Sql::Col(m, rel.local)), Box::new(Sql::Col(rel.target, rel.remote)));
                        s.joins.push((rel.target, Some(on), outer, None));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                let on = args.get(1).map(|o| to_sql(o, None));
                // no ON clause: from the foreign keys, with join_from()'s left side or the statement's entities
                let infer = |s: &Select, right: &Side| -> R<Sql> {
                    match &left_side {
                        Some(l) => infer_between(l, right),
                        None => match right {
                            Side::T(t) => infer_onclause(s, t),
                            _ => {
                                let mut lefts: Vec<Side> = Vec::new();
                                for c in &s.cols {
                                    if let SelCol::Entity(m) = c {
                                        lefts.push(Side::T(m));
                                    }
                                }
                                let mut ts = Vec::new();
                                for c in &s.cols {
                                    if let SelCol::Expr(e) = c {
                                        tables_in(e, &mut ts);
                                    }
                                }
                                ts.extend(s.from.iter().copied());
                                for m in ts {
                                    if !lefts.iter().any(|l| matches!(l, Side::T(x) if std::ptr::eq(*x, m))) {
                                        lefts.push(Side::T(m));
                                    }
                                }
                                let found: Vec<Sql> = lefts.iter().flat_map(|l| fk_paths(l, right)).collect();
                                match found.len() {
                                    1 => Ok(found.into_iter().next().unwrap()),
                                    0 => Err(Exc::msg(&NO_FOREIGN_KEYS_ERROR, format!(
                                        "Can't find any foreign key relationships between the statement and '{}'.", right.name()))),
                                    _ => Err(Exc::msg(&INVALID_REQUEST_ERROR, "Can't determine which FROM clause to join from, there are multiple FROMS which can join to this entity. Please use the .select_from() method to establish an explicit left side, as well as providing an explicit ON clause if not present already to help resolve the ambiguity.")),
                                }
                            }
                        },
                    }
                };
                if let V::Sql(x) = &a {
                    if let Sql::Subquery(q) = &**x {
                        let on = match on {
                            Some(on) => on,
                            None => infer(&s, &Side::Q(q.clone()))?,
                        };
                        s.join_subs.push((s.joins.len(), q.clone(), on, outer));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                if let V::Sql(x) = &a {
                    if let Sql::Alias(al) = &**x {
                        let on = match on {
                            Some(on) => on,
                            None => infer(&s, &Side::A(al.clone()))?,
                        };
                        s.joins.push((al.model, Some(on), outer, Some(al.clone())));
                        return Ok(sql(Sql::Select(Box::new(s))));
                    }
                }
                let target = model_of(&a)?;
                let on = match on {
                    Some(on) => Some(on),
                    None => Some(infer(&s, &Side::T(target))?),
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
                            Sql::Loads(cs) => s.loads.extend(cs.iter().cloned()),
                            Sql::UndeferGroup(g) => {
                                for c in &s.cols {
                                    if let SelCol::Entity(m) = c {
                                        s.loads.extend(group_chains(g, m));
                                    }
                                }
                            }
                            _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
                        },
                        _ => return Err(Exc::msg(&ARGUMENT_ERROR, "unsupported loader option")),
                    }
                }
            }
            "execution_options" => {
                // the ORM options of a SELECT; yield_per only changes how rows are fetched (the result is the same)
                if !args.is_empty() || kw("synchronize_session").is_some() || kw("render_nulls").is_some() {
                    return Err(Exc::type_error("py2axum: execution_options() on a SELECT: populate_existing, autoflush and yield_per only"));
                }
                if let Some(v) = kw("populate_existing") {
                    s.populate_existing = ops::truthy(&v)?;
                }
                if let Some(v) = kw("autoflush") {
                    s.no_autoflush = !ops::truthy(&v)?;
                }
                if let Some(v) = kw("yield_per") {
                    if !matches!(v, V::Int(n) if n > 0) {
                        return Err(Exc::value_error("yield_per must be a positive integer"));
                    }
                }
            }
            "scalar_subquery" => return Ok(sql(Sql::Scalar(Box::new(s)))),
            // `select(...).label("n")`: its scalar subquery, labeled
            "label" => return Ok(sql(Sql::Label(Box::new(Sql::Scalar(Box::new(s))), ops::str_(&one(&args)?)?))),
            "add_columns" => {
                for a in &args {
                    select_item(&mut s, a.clone())?;
                }
            }
            "with_only_columns" => {
                let mut cols = Vec::new();
                for a in &args {
                    match a {
                        V::Class(c) => match c.kind {
                            ClassKind::Model(m) => cols.extend(sel_cols(m)),
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
                row.push((i, value_sql(v, m.cols[i].ty)));
            }
            Ok(row)
        };
        let names_of = |v: &V| -> R<Vec<String>> {
            ops::iter(v)?.iter().map(|x| match x {
                V::Str(s) => Ok(quote_ident(s).into_owned()),
                V::Col(cm, ci) => Ok(cm.cols[*ci].sql.to_string()),
                _ => Err(Exc::type_error("py2axum: index_elements are column names or columns")),
            }).collect()
        };
        let kw = |n: &str| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        match name {
            // INSERT: render_nulls (a bulk INSERT then binds its None values instead of leaving the keys out)
            "execution_options" => {
                if !args.is_empty() || kwargs.iter().any(|(k, _)| k != "render_nulls") {
                    return Err(Exc::type_error("py2axum: execution_options() on an INSERT: render_nulls only"));
                }
                if let Some(v) = kw("render_nulls") {
                    ins.render_nulls = ops::truthy(&v)?;
                }
            }
            "values" => {
                if !kwargs.is_empty() {
                    let mut row = Vec::new();
                    for (k, v) in &kwargs {
                        let i = col_of(&V::str(k))?;
                        row.push((i, value_sql(v, m.cols[i].ty)));
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
            "returning" if kw("sort_by_parameter_order").is_some() => {
                return Err(Exc::msg(&ARGUMENT_ERROR, "sort_by_parameter_order is only supported for INSERT statements"));
            }
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
    if let Sql::Delete(m, w, sync, ret) = &base {
        let (mut w, mut sync, mut ret) = (w.clone(), *sync, ret.clone());
        match name {
            // RETURNING of columns and expressions (deleted entities would need SQLAlchemy's deleted state)
            "returning" if kw("sort_by_parameter_order").is_some() => {
                return Err(Exc::msg(&ARGUMENT_ERROR, "sort_by_parameter_order is only supported for INSERT statements"));
            }
            "returning" => {
                for a in &args {
                    if matches!(a, V::Class(_)) {
                        return Err(Exc::type_error("py2axum: delete().returning(Model) is not supported (columns only)"));
                    }
                    ret.push(SelCol::Expr(to_sql(a, None)));
                }
            }
            "where" | "filter" => w.extend(args.iter().map(|a| to_sql(a, None))),
            "execution_options" => match kw("synchronize_session") {
                Some(V::Bool(false)) => sync = false,
                Some(V::Str(x)) if matches!(&*x, "auto" | "evaluate" | "fetch") => {}
                _ => return Err(Exc::type_error("py2axum: execution_options() on a DELETE supports synchronize_session=False/'auto'/'evaluate'/'fetch' only")),
            },
            _ => return Err(Exc::attr_error(format!("'Delete' object has no attribute '{name}'"))),
        }
        return Ok(sql(Sql::Delete(m, w, sync, ret)));
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
        // PostgreSQL's POSIX regular expressions: `~`, `~*` with flags="i"
        "regexp_match" => {
            let flags = args.get(1).cloned().or_else(|| kw("flags")).unwrap_or(V::None);
            let pat = to_sql(&one(&args)?, Some(ColTy::Str));
            match &flags {
                V::None => Sql::Bin("~", b, Box::new(pat)),
                V::Str(f) if &**f == "i" => Sql::Bin("~*", b, Box::new(pat)),
                // SQLAlchemy's PostgreSQL dialect: the flags inline, as an embedded option of the pattern
                V::Str(f) => {
                    let lit = format!("'{}'", f.replace('\'', "''"));
                    let concat = Sql::Func("CONCAT".into(), vec![Sql::Text("'(?'".into()), Sql::Text(lit), Sql::Text("')'".into()), pat]);
                    Sql::Bin("~", b, Box::new(concat))
                }
                o => return Err(Exc::type_error(format!("py2axum: regexp_match(flags=) must be a str, not {}", o.type_name()))),
            }
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
        // JSONB operators (PostgreSQL's comparator), the argument typed as SQLAlchemy types it: VARCHAR for
        // has_key, JSONB otherwise (so has_any/has_all with a list fail in PostgreSQL, `jsonb ?| jsonb`, as in Python)
        "contains" | "contained_by" | "has_key" | "has_any" | "has_all" if is_jsonb(&b) => {
            let op = match name {
                "contains" => "@>",
                "contained_by" => "<@",
                "has_key" => "?",
                "has_any" => "?|",
                _ => "?&",
            };
            let arg = one(&args)?;
            // a literal SQLAlchemy can type keeps that type (`contains("a")` binds a VARCHAR: PostgreSQL then
            // refuses `jsonb @> character varying`, as in Python); a list, a dict or None takes the column's
            let ty = match (name, &arg) {
                ("has_key", _) | (_, V::Str(_)) => ColTy::Str,
                (_, V::Bool(_)) => ColTy::Bool,
                (_, V::Int(_)) => ColTy::Int,
                (_, V::Float(_)) => ColTy::Float,
                _ => ColTy::Json,
            };
            let right = to_sql(&arg, Some(ty));
            // SQLAlchemy writes the bound literal's type (`%(x)s::VARCHAR`): PostgreSQL does not infer jsonb from @>
            let right = match ty {
                ColTy::Str if name != "has_key" => Sql::Cast(Box::new(right), "VARCHAR"),
                ColTy::Int => Sql::Cast(Box::new(right), "INTEGER"),
                _ => right,
            };
            Sql::Bin(op, b, Box::new(right))
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
    /// tables of the enclosing statements: a nested SELECT whose FROM comes from its WHERE correlates
    /// them away (SQLAlchemy's auto-correlation, `exists().where(...)`)
    outer: Vec<&'static ModelDesc>,
}

impl Rend {
    fn alias_name(&mut self, a: &Alias) -> String {
        if let Some((_, n)) = self.aliases.iter().find(|(id, _)| *id == a.id) {
            return n.clone();
        }
        if let Some(n) = &a.name {
            let n = quote_ident(n).into_owned();
            self.aliases.push((a.id, n.clone()));
            return n;
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
        Sql::Not(a) | Sql::IsNull(a, _) | Sql::Order(a, _, _) | Sql::Label(a, _) | Sql::Distinct(a) | Sql::Cast(a, _) | Sql::Extract(_, a) | Sql::InSelect(a, _, _) | Sql::JsonElem(a, _, _) => aliases_in(a, out),
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

tokio::task_local! {
    /// the parameters of the execution being rendered (`bindparam()` values)
    static BOUND: Vec<(String, V)>;
}

thread_local! {
    /// set while the `bindparam()` names of a statement are collected (a render with no execution)
    static COLLECT: Mutex<Option<Vec<String>>> = const { Mutex::new(None) };
}

/// the names of the `bindparam()`s of a statement, in order
fn bind_names(st: &Sql) -> Vec<String> {
    COLLECT.with(|c| *c.lock() = Some(Vec::new()));
    let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
    let _ = render(&mut r, st);
    COLLECT.with(|c| c.lock().take()).unwrap_or_default()
}

/// `bindparam("name", value)`
pub fn bindparam(name: &V, value: Option<&V>) -> R {
    Ok(sql(Sql::BindParam(ops::str_(name)?.to_string(), value.cloned(), None)))
}

fn render(r: &mut Rend, e: &Sql) -> R<()> {
    match e {
        Sql::Col(m, i) => r.sql += &m.col_ref(*i),
        Sql::InSelect(a, q, neg) => {
            render(r, a)?;
            r.sql += if *neg { " NOT IN (" } else { " IN (" };
            render_select(r, q)?;
            r.sql.push(')');
        }
        Sql::ACol(a, i) => {
            let n = r.alias_name(a);
            r.sql += &format!("{n}.{}", a.model.cols[*i].q());
        }
        Sql::Alias(_) => return Err(Exc::msg(&ARGUMENT_ERROR, "py2axum: an aliased() entity used as an SQL expression")),
        Sql::BadLabel(m) => return Err(Exc::msg(&COMPILE_ERROR, m.clone())),
        Sql::LabelRef(n) => {
            return Err(Exc::msg(&COMPILE_ERROR, format!(
                "Can't resolve label reference for ORDER BY / GROUP BY / DISTINCT etc. Textual SQL expression '{n}' should be explicitly declared as text('{n}')"
            )))
        }
        Sql::BindParam(name, dflt, ty) => {
            let given = BOUND.try_with(|b| b.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())).ok().flatten();
            if let Ok(true) = COLLECT.try_with(|c| c.lock().as_mut().map(|c| c.push(name.clone())).is_some()) {
                r.bind(V::None, *ty);
                return Ok(());
            }
            match given.or_else(|| dflt.clone()) {
                Some(v) => r.bind(v, *ty),
                None => return Err(bind_missing(format!("A value is required for bind parameter '{name}'"))),
            }
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
        Sql::ARel(a, ri) => {
            return Err(Exc::msg(&ARGUMENT_ERROR, format!("{}.{} of an aliased class: only join() is supported", a.model.name, a.model.rels[*ri].name)))
        }
        Sql::Load(_) | Sql::Loads(_) | Sql::UndeferGroup(_) => return Err(Exc::msg(&ARGUMENT_ERROR, "a loader option is not a SQL expression")),
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
            r.sql += &format!(" AS {}", quote_ident(name));
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
        // as SQLAlchemy's PostgreSQL compiler: `->>` / `->` with the index cast (`::TEXT`, `::INT`), and
        // JSONB's subscript for the element itself (PostgreSQL 14+)
        Sql::JsonElem(a, key, cast) => {
            let key_cast = if matches!(key, V::Int(_)) { "::INT" } else { "::TEXT" };
            if cast.is_some() {
                r.sql += "CAST(";
            }
            render(r, a)?;
            let jsonb_sub = cast.is_none() && is_jsonb(a);
            r.sql += match (cast, jsonb_sub) {
                (Some(_), _) => " ->> ",
                (None, true) => "[",
                (None, false) => " -> ",
            };
            r.bind(key.clone(), None);
            r.sql += key_cast;
            if jsonb_sub {
                r.sql.push(']');
            }
            if let Some(t) = cast {
                r.sql += &format!(" AS {t})");
            }
        }
        Sql::SubCol(q, name) => r.sql += &format!("{}.{}", quote_ident(&q.name), quote_ident(name)),
        Sql::Subquery(q) => {
            r.sql.push('(');
            render_select(r, &q.sel)?;
            r.sql += &format!(") AS {}", quote_ident(&q.name));
        }
        Sql::Composite(m, ci) => {
            for (k, i) in m.composites[*ci].cols.iter().enumerate() {
                if k > 0 {
                    r.sql += ", ";
                }
                render(r, &Sql::Col(m, *i))?;
            }
        }
        Sql::SubColumns(_) | Sql::TableCols(_) | Sql::AliasCols(_) | Sql::Cols(_) | Sql::RelJoin { .. } => return Err(Exc::type_error("py2axum: a .c columns namespace is not an expression")),
        Sql::WithPoly(b, _) => return Err(Exc::type_error(format!("py2axum: with_polymorphic({}) is not an expression", b.name))),
        Sql::Scalar(s) => {
            r.sql.push('(');
            render_select(r, s)?;
            r.sql.push(')');
        }
        Sql::Select(s) => render_select(r, s)?,
        Sql::Update(m, w, sets, _, ret) => {
            // joined table inheritance: the table of the columns set; the other tables of the WHERE in FROM
            let mut target = m.table;
            if !m.joins.is_empty() {
                let ts: Vec<&str> = sets.iter().map(|(c, _)| m.cols[*c].table.unwrap_or(m.table)).collect();
                if ts.iter().any(|t| *t != ts[0]) || !ret.is_empty() {
                    return Err(Exc::type_error(format!("py2axum: update({}) of columns of several tables of a joined \
                        inheritance hierarchy (or with returning()) is not supported", m.name)));
                }
                target = ts.first().copied().unwrap_or(m.table);
            }
            r.sql += &format!("UPDATE {} SET ", quote_ident(target));
            for (i, (c, v)) in sets.iter().enumerate() {
                if i > 0 {
                    r.sql += ", ";
                }
                r.sql += &format!("{}=", m.cols[*c].q());
                render(r, v)?;
            }
            if !m.joins.is_empty() {
                let others = hierarchy_tables_in(m, w, target);
                if !others.is_empty() {
                    r.sql += &format!(" FROM {}", others.iter().map(|t| quote_ident(t).into_owned()).collect::<Vec<_>>().join(", "));
                }
            }
            r.outer.push(m);
            render_where(r, &w.iter().cloned().chain(poly_filter(m)).collect::<Vec<_>>())?;
            r.outer.pop();
            render_returning(r, ret)?;
        }
        Sql::Delete(m, w, _, ret) => {
            r.sql += &format!("DELETE FROM {}", m.qt());
            if !m.joins.is_empty() {
                // joined table inheritance: the rows of the subclass's table (SQLAlchemy's ORM delete)
                if !ret.is_empty() {
                    return Err(Exc::type_error(format!("py2axum: delete({}).returning() (joined table inheritance) is not supported", m.name)));
                }
                let others = hierarchy_tables_in(m, w, m.table);
                if !others.is_empty() {
                    r.sql += &format!(" USING {}", others.iter().map(|t| quote_ident(t).into_owned()).collect::<Vec<_>>().join(", "));
                }
            }
            r.outer.push(m);
            render_where(r, &w.iter().cloned().chain(poly_filter(m)).collect::<Vec<_>>())?;
            r.outer.pop();
            render_returning(r, ret)?;
        }
        Sql::Insert(ins) => render_insert(r, ins)?,
        Sql::Excluded(_) => return Err(Exc::msg(&COMPILE_ERROR, "py2axum: `excluded` is only usable through its columns")),
        Sql::ExCol(m, i) => r.sql += &format!("excluded.{}", m.cols[*i].q()),
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
    if !m.joins.is_empty() {
        return Err(Exc::type_error(format!("py2axum: insert({}) of a joined table inheritance subclass is not supported \
            (add the objects to the session)", m.name)));
    }
    let mut cols: Vec<usize> = Vec::new();
    for row in &ins.rows {
        for (i, _) in row {
            if !cols.contains(i) {
                cols.push(*i);
            }
        }
    }
    if cols.is_empty() {
        r.sql += &format!("INSERT INTO {} DEFAULT VALUES", m.qt());
    } else {
        r.sql += &format!("INSERT INTO {} ({}) VALUES ", m.qt(), cols.iter().map(|i| m.cols[*i].q()).collect::<Vec<_>>().join(", "));
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
                r.sql += &format!("{} = ", m.cols[*i].q());
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
                SelCol::Entity(e) => r.sql += &(0..e.cols.len()).map(|i| e.col_ref(i)).collect::<Vec<_>>().join(", "),
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
        Sql::Not(a) | Sql::IsNull(a, _) | Sql::Order(a, _, _) | Sql::Label(a, _) | Sql::Distinct(a) | Sql::Cast(a, _) | Sql::Extract(_, a) | Sql::InSelect(a, _, _) | Sql::JsonElem(a, _, _) => subs_in(a, out),
        _ => {}
    }
}

/// how many result columns an expression of the columns clause takes
fn sql_width(e: &Sql) -> usize {
    match e {
        Sql::Composite(m, ci) => m.composites[*ci].cols.len(),
        _ => 1,
    }
}

/// the tables of `m`'s hierarchy other than `target` whose columns a WHERE reads (UPDATE ... FROM, DELETE ... USING)
fn hierarchy_tables_in(m: &'static ModelDesc, w: &[Sql], target: &'static str) -> Vec<&'static str> {
    let mut cols = Vec::new();
    for e in w {
        cols_in(e, &mut cols);
    }
    let mut out: Vec<&'static str> = Vec::new();
    for (d, i) in cols {
        let t = d.cols[i].table.unwrap_or(d.table);
        if t != target && std::ptr::eq(d.root(), m.root()) && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// the columns an expression reads (not inside subqueries)
fn cols_in(e: &Sql, out: &mut Vec<(&'static ModelDesc, usize)>) {
    match e {
        Sql::Col(m, i) => out.push((m, *i)),
        Sql::Bin(_, a, b) | Sql::Like(a, _, b, _) | Sql::Filter(a, b) => {
            cols_in(a, out);
            cols_in(b, out);
        }
        Sql::Bool(_, items) | Sql::Func(_, items) => items.iter().for_each(|i| cols_in(i, out)),
        Sql::Case(whens, els) => {
            for (c, v) in whens {
                cols_in(c, out);
                cols_in(v, out);
            }
            if let Some(e) = els {
                cols_in(e, out);
            }
        }
        Sql::In(a, items, _) => {
            cols_in(a, out);
            items.iter().for_each(|i| cols_in(i, out));
        }
        Sql::Not(a) | Sql::IsNull(a, _) | Sql::Order(a, _, _) | Sql::Label(a, _) | Sql::Distinct(a) | Sql::Cast(a, _) | Sql::Extract(_, a) | Sql::InSelect(a, _, _) | Sql::JsonElem(a, _, _) => cols_in(a, out),
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
        Sql::Composite(m, _) => {
            if !out.iter().any(|x| std::ptr::eq(*x, *m)) {
                out.push(m)
            }
        }
        Sql::Not(a) | Sql::IsNull(a, _) | Sql::Order(a, _, _) | Sql::Label(a, _) | Sql::Distinct(a) | Sql::Cast(a, _) | Sql::Extract(_, a) | Sql::InSelect(a, _, _) | Sql::JsonElem(a, _, _) => tables_in(a, out),
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
    // the FROM list: the explicit FROM, then the tables of the columns and of the WHERE criteria (SQLAlchemy's
    // froms), less the joined ones; in a nested statement, less the enclosing statement's (auto-correlation)
    let mut froms: Vec<&'static ModelDesc> = Vec::new();
    // every class read (single table inheritance: a subclass among them filters on its identities)
    let mut classes: Vec<&'static ModelDesc> = s.from.clone();
    for c in &s.cols {
        match c {
            SelCol::Entity(m) => classes.push(m),
            SelCol::Expr(e) => tables_in(e, &mut classes),
            SelCol::AEntity(_) => {}
        }
    }
    // (a joined class's identities go in its ON clause, like SQLAlchemy: an outer join stays outer)
    for c in &s.cols {
        match c {
            SelCol::Entity(m) => {
                push_from(&mut froms, m);
            }
            SelCol::AEntity(_) => {}
            SelCol::Expr(e) => tables_in(e, &mut froms),
        }
    }
    for w in &s.wheres {
        tables_in(w, &mut froms);
    }
    let mut from: Vec<&'static ModelDesc> = Vec::new();
    for f in s.from.iter().copied() {
        push_from(&mut from, f);
    }
    for f in froms {
        if !s.joins.iter().any(|(j, _, _, a)| a.is_none() && covers(j, f)) && !poly_covers(s, f) {
            push_from(&mut from, f);
        }
    }
    // (SQLAlchemy's _get_display_froms: only a statement with more than one FROM correlates implicitly)
    if !r.outer.is_empty() && from.len() + s.from_subs.len() + s.from_aliases.len() > 1 {
        from.retain(|m| !r.outer.iter().any(|o| covers(o, m)));
        if from.is_empty() && s.from_subs.is_empty() && s.from_aliases.is_empty() {
            return Err(Exc::msg(&INVALID_REQUEST_ERROR, "Select statement returned no FROM clauses due to auto-correlation; specify correlate(<tables>) to control correlation manually."));
        }
    }
    let outer_len = r.outer.len();
    // the scalar subqueries of the columns correlate to this SELECT's tables too
    r.outer.extend(from.iter().copied().chain(s.joins.iter().map(|j| j.0)));
    for (i, c) in s.cols.iter().enumerate() {
        if i > 0 {
            r.sql += ", ";
        }
        match c {
            SelCol::Entity(m) => {
                r.sql += &(0..m.cols.len()).map(|i| m.col_ref(i)).collect::<Vec<_>>().join(", ");
            }
            SelCol::AEntity(a) => {
                let n = r.alias_name(a);
                r.sql += &a.model.cols.iter().map(|c| format!("{n}.{}", c.q())).collect::<Vec<_>>().join(", ");
            }
            SelCol::Expr(e) => render(r, e)?,
        }
    }
    // the subqueries of the FROM list are not correlated
    r.outer.truncate(outer_len);
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
    let mut alias_from: Vec<Arc<Alias>> = s.from_aliases.clone();
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
        let mut items: Vec<String> = from.iter().map(|m| m.from_sql() + &poly_joins(s, m)).collect();
        for a in &alias_from {
            let n = r.alias_name(a);
            if !a.model.joins.is_empty() {
                return Err(Exc::type_error(format!("py2axum: an alias of {} (joined table inheritance) is not supported", a.model.name)));
            }
            items.push(format!("{} AS {n}", a.model.qt()));
        }
        r.sql += &items.join(", ");
        let from = items;
        for (i, q) in subs.iter().enumerate() {
            if i > 0 || !from.is_empty() {
                r.sql += ", ";
            }
            r.sql.push('(');
            render_select(r, &q.sel)?;
            r.sql += &format!(") AS {}", quote_ident(&q.name));
        }
    }
    let render_subs = |r: &mut Rend, at: usize| -> R<()> {
        for (_, q, on, outer) in s.join_subs.iter().filter(|j| j.0 == at) {
            r.sql += if *outer { " LEFT OUTER JOIN (" } else { " JOIN (" };
            render_select(r, &q.sel)?;
            r.sql += &format!(") AS {} ON ", quote_ident(&q.name));
            render(r, on)?;
        }
        Ok(())
    };
    for (i, (m, on, outer, al)) in s.joins.iter().enumerate() {
        render_subs(r, i)?;
        r.sql += if s.full_joins.contains(&i) { " FULL OUTER JOIN " } else if *outer { " LEFT OUTER JOIN " } else { " JOIN " };
        let pj = if al.is_none() { poly_joins(s, m) } else { String::new() };
        if m.joins.is_empty() && pj.is_empty() {
            r.sql += &m.qt();
        } else if al.is_none() {
            // a joined-inheritance subclass (or a with_polymorphic() entity): its tables joined first, like
            // SQLAlchemy's nested join
            r.sql += &format!("({}{pj})", m.from_sql());
        } else {
            return Err(Exc::type_error(format!("py2axum: an alias of {} (joined table inheritance) is not supported", m.name)));
        }
        if let Some(a) = al {
            let n = r.alias_name(a);
            r.sql += &format!(" AS {n}");
        }
        let on = match (on, if al.is_none() { poly_filter(m) } else { None }) {
            (Some(on), Some(f)) => Some(Sql::Bool("AND", vec![on.clone(), f])),
            (Some(on), None) => Some(on.clone()),
            (None, f) => f,
        };
        if let Some(on) = &on {
            r.sql += " ON ";
            render(r, on)?;
        }
    }
    render_subs(r, s.joins.len())?;
    // subqueries of the WHERE and after see this SELECT's tables as enclosing (auto-correlation)
    r.outer.extend(from.iter().copied().chain(s.joins.iter().map(|j| j.0)));
    // single table inheritance: a subclass selected or joined reads only the rows of its identities
    let mut wheres = s.wheres.clone();
    let mut seen: Vec<&'static ModelDesc> = Vec::new();
    for m in classes.into_iter().chain(from.iter().copied()) {
        // a class read through a join has its identities in that join's ON clause; a subclass read through a
        // with_polymorphic() entity has none
        if (s.joins.iter().any(|j| j.3.is_none() && covers(j.0, m)) && !from.iter().any(|f| covers(f, m))) || poly_covers(s, m) {
            continue;
        }
        if !seen.iter().any(|x| std::ptr::eq(*x, m)) {
            seen.push(m);
            wheres.extend(poly_filter(m));
        }
    }
    render_where(r, &wheres)?;
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
    r.outer.truncate(outer_len);
    Ok(())
}

fn render_expr_standalone(e: &Sql) -> R<(String, usize)> {
    let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
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

/// JSON columns: a Python value (`None` included) stored as JSON text, like SQLAlchemy's JSON type: the
/// text of `json.dumps` as is (a `json` column keeps it verbatim, `->>` on an object or a list shows it)
fn json_param(v: &V) -> R<Bind> {
    let text = pyd::to_json(v, &pyd::DUMPS, false)?;
    Ok(Bind::V(V::Str(Arc::from(text)), Some(ColTy::Str)))
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
        SelCol::AEntity(a) => a.name.as_deref().unwrap_or(a.model.name),
        SelCol::Expr(e) => match e {
            Sql::Label(_, l) => l.as_str(),
            Sql::Composite(m, ci) => m.composites[*ci].name,
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
    /// The rows, consumed: `all()`/iteration leave the result exhausted (a second `all()` is `[]`), `first()`/
    /// `one()`/`scalar()` close it (then ResourceClosedError), like SQLAlchemy's results.
    pub fn take_rows_as(&mut self, close: bool) -> R<Vec<V>> {
        let rows = match self.rows.take() {
            Some(rows) => rows,
            None if self.width == 0 || self.width == BULK_INSERT || self.width == BULK_CLOSED => {
                return Err(Exc::msg(&RESOURCE_CLOSED_ERROR, "This result object does not return rows. It has been closed automatically."));
            }
            None => return Err(Exc::msg(&RESOURCE_CLOSED_ERROR, "This result object is closed.")),
        };
        if !close {
            self.rows = Some(vec![]);
        }
        Ok(rows.into_iter().map(|r| self.row_value(r)).collect())
    }

    pub fn take_rows(&mut self) -> R<Vec<V>> {
        self.take_rows_as(false)
    }
}

/// `width` of the result of an ORM bulk INSERT without RETURNING: first()/scalar()/one_or_none() are None
/// (then BULK_CLOSED: they raise "This result object is closed."), all()/fetchall()/scalars() always raise
/// ResourceClosedError "does not return rows"
const BULK_INSERT: usize = usize::MAX;
const BULK_CLOSED: usize = usize::MAX - 1;

pub fn result_method(res: &Arc<Mutex<QResult>>, name: &str) -> R {
    let mut q = res.lock();
    if q.rows.is_none() && matches!(name, "first" | "scalar" | "one_or_none" | "scalar_one_or_none") {
        if q.width == BULK_INSERT {
            q.width = BULK_CLOSED;
            return Ok(V::None);
        }
        if q.width == BULK_CLOSED {
            return Err(Exc::msg(&RESOURCE_CLOSED_ERROR, "This result object is closed."));
        }
    }
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
        "first" => Ok(q.take_rows_as(true)?.into_iter().next().unwrap_or(V::None)),
        "one" | "scalar_one" | "one_or_none" | "scalar_one_or_none" => {
            if name.starts_with("scalar_") {
                q.scalars = true;
            }
            let rows = q.take_rows_as(true)?;
            match (rows.len(), name.ends_with("or_none")) {
                (0, true) => Ok(V::None),
                (0, false) => Err(Exc::msg(&NO_RESULT_FOUND, "No row was found when one was required")),
                (1, _) => Ok(rows.into_iter().next().unwrap()),
                _ => Err(Exc::msg(&MULTIPLE_RESULTS_FOUND, "Multiple rows were found when exactly one was required")),
            }
        }
        "scalar" => {
            q.scalars = true;
            Ok(q.take_rows_as(true)?.into_iter().next().unwrap_or(V::None))
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
        "rowcount" if res.lock().rowcount == i64::MIN => Err(Exc::attr_error("'IteratorResult' object has no attribute 'rowcount'")),
        "rowcount" => Ok(V::Int(res.lock().rowcount)),
        // a statement that returns no rows (INSERT/UPDATE/DELETE without RETURNING) has a closed result
        "returns_rows" => Ok(V::Bool(res.lock().rows.is_some())),
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

static SESSION_PARAMS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// `create_[async_]engine(connect_args=...)`: the session parameters the driver would send at connect
/// (psycopg `options="-c name=value ..."`, asyncpg `server_settings={name: value}`). The pool applies
/// them on every connection (`session_params`); a `TimeZone` among them replaces the discovered one,
/// as psycopg then decodes timestamptz in it. Other `connect_args` keys stay without effect.
pub fn engine_connect_args(v: &V) -> R<()> {
    let items: Vec<(V, V)> = match v {
        V::Dict(d) => d.lock().values().cloned().collect(),
        _ => return Err(Exc::type_error("py2axum: connect_args must be a dict")),
    };
    let mut params = Vec::new();
    for (k, val) in items {
        match (k.as_str(), &val) {
            (Some("options"), V::Str(s)) => params.extend(libpq_options(s)?),
            (Some("server_settings"), V::Dict(d)) => {
                for (n, x) in d.lock().values() {
                    match (n.as_str(), x.as_str()) {
                        (Some(n), Some(x)) => params.push((n.to_string(), x.to_string())),
                        _ => return Err(Exc::type_error("py2axum: server_settings must map str to str")),
                    }
                }
            }
            (Some("options" | "server_settings"), _) => {
                return Err(Exc::type_error("py2axum: connect_args options/server_settings of an unexpected type"))
            }
            _ => {}
        }
    }
    if let Some((_, tz)) = params.iter().rev().find(|(n, _)| n.eq_ignore_ascii_case("timezone")) {
        set_db_tz(tz);
    }
    params.retain(|(n, _)| !n.eq_ignore_ascii_case("timezone"));
    // one value per name (an engine created again, per request even, does not grow the list)
    let mut all = SESSION_PARAMS.lock().unwrap();
    for (n, x) in params {
        match all.iter_mut().find(|(m, _)| m.eq_ignore_ascii_case(&n)) {
            Some(p) => *p = (n, x),
            None => all.push((n, x)),
        }
    }
    Ok(())
}

/// libpq `options`, split as the server does (`pg_split_opts`: whitespace, `\` escapes the next
/// character), then `-c name=value` / `--name=value` (`ParseLongOption`: `-` in the name is `_`).
fn libpq_options(s: &str) -> R<Vec<(String, String)>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    let mut started = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_whitespace() {
            if started {
                words.push(std::mem::take(&mut cur));
                started = false;
            }
        } else if c == '\\' {
            started = true;
            if let Some(n) = chars.next() {
                cur.push(n);
            }
        } else {
            started = true;
            cur.push(c);
        }
    }
    if started {
        words.push(cur);
    }
    let mut out = Vec::new();
    let mut it = words.into_iter();
    while let Some(w) = it.next() {
        let opt = if w == "-c" {
            it.next()
        } else if let Some(r) = w.strip_prefix("-c").or_else(|| w.strip_prefix("--")) {
            Some(r.to_string())
        } else {
            None
        };
        match opt.as_deref().and_then(|o| o.split_once('=')) {
            Some((n, x)) if !n.is_empty() => out.push((n.replace('-', "_"), x.to_string())),
            _ => return Err(Exc::value_error(format!("py2axum: unsupported libpq option in connect_args: {s:?} (only -c name=value and --name=value)"))),
        }
    }
    Ok(out)
}

/// Run on every new pool connection, after its TimeZone.
pub async fn session_params(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    let params = SESSION_PARAMS.lock().unwrap().clone();
    for (n, x) in params {
        sqlx::query("SELECT set_config($1, $2, false)").bind(n).bind(x).execute(&mut *conn).await?;
    }
    Ok(())
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
    /// the open transaction ran something else than a SELECT: other connections do not see what it sees
    wrote: bool,
    /// objects expunged (detached): their relationship cycles are broken with the session
    expunged: Vec<std::sync::Weak<ObjCell>>,
    /// every object attached to the session, weakly: one a failed flush, a rollback or a DELETE took out of
    /// `new`/`identity`/`deleted` keeps its loaded relationships (as in SQLAlchemy) until the session ends,
    /// then they are unloaded (parent <-> children: a cycle CPython's GC frees, `Arc` never does)
    attached: Vec<Weak<ObjCell>>,
    /// association rows to write at this flush (many-to-many collections changed)
    m2m: Vec<M2mOp>,
    /// objects DELETEd by a flush of the open transaction: persistent again (expired) if it rolls back
    tx_deleted: Vec<Weak<ObjCell>>,
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
struct IdMap(HashMap<(usize, Key), (Weak<ObjCell>, u64)>, u64);

impl IdMap {
    fn get(&self, k: &(usize, Key)) -> Option<Arc<ObjCell>> {
        self.0.get(k).and_then(|(w, _)| w.upgrade())
    }
    fn insert(&mut self, k: (usize, Key), o: &Arc<ObjCell>) {
        if self.0.len() > 64 && self.0.len().is_power_of_two() {
            self.0.retain(|_, (w, _)| w.strong_count() > 0);
        }
        self.1 += 1;
        self.0.insert(k, (Arc::downgrade(o), self.1));
    }
    fn remove(&mut self, k: &(usize, Key)) -> Option<Arc<ObjCell>> {
        self.0.remove(k).and_then(|(w, _)| w.upgrade())
    }
    /// in the order the objects entered the map (deterministic: what the flush walks, like SQLAlchemy's
    /// order of objects added to the session)
    fn entries(&self) -> Vec<((usize, Key), Arc<ObjCell>)> {
        let mut out: Vec<(u64, (usize, Key), Arc<ObjCell>)> =
            self.0.iter().filter_map(|(k, (w, n))| w.upgrade().map(|o| (*n, k.clone(), o))).collect();
        out.sort_by_key(|x| x.0);
        out.into_iter().map(|(_, k, o)| (k, o)).collect()
    }
    fn values(&self) -> Vec<Arc<ObjCell>> {
        self.entries().into_iter().map(|(_, o)| o).collect()
    }
}

impl SessInner {
    fn attach(&mut self, o: &Arc<ObjCell>) {
        if self.attached.len() >= 64 && self.attached.len().is_power_of_two() {
            self.attached.retain(|w| w.strong_count() > 0);
        }
        self.attached.push(Arc::downgrade(o));
    }
}

impl Drop for SessInner {
    /// Related objects point at each other (parent <-> children): break the cycles with the session. A
    /// transaction still open is rolled back now (CPython frees a session nothing references at once, and its
    /// connection goes back rolled back; sqlx would only roll back when the connection is next used, its locks
    /// held until then).
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    let _ = tx.rollback().await;
                });
            }
        }
        let gone: Vec<Arc<ObjCell>> = self.expunged.iter().chain(self.attached.iter()).filter_map(|w| w.upgrade()).collect();
        for o in self.identity.values().iter().chain(self.new.iter()).chain(self.deleted.iter()).chain(gone.iter()) {
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
    // inheritance: one identity per row of the hierarchy (`get(Employee, 1)` finds the Manager)
    Ok((m.root() as *const ModelDesc as usize, Key::of(pk)?))
}

/// `select(Subclass)` of single table inheritance: the discriminator among the identities of the class and its
/// subclasses (none for the base of the hierarchy)
fn poly_filter(m: &'static ModelDesc) -> Option<Sql> {
    let (Some(p), Some(parent)) = (m.poly_on, m.parent) else { return None };
    if parent.table != m.table {
        return None; // joined table inheritance: the inner join to the subclass's table selects its rows
    }
    let ids = std::iter::once(m).chain(m.subs.iter().copied()).filter_map(|d| d.identity);
    Some(Sql::In(Box::new(Sql::Col(m, p)), ids.map(|i| Sql::Param(V::str(i), Some(m.cols[p].ty), 0)).collect(), false))
}

/// The class of a row of `desc` (its discriminator's identity among the hierarchy)
fn poly_pick(desc: &'static ModelDesc, row: &PgRow, off: usize) -> R<&'static ModelDesc> {
    let Some(p) = desc.poly_on else { return Ok(desc) };
    if desc.subs.is_empty() {
        return Ok(desc);
    }
    let v = decode(row, off + p, row_tz())?;
    for d in std::iter::once(desc).chain(desc.subs.iter().copied()) {
        if matches!((&v, d.identity), (V::Str(s), Some(i)) if &**s == i) {
            return Ok(d);
        }
    }
    Err(Exc::msg(&ASSERTION_ERROR, format!("No such polymorphic_identity {} is defined", ops::repr(&v)?)))
}

/// a subclass whose tables a `with_polymorphic()` entity of the statement joins
fn poly_covers(s: &Select, m: &'static ModelDesc) -> bool {
    s.polys.iter().any(|(b, subs)| !std::ptr::eq(*b, m) && is_subclass(m, b) && subs.iter().any(|d| covers(d, m)))
}

/// the LEFT OUTER JOINs of a `with_polymorphic()` entity's subclass tables (joined table inheritance)
fn poly_joins(s: &Select, m: &'static ModelDesc) -> String {
    let mut out = String::new();
    let mut seen: Vec<&str> = m.tables();
    for (b, subs) in &s.polys {
        if !std::ptr::eq(*b, m) {
            continue;
        }
        for d in subs {
            for (t, c, pt, pc) in d.joins {
                if !seen.contains(t) {
                    seen.push(t);
                    out += &format!(" LEFT OUTER JOIN {} ON {}.{} = {}.{}", quote_ident(t), quote_ident(pt), quote_ident(pc), quote_ident(t), quote_ident(c));
                }
            }
        }
    }
    out
}

/// `a` already reads every table `b` reads (inheritance: a subclass covers its base; single table: equal)
fn covers(a: &'static ModelDesc, b: &'static ModelDesc) -> bool {
    std::ptr::eq(a, b) || (std::ptr::eq(a.root(), b.root()) && b.tables().iter().all(|t| a.tables().contains(t)))
}

/// a class joins the FROM list once: not when another entry covers its tables, replacing an entry it covers
fn push_from(froms: &mut Vec<&'static ModelDesc>, m: &'static ModelDesc) {
    if froms.iter().any(|x| covers(x, m)) {
        return;
    }
    if let Some(k) = froms.iter().position(|x| covers(m, x)) {
        froms[k] = m;
        froms.retain(|x| std::ptr::eq(*x, m) || !covers(m, x));
        return;
    }
    froms.push(m);
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
                wrote: false,
                expunged: Vec::new(),
                attached: Vec::new(),
                m2m: Vec::new(),
                tx_deleted: Vec::new(),
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
            s.attach(&o);
            s.new.push(o);
        } else if st.status == Status::Persistent && !o.sess.lock().upgrade().is_some_and(|x| Weak::ptr_eq(&Arc::downgrade(&x), &s.me)) {
            // a detached object (closed or expunged session): persistent in this one again
            drop(st);
            let k = ident(o.desc, &o.pk())?;
            if let Some(other) = s.identity.get(&k) {
                if !Arc::ptr_eq(&other, &o) {
                    return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                        "Can't attach instance <{} at 0x{:x}>; another instance with key {} is already present in this session.",
                        o.desc.name, Arc::as_ptr(&o) as usize, ops::repr(&o.pk()).unwrap_or_default())));
                }
            }
            *o.sess.lock() = s.me.clone();
            s.attach(&o);
            s.identity.insert(k, &o);
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
                if rel.m2o || rel.secondary.is_some() || rel.write_only {
                    // many-to-many: its association rows are deleted with the object (flush); write-only
                    // (passive_deletes=True): left to the database
                    continue;
                }
                let loaded = o.st.lock().rels[ri].clone();
                let kids = match loaded {
                    Some(v) => members(&v),
                    None if !rel.passive_deletes => {
                        Session::load_rel(s, &[o.clone()], o.desc, ri, &[]).await?;
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
            // inheritance: the subclass columns loaded now (selectin_polymorphic(), with_polymorphic(),
            // polymorphic_load), for the objects of each subclass whose own columns are not loaded
            let mut subs: Vec<&'static ModelDesc> = Vec::new();
            let listed = opts.iter().filter(|c| c.len() == 1 && c[0].0 == Lazy::SubPoly).map(|c| c[0].1);
            for d in listed.chain(desc.subs.iter().copied().filter(|d| d.poly_load)) {
                if is_subclass(d, desc) && !std::ptr::eq(d, desc) && !subs.iter().any(|x| std::ptr::eq(*x, d)) {
                    subs.push(d);
                }
            }
            for d in subs {
                let mut keys = Vec::new();
                for o in &objs {
                    let st = o.st.lock();
                    if is_subclass(o.desc, d) && st.status == Status::Persistent
                        && (desc.cols.len()..d.cols.len()).any(|i| matches!(st.vals[i], V::Unbound)) {
                        keys.push(st.vals[d.pk].clone());
                    }
                }
                if keys.is_empty() {
                    continue;
                }
                let ty = d.cols[d.pk].ty;
                let sel = Select {
                    cols: vec![SelCol::Entity(d)],
                    wheres: vec![Sql::In(Box::new(Sql::Col(d, d.pk)), keys.into_iter().map(|k| Sql::Param(k, Some(ty), 0)).collect(), false)],
                    ..Default::default()
                };
                let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
                render_select(&mut r, &sel)?;
                for row in Session::run(s, &r).await? {
                    Session::entity(s, d, &row, 0, &[])?;
                }
                Session::decode_pending(s).await?;
            }
            // options on a subclass's relationships (`selectinload(Manager.paperwork)` beside a query on Employee):
            // for the objects of that subclass
            let mut by_sub: Vec<(&'static ModelDesc, Vec<LoadChain>)> = Vec::new();
            for c in &opts {
                let d = c[0].1;
                if c[0].0 == Lazy::SubPoly || std::ptr::eq(d, desc) || !is_subclass(d, desc) {
                    continue;
                }
                match by_sub.iter_mut().find(|(x, _)| std::ptr::eq(*x, d)) {
                    Some((_, cs)) => cs.push(c.clone()),
                    None => by_sub.push((d, vec![c.clone()])),
                }
            }
            for (d, chains) in by_sub {
                let mine: Vec<Arc<ObjCell>> = objs.iter().filter(|o| is_subclass(o.desc, d)).cloned().collect();
                if !mine.is_empty() {
                    Session::eager(s, mine, d, chains, path.clone()).await?;
                }
            }
            for (ri, rel) in desc.rels.iter().enumerate() {
                let mine: Vec<&LoadChain> =
                    opts.iter().filter(|c| !c[0].0.col() && c[0].0 != Lazy::SubPoly && std::ptr::eq(c[0].1, desc) && c[0].2 == ri).collect();
                let target = rel.target as *const ModelDesc as usize;
                let kind = match mine.first() {
                    Some(c) if c[0].0 == Lazy::Default => if rel.lazy == Lazy::Selectin { Lazy::Selectin } else { rel.lazy },
                    Some(c) => c[0].0,
                    None if rel.lazy == Lazy::Selectin && !path.contains(&target) => Lazy::Selectin,
                    None => continue,
                };
                if rel.write_only {
                    return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                        "'{}.{}' does not support object population - eager loading cannot be applied.", desc.name, rel.name)));
                }
                let sub: Vec<LoadChain> = mine.iter().filter(|c| c.len() > 1).map(|c| c[1..].to_vec()).collect();
                if kind == Lazy::Select && !sub.is_empty() {
                    // kept for the lazy load, as SQLAlchemy keeps a query's options on the objects it loads
                    let sub = Arc::new(sub.clone());
                    for o in &objs {
                        o.st.lock().rel_opts[ri] = Some(sub.clone());
                    }
                }
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
                        let plan = col_plan(&sub, rel.target);
                        let loaded = Session::load_rel(s, &objs, desc, ri, &plan).await?;
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
    async fn load_rel(s: &mut SessInner, objs: &[Arc<ObjCell>], desc: &'static ModelDesc, ri: usize, plan: &[ColLoad]) -> R<Vec<Arc<ObjCell>>> {
        let rel = &desc.rels[ri];
        if let Some((assoc, a, b)) = rel.secondary {
            return Session::load_m2m(s, objs, rel, ri, assoc, a, b, plan).await;
        }
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
            let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
            render_select(&mut r, &sel)?;
            let rows = Session::run(s, &r).await?;
            for row in &rows {
                let o = Session::entity(s, t, row, 0, plan)?;
                // from the row: the object's own column may be deferred (`selectinload(A.b).load_only(B.x)`)
                let k = Key::of(&map_col(&t.cols[rel.remote], decode(row, rel.remote, row_tz())?))?;
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

    /// selectin / lazy loading of a many-to-many collection: `SELECT assoc.a, target.* FROM target JOIN assoc ON
    /// target.remote = assoc.b WHERE assoc.a IN (...)` (SQLAlchemy's shape), the rows grouped by `assoc.a`
    #[allow(clippy::too_many_arguments)]
    async fn load_m2m(s: &mut SessInner, objs: &[Arc<ObjCell>], rel: &'static RelDesc, ri: usize, assoc: &'static ModelDesc,
                      a: usize, b: usize, plan: &[ColLoad]) -> R<Vec<Arc<ObjCell>>> {
        let t = rel.target;
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
                st.rels[ri] = Some(V::list(vec![]));
                st.rel_snap[ri] = Some(vec![]);
                continue;
            }
            owners.push((o.clone(), k));
        }
        if owners.is_empty() {
            return Ok(vec![]);
        }
        let mut keys: Vec<V> = Vec::new();
        let mut seen = HashSet::new();
        for (_, k) in &owners {
            if seen.insert(Key::of(k)?) {
                keys.push(k.clone());
            }
        }
        let ty = assoc.cols[a].ty;
        let sel = Select {
            cols: vec![SelCol::Expr(Sql::Col(assoc, a)), SelCol::Entity(t)],
            from: vec![t],
            joins: vec![(assoc, Some(Sql::Bin("=", Box::new(Sql::Col(t, rel.remote)), Box::new(Sql::Col(assoc, b)))), false, None)],
            wheres: vec![Sql::In(Box::new(Sql::Col(assoc, a)), keys.into_iter().map(|k| Sql::Param(k, Some(ty), 0)).collect(), false)],
            ..Default::default()
        };
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
        render_select(&mut r, &sel)?;
        let rows = Session::run(s, &r).await?;
        let mut found: HashMap<Key, Vec<Arc<ObjCell>>> = HashMap::new();
        let mut related: Vec<Arc<ObjCell>> = Vec::new();
        for row in &rows {
            let k = Key::of(&map_col(&assoc.cols[a], decode(row, 0, row_tz())?))?;
            let o = Session::entity(s, t, row, 1, plan)?;
            found.entry(k).or_default().push(o.clone());
            if !related.iter().any(|x| Arc::ptr_eq(x, &o)) {
                related.push(o);
            }
        }
        for (o, k) in owners {
            let items = found.get(&Key::of(&k)?).cloned().unwrap_or_default();
            let v = V::list(items.iter().map(|x| V::Obj(x.clone())).collect());
            let mut st = o.st.lock();
            st.rel_snap[ri] = Some(members(&v));
            st.rels[ri] = Some(v);
        }
        Ok(related)
    }

    /// Relationship changes since the last flush, turned into foreign-key copies (`links`):
    /// many-to-one assignments, collection members added/removed (removed = NULL, or deleted with
    /// delete-orphan), and the save-update cascade (transient related objects become pending).
    fn sync_rels(s: &mut SessInner) -> R<()> {
        s.m2m.clear();
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
                            s.attach(&x);
                            s.new.push(x.clone());
                            queue.push(x.clone());
                        }
                    }
                }
                if let Some((assoc, a, b)) = rel.secondary {
                    // many-to-many: rows of the association table added and removed (once when both sides changed)
                    let before = snap.unwrap_or_default();
                    for (list, other, add) in [(&now, &before, true), (&before, &now, false)] {
                        for m in list {
                            let V::Obj(x) = m else { continue };
                            if other.iter().any(|y| same(y, m)) {
                                continue;
                            }
                            let mut pairs = [(a, o.clone(), rel.local), (b, x.clone(), rel.remote)];
                            pairs.sort_by_key(|p| p.0);
                            if !s.m2m.iter().any(|op| op.add == add && std::ptr::eq(op.assoc, assoc)
                                && op.pairs.iter().zip(pairs.iter()).all(|(p, q)| p.0 == q.0 && Arc::ptr_eq(&p.1, &q.1))) {
                                s.m2m.push(M2mOp { assoc, pairs, add });
                            }
                        }
                    }
                    let mut st = o.st.lock();
                    st.rel_snap[ri] = if rel.write_only { None } else { Some(now) };
                    if rel.write_only {
                        st.rels[ri] = None;
                    }
                    st.rel_set[ri] = false;
                    continue;
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
                // the member's many-to-one of a back reference: what SQLAlchemy's backref event set on the append
                // or the removal (unless assigned since the last flush)
                let back = rel.back.and_then(|b| rel.target.rel_index(b)).filter(|bi| rel.target.rels[*bi].m2o);
                for m in &now {
                    if let V::Obj(x) = m {
                        if !before.iter().any(|b| same(b, m)) {
                            let mut xs = x.st.lock();
                            xs.links.push((rel.remote, Some(o.clone()), rel.local));
                            if let Some(bi) = back.filter(|bi| !xs.rel_set[*bi]) {
                                xs.rels[bi] = Some(V::Obj(o.clone()));
                            }
                            drop(xs);
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
                            let mut xs = x.st.lock();
                            xs.links.push((rel.remote, None, 0));
                            if let Some(bi) = back.filter(|bi| !xs.rel_set[*bi]) {
                                if xs.rels[bi].as_ref().is_none_or(|c| same(c, &V::Obj(o.clone()))) {
                                    xs.rels[bi] = Some(V::None);
                                }
                            }
                            drop(xs);
                            s.strong.push(x.clone());
                        }
                    }
                }
                let mut st = o.st.lock();
                st.rel_snap[ri] = if rel.write_only { None } else { Some(now) };
                if rel.write_only {
                    st.rels[ri] = None; // a write-only collection holds nothing once flushed
                }
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
            st.absent[col] = false;
            st.modified[col] = true;
        }
    }

    async fn begin(s: &mut SessInner) -> R<()> {
        if s.conn_closed {
            return Err(Exc::msg(&RESOURCE_CLOSED_ERROR, "This Connection is closed"));
        }
        if s.tx.is_none() {
            s.tx = Some(s.pool.begin().await?);
            s.wrote = false;
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
        if !r.sql.starts_with("SELECT ") {
            s.wrote = true;
        }
        let args = Session::args(s, r).await?;
        let tx = s.tx.as_mut().unwrap();
        sqlx::query_with(&r.sql, args).fetch_all(&mut **tx).await.map_err(|e| sql_error(e, &r.sql))
    }

    async fn run_exec(s: &mut SessInner, r: &Rend) -> R<u64> {
        Session::begin(s).await?;
        s.wrote = true;
        let args = Session::args(s, r).await?;
        let tx = s.tx.as_mut().unwrap();
        Ok(sqlx::query_with(&r.sql, args).execute(&mut **tx).await.map_err(|e| sql_error(e, &r.sql))?.rows_affected())
    }

    async fn flush_inner(s: &mut SessInner) -> R<()> {
        // the composites of what is INSERTed or UPDATEd are built again when read (SQLAlchemy's after_insert /
        // after_update handlers pop them)
        for o in s.new.iter().chain(s.strong.iter()).filter(|o| !o.desc.composites.is_empty()) {
            o.st.lock().comps.iter_mut().for_each(|c| *c = None);
        }
        Session::sync_rels(s)?;
        // INSERTs, referenced tables first
        let mut new = std::mem::take(&mut s.new);
        new.sort_by_key(|o| o.desc.uow_rank());
        for o in new {
            Session::apply_links(&o);
            let desc = o.desc;
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
            // one INSERT per table (joined table inheritance: the root's first, its key RETURNING, then each
            // subclass table with that key); else the model's table
            let joined = !desc.joins.is_empty();
            let tables: Vec<(&'static str, &'static str)> = if joined {
                std::iter::once((desc.joins[0].2, desc.joins[0].3)).chain(desc.joins.iter().map(|j| (j.0, j.1))).collect()
            } else {
                vec![(desc.table, "")]
            };
            let pki = desc.pk;
            let mut got: Vec<(usize, V)> = Vec::new();
            if let Some(f) = s.savepoints.last_mut() {
                f.inserted.push(o.clone());
            }
            for (ti, (table, key)) in tables.iter().enumerate() {
                let in_t = |i: &usize| desc.cols[*i].table.unwrap_or(desc.table) == *table && !(joined && *i == pki);
                let gcols: Vec<usize> = cols.iter().copied().filter(|i| in_t(i)).collect();
                let mut gfetch: Vec<usize> = fetch.iter().copied().filter(|i| in_t(i)).collect();
                let vals = o.st.lock().vals.clone();
                // the key: generated by the root's INSERT, given to the other tables
                let key_val = if joined && !matches!(vals[pki], V::Unbound) { Some(vals[pki].clone()) } else { None };
                let fetch_key = joined && ti == 0 && key_val.is_none();
                let mut names: Vec<String> = gcols.iter().map(|i| desc.cols[*i].q().into_owned()).collect();
                if key_val.is_some() {
                    names.insert(0, quote_ident(key).into_owned());
                }
                let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
                if names.is_empty() {
                    r.sql = format!("INSERT INTO {} DEFAULT VALUES", quote_ident(table));
                } else {
                    r.sql = format!("INSERT INTO {} ({}) VALUES (", quote_ident(table), names.join(", "));
                    if let Some(v) = &key_val {
                        r.bind(v.clone(), Some(desc.cols[pki].ty));
                        if !gcols.is_empty() {
                            r.sql += ", ";
                        }
                    }
                    for (k, i) in gcols.iter().enumerate() {
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
                let mut ret: Vec<String> = gfetch.iter().map(|i| desc.cols[*i].q().into_owned()).collect();
                if fetch_key {
                    ret.insert(0, quote_ident(key).into_owned());
                    gfetch.insert(0, pki);
                }
                if ret.is_empty() {
                    Session::run_exec(s, &r).await?;
                    continue;
                }
                r.sql += &format!(" RETURNING {}", ret.join(", "));
                let rows = Session::run(s, &r).await?;
                for (k, i) in gfetch.iter().enumerate() {
                    let mut v = map_col(&desc.cols[*i], decode(&rows[0], k, row_tz())?);
                    if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = desc.cols[*i].ty {
                        v = f(&super::root_cx(), V::None, vec![v, V::None]).await?;
                    }
                    if *i == pki && joined {
                        o.st.lock().vals[pki] = v.clone(); // the next tables' key
                    }
                    got.push((*i, v));
                }
            }
            let pk;
            {
                let mut st = o.st.lock();
                for (i, v) in got {
                    st.vals[i] = v;
                }
                pk = desc.pk_of(&st.vals);
                let st = &mut *st;
                for (i, v) in st.vals.iter_mut().enumerate() {
                    if matches!(v, V::Sql(_)) {
                        *v = V::Unbound; // SQL-side default: expired until refreshed
                    } else if matches!(v, V::Unbound) && !desc.cols[i].server_default {
                        *v = V::None; // never set, no default: SQLAlchemy reads None without loading
                        st.absent[i] = true; // ... and leaves it out of the instance dict
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
            // one UPDATE per table with changes (joined table inheritance), each with its own onupdate= columns
            let joined = !desc.joins.is_empty();
            let tables: Vec<(&'static str, &'static str)> = if joined {
                std::iter::once((desc.joins[0].2, desc.joins[0].3)).chain(desc.joins.iter().map(|j| (j.0, j.1))).collect()
            } else {
                vec![(desc.table, "")]
            };
            let (mut expire, mut prefetched, mut fetched) = (Vec::new(), Vec::new(), Vec::new());
            for (table, key) in tables {
                let in_t = |i: usize| desc.cols[i].table.unwrap_or(desc.table) == table;
                if !changes.iter().any(|(i, _)| in_t(*i)) {
                    continue;
                }
                let mut r = Rend { sql: format!("UPDATE {} SET ", quote_ident(table)), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
                let mut first = true;
                for (i, v) in changes.iter().filter(|(i, _)| in_t(*i)) {
                    if !first {
                        r.sql += ", ";
                    }
                    first = false;
                    let c = &desc.cols[*i];
                    r.sql += &format!("{}=", c.q());
                    if c.ty == ColTy::JsonNull && v.is_none() {
                        r.bind(V::None, Some(ColTy::JsonNull));
                    } else if matches!(c.ty, ColTy::Json | ColTy::JsonNull) {
                        r.binds.push(json_param(v)?);
                        r.sql += &format!("CAST(${} AS JSON)", r.binds.len());
                    } else {
                        r.bind(v.clone(), Some(c.ty));
                    }
                }
                let mut texpire = Vec::new();
                for (i, c) in desc.cols.iter().enumerate() {
                    if !in_t(i) || changes.iter().any(|(j, _)| *j == i) {
                        continue;
                    }
                    match c.onupdate.eval().await? {
                        None => {}
                        Some(V::Sql(e)) => {
                            r.sql += &format!(", {}=", c.q());
                            render(&mut r, &e)?;
                            texpire.push(i);
                        }
                        Some(v) => {
                            r.sql += &format!(", {}=", c.q());
                            r.bind(v.clone(), Some(c.ty));
                            prefetched.push((i, v));
                        }
                    }
                }
                if joined {
                    r.sql += &format!(" WHERE {}.{} = ", quote_ident(table), quote_ident(key));
                    r.bind(pk.clone(), Some(desc.cols[desc.pk].ty));
                } else {
                    desc.where_pk(&mut r, &pk);
                }
                let n = if desc.eager_defaults && !texpire.is_empty() {
                    // eager_defaults=True: the SQL `onupdate=` values come back with the UPDATE
                    r.sql += &format!(" RETURNING {}", texpire.iter().map(|i| desc.cols[*i].q()).collect::<Vec<_>>().join(", "));
                    let rows = Session::run(s, &r).await?;
                    if let Some(row) = rows.first() {
                        for (k, i) in texpire.iter().enumerate() {
                            let mut v = map_col(&desc.cols[*i], decode(row, k, row_tz())?);
                            if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = desc.cols[*i].ty {
                                v = f(&super::root_cx(), V::None, vec![v, V::None]).await?;
                            }
                            fetched.push((*i, v));
                        }
                        texpire.clear();
                    }
                    rows.len() as u64
                } else {
                    Session::run_exec(s, &r).await?
                };
                if n != 1 {
                    // the row was deleted (or its key changed) behind the session's back: SQLAlchemy checks the
                    // rowcount of each UPDATE (one statement per object here; see docs/supported.md)
                    return Err(Exc::msg(&STALE_DATA_ERROR, format!(
                        "UPDATE statement on table '{}' expected to update 1 row(s); {n} were matched.", table.trim_matches('"'))));
                }
                expire.extend(texpire);
            }
            if let Some(f) = s.savepoints.last_mut() {
                f.updated.push(o.clone());
            }
            let mut st = o.st.lock();
            for (i, v) in prefetched.into_iter().chain(fetched) {
                st.vals[i] = v;
                st.absent[i] = false;
            }
            for i in expire {
                st.vals[i] = V::Unbound;
                st.absent[i] = false;
                st.expired = true;
            }
            st.committed = st.vals.clone();
            st.modified = vec![false; desc.cols.len()];
        }
        // association rows (many-to-many): removed, then added
        let ops = std::mem::take(&mut s.m2m);
        for add in [false, true] {
            for op in ops.iter().filter(|op| op.add == add) {
                let vals: Vec<(usize, V)> = op.pairs.iter().map(|(c, x, src)| (*c, x.st.lock().vals[*src].clone())).collect();
                let t = op.assoc;
                let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
                if add {
                    r.sql = format!("INSERT INTO {} ({}, {}) VALUES (", t.qt(), t.cols[vals[0].0].q(), t.cols[vals[1].0].q());
                    r.bind(vals[0].1.clone(), Some(t.cols[vals[0].0].ty));
                    r.sql += ", ";
                    r.bind(vals[1].1.clone(), Some(t.cols[vals[1].0].ty));
                    r.sql.push(')');
                } else {
                    r.sql = format!("DELETE FROM {} WHERE {}.{} = ", t.qt(), t.qt(), t.cols[vals[0].0].q());
                    r.bind(vals[0].1.clone(), Some(t.cols[vals[0].0].ty));
                    r.sql += &format!(" AND {}.{} = ", t.qt(), t.cols[vals[1].0].q());
                    r.bind(vals[1].1.clone(), Some(t.cols[vals[1].0].ty));
                }
                Session::run_exec(s, &r).await?;
            }
        }
        // DELETEs
        for o in std::mem::take(&mut s.deleted) {
            let desc = o.desc;
            let pk = o.pk();
            // a deleted object leaves the association tables of its many-to-many relationships
            for rel in desc.rels {
                if let (Some((t, a, _)), false) = (rel.secondary, rel.passive_deletes) {
                    let v = o.st.lock().vals[rel.local].clone();
                    let mut r = Rend { sql: format!("DELETE FROM {} WHERE {}.{} = ", t.qt(), t.qt(), t.cols[a].q()),
                                       binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
                    r.bind(v, Some(t.cols[a].ty));
                    Session::run_exec(s, &r).await?;
                }
            }
            if desc.joins.is_empty() {
                let mut r = Rend { sql: format!("DELETE FROM {}", desc.qt()), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
                desc.where_pk(&mut r, &pk);
                Session::run_exec(s, &r).await?;
            } else {
                // joined table inheritance: the subclass tables first, the root's last
                let tables: Vec<(&'static str, &'static str)> =
                    std::iter::once((desc.joins[0].2, desc.joins[0].3)).chain(desc.joins.iter().map(|j| (j.0, j.1))).collect();
                for (table, key) in tables.into_iter().rev() {
                    let mut r = Rend { sql: format!("DELETE FROM {} WHERE {}.{} = ", quote_ident(table), quote_ident(table), quote_ident(key)),
                                       binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
                    r.bind(pk.clone(), Some(desc.cols[desc.pk].ty));
                    Session::run_exec(s, &r).await?;
                }
            }
            s.identity.remove(&ident(desc, &pk)?);
            o.st.lock().status = Status::Deleted;
            s.tx_deleted.push(Arc::downgrade(&o));
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
        s.wrote = true;
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
                            st.absent[c] = false;
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
        s.wrote = true;
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
    /// `session.expunge_all()` / `expunge(obj)`: the objects leave the session without a flush (pending ones become
    /// transient, the others detached: an attribute that is not loaded raises DetachedInstanceError)
    pub async fn expunge(&self, only: Option<&V>) -> R<()> {
        let mut guard = self.0.lock().await;
        let s = &mut *guard;
        let one = match only {
            Some(V::Obj(o)) => Some(o.clone()),
            Some(other) => return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("Class '{}' is not mapped", other.type_name()))),
            None => None,
        };
        let mine = |o: &Arc<ObjCell>| one.as_ref().is_none_or(|x| Arc::ptr_eq(x, o));
        let mut objs: Vec<Arc<ObjCell>> = Vec::new();
        for (k, o) in s.identity.entries() {
            if mine(&o) {
                s.identity.remove(&k);
                objs.push(o);
            }
        }
        for list in [&mut s.new, &mut s.deleted, &mut s.strong] {
            let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(list).into_iter().partition(|o| mine(o));
            *list = kept;
            objs.extend(gone);
        }
        if let Some(o) = &one {
            if objs.is_empty() {
                return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!("Instance <{} at 0x{:x}> is not present in this Session",
                                                                     o.desc.name, Arc::as_ptr(o) as usize)));
            }
        }
        for o in objs {
            if s.expunged.iter().any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&o))) {
                continue;
            }
            *o.sess.lock() = std::sync::Weak::new();
            {
                let mut st = o.st.lock();
                st.status = match st.status {
                    Status::Pending | Status::Transient => Status::Transient,
                    _ => Status::Persistent,
                };
            }
            s.expunged.push(Arc::downgrade(&o));
        }
        Ok(())
    }

    /// `obj in session`: a pending or persistent object of this session
    pub fn contains(&self, item: &V) -> R<bool> {
        let V::Obj(o) = item else {
            return Err(Exc::msg(&ARGUMENT_ERROR, format!("Class '{}' is not mapped", item.type_name())));
        };
        let mine = o.sess.lock().upgrade().is_some_and(|x| Arc::ptr_eq(&x, &self.0));
        Ok(mine && matches!(o.st.lock().status, Status::Pending | Status::Persistent))
    }

    /// `session.close()`: every object leaves the session (detached, keeping what it loaded), then the
    /// transaction is rolled back; the session stays usable (SQLAlchemy's close, expunge first)
    pub async fn close(&self) -> R<()> {
        self.expunge(None).await?;
        let mut s = self.0.lock().await;
        s.conn_closed = false;
        s.tx_deleted.clear();
        if let Some(tx) = s.tx.take() {
            let _ = tx.rollback().await;
        }
        s.savepoints.clear();
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
        s.tx_deleted.clear();
        if s.expire_on_commit {
            for o in s.identity.values() {
                let mut st = o.st.lock();
                for i in 0..o.desc.cols.len() {
                    if !o.desc.is_pk(i) {
                        st.vals[i] = V::Unbound;
                        st.absent[i] = false;
                    }
                }
                st.unload_rels();
                st.expired = true;
            }
        }
        Ok(())
    }

    /// `session.expire_all()`: every persistent object's attributes are reloaded on next access (changes not
    /// flushed are lost)
    pub async fn expire_all(&self) -> R<()> {
        let mut s = self.0.lock().await;
        for o in s.identity.values() {
            expire_whole(&o);
        }
        s.strong.clear();
        Ok(())
    }

    /// `session.expire(obj)`: the object's attributes, and those of the persistent objects reached through
    /// loaded `refresh-expire` relationships, reloaded on next access; `expire(obj, ["col", "rel"])`: only
    /// those attributes (a falsy list is the whole object, like SQLAlchemy). Changes of the expired
    /// attributes not flushed are lost.
    pub async fn expire(&self, obj: &V, names: Option<&V>) -> R<()> {
        let o = match obj {
            V::Obj(o) => o.clone(),
            V::None | V::Bool(_) | V::Int(_) | V::Float(_) | V::Str(_) | V::Bytes(_) | V::List(_) | V::Tuple(_) | V::Dict(_) | V::Set(_) => {
                return Err(Exc::msg(&UNMAPPED_INSTANCE_ERROR, format!("Class 'builtins.{}' is not mapped", obj.type_name())));
            }
            other => return Err(Exc::type_error(format!("py2axum: session.expire() of a {} is not supported", other.type_name()))),
        };
        let mut s = self.0.lock().await;
        let mine = o.st.lock().status == Status::Persistent
            && ident(o.desc, &o.pk()).ok().and_then(|k| s.identity.get(&k)).is_some_and(|x| Arc::ptr_eq(&x, &o));
        if !mine {
            return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                "Instance '<{} at 0x{:x}>' is not persistent within this Session", o.desc.name, Arc::as_ptr(&o) as usize)));
        }
        let names = match names {
            Some(n) if ops::truthy(n)? => ops::iter(n)?,
            _ => {
                // the refresh-expire cascade is collected first (expiring unloads the relationships)
                let mut todo = vec![o.clone()];
                let mut i = 0;
                while i < todo.len() {
                    let x = todo[i].clone();
                    i += 1;
                    let st = x.st.lock();
                    for (ri, rel) in x.desc.rels.iter().enumerate() {
                        if !rel.refresh_expire {
                            continue;
                        }
                        for m in st.rels[ri].as_ref().map(members).unwrap_or_default() {
                            if let V::Obj(y) = m {
                                if !todo.iter().any(|z| Arc::ptr_eq(z, &y)) {
                                    todo.push(y);
                                }
                            }
                        }
                    }
                }
                for x in todo {
                    // a pending object reached by the cascade stays pending (SQLAlchemy 2.1)
                    if x.st.lock().status == Status::Persistent {
                        expire_whole(&x);
                        s.strong.retain(|y| !Arc::ptr_eq(y, &x));
                    }
                }
                return Ok(());
            }
        };
        let desc = o.desc;
        // one name after the other: an unknown one raises KeyError once the previous ones are expired
        for n in names {
            let key = match &n {
                V::Str(k) => Some(k.to_string()),
                _ => None,
            };
            let key = key.as_deref().unwrap_or("");
            let mut st = o.st.lock();
            if let Some(i) = desc.col_index(key) {
                if desc.is_pk(i) {
                    st.pk_expired = true;
                } else {
                    st.vals[i] = V::Unbound;
                    st.absent[i] = false;
                }
                st.modified[i] = false;
                // an expired column loads with the others; a column deferred by the mapping keeps its deferred
                // loader (its group, when read)
                st.defer[i] = desc.cols[i].default_load();
                for (ci, c) in desc.composites.iter().enumerate() {
                    if c.cols.contains(&i) {
                        st.comps[ci] = None;
                    }
                }
            } else if let Some(ri) = desc.rel_index(key) {
                st.rels[ri] = None;
                st.rel_snap[ri] = None;
                st.rel_set[ri] = false;
            } else if let Some(ci) = desc.composite(key) {
                st.comps[ci] = None;
            } else {
                return Err(Exc::new(&KEY_ERROR, vec![n.clone()]));
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
        // objects DELETEd by a flush of the rolled back transaction: persistent again, expired (SQLAlchemy
        // restores the transaction's deleted states)
        for o in std::mem::take(&mut s.tx_deleted).iter().filter_map(|w| w.upgrade()) {
            if o.st.lock().status == Status::Deleted {
                o.st.lock().status = Status::Persistent;
                if let Ok(k) = ident(o.desc, &o.pk()) {
                    if s.identity.get(&k).is_none() {
                        s.identity.insert(k, &o);
                    }
                }
            }
        }
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
                    st.absent[i] = false;
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
            let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
            render_select(&mut r, &desc.pk_select(&o.pk()))?;
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
            if cols.iter().any(|&i| desc.is_pk(i)) {
                st.pk_expired = false;
            }
            for (i, v) in cols.into_iter().zip(decoded) {
                st.vals[i] = v.clone();
                st.absent[i] = false;
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
            Session::load_rel(&mut s, &[o.clone()], desc, ri, &[]).await?;
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
        // a deferred column: it and its `deferred_group=`; anything else: the expired columns (not the deferred)
        let mut group_only = false;
        let expired: Vec<usize> = {
            let st = o.st.lock();
            let key_expired = o.key_expired(&st);
            let unbound = |i: usize| matches!(st.vals[i], V::Unbound) || (key_expired && desc.is_pk(i));
            if st.status != Status::Persistent {
                vec![]
            } else if let Some(c) = desc.col_index(name).filter(|&c| unbound(c) && o.unloaded(&st, c) == ColLoad::Defer) {
                group_only = true;
                let g = desc.cols[c].group;
                (0..desc.cols.len())
                    .filter(|&i| i == c || (g.is_some() && desc.cols[i].group == g && unbound(i) && o.unloaded(&st, i) == ColLoad::Defer))
                    .collect()
            } else {
                (0..desc.cols.len()).filter(|&i| unbound(i) && o.unloaded(&st, i) == ColLoad::Load).collect()
            }
        };
        if !expired.is_empty() {
            let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
            render_select(&mut r, &desc.pk_select(&o.pk()))?;
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
            if !group_only {
                // the deferred columns keep the mapping's strategy once the object is not expired any more
                for i in 0..desc.cols.len() {
                    st.defer[i] = o.unloaded(&st, i);
                }
            }
            // SQLAlchemy's refresh handler: only the composites over a reloaded column are built again
            let refreshed: Vec<usize> =
                (0..desc.composites.len()).filter(|&ci| desc.composites[ci].cols.iter().any(|i| expired.contains(i))).collect();
            for (i, v) in expired.into_iter().zip(decoded) {
                st.vals[i] = v.clone();
                st.absent[i] = false;
                st.committed[i] = v;
                st.modified[i] = false;
                st.defer[i] = ColLoad::Load;
            }
            if !refreshed.is_empty() {
                drop(st);
                o.build_composites_of(Some(&refreshed))?;
                st = o.st.lock();
            }
            if !group_only {
                st.expired = false;
                st.pk_expired = false;
            }
        }
        if let Some(ri) = desc.rel_index(name) {
            if o.st.lock().rels[ri].is_none() {
                let sub = o.st.lock().rel_opts[ri].clone();
                let target = desc.rels[ri].target;
                let plan = sub.as_ref().map(|c| col_plan(c, target)).unwrap_or_default();
                let loaded = Session::load_rel(&mut s, &[o.clone()], desc, ri, &plan).await?;
                if let Some(sub) = sub {
                    Session::eager(&mut s, loaded, target, (*sub).clone(), vec![target as *const ModelDesc as usize]).await?;
                }
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
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
        render_select(&mut r, &desc.pk_select(&pk))?;
        let rows = Session::run(&mut s, &r).await?;
        match rows.first() {
            Some(row) => {
                o.load_row(row, 0, row_tz(), &[])?;
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
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
        render_select(&mut r, &desc.pk_select(pk))?;
        let rows = Session::run(&mut s, &r).await?;
        match rows.first() {
            Some(row) => {
                let o = Session::entity(&mut s, desc, row, 0, &col_plan(&opts, desc))?;
                Session::eager(&mut s, vec![o.clone()], desc, opts, vec![desc as *const ModelDesc as usize]).await?;
                Session::decode_pending(&mut s).await?;
                Ok(V::Obj(o))
            }
            None => Ok(V::None),
        }
    }

    /// an entity column of a result row: None when its key is NULL (the missing side of an outer join)
    fn entity_v(s: &mut SessInner, desc: &'static ModelDesc, row: &PgRow, off: usize, plan: &[ColLoad]) -> R<V> {
        for i in desc.pks {
            if !decode(row, off + i, row_tz())?.is_none() {
                return Ok(V::Obj(Session::entity(s, desc, row, off, plan)?));
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
            o.load_row_from(row, off, db_tz(), &[], desc.cols.len())?;
        }
        Ok(())
    }

    /// `plan`: the statement's column options (empty: the mapping's); a column it loads is also loaded into an
    /// identity already there whose value is not loaded (SQLAlchemy's partial population of unloaded attributes)
    fn entity(s: &mut SessInner, desc: &'static ModelDesc, row: &PgRow, off: usize, plan: &[ColLoad]) -> R<Arc<ObjCell>> {
        let mut vals = vec![V::None; desc.cols.len()];
        for i in desc.pks {
            vals[*i] = decode(row, off + i, row_tz())?;
        }
        let key = ident(desc, &desc.pk_of(&vals))?;
        let read = desc.cols.len();
        if let Some(o) = s.identity.get(&key) {
            if o.st.lock().expired {
                o.load_row_from(row, off, row_tz(), plan, read)?;
                Session::loaded(s, &o);
            } else {
                let mut st = o.st.lock();
                for i in 0..desc.cols.len() {
                    let how = plan.get(i).copied().unwrap_or_else(|| desc.cols[i].default_load());
                    if how == ColLoad::Load && (matches!(st.vals[i], V::Unbound) || st.absent[i]) && st.status == Status::Persistent {
                        let v = map_col(&desc.cols[i], decode(row, off + i, row_tz())?);
                        st.committed[i] = v.clone();
                        st.vals[i] = v;
                        st.absent[i] = false;
                        st.defer[i] = ColLoad::Load;
                    }
                }
            }
            return Ok(o);
        }
        let o = ObjCell::transient(poly_pick(desc, row, off)?);
        o.load_row_from(row, off, row_tz(), plan, read)?;
        Session::loaded(s, &o);
        *o.sess.lock() = s.me.clone();
        s.attach(&o);
        s.identity.insert(key, &o);
        Ok(o)
    }

    /// decoded rows of a SELECT (or of a RETURNING clause): entities through the identity map, eager
    /// relationships, TypeDecorator results
    async fn select_result(s: &mut SessInner, sel: &Select, rows: Vec<PgRow>) -> R {
        let s = &mut *s;
        let mut out = Vec::with_capacity(rows.len());
        // the column options of each selected entity, once per statement
        let plans: HashMap<usize, Vec<ColLoad>> = sel
            .cols
            .iter()
            .filter_map(|c| match c {
                SelCol::Entity(m) if !sel.loads.is_empty() => Some((*m as *const ModelDesc as usize, col_plan(&sel.loads, m))),
                _ => None,
            })
            .collect();
        for row in &rows {
            let mut vals = Vec::new();
            let mut off = 0;
            for c in &sel.cols {
                match c {
                    SelCol::Entity(m) => {
                        vals.push(Session::entity_v(s, m, row, off, plans.get(&(*m as *const ModelDesc as usize)).map_or(&[][..], |u| &u[..]))?);
                        off += m.cols.len();
                    }
                    SelCol::AEntity(a) => {
                        vals.push(Session::entity_v(s, a.model, row, off, &[])?);
                        off += a.model.cols.len();
                    }
                    SelCol::Expr(Sql::Composite(m, ci)) => {
                        let c = &m.composites[*ci];
                        let mut parts = Vec::new();
                        for (k, i) in c.cols.iter().enumerate() {
                            parts.push(map_col(&m.cols[*i], decode(row, off + k, row_tz())?));
                        }
                        off += c.cols.len();
                        vals.push(composite_build(c, parts)?);
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
                // (eager: relationships, and the subclass columns of an inheritance hierarchy)
                if m.rels.is_empty() && m.subs.is_empty() {
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

    /// ORM bulk INSERT (`execute(insert(Model), [dicts])`) and bulk UPDATE by primary key
    /// (`execute(update(Model), [dicts])`): one statement per dict, in order (SQLAlchemy batches the rows of
    /// the same keys, with the same result), each one like a single-row statement (Python defaults, RETURNING
    /// entities in the identity map, UPDATE applied to the loaded objects)
    fn execute_bulk<'a>(&'a self, x: &'a Sql, many: Vec<V>) -> BoxR<'a, V> {
        Box::pin(async move {
            let mut rows: Option<Vec<Vec<V>>> = None;
            let (mut width, mut names) = (0, None);
            for p in &many {
                let V::Dict(d) = p else {
                    return Err(Exc::type_error("py2axum: a bulk INSERT/UPDATE takes a list of dicts"));
                };
                let mut items: Vec<(String, V)> = Vec::new();
                for (_, (k, v)) in d.lock().iter() {
                    let V::Str(k) = k else { return Err(Exc::type_error("py2axum: bulk INSERT/UPDATE dict keys must be strings")) };
                    items.push((k.to_string(), v.clone()));
                }
                if let Sql::Insert(ins) = x {
                    if !ins.m.joins.is_empty() {
                        // joined table inheritance: the row as an object of the class, flushed (one INSERT per table,
                        // the discriminator set); returned by `returning(Class)`, else not kept in the session
                        if !ins.rows.is_empty() || ins.conflict.is_some()
                            || !(ins.returning.is_empty() || matches!(ins.returning.as_slice(), [SelCol::Entity(e)] if std::ptr::eq(*e, ins.m))) {
                            return Err(Exc::type_error(format!("py2axum: insert({}) of a joined table inheritance subclass with \
                                values(), on_conflict or returning() other than the class is not supported", ins.m.name)));
                        }
                        let kw: Vec<(String, V)> = items.iter().filter(|(_, v)| !v.is_none() || ins.render_nulls).cloned().collect();
                        let obj = construct(ins.m, kw)?;
                        self.add(&obj)?;
                        self.flush().await?;
                        if ins.returning.is_empty() {
                            self.expunge(Some(&obj)).await?;
                        } else {
                            rows.get_or_insert_with(Vec::new).push(vec![obj]);
                            width = 1;
                        }
                        continue;
                    }
                }
                let one = match x {
                    Sql::Insert(ins) => {
                        if (!ins.rows.is_empty() && !ins.fixed) || ins.conflict.is_some() {
                            return Err(Exc::type_error("py2axum: insert(...).values()/on_conflict with a parameter list is not supported"));
                        }
                        // WriteOnlyCollection.insert(): the parent's foreign key first, in every row
                        let mut row = if ins.fixed { ins.rows[0].clone() } else { Vec::new() };
                        for (k, v) in &items {
                            let i = ins.m.col_index(k).ok_or_else(|| Exc::msg(&COMPILE_ERROR, format!("Unconsumed column names: {k}")))?;
                            if v.is_none() && !ins.render_nulls {
                                continue; // SQLAlchemy leaves a None out of the INSERT: the column's default applies
                            }
                            row.push((i, to_sql(v, Some(ins.m.cols[i].ty))));
                        }
                        let mut one = ins.clone();
                        one.rows = vec![row];
                        Sql::Insert(one)
                    }
                    Sql::Update(m, w, sets, _, ret) => {
                        if !w.is_empty() || !sets.is_empty() || !ret.is_empty() {
                            return Err(Exc::type_error("py2axum: update(...).where()/values()/returning() with a parameter list is not supported"));
                        }
                        let missing: Vec<&str> = m.pks.iter().filter(|i| !items.iter().any(|(k, _)| k == m.cols[**i].name))
                            .map(|i| m.cols[*i].sql).collect();
                        if !missing.is_empty() {
                            return Err(Exc::msg(&INVALID_REQUEST_ERROR, format!(
                                "No primary key value supplied for column(s) {}; per-row ORM Bulk UPDATE by Primary Key requires that records contain primary key values",
                                missing.iter().map(|c| format!("{}.{c}", m.table)).collect::<Vec<_>>().join(", "))));
                        }
                        let (mut wheres, mut set) = (Vec::new(), Vec::new());
                        for (k, v) in &items {
                            let i = m.col_index(k).ok_or_else(|| Exc::msg(&COMPILE_ERROR, format!("Unconsumed column names: {k}")))?;
                            if m.is_pk(i) {
                                wheres.push(Sql::Bin("=", Box::new(Sql::Col(m, i)), Box::new(Sql::Param(v.clone(), Some(m.cols[i].ty), 0))));
                            } else {
                                set.push((i, to_sql(v, Some(m.cols[i].ty))));
                            }
                        }
                        if set.is_empty() {
                            continue; // nothing but the key: SQLAlchemy skips the row
                        }
                        Sql::Update(m, wheres, set, true, vec![])
                    }
                    _ => unreachable!(),
                };
                let res = self.execute_params(&sql(one), None).await?;
                if let V::Result(r) = res {
                    let r = r.lock();
                    if let Some(got) = &r.rows {
                        rows.get_or_insert_with(Vec::new).extend(got.iter().cloned());
                        width = r.width;
                        names = r.names.clone();
                    }
                }
            }
            match x {
                Sql::Insert(ins) if !ins.returning.is_empty() => {
                    if rows.is_none() {
                        rows = Some(vec![]);
                        width = ins.returning.len();
                    }
                }
                Sql::Insert(_) => (rows, width) = (None, BULK_INSERT),
                // a bulk UPDATE's result has no rows, but is not closed (SQLAlchemy 2.1)
                _ => (rows, width) = (Some(vec![]), 1),
            }
            let rowcount = i64::MIN; // an ORM bulk result (IteratorResult) has no rowcount
            Ok(V::Result(Arc::new(Mutex::new(QResult { rows, width, scalars: false, rowcount, names }))))
        })
    }

    /// `await session.execute(stmt)`
    pub async fn execute(&self, stmt: &V) -> R {
        self.execute_params(stmt, None).await
    }

    /// `session.execute(stmt, params)`: bind parameters for a `text()` statement; a list of dicts runs it once
    /// per dict (executemany: no rows, the rowcount summed)
    pub async fn execute_params(&self, stmt: &V, params: Option<&V>) -> R {
        self.execute_with(stmt, params, false).await
    }

    /// `conn.execute(stmt, params)` on a `session.connection()`: Core, never the ORM's bulk modes
    pub async fn execute_conn(&self, stmt: &V, params: Option<&V>) -> R {
        self.execute_with(stmt, params, true).await
    }

    /// `execute(stmt, params)` of a Core statement with `bindparam()`s or criteria: executed once per dict
    /// (the dict's `bindparam()` values; its other keys, columns, added to the SET / VALUES as SQLAlchemy's
    /// compiler does), never synchronized with the loaded objects; one dict gives the statement's result, a
    /// list a result with no rows and the summed rowcount
    fn execute_core<'a>(&'a self, x: &'a Sql, p: &'a V, names: Vec<String>, conn: bool) -> BoxR<'a, V> {
        Box::pin(async move {
            let (many, single) = match p {
                V::List(l) => (l.lock().clone(), false),
                V::Dict(_) => (vec![p.clone()], true),
                other => return Err(Exc::type_error(format!("py2axum: execute() parameters must be a dict or a list of dicts, not {}", other.type_name()))),
            };
            if let Sql::Update(m, ..) = x {
                if !single && !m.core && !conn {
                    return Err(Exc::msg(&INVALID_REQUEST_ERROR, "bulk synchronize of persistent objects not supported when using bulk update with additional WHERE criteria right now.  add synchronize_session=None execution option to bypass synchronize of persistent objects."));
                }
            }
            let mut total = 0i64;
            // executemany: the statement is compiled for the keys of the first dict; a later one lacking one of
            // them raises, its other keys are ignored (SQLAlchemy's construct_params)
            let mut first: Option<Vec<String>> = None;
            let mut groups: Vec<Vec<(String, V)>> = Vec::new();
            let (mut rows, mut width, mut rnames): (Option<Vec<Vec<V>>>, usize, Option<Arc<Vec<Arc<str>>>>) = (None, 0, None);
            for (group, d) in many.iter().enumerate() {
                let V::Dict(d) = d else { return Err(Exc::type_error("py2axum: execute() takes a list of dicts")) };
                let mut items: Vec<(String, V)> = Vec::new();
                for (_, (k, v)) in d.lock().iter() {
                    let V::Str(k) = k else { return Err(Exc::type_error("py2axum: parameter names must be strings")) };
                    items.push((k.to_string(), v.clone()));
                }
                // (the ORM's bulk INSERT takes dicts of different keys)
                let orm_insert = matches!(x, Sql::Insert(ins) if !ins.m.core && !conn);
                if !single && !orm_insert {
                    match &first {
                        None => first = Some(items.iter().map(|(k, _)| k.clone()).collect()),
                        Some(keys) => {
                            if let Some(k) = keys.iter().find(|k| !items.iter().any(|(j, _)| j == *k)) {
                                return Err(bind_missing(format!("A value is required for bind parameter '{k}', in parameter group {group}")));
                            }
                            items.retain(|(k, _)| keys.contains(k));
                        }
                    }
                    if let Some(n) = names.iter().find(|n| !items.iter().any(|(k, _)| k == *n)) {
                        return Err(bind_missing(format!("A value is required for bind parameter '{n}', in parameter group {group}")));
                    }
                }
                groups.push(items);
            }
            // every group checked before the first execution, as SQLAlchemy builds all the parameters first
            for items in groups {
                let mut st = x.clone();
                let extra = items.iter().filter(|(k, _)| !names.contains(k));
                match &mut st {
                    Sql::Update(m, _, sets, sync, _) => {
                        *sync = false;
                        for (k, v) in extra {
                            let i = m.col_index(k).ok_or_else(|| Exc::msg(&COMPILE_ERROR, format!("Unconsumed column names: {k}")))?;
                            if !sets.iter().any(|(j, _)| *j == i) {
                                sets.push((i, to_sql(v, Some(m.cols[i].ty))));
                            }
                        }
                    }
                    Sql::Insert(ins) => {
                        if ins.rows.len() > 1 {
                            return Err(Exc::type_error("py2axum: insert().values([...]) with execution parameters is not supported"));
                        }
                        if ins.rows.is_empty() {
                            ins.rows.push(vec![]);
                        }
                        let orm = !ins.m.core && !conn;
                        for (k, v) in extra {
                            let i = ins.m.col_index(k).ok_or_else(|| Exc::msg(&COMPILE_ERROR, format!("Unconsumed column names: {k}")))?;
                            if orm && v.is_none() && !ins.render_nulls {
                                continue; // the ORM's bulk INSERT leaves a None out (the column's default applies)
                            }
                            if !ins.rows[0].iter().any(|(j, _)| *j == i) {
                                ins.rows[0].push((i, to_sql(v, Some(ins.m.cols[i].ty))));
                            }
                        }
                    }
                    Sql::Delete(_, _, sync, _) => *sync = false,
                    _ => {}
                }
                let res = BOUND.scope(items, Box::pin(self.execute_with(&sql(st), None, conn))).await?;
                if single {
                    return Ok(res);
                }
                if let V::Result(r) = &res {
                    let r = r.lock();
                    total += r.rowcount.max(0);
                    if let Some(got) = &r.rows {
                        rows.get_or_insert_with(Vec::new).extend(got.iter().cloned());
                        (width, rnames) = (r.width, r.names.clone());
                    }
                }
            }
            if let Sql::Insert(ins) = x {
                if !ins.returning.is_empty() {
                    // INSERT ... RETURNING once per dict: the rows in the order of the dicts
                    if rows.is_none() {
                        (rows, width) = (Some(vec![]), ins.returning.len());
                    }
                    return Ok(V::Result(Arc::new(Mutex::new(QResult { rows, width, scalars: false, rowcount: -1, names: rnames }))));
                }
            }
            if matches!(x, Sql::Insert(_)) && !asyncpg() {
                total = -1; // an executemany INSERT over psycopg (measured, SQLAlchemy 2.1)
            }
            Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount: total, names: None }))))
        })
    }

    fn execute_with<'a>(&'a self, stmt: &'a V, params: Option<&'a V>, conn: bool) -> BoxR<'a, V> {
        Box::pin(async move {
        if let (V::Sql(x), Some(p)) = (stmt, params) {
            if !matches!(&**x, Sql::Text(_)) && !p.is_none() {
                let names = bind_names(x);
                let criteria = match &**x {
                    Sql::Update(_, w, sets, ..) => !w.is_empty() || !sets.is_empty(),
                    Sql::Delete(..) | Sql::Select(_) => true,
                    _ => false,
                };
                let core_insert = matches!(&**x, Sql::Insert(ins) if conn || ins.m.core || !ins.rows.is_empty());
                if !names.is_empty() || criteria || core_insert {
                    return self.execute_core(x, p, names, conn).await;
                }
            }
        }
        if let (V::Sql(x), Some(V::List(l))) = (stmt, params) {
            if matches!(&**x, Sql::Insert(_) | Sql::Update(..)) {
                let many = l.lock().clone();
                return self.execute_bulk(x, many).await;
            }
        }
        if let (V::Sql(x), Some(p)) = (stmt, params) {
            if !matches!(&**x, Sql::Text(_)) && !p.is_none() {
                return Err(Exc::type_error("py2axum: execute(statement, params) is only supported for text()"));
            }
        }
        if let (V::Sql(x), Some(V::List(l))) = (stmt, params) {
            if let Sql::Text(q) = &**x {
                let many = l.lock().clone();
                if !many.is_empty() {
                    let mut s = self.0.lock().await;
                    if s.autoflush {
                        Session::flush_inner(&mut s).await?;
                    }
                    let mut total = 0i64;
                    for p in &many {
                        if !matches!(p, V::Dict(_)) {
                            return Err(Exc::type_error("py2axum: execute(text(), [...]) takes a list of dicts"));
                        }
                        let r = text_rend(q, Some(p))?;
                        total += Session::run_exec(&mut s, &r).await? as i64;
                    }
                    return Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount: total,
                                                                       names: None }))));
                }
            }
        }
        if let V::Sql(x) = stmt {
            if let Sql::Text(q) = &**x {
                // each value rendered by the usual binder (`$n` with its type), then put in place
                let r = text_rend(q, params)?;
                let mut s = self.0.lock().await;
                if s.autoflush {
                    Session::flush_inner(&mut s).await?;
                }
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
            // joined table inheritance: only the table the statement updates
            let target = sets.first().map(|(j, _)| m.cols[*j].table.unwrap_or(m.table));
            for (i, c) in m.cols.iter().enumerate() {
                if !m.joins.is_empty() && Some(c.table.unwrap_or(m.table)) != target {
                    continue;
                }
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
            // a multi-VALUES insert has the columns of its first row and those with a Python-side default (SQLAlchemy
            // ignores the other keys of the later rows)
            let present: Vec<usize> = ins.rows.first().map(|r| r.iter().map(|(i, _)| *i).collect()).unwrap_or_default();
            let columns: Vec<usize> = (0..m.cols.len())
                .filter(|i| present.contains(i) || matches!(m.cols[*i].default, ColDefault::Value(_) | ColDefault::Dyn(_)))
                .collect();
            for row in ins.rows.iter_mut().skip(1) {
                row.retain(|(i, _)| columns.contains(i));
            }
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
                            m.table, c.sql)));
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
        if s.autoflush && !matches!(&st, Sql::Select(sel) if sel.no_autoflush) {
            Session::flush_inner(&mut s).await?;
        }
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
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
                if sel.populate_existing {
                    // the entities already in the session are overwritten with the rows (pending changes lost)
                    for row in &rows {
                        let mut off = 0;
                        for c in &sel.cols {
                            match c {
                                SelCol::Entity(e) => {
                                    Session::refresh_from_row(&mut s, e, row, off)?;
                                    off += e.cols.len();
                                }
                                SelCol::AEntity(a) => {
                                    Session::refresh_from_row(&mut s, a.model, row, off)?;
                                    off += a.model.cols.len();
                                }
                                SelCol::Expr(e) => off += sql_width(e),
                            }
                        }
                    }
                }
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
                            SelCol::Expr(e) => off += sql_width(e),
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
                                ost.absent[*i] = false;
                                ost.expired = true;
                            } else if let Sql::Param(val, _, _) = v {
                                ost.vals[*i] = val.clone();
                                ost.committed[*i] = val.clone();
                                ost.absent[*i] = false;
                            } else if let Sql::Null = v {
                                ost.vals[*i] = V::None;
                                ost.committed[*i] = V::None;
                                ost.absent[*i] = false;
                            } else {
                                ost.vals[*i] = V::Unbound;
                                ost.absent[*i] = false;
                                ost.expired = true;
                            }
                        }
                    }
                }
                Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount: n, names: None }))))
            }
            Sql::Delete(m, w, sync, ret) => {
                render(&mut r, &st)?;
                let (n, rows) = if ret.is_empty() {
                    (Session::run_exec(&mut s, &r).await? as i64, None)
                } else {
                    let rows = Session::run(&mut s, &r).await?;
                    (rows.len() as i64, Some(rows))
                };
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
                if let Some(rows) = rows {
                    return Session::select_result(&mut s, &Select { cols: ret.clone(), ..Default::default() }, rows).await;
                }
                Ok(V::Result(Arc::new(Mutex::new(QResult { rows: None, width: 0, scalars: false, rowcount: n, names: None }))))
            }
            _ => Err(Exc::type_error("session.execute() needs a select(), update(), delete() or text()")),
        }
        })
    }

    pub async fn scalar(&self, stmt: &V) -> R {
        self.scalar_params(stmt, None).await
    }

    pub async fn scalar_params(&self, stmt: &V, params: Option<&V>) -> R {
        match self.execute_params(stmt, params).await? {
            V::Result(res) => result_method(&res, "scalar"),
            _ => Ok(V::None),
        }
    }

    pub async fn scalars(&self, stmt: &V) -> R {
        self.scalars_params(stmt, None).await
    }

    pub async fn scalars_params(&self, stmt: &V, params: Option<&V>) -> R {
        match self.execute_params(stmt, params).await? {
            V::Result(res) => result_method(&res, "scalars"),
            _ => Ok(V::None),
        }
    }

    /// The statement of a list response, run apart on a pool connection: None unless the session wrote
    /// nothing a fresh connection would not see (autoflush first, as `execute` does).
    async fn detach_select(&self, sel: &Select) -> R<Option<(sqlx::PgPool, String, PgArguments)>> {
        let mut s = self.0.lock().await;
        if s.conn_closed {
            return Ok(None);
        }
        if s.autoflush {
            Session::flush_inner(&mut s).await?;
        }
        if !(s.new.is_empty() && s.deleted.is_empty() && s.strong.is_empty()) || (s.tx.is_some() && s.wrote) {
            return Ok(None);
        }
        let mut r = Rend { sql: String::new(), binds: Vec::new(), aliases: Vec::new(), shared: Vec::new(), outer: Vec::new() };
        render_select(&mut r, sel)?;
        let args = Session::args(&s, &r).await?;
        Ok(Some((s.pool.clone(), r.sql, args)))
    }
}

// ---------------------------------------------------------------- list responses streamed

/// A list response read row by row from its own pool connection (web::respond): the statement of
/// `return (await session.execute(stmt)).scalars().all()`, rendered when the endpoint returned.
pub struct ListStream {
    pub pool: sqlx::PgPool,
    pub sql: String,
    pub args: PgArguments,
    pub model: &'static ModelDesc,
    /// the response_model's item schema
    pub item: &'static pyd::TD,
}

/// (block size in bytes, LIMIT up to which a list is not streamed). Defaults 64 KiB / 1000 rows;
/// PY2AXUM_STREAM_CHUNK and PY2AXUM_STREAM_MIN_ROWS override them (the conformance suite sets them very low
/// to stream small lists in several blocks).
pub fn stream_cfg() -> (usize, i64) {
    static CFG: std::sync::LazyLock<(usize, i64)> = std::sync::LazyLock::new(|| {
        let chunk = std::env::var("PY2AXUM_STREAM_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(64 * 1024);
        let min_rows = std::env::var("PY2AXUM_STREAM_MIN_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
        (chunk, min_rows)
    });
    *CFG
}

/// `return (await session.execute(stmt)).scalars().all()` (or `.scalars(stmt)`) in the endpoint `fname` of a
/// route whose response_model is `list[Schema]`: a `ListStream` instead of the list, when nothing the
/// endpoint or the response could observe differs — `session` is the request's session and wrote nothing
/// yet, `stmt` selects one mapped class without loader options nor locking, its LIMIT (if a literal) is
/// above PY2AXUM_STREAM_MIN_ROWS, and the schema reads only plain columns of that class (no relationship,
/// property nor `mode="before"` model validator). None: the endpoint runs the statement as written.
pub async fn defer_list(cx: &super::Cx, fname: &'static str, session: &V, stmt: &V) -> R<Option<V>> {
    let Some(td) = cx.stream_list.lock().filter(|(f, _)| *f == fname).map(|(_, td)| td) else { return Ok(None) };
    let (V::Session(sess), V::Sql(q)) = (session, stmt) else { return Ok(None) };
    let Sql::Select(sel) = &**q else { return Ok(None) };
    let model = match sel.cols.as_slice() {
        [SelCol::Entity(m)] if sel.loads.is_empty() && sel.for_update.is_none() => *m,
        _ => return Ok(None),
    };
    let pyd::TD::List(Some(item @ pyd::TD::Schema(d))) = td else { return Ok(None) };
    let plain = d.from_attributes
        && d.model_before.is_empty()
        && d.fields.iter().all(|f| model.col_index(f.alias.unwrap_or(f.name)).is_some_and(|i| !model.cols[i].deferred));
    if !plain {
        return Ok(None);
    }
    if let Some(Sql::Param(V::Int(n), ..)) = &sel.limit {
        if *n <= stream_cfg().1 {
            return Ok(None);
        }
    }
    if cx.resp.status.lock().is_some_and(|st| st < 200 || st == 204 || st == 304) {
        return Ok(None);
    }
    let Some((pool, sql, args)) = sess.detach_select(sel).await? else { return Ok(None) };
    *cx.stream_list.lock() = None;
    Ok(Some(V::native(Native::ListStream(Mutex::new(Some(ListStream { pool, sql, args, model, item }))))))
}

/// One row of a `ListStream`: a detached object (not in the session's identity map), TypeDecorator results
/// applied.
pub async fn row_object(cx: &super::Cx, desc: &'static ModelDesc, row: &PgRow) -> R {
    let o = ObjCell::transient(poly_pick(desc, row, 0)?);
    o.load_row_from(row, 0, row_tz(), &[], desc.cols.len())?; // streamed only without loader options: the mapping's column strategies
    for (i, c) in desc.cols.iter().enumerate() {
        if let ColTy::Decorated(TypeDec { result: Some(f), .. }) = c.ty {
            let raw = o.st.lock().vals[i].clone();
            let v = f(cx, V::None, vec![raw, V::None]).await?;
            let mut st = o.st.lock();
            st.vals[i] = v.clone();
            st.committed[i] = v;
        }
    }
    Ok(V::Obj(o))
}

pub fn stream_sql_error(e: sqlx::Error, sql: &str) -> Exc {
    sql_error(e, sql)
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
    let mut from = vec![t];
    if let Some((assoc, a, b)) = rel.secondary {
        // EXISTS (SELECT 1 FROM assoc, target WHERE m.local = assoc.a AND target.remote = assoc.b ...)
        wheres = vec![Sql::Bin("=", Box::new(Sql::Col(m, rel.local)), Box::new(Sql::Col(assoc, a))),
                      Sql::Bin("=", Box::new(Sql::Col(t, rel.remote)), Box::new(Sql::Col(assoc, b)))];
        from = vec![assoc, t];
    }
    for a in args {
        wheres.push(to_sql(a, None));
    }
    for (k, v) in kwargs {
        let i = t.col_index(k).ok_or_else(|| Exc::attr_error(format!("{} has no column {k}", t.name)))?;
        if let V::Sql(x) = sql_cmp(&V::Col(t, i), "=", v)? {
            wheres.push((*x).clone());
        }
    }
    let sel = Select { cols: vec![SelCol::Expr(Sql::Text("1".into()))], from, wheres, ..Default::default() };
    Ok(sql(Sql::Exists(Box::new(sel))))
}

/// `sq.c` / `sq.c.name`
pub fn sql_attr(v: &V, name: &str) -> Option<R> {
    let V::Sql(x) = v else { return None };
    match (&**x, name) {
        // `poly.Manager` (a subclass's namespace: its own attributes), `poly.id` (the base's)
        (Sql::WithPoly(b, subs), n) => Some(match subs.iter().find(|d| d.name == n) {
            Some(d) => Ok(V::Class(d.class)),
            None => match (b.col_index(n), b.rel_index(n)) {
                (Some(i), _) => Ok(V::Col(b, i)),
                (None, Some(ri)) => Ok(sql(Sql::Rel(b, ri))),
                _ => Err(Exc::attr_error(format!("type object 'AliasedClass' has no attribute '{n}' (py2axum: columns, \
                    relationships and subclass namespaces of with_polymorphic())"))),
            },
        }),
        (Sql::Subquery(q), "c" | "columns") => Some(Ok(sql(Sql::SubColumns(q.clone())))),
        (Sql::TableCols(m), n) => Some(match m.col_index(n) {
            Some(i) => Ok(V::Col(m, i)),
            None => Err(Exc::attr_error(n.to_string())),
        }),
        (Sql::Alias(a), "c" | "columns") if a.model.core => Some(Ok(sql(Sql::AliasCols(a.clone())))),
        (Sql::AliasCols(a), n) => Some(match a.model.col_index(n) {
            Some(i) => Ok(sql(Sql::ACol(a.clone(), i))),
            None => Err(Exc::attr_error(n.to_string())),
        }),
        (Sql::Alias(a), n) if a.model.core => Some(Err(Exc::attr_error(format!("'Alias' object has no attribute '{n}'")))),
        (Sql::SubColumns(q), n) => {
            let known = q.sel.cols.iter().any(|c| match c {
                SelCol::Expr(Sql::Label(_, l)) => l == n,
                SelCol::Expr(Sql::Col(m, i)) => m.cols[*i].sql == n,
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
        (Sql::Alias(a), n) if a.model.rel_index(n).is_some() => Some(Ok(sql(Sql::ARel(a.clone(), a.model.rel_index(n).unwrap())))),
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
            match sess.execute(&sql(Sql::Delete(m, w, sync, vec![]))).await? {
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


// ---------------------------------------------------------------- write-only collections

/// The members given to a write-only collection: instances of its target class
fn wo_check(rel: &RelDesc, items: &[V]) -> R<()> {
    for it in items {
        match it {
            V::Obj(o) if std::ptr::eq(o.desc, rel.target) => {}
            other => return Err(Exc::type_error(format!("py2axum: {} is not a {} (write-only collection {})",
                                                        other.type_name(), rel.target.name, rel.name))),
        }
    }
    Ok(())
}

/// The criteria of a write-only collection: the target rows of this parent (one-to-many: their foreign key;
/// many-to-many: through the association table), bound to the parent's key now
fn wo_criteria(o: &Arc<ObjCell>, rel: &'static RelDesc) -> R<Sql> {
    let v = o.st.lock().vals[rel.local].clone();
    if matches!(v, V::Unbound) {
        return Err(Exc::type_error(format!(
            "py2axum: the write-only collection {}.{} of an object whose key is not loaded (pending or expired) is not supported",
            o.desc.name, rel.name)));
    }
    let t = rel.target;
    Ok(match rel.secondary {
        None => Sql::Bin("=", Box::new(Sql::Col(t, rel.remote)), Box::new(Sql::Param(v, Some(t.cols[rel.remote].ty), 0))),
        Some((assoc, a, b)) => {
            let sub = Select {
                cols: vec![SelCol::Expr(Sql::Col(assoc, b))],
                from: vec![assoc],
                wheres: vec![Sql::Bin("=", Box::new(Sql::Col(assoc, a)), Box::new(Sql::Param(v, Some(assoc.cols[a].ty), 0)))],
                ..Default::default()
            };
            Sql::InSelect(Box::new(Sql::Col(t, rel.remote)), Box::new(sub), false)
        }
    })
}

/// `WriteOnlyCollection` methods: add/add_all/remove (applied at the next flush), select/insert/update/delete
pub fn write_only_method(o: &Arc<ObjCell>, ri: usize, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let rel = &o.desc.rels[ri];
    if !kwargs.is_empty() {
        return Err(Exc::type_error(format!("py2axum: WriteOnlyCollection.{name}() takes no keyword arguments")));
    }
    let items = |v: &V| -> R<Vec<V>> {
        match v {
            V::List(l) => Ok(l.lock().clone()),
            V::Tuple(t) => Ok(t.to_vec()),
            other => Err(Exc::type_error(format!("py2axum: add_all() of a {} is not supported (a list)", other.type_name()))),
        }
    };
    let n = match name {
        "add" | "add_all" | "remove" => 1,
        _ => 0,
    };
    if args.len() != n {
        return Err(Exc::type_error(format!("py2axum: WriteOnlyCollection.{name}() takes {n} argument(s)")));
    }
    match name {
        "add" | "add_all" | "remove" => {
            let x = &args[0];
            let xs = if name == "add_all" { items(x)? } else { vec![x.clone()] };
            wo_check(rel, &xs)?;
            o.mark_dirty();
            let mut st = o.st.lock();
            if !matches!(st.rels[ri], Some(V::List(_))) {
                st.rels[ri] = Some(V::list(vec![]));
            }
            let Some(V::List(cur)) = st.rels[ri].clone() else { unreachable!() };
            let mut cur = cur.lock();
            for x in xs {
                if name == "remove" {
                    // a pending member is forgotten; any other one leaves at the flush (foreign key NULL or orphan deleted)
                    if let Some(k) = cur.iter().position(|y| same(y, &x)) {
                        cur.remove(k);
                    } else {
                        st.rel_snap[ri].get_or_insert_with(Vec::new).push(x);
                    }
                } else if !cur.iter().any(|y| same(y, &x)) {
                    cur.push(x);
                }
            }
            Ok(V::None)
        }
        "select" => {
            let t = rel.target;
            Ok(sql(Sql::Select(Box::new(Select {
                cols: vec![SelCol::Entity(t)],
                wheres: vec![wo_criteria(o, rel)?],
                order: rel.order.iter().map(|&(i, desc)| Sql::Order(Box::new(Sql::Col(t, i)), desc, None)).collect(),
                ..Default::default()
            }))))
        }
        "insert" => {
            if rel.secondary.is_some() {
                return Err(Exc::msg(&INVALID_REQUEST_ERROR, "Write only bulk INSERT only supported for one-to-many collections; \
                    for many-to-many, use a separate bulk INSERT along with add_all()."));
            }
            wo_criteria(o, rel)?; // the parent's key is loaded
            let v = o.st.lock().vals[rel.local].clone();
            let t = rel.target;
            Ok(sql(Sql::Insert(Box::new(Ins {
                m: t, rows: vec![vec![(rel.remote, to_sql(&v, Some(t.cols[rel.remote].ty)))]], conflict: None,
                returning: vec![], pg: false, render_nulls: false, fixed: true,
            }))))
        }
        "update" => Ok(sql(Sql::Update(rel.target, vec![wo_criteria(o, rel)?], vec![], true, vec![]))),
        "delete" => Ok(sql(Sql::Delete(rel.target, vec![wo_criteria(o, rel)?], true, vec![]))),
        _ => Err(Exc::attr_error(format!("'WriteOnlyCollection' object has no attribute '{name}'"))),
    }
}

static SQLALCHEMY: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

/// the project's SQLAlchemy version (its messages and documentation links changed with 2.1)
pub fn set_sqlalchemy(major: u32, minor: u32) {
    let _ = SQLALCHEMY.set((major, minor));
}

fn sqlalchemy() -> (u32, u32) {
    SQLALCHEMY.get().copied().unwrap_or((2, 1))
}

/// an IO attempted outside greenlet_spawn (a lazy load, an expired or deferred attribute, in an async
/// session), as measured on 2.0.44/2.0.49/2.1.0/2.1.4: MissingGreenlet itself up to 2.0; since 2.1
/// `StatementError: (sqlalchemy.exc.MissingGreenlet) ...` raised from it (`.orig`). 2.1 also prints the lazy
/// loader's `[SQL: ...]`, `[parameters: ...]` and background-link lines in `str(e)`: not reproduced, as for
/// database errors (docs/tutorial/sql.md). `except MissingGreenlet` does not catch the 2.1 form.
pub fn greenlet_error() -> Exc {
    let (major, minor) = sqlalchemy();
    if (major, minor) < (2, 1) {
        return Exc::msg(&MISSING_GREENLET, format!(
            "greenlet_spawn has not been called; can't call await_only() here. Was IO attempted in an unexpected place? \
             (Background on this error at: https://sqlalche.me/e/{major}{minor}/xd2s)"));
    }
    let msg = "greenlet_spawn has not been called; can't call await_() here. Was IO attempted in an unexpected place?";
    statement_error(&MISSING_GREENLET, "sqlalchemy.exc.MissingGreenlet", msg, Some("xd2s"))
}

/// `StatementError` raised from `orig` (the exception met while executing a statement, its `.orig` and
/// `__cause__`): `(module.Class) message`; an `orig` of SQLAlchemy with an error code ends with its background
/// link, the StatementError not (its `[SQL: ...]` lines are not reproduced)
pub fn statement_error(orig: &'static Class, qual: &str, msg: &str, code: Option<&str>) -> Exc {
    let (major, minor) = sqlalchemy();
    let cause = Exc::msg(orig, match code {
        Some(c) => format!("{msg} (Background on this error at: https://sqlalche.me/e/{major}{minor}/{c})"),
        None => msg.to_string(),
    });
    let e = Exc::msg(&STATEMENT_ERROR, format!("({qual}) {msg}"));
    let mut attrs = e.0.attrs.lock();
    attrs.insert("orig".into(), V::Exc(cause.clone()));
    attrs.insert("__cause__".into(), V::Exc(cause));
    attrs.insert("__suppress_context__".into(), V::Bool(true));
    drop(attrs);
    e
}

/// a bind parameter without a value (`text(":a")` executed without `a`): code cd3x
fn bind_missing(msg: String) -> Exc {
    statement_error(&INVALID_REQUEST_ERROR, "sqlalchemy.exc.InvalidRequestError", &msg, Some("cd3x"))
}

/// the error of `greenlet_error` (an attribute that needs IO, read synchronously), whatever the version:
/// what turns into a load in a synchronous session
pub fn is_greenlet_error(e: &Exc) -> bool {
    e.isinstance(&MISSING_GREENLET)
        || (e.isinstance(&STATEMENT_ERROR) && matches!(e.0.attrs.lock().get("orig"), Some(V::Exc(c)) if c.isinstance(&MISSING_GREENLET)))
}
