//! Pydantic v2 (lax mode) validation and serialisation, driven by static descriptors, plus the
//! JSON writer used for responses (`json.dumps` byte for byte).
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;
use parking_lot::Mutex;

use super::dt::{self, DateTime, Tz};
use super::ops;
use super::v::*;

// ---------------------------------------------------------------- descriptors

#[derive(Clone, Copy)]
pub struct NumC {
    pub ge: Option<f64>,
    pub gt: Option<f64>,
    pub le: Option<f64>,
    pub lt: Option<f64>,
}

pub const NO_NUM: NumC = NumC { ge: None, gt: None, le: None, lt: None };

pub struct Pat {
    pub src: &'static str,
    re: OnceLock<regex::Regex>,
}

impl Pat {
    pub const fn new(src: &'static str) -> Pat {
        Pat { src, re: OnceLock::new() }
    }
    pub fn is_match(&self, s: &str) -> bool {
        self.re.get_or_init(|| regex::Regex::new(self.src).expect("pattern")).is_match(s)
    }
}

pub struct StrC {
    pub min: Option<usize>,
    pub max: Option<usize>,
    pub pattern: Option<&'static Pat>,
    pub strip: bool,
    pub lower: bool,
    pub upper: bool,
}

pub const NO_STR: StrC = StrC { min: None, max: None, pattern: None, strip: false, lower: false, upper: false };

