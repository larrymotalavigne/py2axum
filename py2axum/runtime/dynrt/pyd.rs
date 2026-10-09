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

/// `decimal.Decimal` constraints: bounds as decimal text (pydantic converts them to Decimal)
pub struct DecC {
    pub ge: Option<&'static str>,
    pub gt: Option<&'static str>,
    pub le: Option<&'static str>,
    pub lt: Option<&'static str>,
    pub max_digits: Option<u64>,
    pub decimal_places: Option<u64>,
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
    /// EmailStr, with the str_strip_whitespace / str_to_lower / str_to_upper of model_config
    Email(StrC),
    /// decimal.Decimal
    Decimal(DecC),
    /// pydantic's URL types (AnyUrl, HttpUrl, AnyHttpUrl, RedisDsn)
    Url(&'static UrlSpec),
    /// uuid.UUID
    Uuid,
    /// bytes, with `Field(min_length=, max_length=)`
    /// (min_length, max_length, the model's `val_json_bytes`: a str input is decoded with it)
    Bytes(Option<usize>, Option<usize>, BytesMode),
    /// `Field(min_length=, max_length=)` on a list, set, tuple or dict: (container, min, max)
    Len(&'static TD, Option<usize>, Option<usize>),
}

fn items_word(n: usize) -> &'static str {
    if n == 1 { "item" } else { "items" }
}

impl TD {
    /// the container under its length constraints (`Len`), else itself
    pub fn bare(&self) -> &TD {
        match self {
            TD::Len(t, _, _) => t.bare(),
            t => t,
        }
    }
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
    if s.is_empty() {
        // pydantic-core checks emptiness before the url crate's parser
        e.push("url_parsing", loc, "Input should be a valid URL, input is empty", input, Some(vec![("error", V::str("input is empty"))]));
        return None;
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
    /// `serialization_alias=` (or the name, beside a `validation_alias=`): the key of a dump `by_alias` when it
    /// differs from the key read on input
    pub ser: Option<&'static str>,
}

impl FieldDesc {
    /// the key of this field in a dump `by_alias`
    pub fn out_key(&self) -> &'static str {
        self.ser.or(self.alias).unwrap_or(self.name)
    }
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
    /// what the validator receives besides the value: 0 nothing, 1 a `ValidationInfo` (positional),
    /// 2 `values=info.data` (v1 `@validator`)
    pub info: u8,
}

static ANY_TD: TD = TD::Any;
static STR_TD: TD = TD::Str(NO_STR);
static VALINFO_CLASS: Class = Class { name: "ValidationInfo", qualname: "ValidationInfo", bases: &[], kind: ClassKind::Schema(&VALINFO) };
/// pydantic-core's `ValidationInfo` as a field validator sees it (the transpiler refuses other attributes)
static VALINFO: SchemaDesc = SchemaDesc {
    name: "ValidationInfo",
    class: &VALINFO_CLASS,
    fields: &[
        FieldDesc { name: "data", alias: None, td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false, ser: None },
        FieldDesc { name: "field_name", alias: None, td: &STR_TD, default: Dflt::Required, env: None, validate_default: false, ser: None },
    ],
    from_attributes: false,
    extra: Extra::Ignore,
    validators: &[],
    validate_assignment: false,
    populate_by_name: false,
    methods: &[],
    open: false,
    model_after: &[],
    before: &[],
    model_before: &[],
    has_before: false,
    frozen: false,
    post_init: None,
    hash: HashKind::Unhashable,
    dataclass: false,
    async_methods: &[],
    slots: &[],
    settings: None,
    init: None,
    private: &[],
    ser_bytes: BytesMode::Utf8,
    computed: &[],
    json_schema: None,
};

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
    /// a pydantic-settings class: how calling it reads the environment
    pub settings: Option<SettingsDesc>,
    /// a plain class's `__init__`, or a model's own (pydantic-core calls it for a dict input)
    pub init: Option<MethodFn>,
    /// `ser_json_bytes` of the model config
    pub ser_bytes: BytesMode,
    /// `@computed_field` properties, serialized after the fields and the extras
    pub computed: &'static [(&'static str, MethodFn)],
    /// `model_json_schema()`, computed at translation time for the models used as values (None otherwise:
    /// the translator refused every call that could reach it)
    pub json_schema: Option<&'static str>,
    /// private attributes (`_name: T = PrivateAttr(...)`, any `_name`): set per instance, never
    /// validated nor dumped (held in `extra`)
    pub private: &'static [(&'static str, Dflt)],
}

impl SchemaDesc {
    pub fn is_private(&self, name: &str) -> bool {
        self.private.iter().any(|(n, _)| *n == name)
    }
    /// the private attributes' defaults (a fresh value per instance)
    pub fn private_defaults(&self, extra: &mut IndexMap<String, V>) {
        for (n, d) in self.private {
            match d {
                Dflt::Value(f) | Dflt::Factory(f) => {
                    extra.insert(n.to_string(), f());
                }
                Dflt::Required | Dflt::Dyn(_) => {}
            }
        }
    }
}