pub enum Lit {
    Str(&'static str),
    Int(i64),
    Bool(bool),
    None,
}

pub enum TD {
    Any,
    NoneT,
    Bool,
    Int(NumC),
    Float(NumC),
    Str(StrC),
    DateTime,
    Date,
    Time,
    Delta,
    Dict(Option<(&'static TD, &'static TD)>),
    List(Option<&'static TD>),
    Set(Option<&'static TD>),
    Tuple(Option<&'static TD>),
    Optional(&'static TD),
    Union(&'static [&'static TD]),
    Literal(&'static [Lit]),
    Schema(&'static SchemaDesc),
    /// enum class, use_enum_values
    Enum(&'static EnumDesc, bool),
    /// pydantic.EmailStr
    Email,
    /// pydantic's URL types (AnyUrl, HttpUrl, AnyHttpUrl, RedisDsn)
    Url(&'static UrlSpec),
}

/// pydantic's `UrlConstraints` of a URL type
pub struct UrlSpec {
    pub name: &'static str,
    pub schemes: &'static [&'static str],
    pub host_required: bool,
    pub default_host: Option<&'static str>,
    pub default_port: Option<u16>,
    pub default_path: Option<&'static str>,
    pub max_length: Option<usize>,
}

pub static ANY_URL: UrlSpec = UrlSpec { name: "AnyUrl", schemes: &[], host_required: false, default_host: None, default_port: None, default_path: None, max_length: None };
pub static ANY_HTTP_URL: UrlSpec = UrlSpec { name: "AnyHttpUrl", schemes: &["http", "https"], host_required: false, default_host: None, default_port: None, default_path: None, max_length: None };
pub static HTTP_URL: UrlSpec = UrlSpec { name: "HttpUrl", schemes: &["http", "https"], host_required: false, default_host: None, default_port: None, default_path: None, max_length: Some(2083) };
pub static REDIS_DSN: UrlSpec = UrlSpec { name: "RedisDsn", schemes: &["redis", "rediss"], host_required: true, default_host: Some("localhost"), default_port: Some(6379), default_path: Some("/0"), max_length: None };

/// a URL as pydantic-core validates it (the `url` crate, then the type's defaults)
fn url_val(input: &V, spec: &'static UrlSpec, loc: &[V], e: &mut Errs) -> Option<V> {
    let s = match input {
        V::Str(s) => s.to_string(),
        V::Native(n) if matches!(&**n, Native::PydUrl(..)) => match &**n {
            Native::PydUrl(_, u) => u.as_str().to_string(),
            _ => unreachable!(),
        },
        _ => {
            e.push("url_type", loc, "URL input should be a string or URL", input, None);
            return None;
        }
    };
    if let Some(max) = spec.max_length {
        if s.chars().count() > max {
            e.push("url_too_long", loc, format!("URL should have at most {max} characters"), input, Some(vec![("max_length", V::Int(max as i64))]));
            return None;
        }
    }
    let mut u = match url::Url::parse(&s) {
        Ok(u) => u,
        Err(err) => {
            e.push("url_parsing", loc, format!("Input should be a valid URL, {err}"), input, Some(vec![("error", V::str(err.to_string()))]));
            return None;
        }
    };
    if !spec.schemes.is_empty() && !spec.schemes.contains(&u.scheme()) {
        let expected = spec.schemes.iter().map(|x| format!("'{x}'")).collect::<Vec<_>>();
        let expected = if expected.len() == 1 { expected[0].clone() } else { format!("{} or {}", expected[..expected.len() - 1].join(", "), expected[expected.len() - 1]) };
        e.push("url_scheme", loc, format!("URL scheme should be {expected}"), input, Some(vec![("expected_schemes", V::str(&expected))]));
        return None;
    }
    if u.host_str().is_none_or(|h| h.is_empty()) {
        if let Some(h) = spec.default_host {
            let _ = u.set_host(Some(h));
        }
    }
    if u.port().is_none() {
        if let Some(p) = spec.default_port {
            let _ = u.set_port(Some(p));
        }
    }
    if let Some(dp) = spec.default_path {
        if u.path().is_empty() || u.path() == "/" {
            u.set_path(dp);
        }
    }
    if spec.host_required && u.host_str().is_none_or(|h| h.is_empty()) {
        e.push("url_parsing", loc, "Input should be a valid URL, empty host", input, Some(vec![("error", V::str("empty host"))]));
        return None;
    }
    Some(V::native(Native::PydUrl(spec.name, std::sync::Arc::new(u))))
}

/// attributes of a pydantic URL
pub fn url_attr(u: &url::Url, name: &str) -> R {
    let opt = |s: Option<&str>| s.filter(|x| !x.is_empty()).map(V::str).unwrap_or(V::None);
    Ok(match name {
        "scheme" => V::str(u.scheme()),
        "host" => opt(u.host_str()),
        "port" => u.port_or_known_default().map(|p| V::Int(p as i64)).unwrap_or(V::None),
        "path" => {
            let p = u.path();
            if p.is_empty() { V::None } else { V::str(p) }
        }
        "query" => opt(u.query()),
        "fragment" => opt(u.fragment()),
        "username" => opt(Some(u.username())),
        "password" => opt(u.password()),
        _ => return Err(Exc::attr_error(format!("'Url' object has no attribute '{name}'"))),
    })
}

pub enum Dflt {
    Required,
    Value(fn() -> V),
    Factory(fn() -> V),
    /// computed default (an expression evaluated once, or a default_factory called per instance)
    Dyn(MethodFn),
}

pub struct FieldDesc {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    pub td: &'static TD,
    pub default: Dflt,
    /// pydantic-settings: environment variable read for this field
    pub env: Option<&'static str>,
    /// `Field(validate_default=True)`: an omitted field's default is validated like an input
    pub validate_default: bool,
}

#[derive(PartialEq)]
pub enum Extra {
    Ignore,
    Forbid,
    Allow,
}

pub type MethodFn = for<'a> fn(&'a super::Cx, V, Vec<V>) -> super::BoxFut<'a>;

pub struct ValidatorDesc {
    pub fields: &'static [&'static str],
    pub f: MethodFn,
}

pub struct SchemaDesc {
    pub name: &'static str,
    pub class: &'static Class,
    pub fields: &'static [FieldDesc],
    pub from_attributes: bool,
    pub extra: Extra,
    pub validators: &'static [ValidatorDesc],
    /// `validate_assignment=True`: `inst.field = v` is validated like the input
    pub validate_assignment: bool,
    /// `populate_by_name=True`: an aliased field is also accepted under its name
    pub populate_by_name: bool,
    /// (name, is_property, function)
    pub methods: &'static [(&'static str, bool, MethodFn)],
    /// a plain project class: no fields, any attribute can be set (stored in `extra`)
    pub open: bool,
    /// `@model_validator(mode="after")` methods, in definition order
    pub model_after: &'static [MethodFn],
    /// `@field_validator(..., mode="before")` (definition order: they run in reverse)
    pub before: &'static [ValidatorDesc],
    /// `@model_validator(mode="before")` (definition order: they run in reverse)
    pub model_before: &'static [MethodFn],
    /// this model or one it contains has `before` validators (the pre-pass visits it)
    pub has_before: bool,
    /// `@dataclass(frozen=True)`: assignment raises, instances hash by value
    pub frozen: bool,
    /// a dataclass's `__post_init__`
    pub post_init: Option<MethodFn>,
    /// what `hash()` does with an instance (CPython's rules for the class kind)
    pub hash: HashKind,
    /// a `@dataclass` (frozen assignment raises FrozenInstanceError, not a ValidationError)
    pub dataclass: bool,
    /// the methods that are `async def`s (a call not awaited is a coroutine)
    pub async_methods: &'static [&'static str],
    /// a plain class's `__slots__` (its pickled state is `(None, {slot: value})`)
    pub slots: &'static [&'static str],
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HashKind {
    /// object.__hash__: identity
    Id,
    /// frozen dataclass / frozen Pydantic model: hash of the field values
    Value,
    /// `__hash__ = None`, `__eq__` without `__hash__`, mutable Pydantic model or dataclass
    Unhashable,
}

impl SchemaDesc {
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
}

// ---------------------------------------------------------------- errors

pub struct ErrDetail {
    pub kind: &'static str,
    pub loc: Vec<V>,
    pub msg: String,
    pub input: V,
    pub ctx: Option<Vec<(&'static str, V)>>,
}

impl ErrDetail {
    pub fn to_v(&self) -> V {
        let mut items = vec![
            (V::str("type"), V::str(self.kind)),
            (V::str("loc"), V::tuple(self.loc.clone())),
            (V::str("msg"), V::str(&self.msg)),
            (V::str("input"), self.input.clone()),
        ];
        if let Some(ctx) = &self.ctx {
            let c = V::dict_from(ctx.iter().map(|(k, v)| (V::str(*k), v.clone())).collect()).unwrap();
            items.push((V::str("ctx"), c));
        }
        V::dict_from(items).unwrap()
    }
}

/// One step of a validation, in pydantic-core's order. The type checks are synchronous; the
/// `@field_validator`s and computed defaults are compiled Python (async), so they are recorded
/// here and replayed by `settle`, which slots their errors exactly where Pydantic raises them.
enum Step {
    Err(ErrDetail),
    /// field `field` of the instance in `slot` passed its type check: run its validators on `value`
    Validators { slot: usize, field: usize, desc: &'static SchemaDesc, value: V, loc: Vec<V> },
    /// field `field` of the instance in `slot` was omitted and has a computed default
    Default { slot: usize, field: usize, f: MethodFn },
    /// start of a model that has `@model_validator(mode="after")`: errors counted from here
    ModelStart { slot: usize },
    /// its fields are validated: run the model validators unless an error occurred since ModelStart
    ModelAfter { slot: usize, desc: &'static SchemaDesc, input: V, loc: Vec<V> },
}

// How well the input matched (pydantic-core's `Exactness`): smart unions keep the best member.
const LAX: u8 = 0;
const STRICT: u8 = 1;
const EXACT: u8 = 2;

struct Errs<'a> {
    steps: &'a mut Vec<Step>,
    /// instances that steps refer to; None while being built, and forever if their validation fails
    /// (Pydantic still runs the validators of the valid fields: their errors are reported)
    slots: &'a mut Vec<Option<Arc<Inst>>>,
    n: usize,
    /// lowest exactness met so far (a conversion lowers it, never raises it)
    exact: u8,
    /// fields set in the last model validated (pydantic-core's `fields_set_count`)
    fields_set: Option<usize>,
}

impl Errs<'_> {
    fn push(&mut self, kind: &'static str, loc: &[V], msg: impl Into<String>, input: &V, ctx: Option<Vec<(&'static str, V)>>) {
        self.steps.push(Step::Err(ErrDetail { kind, loc: loc.to_vec(), msg: msg.into(), input: input.clone(), ctx }));
        self.n += 1;
    }
    fn floor(&mut self, x: u8) {
        self.exact = self.exact.min(x);
    }
    fn sub<'b>(&'b mut self, steps: &'b mut Vec<Step>) -> Errs<'b> {
        Errs { steps, slots: self.slots, n: 0, exact: EXACT, fields_set: None }
    }
}

fn with(loc: &[V], k: V) -> Vec<V> {
    let mut l = loc.to_vec();
    l.push(k);
    l
}

fn fmt_bound(x: f64) -> V {
    if x.fract() == 0.0 { V::Int(x as i64) } else { V::Float(x) }
}

/// a float field's bound is a float in the error context (pydantic-core stores it as f64); the message
/// shows it as for an int when integral
fn check_num(x: f64, c: &NumC, loc: &[V], input: &V, e: &mut Errs) -> bool {
    check_num_kind(x, c, loc, input, e, false)
}

fn check_num_kind(x: f64, c: &NumC, loc: &[V], input: &V, e: &mut Errs, float: bool) -> bool {
    let checks = [
        (c.ge, x >= c.ge.unwrap_or(0.0), "greater_than_equal", "greater than or equal to", "ge"),
        (c.gt, x > c.gt.unwrap_or(0.0), "greater_than", "greater than", "gt"),
        (c.le, x <= c.le.unwrap_or(0.0), "less_than_equal", "less than or equal to", "le"),
        (c.lt, x < c.lt.unwrap_or(0.0), "less_than", "less than", "lt"),
    ];
    for (bound, ok, kind, word, key) in checks {
        if let Some(b) = bound {
            if !ok {
                let bv = fmt_bound(b);
                let shown = ops::str_(&bv).unwrap_or_default();
                let ctx = if float { V::Float(b) } else { bv };
                e.push(kind, loc, format!("Input should be {word} {shown}"), input, Some(vec![(key, ctx)]));
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------- validation

pub struct Inst {
    pub desc: &'static SchemaDesc,
    pub vals: Mutex<Vec<V>>,
    pub set: Mutex<Vec<bool>>,
    pub extra: Mutex<IndexMap<String, V>>,
}

/// `TypeAdapter(td).validate_python(input, from_attributes=True)` (FastAPI's mode), validators
/// included. Errors are appended to `errs`; the value is returned only if there were none.
/// Exceptions other than ValueError/AssertionError raised by a validator propagate.
pub async fn validate(cx: &super::Cx, input: &V, td: &'static TD, loc: &[V], errs: &mut Vec<ErrDetail>) -> R<Option<V>> {
    let input = if td_has_before(td) { prepare(cx, input.clone(), td).await? } else { input.clone() };
    run(cx, errs, |e| val(&input, td, loc, e)).await
}

fn td_has_before(td: &TD) -> bool {
    match td {
        TD::Schema(d) => d.has_before,
        TD::List(Some(t)) | TD::Set(Some(t)) | TD::Tuple(Some(t)) | TD::Optional(t) => td_has_before(t),
        TD::Dict(Some((_, v))) => td_has_before(v),
        TD::Enum(d, _) => d.missing.is_some(),
        _ => false,
    }
}

/// A failed `before` validator: ValueError/AssertionError become an error reported where the
/// synchronous validation meets this input; other exceptions propagate.
fn before_error(x: Exc, input: V) -> R {
    let (kind, word) = if x.isinstance(&ASSERTION_ERROR) {
        ("assertion_error", "Assertion failed")
    } else if x.isinstance(&VALUE_ERROR) {
        ("value_error", "Value error")
    } else {
        return Err(x);
    };
    Ok(V::native(Native::ValErr(kind, format!("{word}, {}", x.message()), input)))
}

/// The asynchronous pre-pass: `mode="before"` validators applied to the raw input, model first,
/// then each field (validators in reverse definition order, like pydantic-core), recursively.
fn prepare<'a>(cx: &'a super::Cx, input: V, td: &'static TD) -> super::BoxFut<'a> {
    Box::pin(async move {
        Ok(match (td, &input) {
            (TD::Optional(_), V::None) => input,
            (TD::Optional(t), _) => prepare(cx, input, t).await?,
            (TD::List(Some(t)), V::List(l)) if td_has_before(t) => {
                let items = l.lock().clone();
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(prepare(cx, it, t).await?);
                }
                V::list(out)
            }
            (TD::Dict(Some((_, vt))), V::Dict(d)) if td_has_before(vt) => {
                let items: Vec<(V, V)> = d.lock().values().cloned().collect();
                let mut out = Vec::with_capacity(items.len());
                for (k, v) in items {
                    out.push((k, prepare(cx, v, vt).await?));
                }
                V::dict_from(out)?
            }
            (TD::Schema(d), _) if d.has_before => prepare_schema(cx, input, d).await?,
            // pydantic-core calls a custom `_missing_` for a value that is no member
            (TD::Enum(d, _), _) if d.missing.is_some() && !matches!(&input, V::Enum(x, _) if std::ptr::eq(*x, *d)) && d.by_value(&input).is_none() => {
                match (d.missing.unwrap())(cx, V::Class(d.class), vec![input.clone()]).await {
                    Ok(m @ V::Enum(x, _)) if std::ptr::eq(x, *d) => m,
                    Ok(_) => input,
                    Err(x) => before_error(x, input)?,
                }
            }
            _ => input,
        })
    })
}

async fn prepare_schema(cx: &super::Cx, input: V, desc: &'static SchemaDesc) -> R {
    if let V::Inst(i) = &input {
        if std::ptr::eq(i.desc, desc) {
            return Ok(input); // an instance is not revalidated
        }
    }
    let mut data = input;
    for f in desc.model_before.iter().rev() {
        match f(cx, V::Class(desc.class), vec![data.clone()]).await {
            Ok(v) => data = v,
            Err(x) => return before_error(x, data),
        }
    }
    let touched: Vec<usize> = (0..desc.fields.len())
        .filter(|&i| desc.before.iter().any(|b| b.fields.contains(&desc.fields[i].name)) || td_has_before(desc.fields[i].td))
        .collect();
    if touched.is_empty() {
        return Ok(data);
    }
    // the fields read from an object (from_attributes) become a dict: validation then reads the same values
    let map: IndexMap<Key, (V, V)> = match &data {
        V::Dict(d) => d.lock().clone(),
        V::Obj(_) | V::Inst(_) if desc.from_attributes => {
            let mut m = IndexMap::new();
            for f in desc.fields {
                let key = f.alias.unwrap_or(f.name);
                if let Some(v) = attr_value(&data, key) {
                    m.insert(Key::Str(Arc::from(key)), (V::str(key), v));
                }
            }
            m
        }
        _ => return Ok(data),
    };
    let mut map = map;
    for i in touched {
        let f = &desc.fields[i];
        let key = Key::Str(Arc::from(f.alias.unwrap_or(f.name)));
        let Some((kv, raw)) = map.get(&key).cloned() else { continue };
        let mut v = raw;
        let mut failed = false;
        for b in desc.before.iter().rev().filter(|b| b.fields.contains(&f.name)) {
            match (b.f)(cx, V::Class(desc.class), vec![v.clone()]).await {
                Ok(nv) => v = nv,
                Err(x) => {
                    v = before_error(x, v)?;
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            v = prepare(cx, v, f.td).await?;
        }
        map.insert(key, (kv, v));
    }
    Ok(V::Dict(Arc::new(Mutex::new(map))))
}

async fn run(cx: &super::Cx, errs: &mut Vec<ErrDetail>, check: impl FnOnce(&mut Errs) -> Option<V>) -> R<Option<V>> {
    let (mut steps, mut slots) = (Vec::new(), Vec::new());
    take_fatal();
    let got = check(&mut Errs { steps: &mut steps, slots: &mut slots, n: 0, exact: EXACT, fields_set: None });
    if let Some(x) = take_fatal() {
        return Err(x);
    }
    let start = errs.len();
    settle(cx, steps, slots, errs).await?;
    Ok(if errs.len() > start { None } else { got })
}

/// Type checks only (`validate_assignment`, where the transpiler refuses validators).
pub fn validate_sync(input: &V, td: &'static TD, loc: &[V], errs: &mut Vec<ErrDetail>) -> Option<V> {
    let (mut steps, mut slots) = (Vec::new(), Vec::new());
    let got = val(input, td, loc, &mut Errs { steps: &mut steps, slots: &mut slots, n: 0, exact: EXACT, fields_set: None });
    for st in steps {
        if let Step::Err(d) = st {
            errs.push(d);
        }
    }
    got
}

async fn settle(cx: &super::Cx, steps: Vec<Step>, slots: Vec<Option<Arc<Inst>>>, errs: &mut Vec<ErrDetail>) -> R<()> {
    let mut starts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for st in steps {
        match st {
            Step::Err(d) => errs.push(d),
            Step::ModelStart { slot } => {
                starts.insert(slot, errs.len());
            }
            Step::ModelAfter { slot, desc, input, loc } => {
                let Some(inst) = &slots[slot] else { continue };
                if errs.len() > starts.get(&slot).copied().unwrap_or(0) {
                    continue;
                }
                for f in desc.model_after {
                    match f(cx, V::Inst(inst.clone()), vec![]).await {
                        // the returned instance replaces the model (copied in place: same identity here)
                        Ok(V::Inst(other)) if !Arc::ptr_eq(&other, inst) && std::ptr::eq(other.desc, inst.desc) => {
                            *inst.vals.lock() = other.vals.lock().clone();
                            *inst.set.lock() = other.set.lock().clone();
                            *inst.extra.lock() = other.extra.lock().clone();
                        }
                        Ok(_) => {}
                        Err(x) => {
                            let (kind, word) = if x.isinstance(&ASSERTION_ERROR) {
                                ("assertion_error", "Assertion failed")
                            } else if x.isinstance(&VALUE_ERROR) {
                                ("value_error", "Value error")
                            } else {
                                return Err(x);
                            };
                            errs.push(ErrDetail { kind, loc: loc.clone(), msg: format!("{word}, {}", x.message()), input: input.clone(), ctx: Some(vec![("error", V::empty_dict())]) });
                            break;
                        }
                    }
                }
            }
            Step::Default { slot, field, f } => {
                if let Some(inst) = &slots[slot] {
                    let d = f(cx, V::None, vec![]).await?;
                    inst.vals.lock()[field] = d;
                }
            }
            Step::Validators { slot, field, desc, value, loc } => {
                let name = desc.fields[field].name;
                let mut cur = value;
                let mut failed = false;
                for vd in desc.validators.iter().filter(|vd| vd.fields.contains(&name)) {
                    match (vd.f)(cx, V::Class(desc.class), vec![cur.clone()]).await {
                        Ok(v) => cur = v,
                        Err(x) => {
                            let (kind, word) = if x.isinstance(&ASSERTION_ERROR) {
                                ("assertion_error", "Assertion failed")
                            } else if x.isinstance(&VALUE_ERROR) {
                                ("value_error", "Value error")
                            } else {
                                return Err(x);
                            };
                            // ctx.error is the exception object: FastAPI's jsonable_encoder renders it {}
                            let ctx = Some(vec![("error", V::empty_dict())]);
                            errs.push(ErrDetail { kind, loc: loc.clone(), msg: format!("{word}, {}", x.message()), input: cur.clone(), ctx });
                            failed = true;
                            break;
                        }
                    }
                }
                if !failed {
                    if let Some(inst) = &slots[slot] {
                        inst.vals.lock()[field] = cur;
                    }
                }
            }
        }
    }
    Ok(())
}

fn val(input: &V, td: &'static TD, loc: &[V], e: &mut Errs) -> Option<V> {
    if let V::Native(n) = input {
        if let Native::ValErr(kind, msg, raw) = &**n {
            e.push(kind, loc, msg.clone(), raw, Some(vec![("error", V::empty_dict())]));
            return None;
        }
    }
    match td {
        TD::Any => Some(input.clone()),
        TD::NoneT => match input {
            V::None => Some(V::None),
            _ => {
                e.push("none_required", loc, "Input should be None", input, None);
                None
            }
        },
        TD::Optional(inner) => match input {
            V::None => Some(V::None),
            _ => val(input, inner, loc, e),
        },
        TD::Union(choices) => {
            // smart mode: an exact match wins at once; otherwise the member with the most fields set,
            // then the most exact, then the leftmost. All failed: every member's errors, under its label.
            // (A member's validators run after the choice: Pydantic would try the next member when one
            // raises — see the README.)
            let mut best: Option<(V, Vec<Step>, u8, Option<usize>)> = None;
            let (mut failed, mut nfailed) = (Vec::new(), 0);
            for c in choices.iter() {
                let mut tmp = Vec::new();
                let mut sub = e.sub(&mut tmp);
                let got = val(input, c, &with(loc, V::str(label(c))), &mut sub);
                let (ex, fs, n) = (sub.exact, sub.fields_set, sub.n);
                match got {
                    Some(v) => {
                        if ex == EXACT && fs.is_none() {
                            e.steps.extend(tmp);
                            return Some(v);
                        }
                        let better = match &best {
                            None => true,
                            Some((_, _, bex, bfs)) => match (bfs, fs) {
                                (Some(a), Some(b)) if *a != b => *a < b,
                                _ => *bex < ex,
                            },
                        };
                        if better {
                            best = Some((v, tmp, ex, fs));
                        }
                    }
                    None if best.is_none() => {
                        failed.extend(tmp);
                        nfailed += n;
                    }
                    None => {}
                }
            }
            match best {
                Some((v, steps, ex, fs)) => {
                    e.steps.extend(steps);
                    e.floor(ex);
                    if fs.is_some() {
                        e.fields_set = fs;
                    }
                    Some(v)
                }
                None => {
                    e.steps.extend(failed);
                    e.n += nfailed;
                    None
                }
            }
        }
        TD::Bool => match input {
            V::Bool(b) => Some(V::Bool(*b)),
            V::Int(i @ (0 | 1)) => {
                e.floor(LAX);
                Some(V::Bool(*i == 1))
            }
            V::Float(f) if *f == 0.0 || *f == 1.0 => {
                e.floor(LAX);
                Some(V::Bool(*f == 1.0))
            }
            V::Str(s) => match s.to_ascii_lowercase().as_str() {
                "0" | "off" | "f" | "false" | "n" | "no" => {
                    e.floor(LAX);
                    Some(V::Bool(false))
                }
                "1" | "on" | "t" | "true" | "y" | "yes" => {
                    e.floor(LAX);
                    Some(V::Bool(true))
                }
                _ => {
                    e.push("bool_parsing", loc, "Input should be a valid boolean, unable to interpret input", input, None);
                    None
                }
            },
            V::Int(_) => {
                e.push("bool_parsing", loc, "Input should be a valid boolean, unable to interpret input", input, None);
                None
            }
            _ => {
                e.push("bool_type", loc, "Input should be a valid boolean", input, None);
                None
            }
        },
        TD::Int(c) => {
            if !matches!(input, V::Int(_)) {
                e.floor(LAX);
            }
            let n = match input {
                V::Bool(b) => Some(*b as i64),
                V::Int(i) => Some(*i),
                V::Float(f) => {
                    if f.fract() == 0.0 && f.is_finite() {
                        Some(*f as i64)
                    } else {
                        e.push("int_from_float", loc, "Input should be a valid integer, got a number with a fractional part", input, None);
                        return None;
                    }
                }
                V::Decimal(d) => {
                    if d.is_integral() {
                        num_traits::ToPrimitive::to_i64(&d.to_int())
                    } else {
                        e.push("int_from_float", loc, "Input should be a valid integer, got a number with a fractional part", input, None);
                        return None;
                    }
                }
                V::Str(s) => {
                    let t = s.trim().replace('_', "");
                    match t.parse::<i64>() {
                        Ok(i) => Some(i),
                        Err(_) => match t.parse::<f64>() {
                            Ok(f) if f.fract() == 0.0 && f.is_finite() && t.contains('.') => Some(f as i64),
                            _ => {
                                e.push("int_parsing", loc, "Input should be a valid integer, unable to parse string as an integer", input, None);
                                return None;
                            }
                        },
                    }
                }
                _ => None,
            };
            match n {
                Some(i) => {
                    if check_num(i as f64, c, loc, input, e) { Some(V::Int(i)) } else { None }
                }
                None => {
                    e.push("int_type", loc, "Input should be a valid integer", input, None);
                    None
                }
            }
        }
        TD::Float(c) => {
            e.floor(match input {
                V::Float(_) => EXACT,
                V::Int(_) => STRICT,
                _ => LAX,
            });
            let x = match input {
                V::Bool(b) => Some(*b as i64 as f64),
                V::Int(i) => Some(*i as f64),
                V::Float(f) => Some(*f),
                V::Decimal(d) => Some(d.to_f64()),
                V::Str(s) => match s.trim().parse::<f64>() {
                    Ok(f) => Some(f),
                    Err(_) => {
                        e.push("float_parsing", loc, "Input should be a valid number, unable to parse string as a number", input, None);
                        return None;
                    }
                },
                _ => None,
            };
            match x {
                Some(f) => {
                    if check_num_kind(f, c, loc, input, e, true) { Some(V::Float(f)) } else { None }
                }
                None => {
                    e.push("float_type", loc, "Input should be a valid number", input, None);
                    None
                }
            }
        }
        TD::Str(c) => match input {
            V::Str(s) => {
                let mut s: String = s.to_string();
                if c.strip {
                    s = s.trim().to_string();
                }
                if c.lower {
                    s = s.to_lowercase();
                } else if c.upper {
                    s = s.to_uppercase();
                }
                let n = s.chars().count();
                if let Some(m) = c.min {
                    if n < m {
                        e.push("string_too_short", loc, format!("String should have at least {m} character{}", if m == 1 { "" } else { "s" }), input, Some(vec![("min_length", V::Int(m as i64))]));
                        return None;
                    }
                }
                if let Some(m) = c.max {
                    if n > m {
                        e.push("string_too_long", loc, format!("String should have at most {m} character{}", if m == 1 { "" } else { "s" }), input, Some(vec![("max_length", V::Int(m as i64))]));
                        return None;
                    }
                }
                if let Some(p) = c.pattern {
                    if !p.is_match(&s) {
                        e.push("string_pattern_mismatch", loc, format!("String should match pattern '{}'", p.src), input, Some(vec![("pattern", V::str(p.src))]));
                        return None;
                    }
                }
                Some(V::str(s))
            }
            _ => {
                e.push("string_type", loc, "Input should be a valid string", input, None);
                None
            }
        },
        TD::DateTime => match input {
            V::DateTime(d) => Some(V::DateTime(*d)),
            _ if {
                // anything but a datetime is a lax match
                e.floor(LAX);
                false
            } => None,
            V::Str(s) => match dt::parse_datetime(s) {
                Ok(d) => Some(V::DateTime(d)),
                Err(_) => match dt::parse_date(s) {
                    Ok(d) => Some(V::DateTime(DateTime::naive(d.and_hms_opt(0, 0, 0).unwrap()))),
                    Err(err) => {
                        e.push("datetime_from_date_parsing", loc, format!("Input should be a valid datetime or date, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                        None
                    }
                },
            },
            V::Int(i) => dt::from_timestamp(*i as f64).map(V::DateTime),
            V::Float(f) => dt::from_timestamp(*f).map(V::DateTime),
            _ => {
                e.push("datetime_type", loc, "Input should be a valid datetime", input, None);
                None
            }
        },
        TD::Date => {
            let from_dt = |d: DateTime, e: &mut Errs| {
                if d.wall.time() == chrono::NaiveTime::MIN {
                    Some(V::Date(d.wall.date()))
                } else {
                    e.push("date_from_datetime_inexact", loc, "Datetimes provided to dates should have zero time - e.g. be exact dates", input, None);
                    None
                }
            };
            if !matches!(input, V::Date(_)) {
                e.floor(LAX);
            }
            match input {
                V::Date(d) => Some(V::Date(*d)),
                V::DateTime(d) => from_dt(*d, e),
                V::Str(s) => match dt::parse_date(s) {
                    Ok(d) => Some(V::Date(d)),
                    Err(_) => match dt::parse_datetime(s) {
                        Ok(d) => from_dt(d, e),
                        Err(err) => {
                            e.push("date_from_datetime_parsing", loc, format!("Input should be a valid date or datetime, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                            None
                        }
                    },
                },
                V::Int(i) => dt::from_timestamp(*i as f64).and_then(|d| from_dt(d, e)),
                V::Float(f) => dt::from_timestamp(*f).and_then(|d| from_dt(d, e)),
                _ => {
                    e.push("date_type", loc, "Input should be a valid date", input, None);
                    None
                }
            }
        }
        TD::Time => match input {
            V::Time(t) => Some(V::Time(*t)),
            _ => {
                e.push("time_type", loc, "Input should be a valid time", input, None);
                None
            }
        },
        TD::Delta => match input {
            V::Delta(d) => Some(V::Delta(*d)),
            _ => {
                e.push("time_delta_type", loc, "Input should be a valid timedelta", input, None);
                None
            }
        },
        TD::Dict(kv) => match input {
            V::Dict(d) => {
                let items: Vec<(V, V)> = d.lock().values().cloned().collect();
                let mut out = Vec::with_capacity(items.len());
                let mut ok = true;
                for (k, v) in items {
                    match kv {
                        None => out.push((k, v)),
                        Some((kt, vt)) => {
                            let kk = val(&k, kt, &with(&with(loc, k.clone()), V::str("[key]")), e);
                            let vv = val(&v, vt, &with(loc, k.clone()), e);
                            match (kk, vv) {
                                (Some(kk), Some(vv)) => out.push((kk, vv)),
                                _ => ok = false,
                            }
                        }
                    }
                }
                if ok { V::dict_from(out).ok() } else { None }
            }
            _ => {
                e.push("dict_type", loc, "Input should be a valid dictionary", input, None);
                None
            }
        },
        TD::List(item) | TD::Set(item) | TD::Tuple(item) => {
            let (kind, msg) = match td {
                TD::List(_) => ("list_type", "Input should be a valid list"),
                TD::Set(_) => ("set_type", "Input should be a valid set"),
                _ => ("tuple_type", "Input should be a valid tuple"),
            };
            if !matches!((td, input), (TD::List(_), V::List(_)) | (TD::Set(_), V::Set(_)) | (TD::Tuple(_), V::Tuple(_))) {
                e.floor(LAX);
            }
            let items = match input {
                V::List(l) => l.lock().clone(),
                V::Tuple(t) => t.to_vec(),
                V::Set(s) => s.lock().values().cloned().collect(),
                _ => {
                    e.push(kind, loc, msg, input, None);
                    return None;
                }
            };
            let mut out = Vec::with_capacity(items.len());
            let mut ok = true;
            for (i, it) in items.iter().enumerate() {
                match item {
                    None => out.push(it.clone()),
                    Some(t) => match val(it, t, &with(loc, V::Int(i as i64)), e) {
                        Some(v) => out.push(v),
                        None => ok = false,
                    },
                }
            }
            if !ok {
                return None;
            }
            Some(match td {
                TD::List(_) => V::list(out),
                TD::Tuple(_) => V::tuple(out),
                _ => {
                    let mut m = IndexMap::new();
                    for v in out {
                        if let Ok(k) = Key::of(&v) {
                            m.insert(k, v);
                        }
                    }
                    V::Set(Arc::new(Mutex::new(m)))
                }
            })
        }
        TD::Literal(lits) => {
            for l in lits.iter() {
                let hit = match (l, input) {
                    (Lit::Str(a), V::Str(b)) => *a == &**b,
                    (Lit::Int(a), V::Int(b)) => a == b,
                    // 1.0 == 1 in Python: the literal's own value is returned
                    (Lit::Int(a), V::Float(b)) if *b == *a as f64 => return Some(V::Int(*a)),
                    (Lit::Bool(a), V::Bool(b)) => a == b,
                    (Lit::None, V::None) => true,
                    _ => false,
                };
                if hit {
                    return Some(input.clone());
                }
            }
            let reprs: Vec<String> = lits
                .iter()
                .map(|l| match l {
                    Lit::Str(s) => ops::str_repr(s),
                    Lit::Int(i) => i.to_string(),
                    Lit::Bool(b) => if *b { "True".into() } else { "False".into() },
                    Lit::None => "None".into(),
                })
                .collect();
            let expected = if reprs.len() == 1 {
                reprs[0].clone()
            } else {
                format!("{} or {}", reprs[..reprs.len() - 1].join(", "), reprs[reprs.len() - 1])
            };
            e.push("literal_error", loc, format!("Input should be {expected}"), input, Some(vec![("expected", V::str(&expected))]));
            None
        }
        TD::Schema(desc) => schema_val(input, desc, loc, e),
        TD::Url(spec) => url_val(input, spec, loc, e),
        TD::Email => match input {
            V::Str(s) => match super::email::validate(s) {
                Ok(v) => Some(V::str(v)),
                Err(reason) => {
                    e.push("value_error", loc, format!("value is not a valid email address: {reason}"), input, Some(vec![("reason", V::str(&reason))]));
                    None
                }
            },
            _ => {
                e.push("string_type", loc, "Input should be a valid string", input, None);
                None
            }
        },
        TD::Enum(desc, use_values) => {
            let hit = match input {
                V::Enum(d, _) if std::ptr::eq(*d, *desc) => Some(input.clone()),
                other => {
                    e.floor(LAX);
                    desc.by_value(other)
                }
            };
            match hit {
                Some(V::Enum(d, i)) => Some(if *use_values { d.value(i) } else { V::Enum(d, i) }),
                Some(v) => Some(v),
                None => {
                    let reprs: Vec<String> = (0..desc.members.len() as u16).map(|i| ops::repr(&desc.value(i)).unwrap_or_default()).collect();
                    let expected = if reprs.len() == 1 {
                        reprs[0].clone()
                    } else {
                        format!("{} or {}", reprs[..reprs.len() - 1].join(", "), reprs[reprs.len() - 1])
                    };
                    e.push("enum", loc, format!("Input should be {expected}"), input, Some(vec![("expected", V::str(&expected))]));
                    None
                }
            }
        }
    }
}

/// The member's label in a union error's `loc` (pydantic-core's validator names).
fn label(td: &TD) -> String {
    match td {
        TD::Any => "any".into(),
        TD::NoneT => "none".into(),
        TD::Bool => "bool".into(),
        TD::Int(c) => if c.ge.or(c.gt).or(c.le).or(c.lt).is_some() { "constrained-int" } else { "int" }.into(),
        TD::Float(c) => if c.ge.or(c.gt).or(c.le).or(c.lt).is_some() { "constrained-float" } else { "float" }.into(),
        TD::Str(c) => {
            if c.min.is_some() || c.max.is_some() || c.pattern.is_some() || c.strip || c.lower || c.upper { "constrained-str" } else { "str" }.into()
        }
        TD::DateTime => "datetime".into(),
        TD::Date => "date".into(),
        TD::Time => "time".into(),
        TD::Delta => "timedelta".into(),
        TD::Dict(kv) => match kv {
            Some((k, v)) => format!("dict[{},{}]", label(k), label(v)),
            None => "dict[any,any]".into(),
        },
        TD::List(t) => format!("list[{}]", t.map(label).unwrap_or_else(|| "any".into())),
        TD::Set(t) => format!("set[{}]", t.map(label).unwrap_or_else(|| "any".into())),
        TD::Tuple(t) => format!("tuple[{}, ...]", t.map(label).unwrap_or_else(|| "any".into())),
        TD::Optional(t) => format!("nullable[{}]", label(t)),
        TD::Union(cs) => format!("union[{}]", cs.iter().map(|c| label(c)).collect::<Vec<_>>().join(",")),
        TD::Literal(lits) => format!(
            "literal[{}]",
            lits.iter()
                .map(|l| match l {
                    Lit::Str(s) => ops::str_repr(s),
                    Lit::Int(i) => i.to_string(),
                    Lit::Bool(b) => if *b { "True".into() } else { "False".into() },
                    Lit::None => "None".into(),
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        TD::Schema(d) => d.name.into(),
        TD::Enum(d, _) => match d.kind {
            EnumKind::Str | EnumKind::StrEnum => format!("str-enum[{}]", d.name),
            EnumKind::Int | EnumKind::IntEnum => format!("int-enum[{}]", d.name),
            EnumKind::Plain => format!("enum[{}]", d.name),
        },
        TD::Email => "function-after[_validate(), str]".into(),
        TD::Url(s) => format!("url[{}]", s.name),
    }
}

fn schema_val(input: &V, desc: &'static SchemaDesc, loc: &[V], e: &mut Errs) -> Option<V> {
    if let V::Inst(i) = input {
        if std::ptr::eq(i.desc, desc) {
            return Some(input.clone()); // exact, no fields count: wins a union at once
        }
    }
    enum Src<'a> {
        Map(&'a IndexMap<Key, (V, V)>),
        Attrs(&'a V),
    }
    let dict_snapshot;
    let src = match input {
        V::Dict(d) => {
            dict_snapshot = d.lock().clone();
            Src::Map(&dict_snapshot)
        }
        V::Obj(_) | V::Inst(_) => Src::Attrs(input),
        _ => {
            e.push("model_attributes_type", loc, "Input should be a valid dictionary or object to extract fields from", input, None);
            return None;
        }
    };
    let start = e.n;
    // a slot only for the instances that have validators or computed defaults
    let slot = if !desc.validators.is_empty() || !desc.model_after.is_empty() || desc.fields.iter().any(|f| matches!(f.default, Dflt::Dyn(_))) {
        e.slots.push(None);
        Some(e.slots.len() - 1)
    } else {
        None
    };
    if let (Some(slot), false) = (slot, desc.model_after.is_empty()) {
        e.steps.push(Step::ModelStart { slot });
    }
    let mut vals = Vec::with_capacity(desc.fields.len());
    let mut set = Vec::with_capacity(desc.fields.len());
    for (fi, f) in desc.fields.iter().enumerate() {
        let key = f.alias.unwrap_or(f.name);
        let by_name = desc.populate_by_name && f.alias.is_some();
        // (value, the key it was found under: Pydantic reports errors at that key)
        let found = match &src {
            Src::Map(m) => m
                .get(&Key::Str(Arc::from(key)))
                .map(|(_, v)| (v.clone(), key))
                .or_else(|| if by_name { m.get(&Key::Str(Arc::from(f.name))).map(|(_, v)| (v.clone(), f.name)) } else { None }),
            Src::Attrs(o) => attr_value(o, key).map(|v| (v, key)).or_else(|| if by_name { attr_value(o, f.name).map(|v| (v, f.name)) } else { None }),
        };
        let (got, used) = match found {
            Some((v, k)) => (Some(v), k),
            None => (None, key),
        };
        match got {
            Some(v) => {
                set.push(true);
                let floc = with(loc, V::str(used));
                match val(&v, f.td, &floc, e) {
                    Some(v) => {
                        if let Some(slot) = slot {
                            if desc.validators.iter().any(|vd| vd.fields.contains(&f.name)) {
                                e.steps.push(Step::Validators { slot, field: fi, desc, value: v.clone(), loc: floc });
                            }
                        }
                        vals.push(v)
                    }
                    None => vals.push(V::None),
                }
            }
            None => {
                set.push(false);
                match &f.default {
                    Dflt::Required => {
                        e.push("missing", &with(loc, V::str(key)), "Field required", input, None);
                        vals.push(V::None);
                    }
                    Dflt::Value(d) | Dflt::Factory(d) if f.validate_default => {
                        let floc = with(loc, V::str(key));
                        match val(&d(), f.td, &floc, e) {
                            Some(v) => {
                                if let Some(slot) = slot {
                                    if desc.validators.iter().any(|vd| vd.fields.contains(&f.name)) {
                                        e.steps.push(Step::Validators { slot, field: fi, desc, value: v.clone(), loc: floc });
                                    }
                                }
                                vals.push(v)
                            }
                            None => vals.push(V::None),
                        }
                    }
                    Dflt::Value(f) | Dflt::Factory(f) => vals.push(f()),
                    Dflt::Dyn(f) => {
                        if let Some(slot) = slot {
                            e.steps.push(Step::Default { slot, field: fi, f: *f });
                        }
                        vals.push(V::Unbound)
                    }
                }
            }
        }
    }
    let mut extra = IndexMap::new();
    if let Src::Map(m) = &src {
        if desc.extra != Extra::Ignore {
            for (k, (kv, v)) in m.iter() {
                let name = match k {
                    Key::Str(s) => s.to_string(),
                    _ => continue,
                };
                if desc.fields.iter().any(|f| f.alias.unwrap_or(f.name) == name || (desc.populate_by_name && f.name == name)) {
                    continue;
                }
                match desc.extra {
                    Extra::Forbid => e.push("extra_forbidden", &with(loc, kv.clone()), "Extra inputs are not permitted", v, None),
                    _ => {
                        extra.insert(name, v.clone());
                    }
                }
            }
        }
    }
    if e.n > start {
        return None;
    }
    if let (Some(slot), false) = (slot, desc.model_after.is_empty()) {
        e.steps.push(Step::ModelAfter { slot, desc, input: input.clone(), loc: loc.to_vec() });
    }
    e.fields_set = Some(set.iter().filter(|x| **x).count());
    let inst = Arc::new(Inst { desc, vals: Mutex::new(vals), set: Mutex::new(set), extra: Mutex::new(extra) });
    if let Some(slot) = slot {
        e.slots[slot] = Some(inst.clone());
    }
    Some(V::Inst(inst))
}

thread_local! {
    /// An exception other than AttributeError raised while reading an attribute during validation
    /// (`from_attributes`): pydantic-core lets it propagate (e.g. MissingGreenlet on an unloaded
    /// relationship) instead of reporting the field as missing.
    static FATAL: std::cell::RefCell<Option<Exc>> = const { std::cell::RefCell::new(None) };
}

/// The exception that interrupted the last validation, if any (validation itself is synchronous).
pub fn take_fatal() -> Option<Exc> {
    FATAL.with(|f| f.borrow_mut().take())
}

fn attr_value(o: &V, name: &str) -> Option<V> {
    match o {
        V::Obj(obj) => match obj.get_attr_sync(name) {
            Ok(v) => Some(v),
            Err(x) if x.isinstance(&ATTRIBUTE_ERROR) => None,
            Err(x) => {
                FATAL.with(|f| {
                    f.borrow_mut().get_or_insert(x);
                });
                None
            }
        },
        V::Inst(i) => i.field(name),
        _ => None,
    }
}

/// `Model(**kwargs)` / `Model.model_validate(obj)`: raises ValidationError.
pub async fn construct(cx: &super::Cx, desc: &'static SchemaDesc, input: V) -> R {
    let mut errs = Vec::new();
    let input = if desc.has_before { prepare_schema(cx, input, desc).await? } else { input };
    match run(cx, &mut errs, |e| schema_val(&input, desc, &[], e)).await? {
        Some(v) => Ok(v),
        None => Err(Exc::validation(&VALIDATION_ERROR, errs)),
    }
}

/// `SomeDataclass(*args, **kwargs)`: the generated `__init__` (no validation, CPython's TypeErrors).
pub async fn dataclass_new(cx: &super::Cx, desc: &'static SchemaDesc, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let n = desc.fields.len();
    let init = format!("{}.__init__()", desc.name);
    if args.len() > n {
        let required = desc.fields.iter().filter(|f| matches!(f.default, Dflt::Required)).count();
        let takes = if required == n { format!("{}", n + 1) } else { format!("from {} to {}", required + 1, n + 1) };
        let s = if n == 0 { "" } else { "s" };
        return Err(Exc::type_error(format!("{init} takes {takes} positional argument{s} but {} were given", args.len() + 1)));
    }
    let mut vals: Vec<Option<V>> = args.into_iter().map(Some).chain(std::iter::repeat_n(None, n)).take(n).collect();
    for (k, v) in kwargs {
        match desc.field_index(&k) {
            None => return Err(Exc::type_error(format!("{init} got an unexpected keyword argument '{k}'"))),
            Some(i) if vals[i].is_some() => return Err(Exc::type_error(format!("{init} got multiple values for argument '{k}'"))),
            Some(i) => vals[i] = Some(v),
        }
    }
    let missing: Vec<String> = desc
        .fields
        .iter()
        .zip(&vals)
        .filter(|(f, v)| v.is_none() && matches!(f.default, Dflt::Required))
        .map(|(f, _)| format!("'{}'", f.name))
        .collect();
    if !missing.is_empty() {
        let names = match missing.len() {
            1 => missing[0].clone(),
            2 => format!("{} and {}", missing[0], missing[1]),
            k => format!("{}, and {}", missing[..k - 1].join(", "), missing[k - 1]),
        };
        let s = if missing.len() == 1 { "" } else { "s" };
        return Err(Exc::type_error(format!("{init} missing {} required positional argument{s}: {names}", missing.len())));
    }
    let mut out = Vec::with_capacity(n);
    for (f, v) in desc.fields.iter().zip(vals) {
        out.push(match (v, &f.default) {
            (Some(v), _) => v,
            (None, Dflt::Value(d) | Dflt::Factory(d)) => d(),
            (None, Dflt::Dyn(d)) => d(cx, V::None, vec![]).await?,
            (None, Dflt::Required) => unreachable!(),
        });
    }
    let inst = V::Inst(Arc::new(Inst { desc, vals: Mutex::new(out), set: Mutex::new(vec![true; n]), extra: Mutex::new(IndexMap::new()) }));
    if let Some(f) = desc.post_init {
        f(cx, inst.clone(), vec![]).await?;
    }
    Ok(inst)
}

impl Inst {
    pub fn field(&self, name: &str) -> Option<V> {
        self.desc.field_index(name).map(|i| self.vals.lock()[i].clone()).or_else(|| self.extra.lock().get(name).cloned())
    }
    pub fn set_field(&self, name: &str, v: V) -> R<()> {
        if self.desc.frozen && self.desc.dataclass {
            return Err(Exc::msg(&FROZEN_INSTANCE_ERROR, format!("cannot assign to field '{name}'")));
        }
        if self.desc.frozen {
            // model_config frozen=True
            let e = ErrDetail { kind: "frozen_instance", loc: vec![V::str(name)], msg: "Instance is frozen".into(), input: v, ctx: None };
            return Err(Exc::validation(&VALIDATION_ERROR, vec![e]));
        }
        match self.desc.field_index(name) {
            Some(i) => {
                let v = if self.desc.validate_assignment {
                    let mut errs = Vec::new();
                    match validate_sync(&v, self.desc.fields[i].td, &[V::str(name)], &mut errs) {
                        Some(x) if errs.is_empty() => x,
                        _ => return Err(Exc::validation(&VALIDATION_ERROR, errs)),
                    }
                } else {
                    v
                };
                self.vals.lock()[i] = v;
                self.set.lock()[i] = true;
                Ok(())
            }
            None if self.desc.open => {
                self.extra.lock().insert(name.to_string(), v);
                Ok(())
            }
            None => Err(Exc::value_error(format!("\"{}\" object has no field \"{}\"", self.desc.name, name))),
        }
    }
    pub fn equals(&self, other: &Inst) -> bool {
        // object.__eq__ (a plain class) is identity, decided by the caller (Arc::ptr_eq)
        !self.desc.open && std::ptr::eq(self.desc, other.desc) && {
            let (a, b) = (self.vals.lock().clone(), other.vals.lock().clone());
            a.iter().zip(b.iter()).all(|(x, y)| ops::eq_bool(x, y))
        }
    }
    pub fn getitem(&self, _k: &V) -> R {
        Err(Exc::type_error(format!("'{}' object is not subscriptable", self.desc.name)))
    }
    /// `for name, value in model` iterates (name, value) pairs.
    pub fn iter_fields(&self) -> Vec<V> {
        let vals = self.vals.lock().clone();
        self.desc.fields.iter().zip(vals).map(|(f, v)| V::tuple(vec![V::str(f.name), v])).collect()
    }
    pub fn repr(&self) -> R<String> {
        let vals = self.vals.lock().clone();
        let parts = self
            .desc
            .fields
            .iter()
            .zip(vals.iter())
            .map(|(f, v)| Ok(format!("{}={}", f.name, ops::repr(v)?)))
            .collect::<R<Vec<_>>>()?;
        if self.desc.open {
            if std::ptr::eq(self.desc, &super::libs::NAMESPACE) {
                let extra = self.extra.lock().clone();
                let parts = extra.iter().map(|(k, v)| Ok(format!("{k}={}", ops::repr(v)?))).collect::<R<Vec<_>>>()?;
                return Ok(format!("namespace({})", parts.join(", ")));
            }
            // CPython: <module.Class object at 0x...> (the address differs anyway)
            return Ok(format!("<{} object>", self.desc.class.qualname));
        }
        Ok(format!("{}({})", self.desc.name, parts.join(", ")))
    }
}

// ---------------------------------------------------------------- dump

#[derive(Clone, Copy, Default)]
pub struct DumpOpts {
    pub json: bool,
    pub exclude_none: bool,
    pub exclude_unset: bool,
    pub by_alias: bool,
}

/// `model_dump(...)`: a dict (python mode keeps objects, json mode makes everything JSON-able).
pub fn dump(v: &V, o: DumpOpts) -> R {
    if let Some(t) = ops::row_tuple(v) {
        return dump(&t, o);
    }
    Ok(match v {
        V::Inst(inst) => {
            let vals = inst.vals.lock().clone();
            let set = inst.set.lock().clone();
            let mut items = Vec::new();
            for (i, f) in inst.desc.fields.iter().enumerate() {
                if o.exclude_unset && !set[i] {
                    continue;
                }
                if o.exclude_none && vals[i].is_none() {
                    continue;
                }
                let k = if o.by_alias { f.alias.unwrap_or(f.name) } else { f.name };
                items.push((V::str(k), dump(&vals[i], o)?));
            }
            for (k, ev) in inst.extra.lock().clone() {
                items.push((V::str(k), dump(&ev, o)?));
            }
            V::dict_from(items)?
        }
        V::List(l) => V::list(l.lock().clone().iter().map(|x| dump(x, o)).collect::<R<Vec<_>>>()?),
        V::Tuple(t) => {
            let items = t.iter().map(|x| dump(x, o)).collect::<R<Vec<_>>>()?;
            if o.json { V::list(items) } else { V::tuple(items) }
        }
        V::Set(s) if o.json => V::list(s.lock().values().map(|x| dump(x, o)).collect::<R<Vec<_>>>()?),
        V::Dict(d) => {
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            let mut out = Vec::new();
            for (k, x) in items {
                let k = if o.json { V::str(json_key(&k)?) } else { k };
                out.push((k, dump(&x, o)?));
            }
            V::dict_from(out)?
        }
        V::DateTime(d) if o.json => V::str(d.pydantic()),
        V::Date(d) if o.json => V::str(dt::date_iso(d)),
        V::Time(t) if o.json => V::str(dt::time_iso(t)),
        V::Delta(d) if o.json => V::str(delta_iso(d)),
        // pydantic serializes a Decimal as its str() in JSON mode
        V::Decimal(d) if o.json => V::str(d.to_string()),
        V::Native(n) if o.json && matches!(&**n, Native::PydUrl(..)) => V::str(ops::str_(v)?),
        V::Enum(e, i) if o.json => dump(&e.value(*i), o)?,
        _ => v.clone(),
    })
}

fn json_key(k: &V) -> R<String> {
    Ok(match k {
        V::Enum(e, i) => json_key(&e.value(*i))?,
        V::Str(s) => s.to_string(),
        V::None => "None".into(),
        V::Bool(b) => if *b { "true" } else { "false" }.into(),
        V::Int(_) | V::Float(_) => ops::str_(k)?,
        V::Date(d) => dt::date_iso(d),
        V::DateTime(d) => d.pydantic(),
        other => ops::str_(other)?,
    })
}

/// Pydantic's ISO 8601 duration form (`PT1H`, `P1DT2S`...).
fn delta_iso(d: &chrono::TimeDelta) -> String {
    let us = dt::micros(d);
    let neg = us < 0;
    let us = us.abs();
    let days = us / 86_400_000_000;
    let rem = us % 86_400_000_000;
    let secs = rem / 1_000_000;
    let frac = rem % 1_000_000;
    let mut s = String::from(if neg { "-P" } else { "P" });
    if days > 0 {
        s += &format!("{days}D");
    }
    if secs > 0 || frac > 0 || days == 0 {
        s += "T";
        if frac > 0 {
            s += &format!("{}.{:06}S", secs, frac);
        } else {
            s += &format!("{secs}S");
        }
    }
    s
}

/// FastAPI's `jsonable_encoder` (no response_model): datetimes via `isoformat()`.
pub fn jsonable(v: &V) -> R {
    if let Some(t) = ops::row_tuple(v) {
        return jsonable(&t);
    }
    Ok(match v {
        V::Inst(_) => dump(v, DumpOpts { json: true, by_alias: true, ..Default::default() })?,
        // FastAPI's ENCODERS_BY_TYPE: bytes.decode() (UTF-8)
        V::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => V::str(s),
            Err(e) => {
                return Err(Exc::msg(&UNICODE_DECODE_ERROR, format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte", b[e.valid_up_to()], e.valid_up_to())))
            }
        },
        V::List(l) => V::list(l.lock().clone().iter().map(jsonable).collect::<R<Vec<_>>>()?),
        V::Tuple(t) => V::list(t.iter().map(jsonable).collect::<R<Vec<_>>>()?),
        V::Set(s) => V::list(s.lock().values().map(jsonable).collect::<R<Vec<_>>>()?),
        V::Dict(d) => {
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            let mut out = Vec::new();
            for (k, x) in items {
                out.push((V::str(json_key(&k)?), jsonable(&x)?));
            }
            V::dict_from(out)?
        }
        V::DateTime(d) => V::str(d.isoformat('T', "auto")),
        V::Date(d) => V::str(dt::date_iso(d)),
        V::Time(t) => V::str(dt::time_iso(t)),
        V::Delta(d) => V::Float(dt::micros(d) as f64 / 1e6),
        V::Decimal(d) => super::decimal::jsonable(d),
        V::Native(n) if matches!(&**n, Native::PydUrl(..)) => V::str(ops::str_(v)?),
        V::Exc(e) => V::str(e.message()),
        V::Enum(e, i) => jsonable(&e.value(*i))?,
        _ => v.clone(),
    })
}

// ---------------------------------------------------------------- JSON text

pub struct JsonStyle {
    pub ensure_ascii: bool,
    pub item_sep: &'static str,
    pub key_sep: &'static str,
}

/// FastAPI's JSONResponse: `json.dumps(ensure_ascii=False, separators=(",", ":"))`
pub const RESPONSE: JsonStyle = JsonStyle { ensure_ascii: false, item_sep: ",", key_sep: ":" };
/// plain `json.dumps(x)`
pub const DUMPS: JsonStyle = JsonStyle { ensure_ascii: true, item_sep: ", ", key_sep: ": " };

fn write_str(out: &mut String, s: &str, st: &JsonStyle) {
    out.push('"');
    // fast path: nothing to escape
    if !s.bytes().any(|b| b < 0x20 || b == b'"' || b == b'\\' || (st.ensure_ascii && b > 0x7e)) {
        out.push_str(s);
        out.push('"');
        return;
    }
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if st.ensure_ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Serialise a JSON-able value. `default_str`: json.dumps(default=str) for other objects.
pub fn to_json(v: &V, st: &JsonStyle, default_str: bool) -> R<String> {
    let mut out = String::new();
    write(&mut out, v, st, default_str)?;
    Ok(out)
}

fn write(out: &mut String, v: &V, st: &JsonStyle, default_str: bool) -> R<()> {
    match v {
        V::None => out.push_str("null"),
        V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        V::Int(i) => out.push_str(&i.to_string()),
        V::Decimal(d) if default_str => write_str(out, &d.to_string(), st),
        V::Decimal(_) => return Err(Exc::type_error("Object of type Decimal is not JSON serializable")),
        V::Float(f) => {
            if !f.is_finite() {
                return Err(Exc::value_error("Out of range float values are not JSON compliant"));
            }
            out.push_str(&ops::float_repr(*f))
        }
        V::Str(s) => write_str(out, s, st),
        V::List(_) | V::Tuple(_) => {
            out.push('[');
            for (i, x) in ops::iter(v)?.iter().enumerate() {
                if i > 0 {
                    out.push_str(st.item_sep);
                }
                write(out, x, st, default_str)?;
            }
            out.push(']');
        }
        V::Dict(d) => {
            out.push('{');
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            for (i, (k, x)) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(st.item_sep);
                }
                write_str(out, &json_key(k)?, st);
                out.push_str(st.key_sep);
                write(out, x, st, default_str)?;
            }
            out.push('}');
        }
        V::Enum(e, i) if e.kind != EnumKind::Plain => write(out, &e.value(*i), st, default_str)?,
        other if default_str => write_str(out, &ops::str_(other)?, st),
        other => {
            return Err(Exc::type_error(format!("Object of type {} is not JSON serializable", other.type_name())))
        }
    }
    Ok(())
}

pub fn from_serde(v: &serde_json::Value) -> V {
    match v {
        serde_json::Value::Null => V::None,
        serde_json::Value::Bool(b) => V::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                V::Int(i)
            } else if n.is_u64() {
                V::Float(n.as_f64().unwrap_or(0.0))
            } else {
                V::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => V::str(s),
        serde_json::Value::Array(a) => V::list(a.iter().map(from_serde).collect()),
        serde_json::Value::Object(o) => {
            let mut m = IndexMap::new();
            for (k, x) in o {
                let kv = V::str(k);
                m.insert(Key::Str(Arc::from(k.as_str())), (kv, from_serde(x)));
            }
            V::Dict(Arc::new(Mutex::new(m)))
        }
    }
}

/// `json.loads`
pub fn loads(s: &str) -> R {
    serde_json::from_str::<serde_json::Value>(s)
        .map(|v| from_serde(&v))
        .map_err(|e| Exc::msg(&JSON_DECODE_ERROR, format!("Expecting value: {e}")))
}

pub fn tz_utc() -> V {
    V::Tz(Tz::Utc)
}


/// A new instance of a plain project class (its `__init__` is called by the caller).
/// an instance before its state (`cls.__new__(cls)`, unpickling): fields None, none set
pub fn object_new_blank(desc: &'static SchemaDesc) -> V {
    let n = desc.fields.len();
    V::Inst(Arc::new(Inst { desc, vals: Mutex::new(vec![V::None; n]), set: Mutex::new(vec![false; n]), extra: Mutex::new(IndexMap::new()) }))
}

pub fn object_new(desc: &'static SchemaDesc) -> V {
    V::Inst(Arc::new(Inst { desc, vals: Mutex::new(vec![]), set: Mutex::new(vec![]), extra: Mutex::new(IndexMap::new()) }))
}


/// `TypeAdapter(T).validate_python / validate_json / dump_python / dump_json`
pub async fn adapter_method(cx: &super::Cx, td: &'static TD, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let kw = |k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let allowed: &[&str] = match name {
        "validate_python" | "validate_json" => &["strict"],
        "dump_python" => &["mode", "by_alias", "exclude_none", "exclude_unset"],
        "dump_json" => &["by_alias", "exclude_none", "exclude_unset"],
        _ => return Err(Exc::attr_error(format!("'TypeAdapter' object has no attribute '{name}'"))),
    };
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("py2axum: TypeAdapter.{name}({k}=) is not supported")));
    }
    if matches!(kw("strict"), Some(v) if ops::truthy(&v)?) {
        return Err(Exc::type_error("py2axum: TypeAdapter strict validation is not supported"));
    }
    let [obj] = &args[..] else {
        return Err(Exc::type_error(format!("TypeAdapter.{name}() takes exactly one positional argument ({} given)", args.len())));
    };
    match name {
        "validate_python" | "validate_json" => {
            let input = if name == "validate_json" {
                let text = match obj {
                    V::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    other => ops::str_(other)?,
                };
                loads(&text)?
            } else {
                obj.clone()
            };
            let mut errs = Vec::new();
            match validate(cx, &input, td, &[], &mut errs).await? {
                Some(v) if errs.is_empty() => Ok(v),
                _ => Err(Exc::validation(&VALIDATION_ERROR, errs)),
            }
        }
        _ => {
            let flag = |k: &str| -> R<bool> { kw(k).map(|v| ops::truthy(&v)).transpose().map(|b| b.unwrap_or(false)) };
            let json = name == "dump_json" || matches!(kw("mode"), Some(V::Str(m)) if &*m == "json");
            let o = DumpOpts { json, exclude_none: flag("exclude_none")?, exclude_unset: flag("exclude_unset")?, by_alias: flag("by_alias")? };
            let d = dump(obj, o)?;
            if name == "dump_json" {
                Ok(V::Bytes(Arc::from(to_json(&d, &RESPONSE, false)?.into_bytes().as_slice())))
            } else {
                Ok(d)
            }
        }
    }
}