/// pydantic-settings' `model_config` as `BaseSettings()` uses it
#[derive(Clone, Copy)]
pub struct SettingsDesc {
    pub prefix: &'static str,
    pub case_sensitive: bool,
    /// `env_parse_none_str`
    pub none_str: Option<&'static str>,
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

/// pydantic-core's `str(ValidationError)`: the title line, then per error its loc, message,
/// `[type=..., input_value=..., input_type=...]` (the repr cut to 25 + 24 bytes around `...` past
/// 50) and the documentation link.
pub fn error_str(title: &str, errs: &[ErrDetail]) -> String {
    let n = errs.len();
    let mut out = format!("{n} validation error{} for {title}", if n == 1 { "" } else { "s" });
    for e in errs {
        let loc = e.loc.iter().map(|l| match l {
            V::Str(s) => s.to_string(),
            o => ops::repr(o).unwrap_or_default(),
        }).collect::<Vec<_>>().join(".");
        if !loc.is_empty() {
            out += "\n";
            out += &loc;
        }
        let r = ops::repr(&e.input).unwrap_or_default();
        let r = if r.len() > 50 {
            let mut a = 25;
            while !r.is_char_boundary(a) {
                a -= 1;
            }
            let mut b = r.len() - 24;
            while !r.is_char_boundary(b) {
                b += 1;
            }
            format!("{}...{}", &r[..a], &r[b..])
        } else {
            r
        };
        out += &format!("\n  {} [type={}, input_value={}, input_type={}]", e.msg, e.kind, r, e.input.type_name());
        out += &format!("\n    For further information visit https://errors.pydantic.dev/{}/v/{}", super::pydantic(), e.kind);
    }
    out
}

/// One step of a validation, in pydantic-core's order. The type checks are synchronous; the
/// `@field_validator`s and computed defaults are compiled Python (async), so they are recorded
/// here and replayed by `settle`, which slots their errors exactly where Pydantic raises them.
enum Step {
    Err(ErrDetail),
    /// an input the runtime cannot represent (raised when the steps settle: a 500, never a wrong value)
    Raise(Exc),
    /// field `field` of the instance in `slot` passed its type check: run its validators on `value`;
    /// `prior` = the earlier fields that passed theirs (`info.data`, once their own validators ran),
    /// filled only when a validator takes `info`
    /// (`raw`: the field's input, what a validator error reports as `input`, like pydantic-core)
    Validators { slot: usize, field: usize, desc: &'static SchemaDesc, value: V, raw: V, loc: Vec<V>, prior: Vec<(usize, V)> },
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
    fn raise(&mut self, x: Exc) {
        self.steps.push(Step::Raise(x));
        self.n += 1;
    }
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

/// An Enum member given to a scalar field (an ORM enum column read into a `str` field...), as pydantic-core
/// does it (2.13, measured): a member of a `str` subclass (`class X(str, Enum)`, `StrEnum`) is validated as its
/// value for `str`, `int`, `float`, `bool` and `Literal`, an `int` subclass's (`IntEnum`...) likewise except for
/// `str`, where any other member becomes `str(value)` (lax); a plain Enum's member is its value, unchecked, for
/// an unconstrained `int`; `float`, `bool`, `Literal` refuse it. The member stays the errors' input. None: not
/// one of these cases, validated as usual.
fn enum_scalar(input: &V, d: &'static EnumDesc, i: u16, td: &'static TD, loc: &[V], e: &mut Errs) -> Option<Option<V>> {
    let is_str = matches!(d.kind, EnumKind::Str | EnumKind::StrEnum);
    let is_int = matches!(d.kind, EnumKind::Int | EnumKind::IntEnum);
    let value = d.value(i);
    let inner = match td {
        TD::Str(_) if is_str => {
            e.floor(STRICT);
            value
        }
        TD::Str(_) => {
            e.floor(LAX);
            V::str(ops::str_(&value).ok()?)
        }
        TD::Int(_) | TD::Float(_) | TD::Bool | TD::Literal(_) if is_str || is_int => {
            e.floor(STRICT);
            value
        }
        TD::Int(c) => {
            e.floor(LAX);
            if c.ge.or(c.gt).or(c.le).or(c.lt).is_none() {
                return Some(Some(value));
            }
            value
        }
        _ => return None,
    };
    let start = e.steps.len();
    let out = val(&inner, td, loc, e);
    for s in &mut e.steps[start..] {
        if let Step::Err(x) = s {
            x.input = input.clone();
        }
    }
    Some(out)
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
    pub vals: Slots<Vec<V>>,
    pub set: Mutex<Vec<bool>>,
    pub extra: Slots<IndexMap<String, V>>,
}

/// An instance's attributes. An instance shared by every request (a middleware's `self.app`, a service
/// object at module level) is read by all of them at once: `read` lets readers in together (a mutex made
/// them spin on each other); `lock` is the exclusive access every other use keeps.
pub struct Slots<T>(parking_lot::RwLock<T>);

impl<T> Slots<T> {
    pub fn new(t: T) -> Self {
        Slots(parking_lot::RwLock::new(t))
    }
    pub fn lock(&self) -> parking_lot::RwLockWriteGuard<'_, T> {
        self.0.write()
    }
    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, T> {
        self.0.read()
    }
}

/// `TypeAdapter(td).validate_python(input, from_attributes=True)` (FastAPI's mode), validators
/// included. Errors are appended to `errs`; the value is returned only if there were none.
/// Exceptions other than ValueError/AssertionError raised by a validator propagate.
pub async fn validate(cx: &super::Cx, input: &V, td: &'static TD, loc: &[V], errs: &mut Vec<ErrDetail>) -> R<Option<V>> {
    let input = if td_has_before(td) { prepare(cx, input.clone(), td).await? } else { input.clone() };
    let pf = Pf { cx, keys: Default::default() };
    prefetch(&pf, td, &input).await?;
    run(cx, errs, |e| val(&input, td, loc, e)).await
}

fn td_has_before(td: &TD) -> bool {
    match td {
        TD::Schema(d) => d.has_before,
        TD::List(Some(t)) | TD::Set(Some(t)) | TD::Tuple(Some(t)) | TD::Optional(t) | TD::Len(t, _, _) => td_has_before(t),
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
    let msg = format!("{word}, {}", x.message());
    Ok(V::native(Native::ValErr(kind, msg, input, V::Exc(x))))
}

/// The asynchronous pre-pass: `mode="before"` validators applied to the raw input, model first,
/// then each field (validators in reverse definition order, like pydantic-core), recursively.
fn prepare<'a>(cx: &'a super::Cx, input: V, td: &'static TD) -> super::BoxFut<'a> {
    Box::pin(async move {
        Ok(match (td, &input) {
            (TD::Optional(_), V::None) => input,
            (TD::Optional(t), _) => prepare(cx, input, t).await?,
            (TD::Len(t, _, _), _) => prepare(cx, input, t).await?,
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
            // pydantic-core calls a custom `_missing_` for a value that is no member (as `Enum(value)` does): None
            // or an exception is the `enum` error; any exception before pydantic 2.14, a ValueError only since
            (TD::Enum(d, _), _) if d.missing.is_some() && !matches!(&input, V::Enum(x, _) if std::ptr::eq(*x, *d)) && d.by_value(&input).is_none() => {
                match (d.missing.unwrap())(cx, V::Class(d.class), vec![input.clone()]).await {
                    Ok(m @ V::Enum(x, _)) if std::ptr::eq(x, *d) => m,
                    Ok(V::None) => input,
                    Ok(other) => {
                        return Err(Exc::type_error(format!("error in {}._missing_: returned {} instead of None or a valid member", d.name, super::ops::repr(&other)?)))
                    }
                    Err(x) if x.isinstance(&VALUE_ERROR) || super::pydantic_before(2, 14) => input,
                    Err(x) => return Err(x),
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
    let from_attrs = matches!(&data, V::Obj(_) | V::Inst(_));
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
    let mut originals: Vec<(&'static str, V)> = Vec::new();
    for i in touched {
        let f = &desc.fields[i];
        let key = Key::Str(Arc::from(f.alias.unwrap_or(f.name)));
        let Some((kv, raw)) = map.get(&key).cloned() else { continue };
        originals.push((f.name, raw.clone()));
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
    let d = Arc::new(Mutex::new(map));
    if from_attrs && desc.init.is_some() {
        mark_from_attrs(&d);
    }
    if !desc.validators.is_empty() && !originals.is_empty() {
        mark_originals(&d, originals);
    }
    Ok(V::Dict(d))
}

type DictCell = Mutex<IndexMap<Key, (V, V)>>;

/// The inputs `prepare_schema` replaced (field name, value before the `mode="before"` validators): an
/// `after` validator's error reports the raw input (the transpiler refuses a `before` defined after an `after`)
static ORIGINALS: Mutex<Vec<(std::sync::Weak<DictCell>, Arc<Vec<(&'static str, V)>>)>> = Mutex::new(Vec::new());

fn mark_originals(d: &Arc<DictCell>, originals: Vec<(&'static str, V)>) {
    let mut m = ORIGINALS.lock();
    m.retain(|(w, _)| w.strong_count() > 0);
    m.push((Arc::downgrade(d), Arc::new(originals)));
}

fn originals_of(d: &Arc<DictCell>) -> Option<Arc<Vec<(&'static str, V)>>> {
    ORIGINALS.lock().iter().find(|(w, _)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(d)) && w.strong_count() > 0).map(|(_, o)| o.clone())
}

/// The dicts `prepare_schema` built from an object's attributes, for a model with its own `__init__`:
/// pydantic-core validates such an input attribute by attribute, without calling `__init__`.
static FROM_ATTRS: Mutex<Vec<std::sync::Weak<DictCell>>> = Mutex::new(Vec::new());

fn mark_from_attrs(d: &Arc<DictCell>) {
    let mut m = FROM_ATTRS.lock();
    m.retain(|w| w.strong_count() > 0);
    m.push(Arc::downgrade(d));
}

fn is_from_attrs(d: &Arc<DictCell>) -> bool {
    FROM_ATTRS.lock().iter().any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(d)) && w.strong_count() > 0)
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
        match st {
            Step::Err(d) => errs.push(d),
            // nowhere to raise from here: reported, never a silently wrong value
            Step::Raise(x) => errs.push(ErrDetail { kind: "py2axum_unsupported", loc: loc.to_vec(), msg: x.message(),
                                                    input: input.clone(), ctx: None }),
            _ => {}
        }
    }
    got
}

/// A validator's ValueError/AssertionError as an error detail; other exceptions propagate.
fn validator_error(x: Exc, loc: Vec<V>, input: V) -> R<ErrDetail> {
    let (kind, word) = if x.isinstance(&ASSERTION_ERROR) {
        ("assertion_error", "Assertion failed")
    } else if x.isinstance(&VALUE_ERROR) {
        ("value_error", "Value error")
    } else {
        return Err(x);
    };
    let msg = format!("{word}, {}", x.message());
    Ok(ErrDetail { kind, loc, msg, input, ctx: Some(vec![("error", V::Exc(x))]) })
}

/// `inst.field = v` under `validate_assignment=True`, as pydantic-core's `validate_assignment`: the
/// field's `before` validators, its type (nested validators included), its `after` validators
/// (`info.data` = every other field), then the model's `after` validators, which see the new value: one
/// that raises leaves it assigned (error at loc `()`, input the instance). Measured on pydantic 2.13.
pub async fn assign_validated(cx: &super::Cx, inst: &Arc<Inst>, name: &str, v: V) -> R<()> {
    let desc = inst.desc;
    let Some(i) = desc.field_index(name).filter(|_| !desc.frozen) else { return inst.set_field(name, v) };
    let fname = desc.fields[i].name;
    let loc = vec![V::str(name)];
    let raw = v.clone();
    let mut cur = v;
    for b in desc.before.iter().rev().filter(|b| b.fields.contains(&fname)) {
        match (b.f)(cx, V::Class(desc.class), vec![cur.clone()]).await {
            Ok(nv) => cur = nv,
            Err(x) => return Err(Exc::validation_titled(&VALIDATION_ERROR, vec![validator_error(x, loc, cur)?], desc.name)),
        }
    }
    let pre = cur.clone();
    let mut errs = Vec::new();
    let Some(mut cur) = validate(cx, &pre, desc.fields[i].td, &loc, &mut errs).await? else {
        return Err(Exc::validation_titled(&VALIDATION_ERROR, errs, desc.name));
    };
    for vd in desc.validators.iter().filter(|vd| vd.fields.contains(&fname)) {
        let mut args = vec![cur.clone()];
        if vd.info != 0 {
            let data: Vec<(V, V)> = {
                let vals = inst.vals.lock();
                desc.fields.iter().enumerate().filter(|(j, _)| *j != i).map(|(j, f)| (V::str(f.name), vals[j].clone())).collect()
            };
            let data = V::dict_from(data)?;
            args.push(if vd.info == 1 {
                V::Inst(Arc::new(Inst {
                    desc: &VALINFO,
                    vals: Slots::new(vec![data, V::str(fname)]),
                    set: Mutex::new(vec![true, true]),
                    extra: Slots::new(IndexMap::new()),
                }))
            } else {
                V::native(Native::Kwargs(vec![("values".to_string(), data)]))
            });
        }
        match (vd.f)(cx, V::Class(desc.class), args).await {
            Ok(nv) => cur = nv,
            Err(x) => return Err(Exc::validation_titled(&VALIDATION_ERROR, vec![validator_error(x, loc, raw)?], desc.name)),
        }
    }
    inst.vals.lock()[i] = cur;
    inst.set.lock()[i] = true;
    for f in desc.model_after {
        if let Err(x) = f(cx, V::Inst(inst.clone()), vec![]).await {
            return Err(Exc::validation_titled(&VALIDATION_ERROR, vec![validator_error(x, vec![], V::Inst(inst.clone()))?], desc.name));
        }
    }
    Ok(())
}

async fn settle(cx: &super::Cx, steps: Vec<Step>, slots: Vec<Option<Arc<Inst>>>, errs: &mut Vec<ErrDetail>) -> R<()> {
    let mut starts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    // (slot, field) -> the value after its validators, None if one raised (read back by `info.data`)
    let mut outcomes: std::collections::HashMap<(usize, usize), Option<V>> = std::collections::HashMap::new();
    for st in steps {
        match st {
            Step::Err(d) => errs.push(d),
            Step::Raise(x) => return Err(x),
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
                            let msg = format!("{word}, {}", x.message());
                            errs.push(ErrDetail { kind, loc: loc.clone(), msg, input: input.clone(), ctx: Some(vec![("error", V::Exc(x))]) });
                            break;
                        }
                    }
                }
            }
            Step::Default { slot, field, f } => {
                if let Some(inst) = &slots[slot] {
                    let d = f(cx, V::None, vec![]).await?;
                    outcomes.insert((slot, field), Some(d.clone()));
                    inst.vals.lock()[field] = d;
                }
            }
            Step::Validators { slot, field, desc, value, raw, loc, prior } => {
                let name = desc.fields[field].name;
                let mut cur = value;
                let mut failed = false;
                for vd in desc.validators.iter().filter(|vd| vd.fields.contains(&name)) {
                    let mut args = vec![cur.clone()];
                    if vd.info != 0 {
                        let mut data = Vec::new();
                        for (i, v) in &prior {
                            match outcomes.get(&(slot, *i)) {
                                Some(Some(x)) => data.push((V::str(desc.fields[*i].name), x.clone())),
                                Some(None) => {}
                                None if matches!(v, V::Unbound) => {}
                                None => data.push((V::str(desc.fields[*i].name), v.clone())),
                            }
                        }
                        let data = V::dict_from(data)?;
                        args.push(if vd.info == 1 {
                            V::Inst(Arc::new(Inst {
                                desc: &VALINFO,
                                vals: Slots::new(vec![data, V::str(name)]),
                                set: Mutex::new(vec![true, true]),
                                extra: Slots::new(IndexMap::new()),
                            }))
                        } else {
                            V::native(Native::Kwargs(vec![("values".to_string(), data)]))
                        });
                    }
                    match (vd.f)(cx, V::Class(desc.class), args).await {
                        Ok(v) => cur = v,
                        Err(x) => {
                            let (kind, word) = if x.isinstance(&ASSERTION_ERROR) {
                                ("assertion_error", "Assertion failed")
                            } else if x.isinstance(&VALUE_ERROR) {
                                ("value_error", "Value error")
                            } else {
                                return Err(x);
                            };
                            // ctx.error is the exception object (FastAPI's jsonable_encoder renders its vars)
                            let msg = format!("{word}, {}", x.message());
                            errs.push(ErrDetail { kind, loc: loc.clone(), msg, input: raw.clone(), ctx: Some(vec![("error", V::Exc(x))]) });
                            failed = true;
                            break;
                        }
                    }
                }
                outcomes.insert((slot, field), if failed { None } else { Some(cur.clone()) });
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
        if let Native::ValErr(kind, msg, raw, exc) = &**n {
            e.push(kind, loc, msg.clone(), raw, Some(vec![("error", exc.clone())]));
            return None;
        }
    }
    if let V::Enum(d, i) = input {
        if let Some(out) = enum_scalar(input, d, *i, td, loc, e) {
            return out;
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
            // pydantic-core: an integral float or Decimal is read as an int (0/1 only: bool_parsing otherwise),
            // a fractional or non-finite one is not a boolean at all (bool_type)
            V::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.223372036854775808e18 => {
                e.push("bool_parsing", loc, "Input should be a valid boolean, unable to interpret input", input, None);
                None
            }
            V::Decimal(d) if d.is_integral() => match num_traits::ToPrimitive::to_i64(&d.to_int()) {
                Some(i @ (0 | 1)) => {
                    e.floor(LAX);
                    Some(V::Bool(i == 1))
                }
                _ => {
                    e.push("bool_parsing", loc, "Input should be a valid boolean, unable to interpret input", input, None);
                    None
                }
            },
            V::Str(_) | V::Bytes(_) => match (match input {
                V::Str(s) => s.to_ascii_lowercase(),
                V::Bytes(b) => String::from_utf8_lossy(b).to_ascii_lowercase(),
                _ => String::new(),
            })
            .as_str()
            {
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
                    if f.fract() == 0.0 && f.is_finite() && f.abs() >= 9.223372036854775808e18 {
                        e.push("int_parsing_size", loc, "Unable to parse input string as an integer, exceeded maximum size", input, None);
                        return None;
                    }
                    if f.fract() == 0.0 && f.is_finite() {
                        Some(*f as i64)
                    } else {
                        e.push("int_from_float", loc, "Input should be a valid integer, got a number with a fractional part", input, None);
                        return None;
                    }
                }
                V::Decimal(d) => {
                    if d.is_integral() {
                        match num_traits::ToPrimitive::to_i64(&d.to_int()) {
                            Some(i) => Some(i),
                            None => return beyond_i64(&d.to_string()),
                        }
                    } else {
                        e.push("int_from_float", loc, "Input should be a valid integer, got a number with a fractional part", input, None);
                        return None;
                    }
                }
                V::Str(s) => {
                    let t = s.trim().replace('_', "");
                    match t.parse::<i64>() {
                        Ok(i) => Some(i),
                        Err(_) if { let d = t.strip_prefix(['+', '-']).unwrap_or(&t); !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()) } => return beyond_i64(&t),
                        // pydantic-core: digits, then only zeros after a point ("12.000"); no exponent
                        Err(_) => match t.split_once('.') {
                            Some((whole, frac)) if !frac.is_empty() && frac.bytes().all(|c| c == b'0') => {
                                let d = whole.strip_prefix(['+', '-']).unwrap_or(whole);
                                if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
                                    e.push("int_parsing", loc, "Input should be a valid integer, unable to parse string as an integer", input, None);
                                    return None;
                                }
                                match whole.parse::<i64>() {
                                    Ok(i) => Some(i),
                                    Err(_) => return beyond_i64(whole),
                                }
                            }
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
                let s = str_settings(s, c);
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
            // an Enum member: enum_scalar (before this match)
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
                // a numeric timestamp out of speedate's years: its own message, not the date parser's
                Err(err @ (dt::PErr::DateTooLarge | dt::PErr::DateTooSmall)) => {
                    e.push("datetime_from_date_parsing", loc, format!("Input should be a valid datetime or date, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                    None
                }
                Err(_) => match dt::parse_date(s) {
                    Ok(d) => Some(V::DateTime(DateTime::naive(d.and_hms_opt(0, 0, 0).unwrap()))),
                    Err(err) => {
                        e.push("datetime_from_date_parsing", loc, format!("Input should be a valid datetime or date, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                        None
                    }
                },
            },
            V::Int(_) | V::Float(_) => match dt::from_timestamp(match input { V::Int(i) => *i as f64, V::Float(f) => *f, _ => 0.0 }) {
                Ok(d) => Some(V::DateTime(d)),
                Err(err) => {
                    e.push("datetime_parsing", loc, format!("Input should be a valid datetime, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                    None
                }
            },
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
                V::Int(_) | V::Float(_) => match dt::from_timestamp(match input { V::Int(i) => *i as f64, V::Float(f) => *f, _ => 0.0 }) {
                    Ok(d) => from_dt(d, e),
                    Err(err) => {
                        e.push("date_from_datetime_parsing", loc, format!("Input should be a valid date or datetime, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                        None
                    }
                },
                _ => {
                    e.push("date_type", loc, "Input should be a valid date", input, None);
                    None
                }
            }
        }
        // pydantic-core's time validator (speedate): `HH:MM[:SS[.f]]` strings, numbers as seconds since midnight
        TD::Time => {
            if !matches!(input, V::Time(_)) {
                e.floor(LAX);
            }
            let parsed = match input {
                V::Time(t) => return Some(V::Time(*t)),
                V::Str(s) => dt::parse_time(s),
                V::Int(i) => dt::time_from_seconds(*i as f64).map(|t| (t, Some(0))),
                V::Float(f) => dt::time_from_seconds(*f).map(|t| (t, Some(0))),
                _ => {
                    e.push("time_type", loc, "Input should be a valid time", input, None);
                    return None;
                }
            };
            match parsed {
                Ok((t, None)) => Some(V::Time(t)),
                // an aware time (`15:30:00Z`, a number: UTC): the runtime's times are naive (docs/supported.md)
                Ok((_, Some(_))) => {
                    e.raise(Exc::type_error("py2axum: a timezone-aware time (a UTC offset, or a number of seconds) is not supported"));
                    None
                }
                Err(err) => {
                    e.push("time_parsing", loc, format!("Input should be in a valid time format, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                    None
                }
            }
        }
        // pydantic-core's timedelta validator (speedate, python mode): ISO 8601 or `[D days, ]HH:MM:SS` strings,
        // numbers (and bools) as seconds
        TD::Delta => {
            if !matches!(input, V::Delta(_)) {
                e.floor(LAX);
            }
            let parsed = match input {
                V::Delta(d) => return Some(V::Delta(*d)),
                V::Str(s) => dt::parse_duration(s),
                V::Bool(b) => Ok(chrono::TimeDelta::seconds(*b as i64)),
                V::Int(i) => dt::duration_from_seconds(*i as f64).and_then(|_| Ok(chrono::TimeDelta::seconds(*i))),
                V::Float(f) => dt::duration_from_seconds(*f),
                _ => {
                    e.push("time_delta_type", loc, "Input should be a valid timedelta", input, None);
                    return None;
                }
            };
            match parsed {
                Ok(d) => Some(V::Delta(d)),
                Err(err) => {
                    e.push("time_delta_parsing", loc, format!("Input should be a valid timedelta, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
                    None
                }
            }
        }
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
        TD::Len(inner, mn, mx) => {
            let ft = match inner {
                TD::List(_) => "List",
                TD::Tuple(_) => "Tuple",
                TD::Set(_) => "Set",
                _ => "Dictionary",
            };
            let items = items_word;
            // a set stops as soon as its distinct valid items pass the bound: that error alone
            if let (Some(mx), TD::Set(Some(it))) = (mx, inner) {
                let items = match input {
                    V::List(l) => l.lock().clone(),
                    V::Tuple(t) => t.to_vec(),
                    V::Set(s) => s.lock().values().cloned().collect(),
                    _ => vec![],
                };
                let mut seen = std::collections::HashSet::new();
                for (i, x) in items.iter().enumerate() {
                    let mut scratch = Vec::new();
                    let mut se = e.sub(&mut scratch);
                    if let Some(v) = val(x, it, &with(loc, V::Int(i as i64)), &mut se) {
                        if se.n == 0 {
                            if let Ok(k) = Key::of(&v) {
                                seen.insert(k);
                            }
                        }
                    }
                    if seen.len() > *mx {
                        e.push("too_long", loc, format!("Set should have at most {mx} {} after validation, not more", items_word(*mx)), input,
                            Some(vec![("field_type", V::str("Set")), ("max_length", V::Int(*mx as i64)), ("actual_length", V::None)]));
                        return None;
                    }
                }
            }
            // a list too long is refused before its items are looked at
            if let (Some(mx), TD::List(_)) = (mx, inner) {
                let n = match input {
                    V::List(l) => Some(l.lock().len()),
                    V::Tuple(t) => Some(t.len()),
                    V::Set(s) => Some(s.lock().len()),
                    _ => None,
                };
                if let Some(n) = n.filter(|n| n > mx) {
                    e.push("too_long", loc, format!("{ft} should have at most {mx} {} after validation, not {n}", items(*mx)), input,
                        Some(vec![("field_type", V::str(ft)), ("max_length", V::Int(*mx as i64)), ("actual_length", V::Int(n as i64))]));
                    return None;
                }
            }
            let out = val(input, inner, loc, e)?;
            let n = match &out {
                V::List(l) => l.lock().len(),
                V::Tuple(t) => t.len(),
                V::Set(s) => s.lock().len(),
                V::Dict(d) => d.lock().len(),
                _ => 0,
            };
            if let Some(mx) = mx.filter(|mx| n > *mx) {
                // a set stops at the first item past the bound: no actual length
                let (not, actual) = if ft == "Set" { ("more".to_string(), V::None) } else { (n.to_string(), V::Int(n as i64)) };
                e.push("too_long", loc, format!("{ft} should have at most {mx} {} after validation, not {not}", items(mx)), input,
                    Some(vec![("field_type", V::str(ft)), ("max_length", V::Int(mx as i64)), ("actual_length", actual)]));
                return None;
            }
            if let Some(mn) = mn.filter(|mn| n < *mn) {
                e.push("too_short", loc, format!("{ft} should have at least {mn} {} after validation, not {n}", items(mn)), input,
                    Some(vec![("field_type", V::str(ft)), ("min_length", V::Int(mn as i64)), ("actual_length", V::Int(n as i64))]));
                return None;
            }
            Some(out)
        }
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
        TD::Decimal(c) => decimal_val(input, c, loc, e),
        TD::Url(spec) => url_val(input, spec, loc, e),
        TD::Uuid => uuid_val(input, loc, e),
        // pydantic-core's bytes validator: bytes exactly, a str (UTF-8 encoded) leniently, then the length
        TD::Bytes(min, max, mode) => {
            let b: Arc<[u8]> = match input {
                V::Bytes(b) => b.clone(),
                V::Str(s) => {
                    e.floor(LAX);
                    match mode.decode(s) {
                        Ok(b) => b,
                        Err(err) => {
                            let enc = mode.name();
                            e.push("bytes_invalid_encoding", loc, format!("Data should be valid {enc}: {err}"), input,
                                   Some(vec![("encoding", V::str(enc)), ("encoding_error", V::str(&err))]));
                            return None;
                        }
                    }
                }
                _ => {
                    e.push("bytes_type", loc, "Input should be a valid bytes", input, None);
                    return None;
                }
            };
            if let Some(m) = *min {
                if b.len() < m {
                    e.push("bytes_too_short", loc, format!("Data should have at least {m} byte{}", if m == 1 { "" } else { "s" }), input, Some(vec![("min_length", V::Int(m as i64))]));
                    return None;
                }
            }
            if let Some(m) = *max {
                if b.len() > m {
                    e.push("bytes_too_long", loc, format!("Data should have at most {m} byte{}", if m == 1 { "" } else { "s" }), input, Some(vec![("max_length", V::Int(m as i64))]));
                    return None;
                }
            }
            Some(V::Bytes(b))
        }
        TD::Email(c) => match input {
            V::Str(s) => match super::email::validate(&str_settings(s, c)) {
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
        TD::Len(t, _, _) => label(t),
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
        TD::Email(_) => "function-after[_validate(), str]".into(),
        TD::Decimal(_) => "decimal".into(),
        TD::Url(s) => format!("url[{}]", s.name),
        TD::Uuid => "uuid".into(),
        TD::Bytes(..) => "bytes".into(),
    }
}

/// pydantic-core's uuid validator: a UUID instance, else a str (`Uuid::parse_str` of the uuid crate it
/// pins, messages included), else bytes (as text, then as 16 raw bytes)
fn uuid_val(input: &V, loc: &[V], e: &mut Errs) -> Option<V> {
    if let V::Native(n) = input {
        if let Native::Uuid(_) = &**n {
            return Some(input.clone());
        }
    }
    e.floor(LAX);
    let parsed = match input {
        V::Str(s) => uuid_parse(s),
        V::Bytes(b) => match std::str::from_utf8(b).ok().and_then(|t| uuid_parse(t).ok()) {
            Some(u) => Ok(u),
            None if b.len() == 16 => Ok(u128::from_be_bytes(b[..].try_into().unwrap())),
            None => Err(format!("invalid length: expected 16 bytes, found {}", b.len())),
        },
        _ => {
            e.push("uuid_type", loc, "UUID input should be a string, bytes or UUID object", input, None);
            return None;
        }
    };
    match parsed {
        Ok(u) => Some(V::native(Native::Uuid(u))),
        Err(err) => {
            e.push("uuid_parsing", loc, format!("Input should be a valid UUID, {err}"), input, Some(vec![("error", V::str(&err))]));
            None
        }
    }
}

/// The str schema's `strip_whitespace`, `to_lower`, `to_upper` (from model_config)
fn str_settings(s: &str, c: &StrC) -> String {
    let mut s: String = s.to_string();
    if c.strip {
        s = s.trim().to_string();
    }
    if c.lower {
        s = s.to_lowercase();
    } else if c.upper {
        s = s.to_uppercase();
    }
    s
}

/// `uuid::Uuid::parse_str` as pydantic-core's uuid crate does it: simple, hyphenated, `{braced}` and
/// `urn:uuid:` forms; on failure, the crate's `InvalidUuid::into_err` of the locked version
pub fn uuid_parse(s: &str) -> Result<u128, String> {
    fn hex(b: &[u8]) -> Option<u128> {
        b.iter().try_fold(0u128, |acc, c| (*c as char).to_digit(16).map(|d| acc << 4 | d as u128))
    }
    fn hyphenated(b: &[u8]) -> Option<u128> {
        if [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
            return None;
        }
        let digits: Vec<u8> = b.iter().copied().filter(|c| *c != b'-').collect();
        if digits.len() != 32 { None } else { hex(&digits) }
    }
    let b = s.as_bytes();
    // try_parse: the slice the error is computed on, and whether the hyphenated form was attempted
    let (res, failed, hyph) = match b.len() {
        32 => (hex(b), s, false),
        36 => (hyphenated(b), s, true),
        38 if b[0] == b'{' && b[37] == b'}' => (hyphenated(&b[1..37]), &s[1..37], true),
        45 if s.starts_with("urn:uuid:") => (hyphenated(&b[9..]), &s[9..], true),
        _ => (None, s, false),
    };
    match res {
        Some(u) => Ok(u),
        // pydantic-core 2.50 (pydantic 2.14) moved to uuid 1.23.4: 0-based positions, the requested form
        None if super::pydantic_before(2, 14) => Err(uuid_err_1_23_0(failed)),
        None => Err(uuid_err_1_23_4(failed, hyph)),
    }
}

/// uuid 1.23.0's `InvalidUuid::into_err` (pydantic-core 2.41 to 2.46)
fn uuid_err_1_23_0(failed: &str) -> String {
    let (inner, offset, simple) = if failed.len() >= 2 && failed.starts_with('{') && failed.ends_with('}') {
        (&failed[1..failed.len() - 1], 1, false)
    } else if let Some(rest) = failed.strip_prefix("urn:uuid:") {
        (rest, 9, false)
    } else {
        (failed, 0, true)
    };
    let mut hyphens = 0;
    let mut bounds = [0usize; 4];
    for (i, c) in inner.char_indices() {
        if c == '-' {
            if hyphens < 4 {
                bounds[hyphens] = i;
            }
            hyphens += 1;
        } else if !c.is_ascii_hexdigit() {
            // pydantic-core < 2.46 (pydantic 2.12) used a uuid crate that listed the expected characters
            let expected = if super::pydantic_before(2, 13) { "expected an optional prefix of `urn:uuid:` followed by [0-9a-fA-F-], " } else { "" };
            return format!("invalid character: {expected}found `{c}` at {}", i + offset + 1);
        }
    }
    if hyphens == 0 && simple {
        return format!("invalid length: expected length 32 for simple format, found {}", failed.len());
    }
    uuid_groups(failed, hyphens, &bounds)
}

/// uuid 1.23.4's `InvalidUuid::into_err` (pydantic-core 2.50): `hyph` = the input was handed to
/// `parse_hyphenated` (RequestedUuid::Hyphenated), else RequestedUuid::Any
fn uuid_err_1_23_4(failed: &str, hyph: bool) -> String {
    #[derive(PartialEq, Clone, Copy)]
    enum Form {
        Any,
        Hyphenated,
        Braced,
        Urn,
    }
    let b = failed.as_bytes();
    if b.is_empty() || b.len() > 45 {
        return format!("invalid length: found {}", b.len());
    }
    let (start, end, mut form) = if hyph {
        (0, b.len(), Form::Hyphenated)
    } else if b.len() >= 2 && b[0] == b'{' && b[b.len() - 1] == b'}' {
        (1, b.len() - 1, Form::Braced)
    } else if b.starts_with(b"urn:uuid:") {
        (9, b.len(), Form::Urn)
    } else {
        (0, b.len(), Form::Any)
    };
    let mut hyphens = 0;
    let mut bounds = [0usize; 4];
    for (i, c) in failed[start..end].char_indices() {
        // the crate looks at the low byte of each character (`character as u8`)
        match (c as u32 as u8).to_ascii_lowercase() {
            b'0'..=b'9' | b'a'..=b'f' => (),
            b'-' => {
                if form == Form::Any {
                    form = Form::Hyphenated;
                }
                if hyphens < 4 {
                    bounds[hyphens] = i;
                }
                hyphens += 1;
            }
            _ => return format!("invalid character: found `{c}` at {}", i + start),
        }
    }
    if form == Form::Any {
        return format!("invalid length: found {}", b.len());
    }
    uuid_groups(failed, hyphens, &bounds)
}

/// The group count, then the first group of the wrong length (both crate versions)
fn uuid_groups(failed: &str, hyphens: usize, bounds: &[usize; 4]) -> String {
    if hyphens != 4 {
        return format!("invalid group count: expected 5, found {}", hyphens + 1);
    }
    const STARTS: [usize; 5] = [0, 9, 14, 19, 24];
    const EXPECTED: [usize; 5] = [8, 4, 4, 4, 12];
    for i in 0..4 {
        if bounds[i] != STARTS[i + 1] - 1 {
            return format!("invalid group length in group {i}: expected {}, found {}", EXPECTED[i], bounds[i] - STARTS[i]);
        }
    }
    format!("invalid group length in group 4: expected 12, found {}", failed.len() - STARTS[4])
}

/// pydantic-core's decimal validator (lax mode): a float through its repr, a string through
/// `Decimal(str)`, finite only, then digits, then bounds (le, lt, ge, gt)
fn decimal_val(input: &V, c: &DecC, loc: &[V], e: &mut Errs) -> Option<V> {
    use super::decimal::Dec;
    e.floor(if matches!(input, V::Decimal(_)) { EXACT } else { LAX });
    let finite = |e: &mut Errs| {
        e.push("finite_number", loc, "Input should be a finite number", input, None);
        None
    };
    let d = match input {
        V::Decimal(d) => (**d).clone(),
        V::Int(i) => Dec::from_i64(*i),
        V::Float(f) if !f.is_finite() => return finite(e),
        V::Float(f) => Dec::parse(&ops::float_repr(*f)).ok()?,
        V::Str(s) => {
            let t = s.trim().to_ascii_lowercase();
            let t = t.trim_start_matches(['+', '-']);
            let special = ["nan", "snan"].iter().any(|p| t.strip_prefix(p).is_some_and(|r| r.chars().all(|c| c.is_ascii_digit())))
                || t == "inf" || t == "infinity";
            if special {
                return finite(e);
            }
            match Dec::parse(s) {
                Ok(d) => d,
                Err(_) => {
                    e.push("decimal_parsing", loc, "Input should be a valid decimal", input, None);
                    return None;
                }
            }
        }
        _ => {
            e.push("decimal_type", loc, "Decimal input should be an integer, float, string or Decimal object", input, None);
            return None;
        }
    };
    if c.max_digits.is_some() || c.decimal_places.is_some() {
        // digits of the normalized value (trailing zeros dropped)
        let (mut coeff, mut exp) = (d.coeff.clone(), d.exp);
        let ten = num_bigint::BigUint::from(10u32);
        if num_traits::Zero::is_zero(&coeff) {
            exp = 0;
        } else {
            // pydantic-core < 2.50 (pydantic 2.13) normalized with `Decimal.normalize()`, which also rounds to
            // the context's 28 digits (ROUND_HALF_EVEN): a longer value can pass
            let text = coeff.to_str_radix(10);
            if super::pydantic_before(2, 14) && text.len() > 28 {
                let drop = (text.len() - 28) as u32;
                let unit = ten.pow(drop);
                let (q, r) = (&coeff / &unit, &coeff % &unit);
                let half = &unit / 2u32;
                let odd = (&q % 2u32) == num_bigint::BigUint::from(1u32);
                coeff = if r > half || (r == half && odd) { q + 1u32 } else { q };
                exp += drop as i64;
            }
            while num_traits::Zero::is_zero(&(&coeff % &ten)) {
                coeff /= &ten;
                exp += 1;
            }
        }
        let count = |nd: i64, exp: i64| if exp >= 0 { (nd + exp, 0) } else { (nd.max(-exp), -exp) };
        let (digits, decimals) = count(coeff.to_str_radix(10).len() as i64, exp);
        // a bound is exceeded when both the value as written and the normalized one exceed it
        let (raw_digits, raw_decimals) = count(d.coeff.to_str_radix(10).len() as i64, d.exp);
        let (digits, decimals, whole_digits) = (digits.min(raw_digits), decimals.min(raw_decimals), (digits - decimals).min(raw_digits - raw_decimals));
        let pl = |n: u64| if n == 1 { "" } else { "s" };
        if let Some(m) = c.max_digits {
            if digits > m as i64 {
                e.push("decimal_max_digits", loc, format!("Decimal input should have no more than {m} digit{} in total", pl(m)), input, Some(vec![("max_digits", V::Int(m as i64))]));
                return None;
            }
        }
        if let Some(p) = c.decimal_places {
            if decimals > p as i64 {
                e.push("decimal_max_places", loc, format!("Decimal input should have no more than {p} decimal place{}", pl(p)), input, Some(vec![("decimal_places", V::Int(p as i64))]));
                return None;
            }
            if let Some(m) = c.max_digits {
                let whole = m.saturating_sub(p);
                if whole_digits > whole as i64 {
                    e.push("decimal_whole_digits", loc, format!("Decimal input should have no more than {whole} digit{} before the decimal point", pl(whole)), input, Some(vec![("whole_digits", V::Int(whole as i64))]));
                    return None;
                }
            }
        }
    }
    let checks = [
        (c.le, [std::cmp::Ordering::Less, std::cmp::Ordering::Equal].as_slice(), "less_than_equal", "less than or equal to", "le"),
        (c.lt, [std::cmp::Ordering::Less].as_slice(), "less_than", "less than", "lt"),
        (c.ge, [std::cmp::Ordering::Greater, std::cmp::Ordering::Equal].as_slice(), "greater_than_equal", "greater than or equal to", "ge"),
        (c.gt, [std::cmp::Ordering::Greater].as_slice(), "greater_than", "greater than", "gt"),
    ];
    for (bound, ok, kind, word, key) in checks {
        if let Some(b) = bound {
            let bd = Dec::parse(b).ok()?;
            if !ok.contains(&d.cmp(&bd)) {
                e.push(kind, loc, format!("Input should be {word} {bd}"), input, Some(vec![(key, super::decimal::v(bd))]));
                return None;
            }
        }
    }
    Some(super::decimal::v(d))
}

fn schema_val(input: &V, desc: &'static SchemaDesc, loc: &[V], e: &mut Errs) -> Option<V> {
    if let V::Inst(i) = input {
        if std::ptr::eq(i.desc, desc) {
            return Some(input.clone()); // exact, no fields count: wins a union at once
        }
    }
    if desc.init.is_some() && !desc.open && matches!(input, V::Dict(d) if !is_from_attrs(d)) && !SKIP_INIT.with(|s| s.replace(0) == desc as *const _ as usize) {
        // pydantic-core would call the model's own __init__(**input) here (validation is synchronous here)
        FATAL.with(|f| {
            f.borrow_mut().get_or_insert(Exc::runtime(format!(
                "py2axum: validating {} from a dict would run its own __init__, which is not supported here", desc.name)));
        });
        return None;
    }
    enum Src<'a> {
        Map(&'a IndexMap<Key, (V, V)>),
        Attrs(&'a V),
    }
    let originals = match input {
        V::Dict(d) if !desc.validators.is_empty() => originals_of(d),
        _ => None,
    };
    let raw_of = |name: &str, v: &V| originals.as_ref().and_then(|o| o.iter().find(|(n, _)| *n == name).map(|(_, x)| x.clone())).unwrap_or_else(|| v.clone());
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
    // fields that passed their type check (`info.data` holds those before the validated field)
    let mut ok: Vec<bool> = Vec::with_capacity(desc.fields.len());
    let prior = |fi: usize, vals: &Vec<V>, ok: &Vec<bool>| -> Vec<(usize, V)> {
        let name = desc.fields[fi].name;
        if !desc.validators.iter().any(|vd| vd.info != 0 && vd.fields.contains(&name)) {
            return Vec::new();
        }
        (0..fi).filter(|&i| ok[i]).map(|i| (i, vals[i].clone())).collect()
    };
    for (fi, f) in desc.fields.iter().enumerate() {
        let n_before = e.n;
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
                let n_field = e.n;
                match val(&v, f.td, &floc, e) {
                    Some(x) => {
                        // a value with an error (a length constraint, an item) never reaches the validators
                        if let (Some(slot), true) = (slot, e.n == n_field) {
                            if desc.validators.iter().any(|vd| vd.fields.contains(&f.name)) {
                                let prior = prior(fi, &vals, &ok);
                                e.steps.push(Step::Validators { slot, field: fi, desc, value: x.clone(), raw: raw_of(f.name, &v), loc: floc, prior });
                            }
                        }
                        vals.push(x)
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
                        let n_field = e.n;
                        let raw = d();
                        match val(&raw, f.td, &floc, e) {
                            Some(v) => {
                                if let (Some(slot), true) = (slot, e.n == n_field) {
                                    if desc.validators.iter().any(|vd| vd.fields.contains(&f.name)) {
                                        let prior = prior(fi, &vals, &ok);
                                        e.steps.push(Step::Validators { slot, field: fi, desc, value: v.clone(), raw: raw.clone(), loc: floc, prior });
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
        ok.push(e.n == n_before);
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
    let mut extra = extra;
    desc.private_defaults(&mut extra);
    let inst = Arc::new(Inst { desc, vals: Slots::new(vals), set: Mutex::new(set), extra: Slots::new(extra) });
    if let Some(slot) = slot {
        e.slots[slot] = Some(inst.clone());
    }
    Some(V::Inst(inst))
}

thread_local! {
    /// `super().__init__(**data)` validating `data` into the model whose own `__init__` is running
    static SKIP_INIT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// An exception other than AttributeError raised while reading an attribute during validation
    /// (`from_attributes`): pydantic-core lets it propagate (e.g. MissingGreenlet on an unloaded
    /// relationship) instead of reporting the field as missing.
    static FATAL: std::cell::RefCell<Option<Exc>> = const { std::cell::RefCell::new(None) };
}

/// The exception that interrupted the last validation, if any (validation itself is synchronous).
/// A valid Python int this runtime cannot hold (docs/supported.md): the request fails (500), it is not
/// reported as a validation error CPython would not raise.
fn beyond_i64(text: &str) -> Option<V> {
    FATAL.with(|f| {
        f.borrow_mut().get_or_insert(Exc::msg(&OVERFLOW_ERROR, format!("py2axum: the integer {} is outside the signed 64-bit range", text.chars().take(40).collect::<String>())));
    });
    None
}

pub fn take_fatal() -> Option<Exc> {
    FATAL.with(|f| f.borrow_mut().take())
}

type Fut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = R<()>> + Send + 'a>>;

/// Values of ORM `@property`s computed (asynchronously) before a validation reads them synchronously,
/// by (object, name); a property that raised keeps its exception, raised where pydantic-core reads it.
static PROPS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<(usize, &'static str), Result<V, Exc>>>> =
    std::sync::LazyLock::new(Default::default);

/// One validation's prefetch: the context properties run in, the property values it stored.
struct Pf<'a> {
    cx: &'a super::Cx,
    keys: parking_lot::Mutex<Vec<(usize, &'static str)>>,
}

impl Drop for Pf<'_> {
    fn drop(&mut self) {
        let keys = std::mem::take(&mut *self.keys.lock());
        if !keys.is_empty() {
            let mut props = PROPS.lock();
            for k in keys {
                props.remove(&k);
            }
        }
    }
}

/// Validation reads ORM attributes synchronously (`from_attributes`); in a synchronous `Session` some
/// of them need SQL (lazy loads): those the validation will read are loaded first, in its order.
/// MissingGreenlet (an async session) is left for the validation to raise where pydantic-core does.
/// The model's `@property`s are evaluated here too (they may lazy-load), their values kept for the
/// validation that follows.
fn prefetch<'a>(pf: &'a Pf<'a>, td: &'static TD, v: &'a V) -> Fut<'a> {
    Box::pin(async move {
        match (td, v) {
            (TD::Optional(t) | TD::Len(t, _, _), _) => prefetch(pf, t, v).await,
            (TD::Union(ts), _) => {
                for t in ts.iter() {
                    prefetch(pf, t, v).await?;
                }
                Ok(())
            }
            (TD::List(Some(t)) | TD::Set(Some(t)) | TD::Tuple(Some(t)), V::List(_) | V::Tuple(_)) => {
                let items = match v {
                    V::List(l) => l.lock().clone(),
                    V::Tuple(t) => t.to_vec(),
                    _ => vec![],
                };
                for x in &items {
                    prefetch(pf, t, x).await?;
                }
                Ok(())
            }
            (TD::Schema(s), V::Obj(_)) => prefetch_schema(pf, s, v).await,
            _ => Ok(()),
        }
    })
}

fn prefetch_schema<'a>(pf: &'a Pf<'a>, s: &'static SchemaDesc, v: &'a V) -> Fut<'a> {
    Box::pin(async move {
        if let V::Dict(d) = v {
            // `Model(field=orm_objects, ...)`: the ORM values its fields will validate
            let items: Vec<(V, V)> = d.lock().values().cloned().collect();
            for f in s.fields {
                let hit = items.iter().find(|(k, _)| [f.alias, Some(f.name)].into_iter().flatten().any(|n| k.as_str() == Some(n)));
                if let Some((_, x)) = hit {
                    prefetch(pf, f.td, x).await?;
                }
            }
            return Ok(());
        }
        let V::Obj(o) = v else { return Ok(()) };
        for f in s.fields {
            for key in [f.alias, Some(f.name)].into_iter().flatten() {
                if o.desc.col_index(key).is_none() && o.desc.rel_index(key).is_none() {
                    if o.desc.methods.iter().any(|(n, prop, _)| *prop && *n == key) {
                        let got = super::methods::getattr(pf.cx, v, key).await;
                        if let Ok(x) = &got {
                            prefetch(pf, f.td, x).await?;
                        }
                        let k = (Arc::as_ptr(o) as usize, key);
                        PROPS.lock().insert(k, got);
                        pf.keys.lock().push(k);
                        break;
                    }
                    continue;
                }
                match o.get_attr(key).await {
                    Ok(x) => prefetch(pf, f.td, &x).await?,
                    Err(e) if e.isinstance(&super::v::MISSING_GREENLET) => {}
                    Err(e) => return Err(e),
                }
                break;
            }
        }
        Ok(())
    })
}

fn attr_value(o: &V, name: &str) -> Option<V> {
    if let V::Obj(obj) = o {
        let hit = PROPS.lock().get(&(Arc::as_ptr(obj) as usize, name)).cloned();
        match hit {
            Some(Ok(v)) => return Some(v),
            Some(Err(x)) if x.isinstance(&ATTRIBUTE_ERROR) => return None,
            Some(Err(x)) => {
                FATAL.with(|f| {
                    f.borrow_mut().get_or_insert(x);
                });
                return None;
            }
            None => {}
        }
    }
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
    let pf = Pf { cx, keys: Default::default() };
    prefetch_schema(&pf, desc, &input).await?;
    match run(cx, &mut errs, |e| schema_val(&input, desc, &[], e)).await? {
        Some(v) => Ok(v),
        None => Err(Exc::validation(&VALIDATION_ERROR, errs)),
    }
}

/// `super().__init__(**data)` in a model's own `__init__`: validated into the instance being built
pub async fn init_in_place(cx: &super::Cx, me: &V, input: V) -> R {
    let V::Inst(inst) = me else {
        return Err(Exc::type_error("py2axum: BaseModel.__init__ outside a model"));
    };
    let desc = inst.desc;
    let mut errs = Vec::new();
    let input = if desc.has_before { prepare_schema(cx, input, desc).await? } else { input };
    let got = run(cx, &mut errs, |e| {
        SKIP_INIT.with(|s| s.set(desc as *const _ as usize));
        let v = schema_val(&input, desc, &[], e);
        SKIP_INIT.with(|s| s.set(0));
        v
    })
    .await?;
    let Some(built) = got else {
        return Err(Exc::validation(&VALIDATION_ERROR, errs));
    };
    if let V::Inst(built) = built {
        *inst.vals.lock() = built.vals.lock().clone();
        *inst.set.lock() = built.set.lock().clone();
        *inst.extra.lock() = built.extra.lock().clone();
    }
    Ok(V::None)
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
    let inst = V::Inst(Arc::new(Inst { desc, vals: Slots::new(out), set: Mutex::new(vec![true; n]), extra: Slots::new(IndexMap::new()) }));
    if let Some(f) = desc.post_init {
        f(cx, inst.clone(), vec![]).await?;
    }
    Ok(inst)
}

impl Inst {
    pub fn field(&self, name: &str) -> Option<V> {
        self.desc.field_index(name).map(|i| self.vals.read()[i].clone()).or_else(|| self.extra.read().get(name).cloned())
    }
    pub fn set_field(&self, name: &str, v: V) -> R<()> {
        if self.desc.frozen && self.desc.dataclass {
            return Err(Exc::msg(&FROZEN_INSTANCE_ERROR, format!("cannot assign to field '{name}'")));
        }
        if self.desc.frozen {
            // model_config frozen=True
            let e = ErrDetail { kind: "frozen_instance", loc: vec![V::str(name)], msg: "Instance is frozen".into(), input: v, ctx: None };
            return Err(Exc::validation_titled(&VALIDATION_ERROR, vec![e], self.desc.name));
        }
        match self.desc.field_index(name) {
            Some(i) => {
                let v = if self.desc.validate_assignment {
                    let mut errs = Vec::new();
                    match validate_sync(&v, self.desc.fields[i].td, &[V::str(name)], &mut errs) {
                        Some(x) if errs.is_empty() => x,
                        _ => return Err(Exc::validation_titled(&VALIDATION_ERROR, errs, self.desc.name)),
                    }
                } else {
                    v
                };
                self.vals.lock()[i] = v;
                self.set.lock()[i] = true;
                Ok(())
            }
            None if self.desc.open || name.starts_with('_') => {
                self.extra.lock().insert(name.to_string(), v);
                Ok(())
            }
            None if self.desc.methods.iter().any(|(n, prop, _)| *prop && *n == name) => {
                Err(Exc::attr_error(format!("property '{name}' of '{}' object has no setter", self.desc.name)))
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
        let mut parts = parts;
        if !self.desc.computed.is_empty() {
            let me = Arc::new(Inst {
                desc: self.desc,
                vals: Slots::new(vals.clone()),
                set: Mutex::new(self.set.lock().clone()),
                extra: Slots::new(self.extra.lock().clone()),
            });
            for (n, v) in computed_values(&me)? {
                parts.push(format!("{n}={}", ops::repr(&v)?));
            }
        }
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

/// The `@computed_field` values of an instance. Serialization is synchronous: the property must not
/// await (it would need the event loop in the middle of a dump).
pub fn computed_values(inst: &Arc<Inst>) -> R<Vec<(&'static str, V)>> {
    if inst.desc.computed.is_empty() {
        return Ok(Vec::new());
    }
    let cx = super::root_cx();
    let mut out = Vec::with_capacity(inst.desc.computed.len());
    for (name, f) in inst.desc.computed {
        match futures_util::FutureExt::now_or_never(f(&cx, V::Inst(inst.clone()), vec![])) {
            Some(v) => out.push((*name, v?)),
            None => return Err(Exc::runtime(format!("py2axum: @computed_field {}.{name} awaited while being serialized", inst.desc.name))),
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Default)]
pub struct DumpOpts {
    pub json: bool,
    pub exclude_none: bool,
    pub exclude_unset: bool,
    pub by_alias: bool,
    /// the `ser_json_bytes` of the model being dumped (each model its own)
    pub bytes: BytesMode,
}

/// `val_json_bytes` / `ser_json_bytes` of a model config: how bytes travel as a JSON string.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum BytesMode {
    #[default]
    Utf8,
    Base64,
    Hex,
}

impl BytesMode {
    fn name(self) -> &'static str {
        match self {
            BytesMode::Utf8 => "utf8",
            BytesMode::Base64 => "base64",
            BytesMode::Hex => "hex",
        }
    }

    /// pydantic-core: URL-safe base64 (padding optional), the standard alphabet when the input holds `+` or
    /// `/`; hex (either case). The messages are pydantic-core's (measured on 2.50).
    fn decode(self, s: &str) -> Result<Arc<[u8]>, String> {
        use base64::engine::{general_purpose::GeneralPurpose, general_purpose::GeneralPurposeConfig, DecodePaddingMode};
        use base64::{alphabet, DecodeError, Engine};
        match self {
            BytesMode::Utf8 => Ok(Arc::from(s.as_bytes())),
            BytesMode::Base64 => {
                let cfg = GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
                let url = GeneralPurpose::new(&alphabet::URL_SAFE, cfg);
                let std = GeneralPurpose::new(&alphabet::STANDARD, cfg);
                let r = match url.decode(s) {
                    Err(DecodeError::InvalidByte(_, b'+' | b'/')) => std.decode(s),
                    r => r,
                };
                r.map(Arc::from).map_err(|err| match err {
                    DecodeError::InvalidByte(off, b) => format!("Invalid symbol {b}, offset {off}."),
                    DecodeError::InvalidLength(n) => format!("Invalid input length: {n}"),
                    // pydantic-core 2.46 (pydantic 2.13) and 2.50 (2.14) word it differently (measured)
                    DecodeError::InvalidLastSymbol(off, b) if super::pydantic_before(2, 14) => format!("Invalid last symbol {b}, offset {off}."),
                    DecodeError::InvalidLastSymbol(off, b) => {
                        let bits = alphabet_value(b);
                        format!("Invalid last symbol 0x{b:02X} ('{}') at offset {off}, decoded as 0b{bits:08b}.", b as char)
                    }
                    DecodeError::InvalidPadding => "Invalid padding".into(),
                })
            }
            BytesMode::Hex => {
                let b = s.as_bytes();
                if b.len() % 2 == 1 {
                    return Err("Odd number of digits".into());
                }
                let nib = |i: usize| -> Result<u8, String> {
                    (b[i] as char).to_digit(16).map(|d| d as u8)
                        .ok_or_else(|| format!("Invalid character {:?} at position {i}", b[i] as char))
                };
                (0..b.len() / 2).map(|i| Ok(nib(2 * i)? << 4 | nib(2 * i + 1)?)).collect::<Result<Vec<u8>, String>>().map(Arc::from)
            }
        }
    }

    fn encode(self, b: &[u8]) -> R {
        use base64::Engine;
        Ok(match self {
            BytesMode::Utf8 => match std::str::from_utf8(b) {
                Ok(s) => V::str(s),
                // pydantic-core's error (Rust's Utf8Error)
                Err(err) => return Err(Exc::msg(&PYDANTIC_SERIALIZATION_ERROR, format!("Error serializing to JSON: {err}"))),
            },
            BytesMode::Base64 => V::str(base64::engine::general_purpose::URL_SAFE.encode(b)),
            BytesMode::Hex => V::str(b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        })
    }
}

/// The 6-bit value of a base64 symbol (either alphabet).
fn alphabet_value(b: u8) -> u8 {
    match b {
        b'A'..=b'Z' => b - b'A',
        b'a'..=b'z' => b - b'a' + 26,
        b'0'..=b'9' => b - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => 0,
    }
}

/// `model_dump(...)`: a dict (python mode keeps objects, json mode makes everything JSON-able).
pub fn dump(v: &V, o: DumpOpts) -> R {
    super::stack_guard()?;
    if let Some(t) = ops::row_tuple(v) {
        return dump(&t, o);
    }
    Ok(match v {
        V::Inst(inst) => {
            let vals = inst.vals.lock().clone();
            let set = inst.set.lock().clone();
            let mut items = Vec::new();
            let o = DumpOpts { bytes: inst.desc.ser_bytes, ..o };
            for (i, f) in inst.desc.fields.iter().enumerate() {
                if o.exclude_unset && !set[i] {
                    continue;
                }
                if o.exclude_none && vals[i].is_none() {
                    continue;
                }
                let k = if o.by_alias { f.out_key() } else { f.name };
                items.push((V::str(k), dump(&vals[i], o)?));
            }
            for (k, ev) in inst.extra.lock().clone() {
                // private attributes (declared, or `_name` set on the instance) are not dumped
                if inst.desc.is_private(&k) || (k.starts_with('_') && !inst.desc.open && inst.desc.extra != Extra::Allow) {
                    continue;
                }
                if !(o.exclude_none && ev.is_none()) {
                    items.push((V::str(k), dump(&ev, o)?));
                }
            }
            for (k, cv) in computed_values(inst)? {
                if !(o.exclude_none && cv.is_none()) {
                    items.push((V::str(k), dump(&cv, o)?));
                }
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
        V::Delta(d) if o.json => V::str(dt::delta_iso(d)),
        // bytes in JSON mode: the model's ser_json_bytes (utf8: the text, else pydantic-core's error)
        V::Bytes(b) if o.json => o.bytes.encode(b)?,
        // pydantic serializes a Decimal as its str() in JSON mode
        V::Decimal(d) if o.json => V::str(d.to_string()),
        V::Native(n) if o.json && matches!(&**n, Native::PydUrl(..) | Native::Uuid(_)) => V::str(ops::str_(v)?),
        V::Enum(e, i) if o.json => dump(&e.value(*i), o)?,
        _ => v.clone(),
    })
}

/// a dict key as `json.dumps` writes it (also after FastAPI's `jsonable_encoder`, which keeps None keys): `None`
/// is "null", where pydantic's serializer writes "None"
pub fn dumps_key(k: &V) -> R<String> {
    match k {
        V::None => Ok("null".into()),
        _ => json_key(k),
    }
}

pub fn json_key(k: &V) -> R<String> {
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
/// FastAPI's `jsonable_encoder` (no response_model): datetimes via `isoformat()`.
pub fn jsonable(v: &V) -> R {
    super::stack_guard()?;
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
        // FastAPI's jsonable_encoder: a deque is a sequence
        V::Native(n) if matches!(&**n, Native::Deque(_)) => V::list(ops::iter(v)?.iter().map(jsonable).collect::<R<Vec<_>>>()?),
        V::Dict(d) => {
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            let mut out = Vec::new();
            for (k, x) in items {
                out.push((V::str(dumps_key(&k)?), jsonable(&x)?));
            }
            V::dict_from(out)?
        }
        V::DateTime(d) => V::str(d.isoformat('T', "auto")),
        V::Date(d) => V::str(dt::date_iso(d)),
        V::Time(t) => V::str(dt::time_iso(t)),
        V::Delta(d) => V::Float(dt::micros_wide(d) as f64 / 1e6),
        V::Decimal(d) => super::decimal::jsonable(d)?,
        V::Native(n) if matches!(&**n, Native::PydUrl(..) | Native::Uuid(_)) => V::str(ops::str_(v)?),
        // `vars(exc)` (dict(exc) fails): HTTPException's fields, then the attributes its `__init__` set
        V::Exc(e) => {
            let mut items = Vec::new();
            if let Some((code, detail, headers)) = e.http_info() {
                let h = if headers.is_empty() { V::None } else { V::dict_from(headers.into_iter().map(|(k, x)| (V::str(k), V::str(x))).collect())? };
                items.extend([(V::str("status_code"), V::Int(code as i64)), (V::str("detail"), detail), (V::str("headers"), h)]);
            }
            items.extend(e.0.attrs.lock().iter().map(|(k, x)| (V::str(k), x.clone())));
            jsonable(&V::dict_from(items)?)?
        }
        V::Enum(e, i) => jsonable(&e.value(*i))?,
        // a mapped object: `vars(obj)` without SQLAlchemy's `_sa_*` keys (`sqlalchemy_safe`), its loaded
        // attributes in column order (CPython's order follows a set of columns: see docs/supported.md)
        V::Obj(o) => {
            let V::Dict(d) = o.instance_dict()? else { unreachable!() };
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            let mut out = Vec::new();
            for (k, x) in items {
                if !ops::str_(&k)?.starts_with("_sa") {
                    out.push((k, jsonable(&x)?));
                }
            }
            V::dict_from(out)?
        }
        _ => v.clone(),
    })
}

// ---------------------------------------------------------------- JSON text

pub struct JsonStyle {
    pub ensure_ascii: bool,
    pub item_sep: &'static str,
    pub key_sep: &'static str,
    /// pydantic's `dump_json`: NaN and infinities are `null` (ser_json_inf_nan="null"), not an error
    pub nan_null: bool,
}

/// FastAPI's JSONResponse: `json.dumps(ensure_ascii=False, separators=(",", ":"))`
pub const RESPONSE: JsonStyle = JsonStyle { ensure_ascii: false, item_sep: ",", key_sep: ":", nan_null: false };
/// plain `json.dumps(x)`
/// A response_model's JSON (FastAPI 0.130+: `TypeAdapter.dump_json`, compact, non-ASCII kept)
pub const DUMP_JSON: JsonStyle = JsonStyle { ensure_ascii: false, item_sep: ",", key_sep: ":", nan_null: true };
pub const DUMPS: JsonStyle = JsonStyle { ensure_ascii: true, item_sep: ", ", key_sep: ": ", nan_null: false };

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
    super::stack_guard()?;
    match v {
        V::None => out.push_str("null"),
        V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        V::Int(i) => out.push_str(&i.to_string()),
        V::Decimal(d) if default_str => write_str(out, &d.to_string(), st),
        V::Decimal(_) => return Err(Exc::type_error("Object of type Decimal is not JSON serializable")),
        V::Float(f) => {
            if !f.is_finite() {
                if st.nan_null {
                    out.push_str("null");
                    return Ok(());
                }
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
                // pydantic's serializer (`nan_null`) writes a None key as "None", json.dumps as "null"
                write_str(out, &if st.nan_null { json_key(k)? } else { dumps_key(k)? }, st);
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

/// `json.loads` (CPython's decoder, dynrt/pyjson.rs)
pub fn loads(s: &str) -> R {
    super::pyjson::loads(s)
}

pub fn tz_utc() -> V {
    V::Tz(Tz::Utc)
}


/// A new instance of a plain project class (its `__init__` is called by the caller).
/// an instance before its state (`cls.__new__(cls)`, unpickling): fields None, none set
pub fn object_new_blank(desc: &'static SchemaDesc) -> V {
    let n = desc.fields.len();
    V::Inst(Arc::new(Inst { desc, vals: Slots::new(vec![V::None; n]), set: Mutex::new(vec![false; n]), extra: Slots::new(IndexMap::new()) }))
}

pub fn object_new(desc: &'static SchemaDesc) -> V {
    V::Inst(Arc::new(Inst { desc, vals: Slots::new(vec![]), set: Mutex::new(vec![]), extra: Slots::new(IndexMap::new()) }))
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
            let o = DumpOpts { json, exclude_none: flag("exclude_none")?, exclude_unset: flag("exclude_unset")?, by_alias: flag("by_alias")?, ..Default::default() };
            let d = dump(obj, o)?;
            if name == "dump_json" {
                Ok(V::Bytes(Arc::from(to_json(&d, &RESPONSE, false)?.into_bytes().as_slice())))
            } else {
                Ok(d)
            }
        }
    }
}
