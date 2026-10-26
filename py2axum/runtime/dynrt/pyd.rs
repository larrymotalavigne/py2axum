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
    /// `multiple_of=`
    pub mul: Option<f64>,
    /// the model's `allow_inf_nan=False`: a float that is not finite is a `finite_number` error
    pub finite: bool,
}

pub const NO_NUM: NumC = NumC { ge: None, gt: None, le: None, lt: None, mul: None, finite: false };

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
    /// `StringConstraints(ascii_only=True)`
    pub ascii: bool,
}

pub const NO_STR: StrC = StrC { min: None, max: None, pattern: None, strip: false, lower: false, upper: false, ascii: false };

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
    /// `tuple[A, B, C]` (`tuple[()]`: no item): one type per position
    TupleFix(&'static [&'static TD]),
    /// a collection validated as the list, set or dict under it, with its own type error and result
    Coll(&'static TD, Coll),
    Optional(&'static TD),
    Union(&'static [&'static TD]),
    /// `Field(union_mode='left_to_right')`: the first member that validates
    UnionLtr(&'static [&'static TD]),
    /// strict mode on (true) or off (false) for the whole type, items included, up to the next model
    /// (`model_config strict=`, or `Field(strict=False)` on a scalar)
    Strict(&'static TD, bool),
    /// `Field(strict=True)`: the type itself strict (a container's items keep the model's mode)
    StrictTop(&'static TD),
    /// a discriminated union (`Field(discriminator='field')`)
    Tagged(&'static Tagged),
    /// `type[X]`: a class, X or a subclass of it ((name, the class; None: any Pydantic model, `type[BaseModel]`,
    /// or a class known by name only)
    Subclass(&'static str, Option<&'static Class>),
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
    /// `pathlib.Path`
    Path,
    /// pydantic's `UUID1`...`UUID8`: a UUID of that version
    UuidV(u8),
    /// `pydantic.SecretStr`
    Secret,
    /// `Callable`: anything `callable()`
    Callable,
    /// `fractions.Fraction`
    Fraction,
    /// `complex`
    Complex,
    /// `types.EllipsisType`: `...` only (no JSON value)
    Ellipsis,
    /// `pydantic.MISSING` as a type: the sentinel only (no JSON value)
    MissingSentinel,
    /// a plain class of the project (`arbitrary_types_allowed`): an instance of it (its name)
    Instance(&'static str),
    /// bytes, with `Field(min_length=, max_length=)`
    /// (min_length, max_length, the model's `val_json_bytes`: a str input is decoded with it)
    Bytes(Option<usize>, Option<usize>, BytesMode),
    /// `Field(min_length=, max_length=)` on a list, set, tuple or dict: (container, min, max)
    Len(&'static TD, Option<usize>, Option<usize>),
    /// `Iterable[T]`: any iterable, its items validated one by one when iterated (`ValidatorIterator`)
    Iter(&'static TD),
    /// `pydantic.Json` / `Json[T]`: a str or bytes parsed by jiter (inf and nan allowed), then validated as T
    Json(Option<&'static TD>),
    /// a datetime or date with `Field(gt=, ge=, lt=, le=)` and/or `AwareDatetime` / `NaiveDatetime`
    DtC(&'static TD, DtC),
}

/// a datetime or date constant of a constraint: (year, month, day, hour, minute, second, microsecond, UTC offset
/// in seconds; a date: the time fields are 0 and `date` is true)
#[derive(Clone, Copy)]
pub struct DtLit {
    pub ymd: (i32, u32, u32),
    pub hms: (u32, u32, u32, u32),
    pub offset: Option<i32>,
    pub date: bool,
}

/// pydantic-core's datetime/date constraints, checked after the type in this order: le, lt, ge, gt, then the
/// timezone (`Some(true)`: aware, `Some(false)`: naive)
#[derive(Clone, Copy)]
pub struct DtC {
    pub le: Option<DtLit>,
    pub lt: Option<DtLit>,
    pub ge: Option<DtLit>,
    pub gt: Option<DtLit>,
    pub tz: Option<bool>,
}

impl DtLit {
    fn wall(&self) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(self.ymd.0, self.ymd.1, self.ymd.2).unwrap()
            .and_hms_micro_opt(self.hms.0, self.hms.1, self.hms.2, self.hms.3).unwrap()
    }
    /// speedate's Display (pydantic-core puts it in the error message and ctx)
    fn text(&self) -> String {
        if self.date {
            return dt::date_iso(&self.wall().date());
        }
        let d = DateTime { wall: self.wall(), tz: self.offset.map(dt::Tz::Fixed), fold: 0 };
        d.pydantic()
    }
}

/// speedate's order: two aware values by their instants, otherwise (one naive at least) by their wall times
fn dt_cmp(v: &V, c: &DtLit) -> std::cmp::Ordering {
    match v {
        V::Date(d) => d.cmp(&c.wall().date()),
        V::DateTime(d) => match (d.offset(), c.offset) {
            (Some(_), Some(o)) => d.utc().cmp(&(c.wall() - chrono::Duration::seconds(o as i64))),
            _ => d.wall.cmp(&c.wall()),
        },
        _ => std::cmp::Ordering::Equal,
    }
}

fn dt_checks(v: V, c: &DtC, loc: &[V], input: &V, e: &mut Errs) -> Option<V> {
    use std::cmp::Ordering::*;
    let checks: [(&Option<DtLit>, &'static str, &'static str, &'static str, fn(std::cmp::Ordering) -> bool); 4] = [
        (&c.le, "le", "less_than_equal", "less than or equal to", |o| o != Greater),
        (&c.lt, "lt", "less_than", "less than", |o| o == Less),
        (&c.ge, "ge", "greater_than_equal", "greater than or equal to", |o| o != Less),
        (&c.gt, "gt", "greater_than", "greater than", |o| o == Greater),
    ];
    for (lit, key, kind, words, ok) in checks {
        if let Some(lit) = lit {
            if !ok(dt_cmp(&v, lit)) {
                let t = lit.text();
                e.push(kind, loc, format!("Input should be {words} {t}"), input, Some(vec![(key, V::str(&t))]));
                return None;
            }
        }
    }
    if let (Some(aware), V::DateTime(d)) = (c.tz, &v) {
        if aware && d.tz.is_none() {
            e.push("timezone_aware", loc, "Input should have timezone info", input, None);
            return None;
        }
        if !aware && d.tz.is_some() {
            e.push("timezone_naive", loc, "Input should not have timezone info", input, None);
            return None;
        }
    }
    Some(v)
}

/// pydantic-core's `ValidatorIterator`: the input's items (taken at validation, like `iter(input)`), each validated
/// against the item type when it is reached (`next`, a loop, a JSON dump), with the strictness of its model
pub struct ValIter {
    items: Mutex<std::collections::VecDeque<V>>,
    index: Mutex<usize>,
    td: &'static TD,
    strict: bool,
}

impl ValIter {
    /// the next item, validated (a ValidationError titled `ValidatorIterator`, loc = its index); None at the end
    pub fn next(&self) -> R<Option<V>> {
        let Some(item) = self.items.lock().pop_front() else { return Ok(None) };
        let i = {
            let mut ix = self.index.lock();
            *ix += 1;
            *ix - 1
        };
        let (mut steps, mut slots) = (Vec::new(), Vec::new());
        let loc = [V::Int(i as i64)];
        let got = val(&item, self.td, &loc, &mut Errs { steps: &mut steps, slots: &mut slots, n: 0, exact: EXACT, fields_set: None, strict: self.strict, json: false });
        let mut errs = Vec::new();
        for st in steps {
            match st {
                Step::Err(d) => errs.push(d),
                Step::Raise(x) => return Err(x),
                _ => {}
            }
        }
        match got {
            Some(v) if errs.is_empty() => Ok(Some(v)),
            _ => Err(Exc::validation_titled(&VALIDATION_ERROR, errs, "ValidatorIterator")),
        }
    }

    /// every remaining item, validated (iteration stops at the first invalid one, which is raised)
    pub fn drain(&self) -> R<Vec<V>> {
        let mut out = Vec::new();
        while let Some(v) = self.next()? {
            out.push(v);
        }
        Ok(out)
    }
}

/// `to_json`'s wrapping of an error raised while serializing (`Error serializing to JSON: ValidationError: ...`):
/// the items of an `Iterable` field validated by `model_dump_json` or a response
pub fn ser_json_error(x: Exc) -> Exc {
    if x.0.errors.is_some() && x.0.attrs.lock().get("title").is_some_and(|t| matches!(t, V::Str(s) if &**s == "ValidatorIterator")) {
        return Exc::msg(&PYDANTIC_SERIALIZATION_ERROR, format!("Error serializing to JSON: ValidationError: {}", x.message()));
    }
    x
}

/// a union discriminated by a field of its models: the tag read from the input picks the member
pub struct Tagged {
    /// the key read in a dict (the discriminator field's validation alias)
    pub key: &'static str,
    /// the attribute read on an instance (the field's name)
    pub attr: &'static str,
    /// (tag, member): the literal values of the field in each member
    pub tags: &'static [(&'static str, &'static TD)],
    /// `Discriminator(fn)`: the function returning the tag (then `key` is how errors name it, `fn()`)
    pub func: Option<MethodFn>,
    /// `custom_error_type=`, `custom_error_message=`, `custom_error_context=` (str values)
    pub custom: Option<(&'static str, &'static str, &'static [(&'static str, &'static str)])>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Coll {
    /// `collections.deque[T]` (over a List)
    Deque,
    /// `frozenset[T]` (over a Set)
    Frozen,
    /// `collections.Counter[K]` (over a Dict[K, int])
    Counter,
    /// `collections.OrderedDict[K, V]` (over a Dict)
    Ordered,
    /// `collections.abc.Sequence[T]` (over a List): a str is refused, a tuple stays a tuple
    Seq,
}

fn items_word(n: usize) -> &'static str {
    if n == 1 { "item" } else { "items" }
}

impl TD {
    /// the container under its length constraints (`Len`), else itself
    pub fn bare(&self) -> &TD {
        match self {
            TD::Len(t, _, _) | TD::DtC(t, _) => t.bare(),
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
    /// a `MultiHostUrl` type (PostgresDsn, MongoDsn): one host only here, the others are refused at run time
    pub multi: bool,
}

pub static ANY_URL: UrlSpec = UrlSpec { name: "AnyUrl", schemes: &[], host_required: false, default_host: None, default_port: None, default_path: None, max_length: None, multi: false };
pub static ANY_HTTP_URL: UrlSpec = UrlSpec { name: "AnyHttpUrl", schemes: &["http", "https"], host_required: false, default_host: None, default_port: None, default_path: None, max_length: None, multi: false };
pub static HTTP_URL: UrlSpec = UrlSpec { name: "HttpUrl", schemes: &["http", "https"], host_required: false, default_host: None, default_port: None, default_path: None, max_length: Some(2083), multi: false };
pub static REDIS_DSN: UrlSpec = UrlSpec { name: "RedisDsn", schemes: &["redis", "rediss"], host_required: true, default_host: Some("localhost"), default_port: Some(6379), default_path: Some("/0"), max_length: None, multi: false };
pub static POSTGRES_DSN: UrlSpec = UrlSpec { name: "PostgresDsn", schemes: &["postgres", "postgresql", "postgresql+asyncpg", "postgresql+pg8000", "postgresql+psycopg", "postgresql+psycopg2", "postgresql+psycopg2cffi", "postgresql+py-postgresql", "postgresql+pygresql"], host_required: true, default_host: None, default_port: None, default_path: None, max_length: None, multi: true };
pub static COCKROACH_DSN: UrlSpec = UrlSpec { name: "CockroachDsn", schemes: &["cockroachdb", "cockroachdb+psycopg2", "cockroachdb+asyncpg"], host_required: true, default_host: None, default_port: None, default_path: None, max_length: None, multi: false };
pub static MONGO_DSN: UrlSpec = UrlSpec { name: "MongoDsn", schemes: &["mongodb", "mongodb+srv"], host_required: false, default_host: None, default_port: Some(27017), default_path: None, max_length: None, multi: true };
pub static AMQP_DSN: UrlSpec = UrlSpec { name: "AmqpDsn", schemes: &["amqp", "amqps"], host_required: false, default_host: None, default_port: None, default_path: None, max_length: None, multi: false };
pub static KAFKA_DSN: UrlSpec = UrlSpec { name: "KafkaDsn", schemes: &["kafka"], host_required: false, default_host: Some("localhost"), default_port: Some(9092), default_path: None, max_length: None, multi: false };
pub static MYSQL_DSN: UrlSpec = UrlSpec { name: "MySQLDsn", schemes: &["mysql", "mysql+mysqlconnector", "mysql+aiomysql", "mysql+asyncmy", "mysql+mysqldb", "mysql+pymysql", "mysql+cymysql", "mysql+pyodbc"], host_required: true, default_host: None, default_port: Some(3306), default_path: None, max_length: None, multi: false };
pub static MARIADB_DSN: UrlSpec = UrlSpec { name: "MariaDBDsn", schemes: &["mariadb", "mariadb+mariadbconnector", "mariadb+pymysql"], host_required: false, default_host: None, default_port: Some(3306), default_path: None, max_length: None, multi: false };

/// a URL as pydantic-core validates it (the `url` crate, then the type's defaults)
fn url_val(input: &V, spec: &'static UrlSpec, loc: &[V], e: &mut Errs) -> Option<V> {
    if spec.multi {
        if let V::Str(t) = input {
            let authority = t.split_once("://").map(|(_, r)| r.split(['/', '?', '#']).next().unwrap_or("")).unwrap_or("");
            if authority.rsplit_once('@').map_or(authority, |(_, h)| h).contains(',') {
                FATAL.with(|f| {
                    f.borrow_mut().get_or_insert(Exc::runtime("py2axum: a DSN with several hosts (MultiHostUrl) is not supported"));
                });
                e.n += 1;
                return None;
            }
        }
    }
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
    if e.strict {
        // strict mode: pydantic-core refuses what the url crate reports as a syntax violation
        let first: std::cell::RefCell<Option<url::SyntaxViolation>> = std::cell::RefCell::new(None);
        let cb = |v: url::SyntaxViolation| {
            first.borrow_mut().get_or_insert(v);
        };
        let parsed = url::Url::options().syntax_violation_callback(Some(&cb)).parse(&s);
        let violation = first.borrow().map(|v| v.description());
        if let (Ok(_), Some(d)) = (&parsed, violation) {
            e.push("url_syntax_violation", loc, format!("Input violated strict URL syntax rules, {d}"), input, Some(vec![("error", V::str(d))]));
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
const MULTI_HOST: [&str; 2] = ["PostgresDsn", "MongoDsn"];

pub fn is_multi_host(kind: &str) -> bool {
    MULTI_HOST.contains(&kind)
}

/// `hosts()` of a MultiHostUrl with one host
pub fn url_hosts(u: &url::Url) -> R {
    let opt = |s: Option<&str>| s.filter(|x| !x.is_empty()).map(V::str).unwrap_or(V::None);
    let host = V::dict_from(vec![
        (V::str("username"), opt(Some(u.username()))),
        (V::str("password"), opt(u.password())),
        (V::str("host"), opt(u.host_str())),
        (V::str("port"), u.port().map(|p| V::Int(p as i64)).unwrap_or(V::None)),
    ])?;
    Ok(V::list(vec![host]))
}

pub fn url_attr(kind: &str, u: &url::Url, name: &str) -> R {
    if MULTI_HOST.contains(&kind) {
        // pydantic-core's MultiHostUrl: no `host`, `port`, `username` or `password` (the hosts are in `hosts()`)
        return match name {
            "scheme" | "path" | "query" | "fragment" => url_attr("", u, name),
            "host" => Err(Exc::attr_error(format!("'pydantic_core._pydantic_core.MultiHostUrl' object has no attribute '{name}'"))),
            _ => Err(Exc::attr_error(format!("'{kind}' object has no attribute '{name}'"))),
        };
    }
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
    /// a `default_factory` taking the data validated so far (the earlier fields, as a dict)
    Data(MethodFn),
}

/// one step of a `validation_alias` path (`AliasPath('names', 0)`): a key, or a list index
pub enum PK {
    S(&'static str),
    I(i64),
}

pub struct FieldDesc {
    pub name: &'static str,
    /// the validation alias (a plain key), None: the name
    pub alias: Option<&'static str>,
    /// the serialization alias (`by_alias=True`), None: the name
    pub ser: Option<&'static str>,
    /// `AliasPath`/`AliasChoices`: the paths tried in order (then `alias` is None); empty otherwise
    pub paths: &'static [&'static [PK]],
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
    /// what the validator receives besides the value: 0 nothing, 1 a `ValidationInfo` (positional),
    /// 2 `values=info.data` (v1 `@validator`)
    pub info: u8,
    /// a wrap validator (`WrapValidator`, `mode="wrap"`): its `handler` validates against this type (the field's
    /// own type is then `Any`: the validator decides)
    pub wrap: Option<&'static TD>,
}

/// the `handler` of a wrap validator: validation against `td`, a `ValidationError` (title `ValidatorCallable`,
/// locations from the value) when it fails
fn wrap_handler(td: &'static TD) -> V {
    super::func(Arc::new(move |cx, args| {
        Box::pin(async move {
            let (args, _) = super::unpack(args);
            let [v] = args.as_slice() else { return Err(Exc::type_error("handler() takes 1 positional argument")) };
            let mut errs = Vec::new();
            match validate(cx, v, td, &[], &mut errs).await? {
                Some(out) if errs.is_empty() => Ok(out),
                _ => Err(Exc::validation_titled(&VALIDATION_ERROR, errs, "ValidatorCallable")),
            }
        })
    }))
}

static ANY_TD: TD = TD::Any;
static STR_TD: TD = TD::Str(NO_STR);
static VALINFO_CLASS: Class = Class { name: "ValidationInfo", qualname: "ValidationInfo", bases: &[], kind: ClassKind::Schema(&VALINFO) };
/// pydantic-core's `ValidationInfo` as a field validator sees it (the transpiler refuses other attributes)
static VALINFO: SchemaDesc = SchemaDesc {
    name: "ValidationInfo",
    class: &VALINFO_CLASS,
    fields: &[
        FieldDesc { name: "data", alias: None, ser: None, paths: &[], td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false },
        FieldDesc { name: "field_name", alias: None, ser: None, paths: &[], td: &STR_TD, default: Dflt::Required, env: None, validate_default: false },
        FieldDesc { name: "context", alias: None, ser: None, paths: &[], td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false },
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
    x: SchemaX::NONE,
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
    /// the rarer model options (one struct, so a new one does not touch every descriptor)
    pub x: SchemaX,
}

pub struct SchemaX {
    /// `serialize_by_alias=True`: a dump without `by_alias=` uses the serialization aliases
    pub ser_by_alias: bool,
    /// `Field(exclude=True)`: never dumped
    pub exclude: &'static [&'static str],
    /// `Field(repr=False)`: left out of `repr()`
    pub norepr: &'static [&'static str],
    /// `Field(frozen=True)`: an assignment raises `frozen_field`
    pub frozen: &'static [&'static str],
    /// `Field(exclude_if=fn)`: the field is not dumped when `fn(value)` is true
    pub exclude_if: &'static [(&'static str, MethodFn)],
    /// `@field_serializer`, `PlainSerializer`/`WrapSerializer` in a field's annotation
    pub field_ser: &'static [Ser],
    /// `@model_serializer`
    pub model_ser: Option<Ser>,
    /// a `RootModel`: its one field `root` is the whole input and the whole dump
    pub root: bool,
    /// a plain class's `__new__(cls, ...)`: called before `__init__`, which runs only on an instance of the class
    pub new_: Option<MethodFn>,
    /// `@dataclass(eq=False)`: no generated `__eq__` (identity, or the base class's)
    pub eq_id: bool,
    /// `class P(NamedTuple)`: built like a dataclass, a tuple everywhere else (`ops::seq_tuple`)
    pub ntuple: bool,
    /// `hide_input_in_errors=True`: `str()` of the model's ValidationError leaves out `input_value`/`input_type`
    /// (its `errors()` keep the input)
    pub hide_input: bool,
    /// the `@classmethod`s: read on the class, bound to it (the other methods are plain functions)
    pub classmethods: &'static [&'static str],
    /// the `@staticmethod`s: read on the class, functions that take no receiver
    pub statics: &'static [&'static str],
}

/// a serializer function: plain (its result replaces the value) or wrap (it gets a `handler` that dumps the
/// value as usual)
pub struct Ser {
    /// the field (`""` for a model serializer)
    pub field: &'static str,
    pub f: MethodFn,
    pub wrap: bool,
    /// it takes an `info` (`SerializationInfo`) last
    pub info: bool,
    /// `when_used`: 0 always, 1 unless-none, 2 json, 3 json-unless-none
    pub when: u8,
    /// a `@field_serializer` (around the field's default: `exclude_defaults` never leaves it out), not one of
    /// the annotation
    pub deco: bool,
}

impl SchemaDesc {
    /// the model's `ValidationError` (`title` = its name)
    pub fn verror(&self, errs: Vec<ErrDetail>) -> Exc {
        let e = Exc::validation_titled(&VALIDATION_ERROR, errs, self.name);
        if self.x.hide_input {
            e.0.attrs.lock().insert("__hide_input__".into(), V::Bool(true));
        }
        e
    }
}

impl SchemaX {
    pub const NONE: SchemaX = SchemaX { ser_by_alias: false, exclude: &[], norepr: &[], frozen: &[], exclude_if: &[], field_ser: &[], model_ser: None, root: false, new_: None, eq_id: false, ntuple: false, hide_input: false, classmethods: &[], statics: &[] };
}

impl FieldDesc {
    /// the key of the field in a dump `by_alias=True` (the translator sets `ser` from `serialization_alias=`, else
    /// `alias=`; a `validation_alias=` alone does not change it)
    pub fn ser_key(&self) -> &'static str {
        self.ser.unwrap_or(self.name)
    }
    pub fn out_key(&self) -> &'static str {
        self.ser_key()
    }
}

/// the value at an alias path of a validated dict, and the path as a `loc`
fn path_lookup(m: &IndexMap<Key, (V, V)>, path: &[PK]) -> Option<V> {
    let PK::S(first) = path.first()? else { return None };
    let mut cur = m.get(&Key::Str(Arc::from(*first)))?.1.clone();
    for step in &path[1..] {
        cur = match (step, &cur) {
            (PK::S(k), V::Dict(d)) => d.lock().get(&Key::Str(Arc::from(*k)))?.1.clone(),
            (PK::I(i), V::List(l)) => {
                let l = l.lock();
                let n = l.len() as i64;
                let at = if *i < 0 { n + i } else { *i };
                if at < 0 || at >= n {
                    return None;
                }
                l[at as usize].clone()
            }
            (PK::I(i), V::Tuple(t)) => {
                let n = t.len() as i64;
                let at = if *i < 0 { n + i } else { *i };
                if at < 0 || at >= n {
                    return None;
                }
                t[at as usize].clone()
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn path_loc(path: &[PK]) -> Vec<V> {
    path.iter().map(|p| match p {
        PK::S(s) => V::str(*s),
        PK::I(i) => V::Int(*i),
    }).collect()
}

impl SchemaDesc {
    pub fn is_private(&self, name: &str) -> bool {
        self.private.iter().any(|(n, _)| *n == name)
    }
    /// a model's `model_post_init`, or a private attribute whose default is computed: a step after validation
    pub fn has_post_init(&self) -> bool {
        !self.dataclass && (self.post_init.is_some() || self.private.iter().any(|(_, d)| matches!(d, Dflt::Dyn(_))))
    }
    /// the private attributes' defaults (a fresh value per instance)
    pub fn private_defaults(&self, extra: &mut IndexMap<String, V>) {
        for (n, d) in self.private {
            match d {
                Dflt::Value(f) | Dflt::Factory(f) => {
                    extra.insert(n.to_string(), f());
                }
                Dflt::Required | Dflt::Dyn(_) | Dflt::Data(_) => {}
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
    /// `env_ignore_empty`
    pub ignore_empty: bool,
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

#[derive(Clone)]
pub struct ErrDetail {
    pub kind: &'static str,
    pub loc: Vec<V>,
    pub msg: String,
    pub input: V,
    pub ctx: Option<Vec<(&'static str, V)>>,
    /// a PydanticCustomError's (EmailStr's, a validator's): no documentation link in `str()`
    pub custom: bool,
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
    error_str_hide(title, errs, false)
}

/// `error_str`; `hide`: the model's `hide_input_in_errors=True` (`[type=...]` alone)
pub fn error_str_hide(title: &str, errs: &[ErrDetail], hide: bool) -> String {
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
        if hide {
            out += &format!("\n  {} [type={}]", e.msg, e.kind);
            if !e.custom {
                out += &format!("\n    For further information visit https://errors.pydantic.dev/{}/v/{}", super::pydantic(), e.kind);
            }
            continue;
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
        if !e.custom {
            out += &format!("\n    For further information visit https://errors.pydantic.dev/{}/v/{}", super::pydantic(), e.kind);
        }
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
    /// field `field` of the instance in `slot` was omitted and has a computed default (`data`: its factory takes
    /// the earlier fields)
    Default { slot: usize, field: usize, f: MethodFn, data: bool },
    /// start of a model that has `@model_validator(mode="after")`: errors counted from here
    ModelStart { slot: usize },
    /// its fields are validated: run the model validators unless an error occurred since ModelStart
    ModelAfter { slot: usize, desc: &'static SchemaDesc, input: V, loc: Vec<V> },
    /// its fields are validated: unless an error occurred since ModelStart, the private attributes' computed
    /// defaults, then `model_post_init(self, None)` (pydantic-core's `post_init`, before the after validators)
    PostInit { slot: usize, desc: &'static SchemaDesc, input: V, loc: Vec<V> },
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
    /// strict mode (`strict=True` of the model config or of a field): only the exact input types
    strict: bool,
    /// JSON mode (`model_validate_json`, `TypeAdapter.validate_json`, a `Json[T]` field): the input parsed by
    /// jiter, validated with pydantic-core's JSON-input rules (its own type errors and exactness)
    json: bool,
}

impl Errs<'_> {
    fn raise(&mut self, x: Exc) {
        self.steps.push(Step::Raise(x));
        self.n += 1;
    }
    fn push(&mut self, kind: &'static str, loc: &[V], msg: impl Into<String>, input: &V, ctx: Option<Vec<(&'static str, V)>>) {
        self.steps.push(Step::Err(ErrDetail { kind, loc: loc.to_vec(), msg: msg.into(), input: input.clone(), ctx, custom: false }));
        self.n += 1;
    }
    fn push_custom(&mut self, kind: &'static str, loc: &[V], msg: impl Into<String>, input: &V, ctx: Option<Vec<(&'static str, V)>>) {
        self.steps.push(Step::Err(ErrDetail { kind, loc: loc.to_vec(), msg: msg.into(), input: input.clone(), ctx, custom: true }));
        self.n += 1;
    }
    fn floor(&mut self, x: u8) {
        self.exact = self.exact.min(x);
    }
    fn sub<'b>(&'b mut self, steps: &'b mut Vec<Step>) -> Errs<'b> {
        Errs { steps, slots: self.slots, n: 0, exact: EXACT, fields_set: None, strict: self.strict, json: self.json }
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
            if c.ge.or(c.gt).or(c.le).or(c.lt).or(c.mul).is_none() {
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
    if let Some(m) = c.mul {
        // pydantic-core: exact for an int, within |x| / 1e9 for a float; checked before the bounds
        let bad = if float {
            let rem = x % m;
            let threshold = x.abs() / 1e9;
            rem.abs() > threshold && (rem - m).abs() > threshold
        } else {
            x % m != 0.0
        };
        if bad {
            let mv = fmt_bound(m);
            let shown = ops::str_(&mv).unwrap_or_default();
            let ctx = if float { V::Float(m) } else { mv };
            e.push("multiple_of", loc, format!("Input should be a multiple of {shown}"), input, Some(vec![("multiple_of", ctx)]));
            return false;
        }
    }
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
    /// built by `model_construct()`: its values were never validated (`dump` checks each against its field)
    pub raw: bool,
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
    validate_mode(cx, input, td, loc, false, false, errs).await
}

thread_local! {
    /// validating without `from_attributes` (a direct `model_validate`, `TypeAdapter.validate_python`, `Model(**kw)`):
    /// a model field given neither a dict nor an instance fails as `model_type`, not `model_attributes_type`
    static PLAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `validate` in python mode (false) or JSON mode (true: an input parsed from JSON); `plain`: pydantic's own
/// default, no `from_attributes` (FastAPI validates with it)
async fn validate_mode(cx: &super::Cx, input: &V, td: &'static TD, loc: &[V], json: bool, plain: bool, errs: &mut Vec<ErrDetail>) -> R<Option<V>> {
    let input = if td_has_before(td) { prepare(cx, input.clone(), td).await? } else { input.clone() };
    let pf = Pf { cx, keys: Default::default() };
    prefetch(&pf, td, &input).await?;
    run_mode(cx, errs, json, |e| {
        PLAIN.with(|p| p.set(plain));
        let got = val(&input, td, loc, e);
        PLAIN.with(|p| p.set(false));
        got
    })
    .await
}

fn td_has_before(td: &TD) -> bool {
    match td {
        TD::Schema(d) => d.has_before,
        TD::List(Some(t)) | TD::Set(Some(t)) | TD::Tuple(Some(t)) | TD::Optional(t) | TD::Len(t, _, _) | TD::DtC(t, _) | TD::Strict(t, _) | TD::StrictTop(t) => td_has_before(t),
        TD::Dict(Some((_, v))) => td_has_before(v),
        TD::TupleFix(ts) => ts.iter().any(|t| td_has_before(t)),
        TD::Enum(d, _) => d.missing.is_some(),
        _ => false,
    }
}

/// A failed `before` validator: ValueError/AssertionError become an error reported where the
/// synchronous validation meets this input; other exceptions propagate.
/// a `PydanticCustomError(type, template, context)` raised by a validator: its own error type, the template
/// with the context's values (a str as is, a bool as an int, others by repr), the context
fn custom_error(x: &Exc, loc: &[V], input: &V) -> Option<ErrDetail> {
    if !x.isinstance(&PYDANTIC_CUSTOM_ERROR) {
        return None;
    }
    let args = x.args();
    let kind = super::types::intern(&ops::str_(args.first()?).ok()?);
    let mut msg = ops::str_(args.get(1)?).ok()?;
    let items: Vec<(V, V)> = match args.get(2) {
        Some(V::Dict(d)) => d.lock().values().cloned().collect(),
        _ => Vec::new(),
    };
    for (k, v) in &items {
        let shown = match v {
            V::Str(s) => s.to_string(),
            V::Bool(b) => (*b as i64).to_string(),
            V::Int(_) | V::Float(_) => ops::str_(v).ok()?,
            _ => ops::repr(v).ok()?,
        };
        msg = msg.replace(&format!("{{{}}}", ops::str_(k).ok()?), &shown);
    }
    let ctx = if items.is_empty() {
        None
    } else {
        Some(items.iter().map(|(k, v)| Ok((super::types::intern(&ops::str_(k)?), v.clone()))).collect::<R<Vec<_>>>().ok()?)
    };
    Some(ErrDetail { kind, loc: loc.to_vec(), msg, input: input.clone(), ctx, custom: true })
}

fn before_error(x: Exc, input: V) -> R {
    if x.isinstance(&PYDANTIC_USE_DEFAULT) {
        return Ok(V::native(Native::UseDefault));
    }
    if x.isinstance(&PYDANTIC_CUSTOM_ERROR) {
        return Ok(V::native(Native::ValErr("", String::new(), input, V::Exc(x))));
    }
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
            (TD::Optional(t), _) | (TD::Strict(t, _), _) | (TD::StrictTop(t), _) => prepare(cx, input, t).await?,
            (TD::Len(t, _, _) | TD::DtC(t, _), _) => prepare(cx, input, t).await?,
            (TD::List(Some(t)) | TD::Tuple(Some(t)) | TD::Set(Some(t)), V::List(_) | V::Tuple(_)) if td_has_before(t) => {
                // the items of a list or tuple input (what a set or tuple validates too)
                let items = match &input {
                    V::List(l) => l.lock().clone(),
                    V::Tuple(x) => x.to_vec(),
                    _ => unreachable!(),
                };
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(prepare(cx, it, t).await?);
                }
                if matches!(input, V::Tuple(_)) { V::tuple(out) } else { V::list(out) }
            }
            (TD::TupleFix(ts), V::List(_) | V::Tuple(_)) if ts.iter().any(|t| td_has_before(t)) => {
                let items = match &input {
                    V::List(l) => l.lock().clone(),
                    V::Tuple(x) => x.to_vec(),
                    _ => unreachable!(),
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, it) in items.into_iter().enumerate() {
                    out.push(match ts.get(i) {
                        Some(t) => prepare(cx, it, t).await?,
                        None => it,
                    });
                }
                if matches!(input, V::Tuple(_)) { V::tuple(out) } else { V::list(out) }
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
                let invalid = || Exc::value_error(format!("{} is not a valid {}", super::ops::repr(&input).unwrap_or_default(), d.name));
                match (d.missing.unwrap())(cx, V::Class(d.class), vec![input.clone()]).await {
                    Ok(m @ V::Enum(x, _)) if std::ptr::eq(x, *d) => m,
                    Ok(V::None) => input,
                    Ok(other) => {
                        let e = Exc::type_error(format!("error in {}._missing_: returned {} instead of None or a valid member", d.name, super::ops::repr(&other)?));
                        // pydantic-core 2.50 (pydantic 2.14) goes through `Enum(value)`, which chains the enum's
                        // ValueError; before, it calls `_missing_` itself and raises its own TypeError
                        return Err(if super::pydantic_before(2, 14) { e } else { super::methods::missing_context(e, invalid()) });
                    }
                    Err(x) if x.isinstance(&VALUE_ERROR) || super::pydantic_before(2, 14) => input,
                    Err(x) => return Err(super::methods::missing_context(x, invalid())),
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
    run_mode(cx, errs, false, check).await
}

/// `run` in python mode (false) or JSON mode (true)
async fn run_mode(cx: &super::Cx, errs: &mut Vec<ErrDetail>, json: bool, check: impl FnOnce(&mut Errs) -> Option<V>) -> R<Option<V>> {
    let (mut steps, mut slots) = (Vec::new(), Vec::new());
    take_fatal();
    let got = check(&mut Errs { steps: &mut steps, slots: &mut slots, n: 0, exact: EXACT, fields_set: None, strict: false, json });
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
    let got = val(input, td, loc, &mut Errs { steps: &mut steps, slots: &mut slots, n: 0, exact: EXACT, fields_set: None, strict: false, json: false });
    for st in steps {
        match st {
            Step::Err(d) => errs.push(d),
            // nowhere to raise from here: reported, never a silently wrong value
            Step::Raise(x) => errs.push(ErrDetail { kind: "py2axum_unsupported", loc: loc.to_vec(), msg: x.message(),
                                                    input: input.clone(), ctx: None, custom: false }),
            _ => {}
        }
    }
    got
}

/// A validator's ValueError/AssertionError as an error detail; other exceptions propagate.
pub fn validator_error(x: Exc, loc: Vec<V>, input: V) -> R<ErrDetail> {
    if let Some(d) = custom_error(&x, &loc, &input) {
        return Ok(d);
    }
    let (kind, word) = if x.isinstance(&ASSERTION_ERROR) {
        ("assertion_error", "Assertion failed")
    } else if x.isinstance(&VALUE_ERROR) {
        ("value_error", "Value error")
    } else {
        return Err(x);
    };
    let msg = format!("{word}, {}", x.message());
    Ok(ErrDetail { kind, loc, msg, input, ctx: Some(vec![("error", V::Exc(x))]), custom: false })
}

/// `inst.field = v` under `validate_assignment=True`, as pydantic-core's `validate_assignment`: the
/// field's `before` validators, its type (nested validators included), its `after` validators
/// (`info.data` = every other field), then the model's `after` validators, which see the new value: one
/// that raises leaves it assigned (error at loc `()`, input the instance). Measured on pydantic 2.13.
pub async fn assign_validated(cx: &super::Cx, inst: &Arc<Inst>, name: &str, v: V) -> R<()> {
    let desc = inst.desc;
    let Some(i) = desc.field_index(name).filter(|_| !desc.frozen && !desc.x.frozen.contains(&name)) else { return inst.set_field(name, v) };
    let fname = desc.fields[i].name;
    let loc = vec![V::str(name)];
    let raw = v.clone();
    let mut cur = v;
    for b in desc.before.iter().rev().filter(|b| b.fields.contains(&fname)) {
        match (b.f)(cx, V::Class(desc.class), vec![cur.clone()]).await {
            Ok(nv) => cur = nv,
            Err(x) => return Err(desc.verror(vec![validator_error(x, loc, cur)?])),
        }
    }
    let pre = cur.clone();
    let mut errs = Vec::new();
    let Some(mut cur) = validate(cx, &pre, desc.fields[i].td, &loc, &mut errs).await? else {
        return Err(desc.verror(errs));
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
                V::Inst(Arc::new(Inst { raw: false,
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
            Err(x) => return Err(desc.verror(vec![validator_error(x, loc, raw)?])),
        }
    }
    inst.vals.lock()[i] = cur;
    inst.set.lock()[i] = true;
    for f in desc.model_after {
        if let Err(x) = f(cx, V::Inst(inst.clone()), vec![]).await {
            return Err(desc.verror(vec![validator_error(x, vec![], V::Inst(inst.clone()))?]));
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
            Step::PostInit { slot, desc, input, loc } => {
                let Some(inst) = &slots[slot] else { continue };
                if errs.len() > starts.get(&slot).copied().unwrap_or(0) {
                    continue;
                }
                for (n, d) in desc.private {
                    if let Dflt::Dyn(f) = d {
                        let v = f(cx, V::None, vec![]).await?;
                        inst.extra.lock().insert(n.to_string(), v);
                    }
                }
                if let Some(f) = desc.post_init {
                    if let Err(x) = f(cx, V::Inst(inst.clone()), vec![V::None]).await {
                        let (kind, word) = if x.isinstance(&ASSERTION_ERROR) {
                            ("assertion_error", "Assertion failed")
                        } else if x.isinstance(&VALUE_ERROR) {
                            ("value_error", "Value error")
                        } else {
                            return Err(x);
                        };
                        let msg = format!("{word}, {}", x.message());
                        errs.push(ErrDetail { kind, loc: loc.clone(), msg, input: input.clone(), ctx: Some(vec![("error", V::Exc(x))]), custom: false });
                    }
                }
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
                            errs.push(ErrDetail { kind, loc: loc.clone(), msg, input: input.clone(), ctx: Some(vec![("error", V::Exc(x))]), custom: false });
                            break;
                        }
                    }
                }
            }
            Step::Default { slot, field, f, data } => {
                if let Some(inst) = &slots[slot] {
                    let args = if data {
                        let vals = inst.vals.lock().clone();
                        vec![V::dict_from(inst.desc.fields[..field].iter().zip(vals).map(|(fd, v)| (V::str(fd.name), v)).collect())?]
                    } else {
                        vec![]
                    };
                    let d = f(cx, V::None, args).await?;
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
                    if let Some(td) = vd.wrap {
                        args.push(wrap_handler(td));
                    }
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
                            V::Inst(Arc::new(Inst { raw: false,
                                desc: &VALINFO,
                                // `context`: FastAPI and model validation pass none
                                vals: Slots::new(vec![data, V::str(name), V::None]),
                                set: Mutex::new(vec![true, true, true]),
                                extra: Slots::new(IndexMap::new()),
                            }))
                        } else {
                            V::native(Native::Kwargs(vec![("values".to_string(), data)]))
                        });
                    }
                    match (vd.f)(cx, V::Class(desc.class), args).await {
                        Ok(v) => cur = v,
                        Err(x) if x.isinstance(&PYDANTIC_USE_DEFAULT) => {
                            // the field's default, as is; no default: pydantic-core's SchemaError
                            cur = match &desc.fields[field].default {
                                Dflt::Value(d) | Dflt::Factory(d) => d(),
                                Dflt::Dyn(d) => d(cx, V::None, vec![]).await?,
                                _ => return Err(Exc::runtime("Uncaught `PydanticUseDefault` exception: the error was raised in a field validator and no default value is available for that field.")),
                            };
                            break;
                        }
                        Err(x) if custom_error(&x, &loc, &raw).is_some() => {
                            errs.push(custom_error(&x, &loc, &raw).unwrap());
                            failed = true;
                            break;
                        }
                        Err(x) if vd.wrap.is_some() && x.isinstance(&VALIDATION_ERROR) && x.0.errors.is_some() => {
                            // a ValidationError out of a wrap validator (its handler's): its errors, under the field
                            for d in x.0.errors.as_ref().into_iter().flatten() {
                                let mut l = loc.clone();
                                l.extend(d.loc.iter().cloned());
                                errs.push(ErrDetail { kind: d.kind, loc: l, msg: d.msg.clone(), input: d.input.clone(), ctx: d.ctx.clone(), custom: false });
                            }
                            failed = true;
                            break;
                        }
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
                            errs.push(ErrDetail { kind, loc: loc.clone(), msg, input: raw.clone(), ctx: Some(vec![("error", V::Exc(x))]), custom: false });
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
    if !e.json {
        return val_in(input, td, loc, e);
    }
    if let Some(out) = json_val(input, td, loc, e) {
        return out;
    }
    // JSON mode: a string is an exact input for the types parsed from strings (`date | str` keeps the date),
    // measured on pydantic 2.13
    let parsed_from_str = matches!(input, V::Str(_))
        && matches!(td, TD::Date | TD::DateTime | TD::Time | TD::Delta | TD::Uuid | TD::UuidV(_) | TD::Path | TD::Bytes(..) | TD::Decimal(_));
    let before = e.exact;
    let out = val_in(input, td, loc, e);
    if parsed_from_str && out.is_some() {
        e.exact = before;
    }
    out
}

/// A construct whose JSON-mode behaviour is not reproduced: refused when reached (a 500, never a wrong value)
fn json_refuse(e: &mut Errs, what: &str) -> Option<Option<V>> {
    FATAL.with(|f| {
        f.borrow_mut().get_or_insert(Exc::runtime(format!("py2axum: JSON-mode validation (model_validate_json, validate_json, Json[T]) of {what} is not supported")));
    });
    e.n += 1;
    Some(None)
}

/// pydantic-core's JSON-mode type errors where they differ from python mode (pydantic 2.13 and 2.14, measured
/// input by input): arrays and objects named as JSON names them; constructs measured to differ otherwise are
/// refused. None: validated as in python mode.
fn json_val(input: &V, td: &'static TD, loc: &[V], e: &mut Errs) -> Option<Option<V>> {
    if e.strict {
        return json_refuse(e, "a strict field");
    }
    match td {
        TD::Strict(_, true) | TD::StrictTop(_) => json_refuse(e, "a strict field"),
        TD::Complex => json_refuse(e, "a complex field"),
        TD::Iter(_) => json_refuse(e, "an Iterable field"),
        TD::Coll(_, Coll::Frozen) if !matches!(input, V::List(_)) => {
            e.push("frozen_set_type", loc, "Input should be a valid array", input, None);
            Some(None)
        }
        TD::Coll(_, Coll::Frozen) => None,
        TD::Coll(..) => json_refuse(e, "a deque, Counter, OrderedDict or Sequence field"),
        TD::List(_) | TD::Set(_) | TD::Tuple(_) | TD::TupleFix(_) if !matches!(input, V::List(_)) => {
            let kind = match td {
                TD::List(_) => "list_type",
                TD::Set(_) => "set_type",
                _ => "tuple_type",
            };
            e.push(kind, loc, "Input should be a valid array", input, None);
            Some(None)
        }
        TD::Dict(_) if !matches!(input, V::Dict(_)) => {
            e.push("dict_type", loc, "Input should be an object", input, None);
            Some(None)
        }
        TD::Tagged(t) if t.func.is_some() => json_refuse(e, "a union discriminated by a function"),
        TD::Tagged(t) if t.custom.is_none() && !matches!(input, V::Dict(_)) => {
            e.push("dict_type", loc, "Input should be an object", input, None);
            Some(None)
        }
        TD::Schema(d) => json_schema(input, d, loc, e),
        _ => None,
    }
}

/// a model (or dataclass) in JSON mode: an object only; None: validated as in python mode
fn json_schema(input: &V, d: &'static SchemaDesc, loc: &[V], e: &mut Errs) -> Option<Option<V>> {
    if d.validators.iter().any(|v| v.wrap.is_some()) {
        return json_refuse(e, "a model with a wrap validator");
    }
    if d.x.root || matches!(input, V::Dict(_)) {
        return None;
    }
    let kind = if d.dataclass { "dataclass_type" } else { "model_type" };
    e.push(kind, loc, "Input should be an object", input, Some(vec![("class_name", V::str(d.name))]));
    Some(None)
}

pub enum JsonIn {
    /// `json_type` / `json_invalid`: (type, msg, ctx), at the location of the input
    Invalid(&'static str, String, Option<Vec<(&'static str, V)>>),
    /// what the runtime cannot hold (an integer beyond 64 bits)
    Raise(Exc),
}

/// The input of a JSON-mode validation (str, bytes or bytearray) parsed as pydantic-core does: jiter decides
/// what is valid (inf and nan allowed, invalid UTF-8 and lone surrogates refused) and how errors read.
pub fn json_input(input: &V) -> Result<V, JsonIn> {
    let bytes: &[u8] = match input {
        V::Str(s) => s.as_bytes(),
        V::Bytes(b) => b,
        _ => return Err(JsonIn::Invalid("json_type", "JSON input should be string, bytes or bytearray".into(), None)),
    };
    if let Err(err) = jiter::JsonValue::parse(bytes, true) {
        let msg = err.description(bytes);
        return Err(JsonIn::Invalid("json_invalid", format!("Invalid JSON: {msg}"), Some(vec![("error", V::str(&msg))])));
    }
    // jiter accepted it, UTF-8 included: the value as the runtime's JSON reader builds it (same grammar)
    let text = std::str::from_utf8(bytes).map_err(|_| JsonIn::Raise(Exc::runtime("py2axum: JSON accepted by jiter is not UTF-8")))?;
    super::pyjson::loads(text).map_err(JsonIn::Raise)
}

/// `Model.model_validate_json(data)`: `data` parsed by jiter, then validated in JSON mode
pub async fn construct_json(cx: &super::Cx, desc: &'static SchemaDesc, data: &V) -> R {
    let input = match json_input(data) {
        Ok(v) => v,
        Err(JsonIn::Invalid(kind, msg, ctx)) => {
            let d = ErrDetail { kind, loc: vec![], msg, input: data.clone(), ctx, custom: false };
            return Err(desc.verror(vec![d]));
        }
        Err(JsonIn::Raise(x)) => return Err(x),
    };
    let mut errs = Vec::new();
    let input = if desc.has_before { prepare_schema(cx, input, desc).await? } else { input };
    let pf = Pf { cx, keys: Default::default() };
    prefetch_schema(&pf, desc, &input).await?;
    // the model itself checked as a field of that model would be (json_val)
    match run_mode(cx, &mut errs, true, |e| json_schema(&input, desc, &[], e).unwrap_or_else(|| schema_val(&input, desc, &[], e))).await? {
        Some(v) => Ok(v),
        None => Err(desc.verror(errs)),
    }
}

fn val_in(input: &V, td: &'static TD, loc: &[V], e: &mut Errs) -> Option<V> {
    if let V::Native(n) = input {
        if let Native::ValErr(kind, msg, raw, exc) = &**n {
            if let Some(d) = match exc { V::Exc(x) => custom_error(x, loc, raw), _ => None } {
                e.steps.push(Step::Err(d));
                e.n += 1;
                return None;
            }
            e.push(kind, loc, msg.clone(), raw, Some(vec![("error", exc.clone())]));
            return None;
        }
    }
    if let V::Enum(d, i) = input {
        if let Some(out) = enum_scalar(input, d, *i, td, loc, e) {
            return out;
        }
    }
    if e.strict {
        if let Some(out) = strict_refuse(input, td, loc, e) {
            return out;
        }
    }
    match td {
        TD::Strict(inner, on) => {
            let prev = std::mem::replace(&mut e.strict, *on);
            let out = val(input, inner, loc, e);
            e.strict = prev;
            out
        }
        TD::StrictTop(inner) => {
            let base = match inner {
                TD::Optional(t) if input.is_none() => return Some(V::None),
                TD::Optional(t) => t,
                t => t,
            };
            if !matches!(base.bare(), TD::List(_) | TD::Tuple(_) | TD::TupleFix(_) | TD::Set(_) | TD::Dict(_) | TD::Coll(..)) {
                // a scalar: strict all the way (a URL's syntax violations, ...)
                let prev = std::mem::replace(&mut e.strict, true);
                let out = val(input, base, loc, e);
                e.strict = prev;
                return out;
            }
            if !e.strict {
                if let Some(out) = strict_refuse(input, base, loc, e) {
                    return out;
                }
            }
            val(input, base, loc, e)
        }
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
        TD::Subclass(name, cls) => {
            let ok = match (input, cls) {
                (V::Class(c), Some(k)) => c.is_subclass(k),
                (V::Class(c), None) if *name == "BaseModel" => matches!(c.kind, ClassKind::Schema(d) if !d.open && !d.dataclass),
                // a plain class of the project, known by name: itself (it has no bases)
                (V::Class(c), None) => c.name == *name && matches!(c.kind, ClassKind::Schema(d) if d.open),
                _ => false,
            };
            if ok {
                return Some(input.clone());
            }
            e.push("is_subclass_of", loc, format!("Input should be a subclass of {name}"), input, Some(vec![("class", V::str(*name))]));
            None
        }
        TD::Tagged(t) => {
            let found = match (t.func, input) {
                (Some(f), _) => match sync_call(f, V::None, vec![input.clone()], "a discriminator function") {
                    Ok(V::None) => Some(None),
                    Ok(v) => Some(Some(v)),
                    Err(x) => {
                        FATAL.with(|f| {
                            f.borrow_mut().get_or_insert(x);
                        });
                        return None;
                    }
                },
                (None, V::Dict(d)) => Some(d.lock().get(&Key::Str(Arc::from(t.key))).map(|(_, v)| v.clone())),
                (None, V::Inst(_) | V::Obj(_)) => Some(attr_value(input, t.attr)),
                _ => None,
            };
            let disc = if t.func.is_some() { t.key.to_string() } else { format!("'{}'", t.key) };
            let tag_ok = matches!(&found, Some(Some(V::Str(s))) if t.tags.iter().any(|(k, _)| *k == &**s));
            if let (Some((kind, msg, ctx)), false) = (t.custom, tag_ok) {
                e.push(kind, loc, msg.to_string(), input, Some(ctx.iter().map(|(k, v)| (*k, V::str(*v))).collect()));
                return None;
            }
            match found {
                None => {
                    e.push("model_attributes_type", loc, "Input should be a valid dictionary or object to extract fields from", input, None);
                    None
                }
                Some(None) => {
                    e.push("union_tag_not_found", loc, format!("Unable to extract tag using discriminator {disc}"), input, Some(vec![("discriminator", V::str(&disc))]));
                    None
                }
                Some(Some(tag)) => {
                    let member = match &tag {
                        V::Str(s) => t.tags.iter().find(|(k, _)| *k == &**s),
                        _ => None,
                    };
                    match member {
                        Some((k, td)) => val(input, td, &with(loc, V::str(*k)), e),
                        None => {
                            let shown = ops::str_(&tag).unwrap_or_default();
                            let expected = t.tags.iter().map(|(k, _)| format!("'{k}'")).collect::<Vec<_>>().join(", ");
                            e.push("union_tag_invalid", loc, format!("Input tag '{shown}' found using {disc} does not match any of the expected tags: {expected}"), input,
                                Some(vec![("discriminator", V::str(&disc)), ("tag", V::str(&shown)), ("expected_tags", V::str(&expected))]));
                            None
                        }
                    }
                }
            }
        }
        TD::UnionLtr(choices) => {
            let (mut failed, mut nfailed) = (Vec::new(), 0);
            for c in choices.iter() {
                let mut tmp = Vec::new();
                let mut sub = e.sub(&mut tmp);
                let got = val(input, c, &with(loc, V::str(label(c))), &mut sub);
                let (ex, fs, n) = (sub.exact, sub.fields_set, sub.n);
                match got {
                    Some(v) => {
                        e.steps.extend(tmp);
                        e.floor(ex);
                        if fs.is_some() {
                            e.fields_set = fs;
                        }
                        return Some(v);
                    }
                    None => {
                        failed.extend(tmp);
                        nfailed += n;
                    }
                }
            }
            e.steps.extend(failed);
            e.n += nfailed;
            None
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
            V::Decimal(d) if d.is_integral() => match d.to_i64() {
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
                    if !f.is_finite() {
                        e.push("finite_number", loc, "Input should be a finite number", input, None);
                        return None;
                    }
                    if f.fract() == 0.0 {
                        Some(*f as i64)
                    } else {
                        e.push("int_from_float", loc, "Input should be a valid integer, got a number with a fractional part", input, None);
                        return None;
                    }
                }
                V::Decimal(d) => {
                    if d.is_integral() {
                        match d.to_i64() {
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
                Some(f) if c.finite && !f.is_finite() => {
                    e.push("finite_number", loc, "Input should be a finite number", input, None);
                    None
                }
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
                // pydantic-core: strip, then the ASCII check, the lengths and the pattern, then the case
                let s: String = if c.strip { s.trim().to_string() } else { s.to_string() };
                if c.ascii && !s.is_ascii() {
                    e.push("string_not_ascii", loc, "String should contain only ASCII characters", input, None);
                    return None;
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
                let s = if c.lower { s.to_lowercase() } else if c.upper { s.to_uppercase() } else { s };
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
            // JSON mode says "duration" and refuses a bool
            let word = if e.json { "duration" } else { "timedelta" };
            let parsed = match input {
                V::Delta(d) => return Some(V::Delta(*d)),
                V::Str(s) => dt::parse_duration(s),
                V::Bool(b) if !e.json => Ok(chrono::TimeDelta::seconds(*b as i64)),
                V::Int(i) => dt::duration_from_seconds(*i as f64).and_then(|_| Ok(chrono::TimeDelta::seconds(*i))),
                V::Float(f) => dt::duration_from_seconds(*f),
                _ => {
                    e.push("time_delta_type", loc, format!("Input should be a valid {word}"), input, None);
                    return None;
                }
            };
            match parsed {
                Ok(d) => Some(V::Delta(d)),
                Err(err) => {
                    e.push("time_delta_parsing", loc, format!("Input should be a valid {word}, {}", err.text()), input, Some(vec![("error", V::str(err.text()))]));
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
        TD::DtC(inner, c) => {
            let v = val(input, inner, loc, e)?;
            dt_checks(v, c, loc, input, e)
        }
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
                    V::Set(s) | V::Frozen(s) => s.lock().values().cloned().collect(),
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
                    V::Set(s) | V::Frozen(s) => Some(s.lock().len()),
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
                V::Set(s) | V::Frozen(s) => s.lock().len(),
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
        TD::Coll(inner, kind) => {
            let seq_like = matches!(input, V::List(_) | V::Tuple(_) | V::Set(_) | V::Frozen(_));
            let deque = matches!(input, V::Native(n) if matches!(&**n, Native::Deque(_)));
            let ok = match kind {
                Coll::Deque | Coll::Frozen => seq_like || deque,
                Coll::Counter | Coll::Ordered => matches!(input, V::Dict(_)),
                Coll::Seq => matches!(input, V::List(_) | V::Tuple(_)) || deque,
            };
            if !ok {
                // pydantic-core < 2.50 (pydantic 2.13): a deque is a list, a Counter or OrderedDict a dict
                let old = super::pydantic_before(2, 14);
                let (k, msg, ctx): (&'static str, String, Option<Vec<(&'static str, V)>>) = match kind {
                    Coll::Deque if old => ("list_type", "Input should be a valid list".into(), None),
                    Coll::Counter | Coll::Ordered if old => ("dict_type", "Input should be a valid dictionary".into(), None),
                    Coll::Deque => ("deque_type", "Input should be a valid deque".into(), None),
                    Coll::Frozen => ("frozen_set_type", "Input should be a valid frozenset".into(), None),
                    Coll::Counter => ("counter_type", "Input should be a valid Counter".into(), None),
                    Coll::Ordered => ("ordered_dict_type", "Input should be a valid OrderedDict".into(), None),
                    Coll::Seq if matches!(input, V::Str(_)) => ("sequence_str", "'str' instances are not allowed as a Sequence value".into(), Some(vec![("type_name", V::str("str"))])),
                    Coll::Seq => ("is_instance_of", "Input should be an instance of Sequence".into(), Some(vec![("class", V::str("Sequence"))])),
                };
                e.push(k, loc, msg, input, ctx);
                return None;
            }
            let plain = match input {
                V::Native(n) => match &**n {
                    Native::Deque(d) => V::list(d.items.lock().iter().cloned().collect()),
                    _ => input.clone(),
                },
                _ => input.clone(),
            };
            let out = val(&plain, inner, loc, e)?;
            Some(match (kind, out) {
                (Coll::Deque, V::List(l)) => {
                    let items = l.lock().iter().cloned().collect();
                    V::native(Native::Deque(Arc::new(super::deque::Deque { items: Mutex::new(items), maxlen: None })))
                }
                (Coll::Counter, d @ V::Dict(_)) => ops::defaultdict("Counter", d).ok()?,
                (Coll::Seq, V::List(l)) if matches!(input, V::Tuple(_)) => V::tuple(l.lock().clone()),
                (Coll::Frozen, V::Set(m)) => V::Frozen(m),
                (_, v) => v,
            })
        }
        TD::Json(inner) => {
            let parsed = match json_input(input) {
                Ok(v) => v,
                Err(JsonIn::Invalid(kind, msg, ctx)) => {
                    e.push(kind, loc, msg, input, ctx);
                    return None;
                }
                Err(JsonIn::Raise(x)) => {
                    e.raise(x);
                    return None;
                }
            };
            // the inner type in the mode of the validation holding it (measured: python mode keeps "valid list")
            match inner {
                Some(t) => val(&parsed, t, loc, e),
                None => Some(parsed),
            }
        }
        TD::Iter(item) => {
            // `iter(input)`: a str gives its characters, bytes their ints, a dict its keys, a model its fields
            // a ValidatorIterator revalidated (a response model of the same class): pydantic wraps it lazily, its
            // items checked once more when reached; kept as it is (the same item type gives the same results)
            if let V::Native(n) = input {
                if matches!(&**n, Native::ValIter(_)) {
                    return Some(input.clone());
                }
            }
            let items = match input {
                V::Int(_) | V::Float(_) | V::Bool(_) | V::None => None,
                _ => ops::iter(input).ok(),
            };
            match items {
                Some(items) => Some(V::native(Native::ValIter(Arc::new(ValIter {
                    items: Mutex::new(items.into()),
                    index: Mutex::new(0),
                    td: item,
                    strict: e.strict,
                })))),
                None => {
                    e.push("iterable_type", loc, "Input should be iterable", input, None);
                    None
                }
            }
        }
        TD::TupleFix(types) => {
            if !matches!(input, V::Tuple(_)) {
                e.floor(LAX);
            }
            let items = match input {
                V::List(l) => l.lock().clone(),
                V::Tuple(t) => t.to_vec(),
                V::Set(s) | V::Frozen(s) => s.lock().values().cloned().collect(),
                _ => {
                    e.push("tuple_type", loc, "Input should be a valid tuple", input, None);
                    return None;
                }
            };
            let mx = types.len();
            if items.len() > mx {
                // pydantic-core: that error alone, the items are not reported
                e.push("too_long", loc, format!("Tuple should have at most {mx} {} after validation, not {}", items_word(mx), items.len()), input,
                    Some(vec![("field_type", V::str("Tuple")), ("max_length", V::Int(mx as i64)), ("actual_length", V::Int(items.len() as i64))]));
                return None;
            }
            let mut out = Vec::with_capacity(mx);
            let mut ok = true;
            for (i, t) in types.iter().enumerate() {
                let iloc = with(loc, V::Int(i as i64));
                match items.get(i) {
                    Some(it) => match val(it, t, &iloc, e) {
                        Some(v) => out.push(v),
                        None => ok = false,
                    },
                    None => {
                        e.push("missing", &iloc, "Field required", input, None);
                        ok = false;
                    }
                }
            }
            if ok { Some(V::tuple(out)) } else { None }
        }
        TD::List(item) | TD::Set(item) | TD::Tuple(item) => {
            let (kind, msg) = match td {
                TD::List(_) => ("list_type", "Input should be a valid list"),
                TD::Set(_) => ("set_type", "Input should be a valid set"),
                _ => ("tuple_type", "Input should be a valid tuple"),
            };
            if !matches!((td, input), (TD::List(_), V::List(_)) | (TD::Set(_), V::Set(_) | V::Frozen(_)) | (TD::Tuple(_), V::Tuple(_))) {
                e.floor(LAX);
            }
            let items = match input {
                V::List(l) => l.lock().clone(),
                V::Tuple(t) => t.to_vec(),
                V::Set(s) | V::Frozen(s) => s.lock().values().cloned().collect(),
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
        TD::Path => path_val(input, loc, e),
        TD::UuidV(ver) => {
            let out = uuid_val(input, loc, e)?;
            match &out {
                V::Native(n) if matches!(&**n, Native::Uuid(u) if ((u >> 76) & 0xf) as u8 != *ver) => {
                    e.push("uuid_version", loc, format!("UUID version {ver} expected"), input, Some(vec![("expected_version", V::Int(*ver as i64))]));
                    None
                }
                _ => Some(out),
            }
        }
        TD::Complex => {
            let msg = "Input should be a valid python complex object, a number, or a valid complex string following the rules at https://docs.python.org/3/library/functions.html#complex";
            let got = match input {
                // pydantic 2.12 refused a bool
                V::Bool(_) if super::pydantic_before(2, 13) => None,
                V::Bool(b) => Some((*b as i64 as f64, 0.0)),
                V::Int(i) => Some((*i as f64, 0.0)),
                V::Float(f) => Some((*f, 0.0)),
                V::Native(n) if matches!(&**n, Native::Complex(..)) => return Some(input.clone()),
                V::Str(s) => match super::complex::parse(s) {
                    Ok(c) => c,
                    Err(()) => {
                        FATAL.with(|f| {
                            f.borrow_mut().get_or_insert(Exc::runtime("py2axum: a complex written with non-ASCII digits is not supported"));
                        });
                        e.n += 1;
                        return None;
                    }
                },
                V::Decimal(_) => {
                    FATAL.with(|f| {
                        f.borrow_mut().get_or_insert(Exc::runtime("py2axum: a Decimal given to a complex field is not supported"));
                    });
                    e.n += 1;
                    return None;
                }
                _ => None,
            };
            match got {
                Some((re, im)) => Some(super::complex::value(re, im)),
                None => {
                    e.push("complex_type", loc, msg, input, None);
                    None
                }
            }
        }
        TD::Fraction => {
            use super::fraction as fr;
            let got = match input {
                V::Bool(b) => fr::Fraction::new((*b as i64).into(), 1.into()),
                V::Int(i) => fr::Fraction::new((*i).into(), 1.into()),
                V::Float(f) => fr::from_f64(*f),
                V::Native(n) if matches!(&**n, Native::Fraction(_)) => return Some(input.clone()),
                V::Decimal(_) | V::Str(_) => match fr::parse(&ops::str_(input).ok()?) {
                    Ok(f) => f,
                    // pydantic-core: the ValueError of `Fraction(text)` is `fraction_parsing`
                    Err(fr::Bad::Limit(_)) => None,
                    Err(fr::Bad::Unsupported) => {
                        FATAL.with(|f| {
                            f.borrow_mut().get_or_insert(Exc::runtime("py2axum: a Fraction with non-ASCII digits or an exponent past 100000 is not supported"));
                        });
                        e.n += 1;
                        return None;
                    }
                },
                _ if super::pydantic_before(2, 14) => {
                    // before pydantic 2.14 the TypeError of `Fraction(x)` propagated
                    FATAL.with(|f| {
                        f.borrow_mut().get_or_insert(Exc::type_error("argument should be a string or a Rational instance"));
                    });
                    e.n += 1;
                    return None;
                }
                _ => {
                    e.push("fraction_type", loc, "Fraction input should be an integer, float, string or Fraction object", input, None);
                    return None;
                }
            };
            // before pydantic 2.14 the ZeroDivisionError of `Fraction('n/0')` propagated
            if got.is_none() && super::pydantic_before(2, 14) {
                if let V::Str(s) = input {
                    if s.split('/').nth(1).is_some_and(|d| !d.trim().is_empty() && d.trim().chars().all(|c| c == '0' || c == '_'))
                        && fr::parse(&s.replacen('/', "/1", 1)).ok().flatten().is_some()
                    {
                        FATAL.with(|f| {
                            f.borrow_mut().get_or_insert(Exc::msg(&ZERO_DIVISION_ERROR, format!("Fraction({}, 0)", s.split('/').next().unwrap_or("").trim())));
                        });
                        e.n += 1;
                        return None;
                    }
                }
            }
            match got {
                Some(f) => Some(fr::value(f)),
                None => {
                    e.push("fraction_parsing", loc, "Input is not a valid fraction", input, None);
                    None
                }
            }
        }
        TD::Callable => {
            if matches!(super::methods::b_callable(std::slice::from_ref(input)), Ok(V::Bool(true))) {
                return Some(input.clone());
            }
            e.push("callable_type", loc, "Input should be callable", input, None);
            None
        }
        TD::Ellipsis if super::pydantic_before(2, 14) => {
            e.push("is_instance_of", loc, "Input should be an instance of ellipsis", input, Some(vec![("class", V::str("ellipsis"))]));
            None
        }
        TD::Ellipsis => {
            e.push("ellipsis_error", loc, "Input should be the 'Ellipsis' literal", input, None);
            None
        }
        TD::MissingSentinel => {
            e.push("missing_sentinel_error", loc, "Input should be the 'MISSING' sentinel", input, None);
            None
        }
        TD::Instance(name) => {
            if matches!(input, V::Inst(i) if i.desc.open && i.desc.name == *name) {
                return Some(input.clone());
            }
            // a library class the binary only stores (`OAuth2PasswordBearer`): its name
            if matches!(input, V::Native(n) if matches!(&**n, Native::LibInstance(_, c) if *c == *name)) {
                return Some(input.clone());
            }
            e.push("is_instance_of", loc, format!("Input should be an instance of {name}"), input, Some(vec![("class", V::str(*name))]));
            None
        }
        TD::Secret => match input {
            V::Str(s) => Some(V::native(Native::Secret(s.clone()))),
            V::Native(n) if matches!(&**n, Native::Secret(_)) => Some(input.clone()),
            _ => {
                e.push("string_type", loc, "Input should be a valid string", input, None);
                None
            }
        },
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
                    // pydantic raises it as a PydanticCustomError: no documentation link
                    e.push_custom("value_error", loc, format!("value is not a valid email address: {reason}"), input, Some(vec![("reason", V::str(&reason))]));
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
        TD::Int(c) => if c.ge.or(c.gt).or(c.le).or(c.lt).or(c.mul).is_some() { "constrained-int" } else { "int" }.into(),
        TD::Float(c) => if c.ge.or(c.gt).or(c.le).or(c.lt).or(c.mul).is_some() { "constrained-float" } else { "float" }.into(),
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
        TD::Len(t, _, _) | TD::DtC(t, _) => label(t),
        TD::Strict(t, _) | TD::StrictTop(t) => label(t),
        TD::Subclass(name, _) => format!("is-subclass[{name}]"),
        TD::Coll(t, _) => label(t),
        TD::Iter(t) => format!("generator[{}]", label(t)),
        TD::Json(t) => format!("json[{}]", t.map(label).unwrap_or_else(|| "any".into())),
        TD::TupleFix(ts) => format!("tuple[{}]", ts.iter().map(|t| label(t)).collect::<Vec<_>>().join(",")),
        TD::Tagged(t) => format!("tagged-union[{}]", t.tags.iter().map(|(_, c)| label(c)).collect::<Vec<_>>().join(",")),
        TD::Union(cs) | TD::UnionLtr(cs) => format!("union[{}]", cs.iter().map(|c| label(c)).collect::<Vec<_>>().join(",")),
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
        TD::Url(s) => if s.multi { "multi-host-url".into() } else { format!("url[{}]", s.name) },
        TD::Uuid | TD::UuidV(_) => "uuid".into(),
        TD::Path => PATH_LABEL.into(),
        TD::Secret => "function-wrap[SecretStr]".into(),
        TD::Callable => "callable".into(),
        TD::Fraction => "fraction".into(),
        TD::Complex => "complex".into(),
        TD::Ellipsis => "literal[Ellipsis]".into(),
        TD::MissingSentinel => "missing-sentinel".into(),
        TD::Instance(n) => format!("is-instance[{n}]"),
        TD::Bytes(..) => "bytes".into(),
    }
}

/// pydantic-core's uuid validator: a UUID instance, else a str (`Uuid::parse_str` of the uuid crate it
/// pins, messages included), else bytes (as text, then as 16 raw bytes)
/// `pathlib.Path`: a Path as it is, a str as the Path it names, nothing else (`path_type`)
fn path_val(input: &V, loc: &[V], e: &mut Errs) -> Option<V> {
    match input {
        V::Native(n) if matches!(&**n, Native::Path(_)) => Some(input.clone()),
        V::Str(s) => {
            e.floor(LAX);
            Some(super::pathio::path(s))
        }
        _ => {
            // 3.13 alone names the class by its private module (`pathlib._local.Path`, pydantic prints `repr(Path)`)
            let cls = if super::python() == (3, 13) { "pathlib._local.Path" } else { "pathlib.Path" };
            e.push("path_type", loc, &format!("Input is not a valid path for <class '{cls}'>"), input, None);
            None
        }
    }
}

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
        // JSON mode: the float as Rust writes it (`1.0` -> Decimal('1'), `1e2` -> Decimal('100'))
        V::Float(f) if e.json => Dec::parse(&format!("{f}")).ok()?,
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
        // digits counted without printing the number, trailing zeros stripped by doubling steps (one division
        // per zero, or printing, is quadratic: a megabyte of digits took seconds)
        use super::decimal::{digits as ndigits, strip_zeros};
        let (mut coeff, mut exp) = (d.coeff.clone(), d.exp);
        if num_traits::Zero::is_zero(&coeff) {
            exp = 0;
        } else {
            // pydantic-core < 2.50 (pydantic 2.13) normalized with `Decimal.normalize()` in the default context:
            // rounded to 28 digits (ROUND_HALF_EVEN) and below Etiny to zero, so a longer or tinier value can pass
            // (an Overflow there leaves the value as written)
            if super::pydantic_before(2, 14) {
                if let Ok(f) = d.clone().fix() {
                    (coeff, exp) = (f.coeff, f.exp);
                }
            }
            if num_traits::Zero::is_zero(&coeff) {
                exp = 0;
            } else {
                let (c, k) = strip_zeros(&coeff);
                coeff = c;
                exp += k;
            }
        }
        let count = |nd: i64, exp: i64| if exp >= 0 { (nd.saturating_add(exp), 0) } else { (nd.max(-exp), -exp) };
        let (digits, decimals) = count(ndigits(&coeff) as i64, exp);
        // a bound is exceeded when both the value as written and the normalized one exceed it
        let (raw_digits, raw_decimals) = count(ndigits(&d.coeff) as i64, d.exp);
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

/// Strict mode (python mode, pydantic-core 2.50, measured on pydantic 2.14): an input of another type than the
/// field's is refused at once with the type's own error. None: the input may go on to the usual validation.
const PATH_LABEL: &str = "lax-or-strict[lax=union[json-or-python[json=function-after[path_validator(), str],python=is-instance[Path]],function-after[path_validator(), str]],strict=json-or-python[json=function-after[path_validator(), str],python=is-instance[Path]]]";

fn strict_refuse(input: &V, td: &'static TD, loc: &[V], e: &mut Errs) -> Option<Option<V>> {
    let (ok, kind, msg): (bool, &'static str, &str) = match td.bare() {
        TD::Int(_) => (matches!(input, V::Int(_)), "int_type", "Input should be a valid integer"),
        TD::Float(_) => (matches!(input, V::Int(_) | V::Float(_)), "float_type", "Input should be a valid number"),
        TD::Str(_) => (matches!(input, V::Str(_)), "string_type", "Input should be a valid string"),
        TD::Bool => (matches!(input, V::Bool(_)), "bool_type", "Input should be a valid boolean"),
        TD::Date => (matches!(input, V::Date(_)), "date_type", "Input should be a valid date"),
        TD::DateTime => (matches!(input, V::DateTime(_)), "datetime_type", "Input should be a valid datetime"),
        TD::Time => (matches!(input, V::Time(_)), "time_type", "Input should be a valid time"),
        TD::Delta => (matches!(input, V::Delta(_)), "time_delta_type", "Input should be a valid timedelta"),
        TD::Bytes(..) => (matches!(input, V::Bytes(_)), "bytes_type", "Input should be a valid bytes"),
        TD::List(_) => (matches!(input, V::List(_)), "list_type", "Input should be a valid list"),
        TD::Tuple(_) | TD::TupleFix(_) => (matches!(input, V::Tuple(_)), "tuple_type", "Input should be a valid tuple"),
        TD::Set(_) => (matches!(input, V::Set(_)), "set_type", "Input should be a valid set"),
        TD::Dict(_) => (matches!(input, V::Dict(_)), "dict_type", "Input should be a valid dictionary"),
        TD::Decimal(_) | TD::Uuid | TD::UuidV(_) | TD::Path | TD::Enum(..) => {
            let (ok, class) = match td.bare() {
                TD::Path => (matches!(input, V::Native(n) if matches!(&**n, Native::Path(_))), "Path"),
                TD::Decimal(_) => (matches!(input, V::Decimal(_)), "Decimal"),
                TD::Uuid | TD::UuidV(_) => (matches!(input, V::Native(n) if matches!(&**n, Native::Uuid(_))), "UUID"),
                TD::Enum(d, _) => (matches!(input, V::Enum(x, _) if std::ptr::eq(*x, *d)), d.name),
                _ => unreachable!(),
            };
            if ok {
                return None;
            }
            e.push("is_instance_of", loc, format!("Input should be an instance of {class}"), input, Some(vec![("class", V::str(class))]));
            return Some(None);
        }
        _ => return None,
    };
    if ok {
        return None;
    }
    e.push(kind, loc, msg, input, None);
    Some(None)
}

/// a model's fields follow its own config, not the strict mode of the field holding it
fn schema_val(input: &V, desc: &'static SchemaDesc, loc: &[V], e: &mut Errs) -> Option<V> {
    let prev = std::mem::replace(&mut e.strict, false);
    let out = schema_val_in(input, desc, loc, e);
    e.strict = prev;
    out
}

fn schema_val_in(input: &V, desc: &'static SchemaDesc, loc: &[V], e: &mut Errs) -> Option<V> {
    if desc.x.root && !matches!(input, V::Inst(i) if std::ptr::eq(i.desc, desc)) {
        let v = val(input, desc.fields[0].td, loc, e)?;
        return Some(V::Inst(Arc::new(Inst { raw: false, desc, vals: Slots::new(vec![v]), set: Mutex::new(vec![true]), extra: Slots::new(IndexMap::new()) })));
    }
    if let V::Inst(i) = input {
        if std::ptr::eq(i.desc, desc) || (desc.dataclass && std::ptr::eq(i.desc.class, desc.class)) {
            // exact, no fields count: wins a union at once (a dataclass instance: not revalidated, like pydantic)
            return Some(input.clone());
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
        _ if desc.dataclass => {
            e.push("dataclass_type", loc, format!("Input should be a dictionary or an instance of {}", desc.name), input,
                   Some(vec![("class_name", V::str(desc.name))]));
            return None;
        }
        _ if e.json || (PLAIN.with(|p| p.get()) && !desc.from_attributes) => {
            // JSON mode has no objects to read attributes from: the input is not an object
            let msg = if e.json { "Input should be an object".to_string() } else { format!("Input should be a valid dictionary or instance of {}", desc.name) };
            e.push("model_type", loc, msg, input, Some(vec![("class_name", V::str(desc.name))]));
            return None;
        }
        _ => {
            e.push("model_attributes_type", loc, "Input should be a valid dictionary or object to extract fields from", input, None);
            return None;
        }
    };
    let start = e.n;
    // a slot only for the instances that have validators, computed defaults or a post-init step
    let post = desc.has_post_init();
    let slot = if !desc.validators.is_empty() || !desc.model_after.is_empty() || post || desc.fields.iter().any(|f| matches!(f.default, Dflt::Dyn(_) | Dflt::Data(_))) {
        e.slots.push(None);
        Some(e.slots.len() - 1)
    } else {
        None
    };
    if let (Some(slot), true) = (slot, !desc.model_after.is_empty() || post) {
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
    let steps_start = e.steps.len();
    // the input keys the fields consumed (the others are extras)
    let mut used_keys: Vec<&'static str> = Vec::with_capacity(desc.fields.len());
    for (fi, f) in desc.fields.iter().enumerate() {
        let n_before = e.n;
        let key = f.alias.unwrap_or(f.name);
        let by_name = desc.populate_by_name && (f.alias.is_some() || !f.paths.is_empty());
        // (value, the loc it was found at: Pydantic reports errors there — a path's, else the input key it consumed)
        let found: Option<(V, Option<Vec<V>>, &'static str)> = match &src {
            Src::Map(m) if !f.paths.is_empty() => f.paths.iter()
                .find_map(|p| path_lookup(m, p).map(|v| (v, Some(path_loc(p)), match p[0] { PK::S(k) => k, PK::I(_) => "" })))
                .or_else(|| if by_name { m.get(&Key::Str(Arc::from(f.name))).map(|(_, v)| (v.clone(), None, f.name)) } else { None }),
            Src::Map(m) => m
                .get(&Key::Str(Arc::from(key)))
                .map(|(_, v)| (v.clone(), None, key))
                .or_else(|| if by_name { m.get(&Key::Str(Arc::from(f.name))).map(|(_, v)| (v.clone(), None, f.name)) } else { None }),
            Src::Attrs(o) => match attr_lookup(o, key) {
                Ok(Some(v)) => Some((v, None, key)),
                Ok(None) if by_name => attr_value(o, f.name).map(|v| (v, None, f.name)),
                Ok(None) => None,
                Err(x) => {
                    // pydantic-core: an exception other than AttributeError from `getattr` is this field's error
                    let mut l = loc.to_vec();
                    l.push(V::str(key));
                    let m = format!("{}: {}", x.0.class.name, x.message());
                    e.push("get_attribute_error", &l, format!("Error extracting attribute: {m}"), o, Some(vec![("error", V::str(&m))]));
                    set.push(false);
                    continue;
                }
            },
        };
        let missing_loc = || -> Vec<V> {
            let mut l = loc.to_vec();
            l.extend(f.paths.first().map(|p| path_loc(p)).unwrap_or_else(|| vec![V::str(key)]));
            l
        };
        // a before validator raised PydanticUseDefault: the default, the field still set
        let use_default = matches!(&found, Some((V::Native(n), _, _)) if matches!(&**n, Native::UseDefault));
        let found = if use_default { None } else { found };
        if use_default && matches!(f.default, Dflt::Required) {
            FATAL.with(|x| {
                x.borrow_mut().get_or_insert(Exc::runtime("Uncaught `PydanticUseDefault` exception: the error was raised in a field validator and no default value is available for that field."));
            });
            e.n += 1;
        }
        let (got, used) = match found {
            Some((v, l, k)) => {
                used_keys.push(k);
                // one allocation per field read (the loc is built even when nothing fails: val() takes it)
                let mut full = Vec::with_capacity(loc.len() + l.as_ref().map_or(1, Vec::len));
                full.extend_from_slice(loc);
                match l {
                    Some(l) => full.extend(l),
                    None => full.push(V::str(k)),
                }
                (Some(v), full)
            }
            None => (None, missing_loc()),
        };
        match got {
            Some(v) => {
                set.push(true);
                let floc = used;
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
                        e.push("missing", &used, "Field required", input, None);
                        vals.push(V::None);
                    }
                    Dflt::Value(d) | Dflt::Factory(d) if f.validate_default => {
                        let floc = used.clone();
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
                            e.steps.push(Step::Default { slot, field: fi, f: *f, data: false });
                        }
                        vals.push(V::Unbound)
                    }
                    // after an error of an earlier field the factory is not called (before pydantic 2.14, a `missing`
                    // error does not count: it is called with the fields validated so far, and what it raises
                    // propagates)
                    Dflt::Data(g) if e.n > start => {
                        let counts = !super::pydantic_before(2, 14)
                            || e.steps[steps_start..].iter().any(|s| matches!(s, Step::Err(d) if d.kind != "missing"));
                        if counts {
                            e.push("default_factory_not_called", &used, "The default factory uses validated data, but at least one validation error occurred", &V::None, None);
                        } else {
                            let partial: Vec<(V, V)> = desc.fields[..fi].iter().zip(&vals).zip(&ok)
                                .filter(|(_, ok)| **ok).map(|((fd, v), _)| (V::str(fd.name), v.clone())).collect();
                            let res = V::dict_from(partial).and_then(|d| sync_call(*g, V::None, vec![d], "a default_factory"));
                            if let Err(x) = res {
                                FATAL.with(|f| {
                                    f.borrow_mut().get_or_insert(x);
                                });
                                e.n += 1;
                            }
                        }
                        vals.push(V::None)
                    }
                    Dflt::Data(f) => {
                        if let Some(slot) = slot {
                            e.steps.push(Step::Default { slot, field: fi, f: *f, data: true });
                        }
                        vals.push(V::Unbound)
                    }
                }
            }
        }
        if use_default {
            *set.last_mut().unwrap() = true;
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
                if used_keys.contains(&name.as_str()) {
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
    if let (Some(slot), true) = (slot, post) {
        e.steps.push(Step::PostInit { slot, desc, input: input.clone(), loc: loc.to_vec() });
    }
    if let (Some(slot), false) = (slot, desc.model_after.is_empty()) {
        e.steps.push(Step::ModelAfter { slot, desc, input: input.clone(), loc: loc.to_vec() });
    }
    e.fields_set = Some(set.iter().filter(|x| **x).count());
    let mut extra = extra;
    desc.private_defaults(&mut extra);
    let inst = Arc::new(Inst { raw: false, desc, vals: Slots::new(vals), set: Mutex::new(set), extra: Slots::new(extra) });
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
            (TD::Optional(t) | TD::Len(t, _, _) | TD::DtC(t, _) | TD::Strict(t, _) | TD::StrictTop(t), _) => prefetch(pf, t, v).await,
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

/// `prefetch` does something under this type (it reaches a model: what an ORM object validates into)
fn prefetches(td: &'static TD) -> bool {
    match td {
        TD::Optional(t) | TD::Len(t, _, _) | TD::DtC(t, _) | TD::Strict(t, _) | TD::StrictTop(t) => prefetches(t),
        TD::Union(ts) => ts.iter().any(|t| prefetches(t)),
        TD::List(Some(t)) | TD::Set(Some(t)) | TD::Tuple(Some(t)) => prefetches(t),
        TD::Schema(_) => true,
        _ => false,
    }
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
                // what reads without IO is read here (every column of every row of a list goes through this); only
                // a load (synchronous session) needs `get_attr`
                let got = match o.get_attr_sync(key) {
                    Err(e) if super::orm::is_greenlet_error(&e) => o.get_attr(key).await,
                    r => r,
                };
                match got {
                    Ok(x) if prefetches(f.td) => prefetch(pf, f.td, &x).await?,
                    Ok(_) => {}
                    Err(e) if super::orm::is_greenlet_error(&e) => {}
                    Err(e) => return Err(e),
                }
                break;
            }
        }
        Ok(())
    })
}

/// `getattr(obj, name)` for from_attributes: None when absent (AttributeError), the exception otherwise
fn attr_lookup(o: &V, name: &str) -> Result<Option<V>, Exc> {
    if let V::Obj(obj) = o {
        let hit = PROPS.lock().get(&(Arc::as_ptr(obj) as usize, name)).cloned();
        match hit {
            Some(Ok(v)) => return Ok(Some(v)),
            Some(Err(x)) if x.isinstance(&ATTRIBUTE_ERROR) => return Ok(None),
            Some(Err(x)) => return Err(x),
            None => {}
        }
        return match obj.get_attr_sync(name) {
            Ok(v) => Ok(Some(v)),
            Err(x) if x.isinstance(&ATTRIBUTE_ERROR) => Ok(None),
            Err(x) => Err(x),
        };
    }
    Ok(attr_value(o, name))
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
    construct_mode(cx, desc, input, true).await
}

/// `construct` with or without `from_attributes` (`plain`: without, pydantic's default)
pub async fn construct_mode(cx: &super::Cx, desc: &'static SchemaDesc, input: V, plain: bool) -> R {
    let mut errs = Vec::new();
    let input = if desc.has_before { prepare_schema(cx, input, desc).await? } else { input };
    let pf = Pf { cx, keys: Default::default() };
    prefetch_schema(&pf, desc, &input).await?;
    let got = run(cx, &mut errs, |e| {
        PLAIN.with(|p| p.set(plain));
        let got = schema_val(&input, desc, &[], e);
        PLAIN.with(|p| p.set(false));
        got
    })
    .await?;
    match got {
        Some(v) => Ok(v),
        None => Err(desc.verror(errs)),
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
        return Err(desc.verror(errs));
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
    let init = format!("{}.{}()", desc.name, if desc.x.ntuple { "__new__" } else { "__init__" });
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
            (None, Dflt::Data(_)) => return Err(Exc::type_error("py2axum: a default_factory taking data in a dataclass is not supported")),
            (None, Dflt::Required) => unreachable!(),
        });
    }
    let inst = V::Inst(Arc::new(Inst { raw: false, desc, vals: Slots::new(out), set: Mutex::new(vec![true; n]), extra: Slots::new(IndexMap::new()) }));
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
        if self.desc.x.ntuple {
            // a tuple: its fields are read-only descriptors, it has no __dict__
            return Err(Exc::attr_error(if self.desc.field_index(name).is_some() {
                "can't set attribute".to_string()
            } else if super::python() >= (3, 13) {
                format!("'{}' object has no attribute '{name}' and no __dict__ for setting new attributes", self.desc.name)
            } else {
                format!("'{}' object has no attribute '{name}'", self.desc.name)
            }));
        }
        if self.desc.frozen && self.desc.dataclass {
            return Err(Exc::msg(&FROZEN_INSTANCE_ERROR, format!("cannot assign to field '{name}'")));
        }
        if self.desc.frozen {
            // model_config frozen=True
            let e = ErrDetail { kind: "frozen_instance", loc: vec![V::str(name)], msg: "Instance is frozen".into(), input: v, ctx: None, custom: false };
            return Err(self.desc.verror(vec![e]));
        }
        if self.desc.x.frozen.contains(&name) {
            let e = ErrDetail { kind: "frozen_field", loc: vec![V::str(name)], msg: "Field is frozen".into(), input: v, ctx: None, custom: false };
            return Err(self.desc.verror(vec![e]));
        }
        match self.desc.field_index(name) {
            Some(i) => {
                let v = if self.desc.validate_assignment {
                    let mut errs = Vec::new();
                    match validate_sync(&v, self.desc.fields[i].td, &[V::str(name)], &mut errs) {
                        Some(x) if errs.is_empty() => x,
                        _ => return Err(self.desc.verror(errs)),
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
        !self.desc.open && !self.desc.x.eq_id && std::ptr::eq(self.desc, other.desc) && {
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
    /// `str(model)`: pydantic's `__repr_str__(" ")` (a dataclass or a plain class: its repr)
    pub fn str_(&self) -> R<String> {
        if self.desc.open || self.desc.dataclass {
            return self.repr();
        }
        Ok(self.repr_parts()?.join(" "))
    }

    pub fn repr(&self) -> R<String> {
        if self.desc.open {
            if std::ptr::eq(self.desc, &super::libs::NAMESPACE) {
                let extra = self.extra.lock().clone();
                let parts = extra.iter().map(|(k, v)| Ok(format!("{k}={}", ops::repr(v)?))).collect::<R<Vec<_>>>()?;
                return Ok(format!("namespace({})", parts.join(", ")));
            }
            // CPython: <module.Class object at 0x...> (the address differs anyway)
            return Ok(format!("<{} object at {}>", self.desc.class.qualname, ops::addr(self as *const Self as usize)));
        }
        Ok(format!("{}({})", self.desc.name, self.repr_parts()?.join(", ")))
    }

    fn repr_parts(&self) -> R<Vec<String>> {
        let vals = self.vals.lock().clone();
        let parts = self
            .desc
            .fields
            .iter()
            .zip(vals.iter())
            .filter(|(f, _)| !self.desc.x.norepr.contains(&f.name))
            .map(|(f, v)| Ok(format!("{}={}", f.name, ops::repr(v)?)))
            .collect::<R<Vec<_>>>()?;
        let mut parts = parts;
        if !self.desc.computed.is_empty() {
            let me = Arc::new(Inst { raw: false,
                desc: self.desc,
                vals: Slots::new(vals.clone()),
                set: Mutex::new(self.set.lock().clone()),
                extra: Slots::new(self.extra.lock().clone()),
            });
            for (n, v) in computed_values(&me)? {
                parts.push(format!("{n}={}", ops::repr(&v)?));
            }
        }
        Ok(parts)
    }
}

// ---------------------------------------------------------------- dump

/// JSON mode: a `float` field holding an int (a default `0`, an assignment without validation) is serialized
/// as a float by pydantic-core's float serializer. None: the value as it is.
fn float_ser(v: &V, td: &TD) -> Option<V> {
    // nothing to convert without a float in the type: no copy of the list or tuple (every field of every row)
    if !has_float(td) {
        return None;
    }
    match (td, v) {
        (TD::Float(_), V::Int(i)) => Some(V::Float(*i as f64)),
        (TD::Optional(t) | TD::Strict(t, _) | TD::StrictTop(t) | TD::Len(t, _, _) | TD::DtC(t, _), _) => float_ser(v, t),
        (TD::TupleFix(ts), V::Tuple(items)) => {
            let conv: Vec<Option<V>> = items.iter().zip(ts.iter()).map(|(x, t)| float_ser(x, t)).collect();
            if conv.iter().all(|c| c.is_none()) {
                return None;
            }
            Some(V::tuple(items.iter().cloned().zip(conv).map(|(x, c)| c.unwrap_or(x)).collect()))
        }
        (TD::List(Some(t)), V::List(l)) => {
            let items = l.lock().clone();
            let conv: Vec<Option<V>> = items.iter().map(|x| float_ser(x, t)).collect();
            if conv.iter().all(|c| c.is_none()) {
                return None;
            }
            Some(V::list(items.into_iter().zip(conv).map(|(x, c)| c.unwrap_or(x)).collect()))
        }
        _ => None,
    }
}

/// the types float_ser goes through hold a `float`
fn has_float(td: &TD) -> bool {
    match td {
        TD::Float(_) => true,
        TD::Optional(t) | TD::Strict(t, _) | TD::StrictTop(t) | TD::Len(t, _, _) | TD::DtC(t, _) | TD::List(Some(t)) => has_float(t),
        TD::TupleFix(ts) => ts.iter().any(|t| has_float(t)),
        _ => false,
    }
}

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
    /// `by_alias=` not given: each model's `serialize_by_alias` decides
    pub alias_cfg: bool,
    /// `exclude_defaults=True`: a field equal to its default is left out
    pub exclude_defaults: bool,
    /// the model being dumped by its own `@model_serializer(mode="wrap")`'s handler: not again
    pub skip_model_ser: bool,
    /// the `ser_json_bytes` of the model being dumped (each model its own)
    pub bytes: BytesMode,
    /// inside a model's fields: pydantic-core's serializer, which has no JSON form for a plain object
    pub in_model: bool,
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
/// `SecretStr(value)`: the secret held as given (pydantic does not validate a direct construction)
pub fn secret_new(args: &[V], kwargs: &[(String, V)]) -> R {
    match (args, kwargs) {
        ([V::Str(s)], []) => Ok(V::native(Native::Secret(s.clone()))),
        ([], [(k, V::Str(s))]) if k == "secret_value" => Ok(V::native(Native::Secret(s.clone()))),
        _ => Err(Exc::type_error("py2axum: SecretStr() takes one str")),
    }
}

/// a project function called while dumping (serialization is synchronous: it must not await)
fn sync_call(f: MethodFn, recv: V, args: Vec<V>, what: &str) -> R {
    let cx = super::root_cx();
    match futures_util::FutureExt::now_or_never(f(&cx, recv, args)) {
        Some(r) => r,
        None => Err(Exc::runtime(format!("py2axum: {what} awaited while being serialized"))),
    }
}

static SERINFO_CLASS: Class = Class { name: "SerializationInfo", qualname: "SerializationInfo", bases: &[], kind: ClassKind::Schema(&SERINFO) };
/// pydantic-core's `SerializationInfo` / `FieldSerializationInfo` as a serializer sees it
static SERINFO: SchemaDesc = SchemaDesc {
    name: "SerializationInfo",
    class: &SERINFO_CLASS,
    fields: &[
        FieldDesc { name: "mode", alias: None, ser: None, paths: &[], td: &STR_TD, default: Dflt::Required, env: None, validate_default: false },
        FieldDesc { name: "field_name", alias: None, ser: None, paths: &[], td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false },
        FieldDesc { name: "context", alias: None, ser: None, paths: &[], td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false },
        FieldDesc { name: "by_alias", alias: None, ser: None, paths: &[], td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false },
        FieldDesc { name: "exclude_none", alias: None, ser: None, paths: &[], td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false },
        FieldDesc { name: "exclude_unset", alias: None, ser: None, paths: &[], td: &ANY_TD, default: Dflt::Required, env: None, validate_default: false },
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
    x: SchemaX::NONE,
};

thread_local! {
    /// the `context=` of the dump in progress (serialization is synchronous)
    static DUMP_CONTEXT: std::cell::RefCell<Option<V>> = const { std::cell::RefCell::new(None) };
}

/// a dump with `context=` (`info.context` of its serializers)
pub fn with_context<T>(ctx: Option<V>, f: impl FnOnce() -> T) -> T {
    let prev = DUMP_CONTEXT.with(|c| std::mem::replace(&mut *c.borrow_mut(), ctx));
    let out = f();
    DUMP_CONTEXT.with(|c| *c.borrow_mut() = prev);
    out
}

fn ser_info(o: &DumpOpts, field: Option<&str>) -> V {
    let vals = vec![
        V::str(if o.json { "json" } else { "python" }),
        field.map(V::str).unwrap_or(V::None),
        DUMP_CONTEXT.with(|c| c.borrow().clone()).unwrap_or(V::None),
        V::Bool(o.by_alias),
        V::Bool(o.exclude_none),
        V::Bool(o.exclude_unset),
    ];
    V::Inst(Arc::new(Inst { raw: false, desc: &SERINFO, set: Mutex::new(vec![true; vals.len()]), vals: Slots::new(vals), extra: Slots::new(IndexMap::new()) }))
}

/// the `handler` of a wrap serializer: the usual dump with the same options
fn ser_handler(o: DumpOpts, skip_model: bool) -> V {
    super::func(Arc::new(move |_cx, args| {
        Box::pin(async move {
            let (args, _) = super::unpack(args);
            let [v] = args.as_slice() else { return Err(Exc::type_error("handler() takes 1 positional argument")) };
            dump(v, DumpOpts { skip_model_ser: skip_model, ..o })
        })
    }))
}

/// a serializer applied to `value` (a field's value, or the instance for a model serializer)
fn run_ser(s: &Ser, inst: &V, value: &V, o: DumpOpts, field: Option<&str>) -> R {
    let mut args = Vec::new();
    if !s.field.is_empty() {
        args.push(value.clone());
    }
    if s.wrap {
        args.push(ser_handler(o, s.field.is_empty()));
    }
    if s.info {
        args.push(ser_info(&o, field));
    }
    let out = sync_call(s.f, inst.clone(), args, "a serializer")?;
    // its result is serialized by its type, as pydantic does for an unannotated return
    dump(&out, DumpOpts { skip_model_ser: false, ..o })
}

fn ser_applies(s: &Ser, v: &V, o: &DumpOpts) -> bool {
    match s.when {
        1 => !v.is_none(),
        2 => o.json,
        3 => o.json && !v.is_none(),
        _ => true,
    }
}

pub fn dump(v: &V, o: DumpOpts) -> R {
    super::stack_guard()?;
    if let Some(s) = super::pgpy::as_str(v) {
        return dump(&s, o);
    }
    // a Row, a NamedTuple: pydantic-core dumps them as plain tuples (lists in JSON)
    if let Some(t) = ops::seq_tuple(v) {
        return dump(&t, o);
    }
    Ok(match v {
        // a plain project object in a model's field (an after validator's result, `arbitrary_types_allowed`): no JSON
        // form in pydantic-core (FastAPI's jsonable_encoder takes `vars()` of one it is given directly)
        V::Inst(inst) if inst.desc.open && o.json && o.in_model => {
            return Err(Exc::msg(&PYDANTIC_SERIALIZATION_ERROR,
                                format!("Unable to serialize unknown type: <class '{}'>", inst.desc.class.qualname)));
        }
        V::Inst(inst) => {
            if let Some(ms) = &inst.desc.x.model_ser {
                if !o.skip_model_ser && ser_applies(ms, v, &o) {
                    return run_ser(ms, v, v, DumpOpts { bytes: inst.desc.ser_bytes, ..o }, None);
                }
            }
            let o = DumpOpts { skip_model_ser: false, ..o };
            if inst.desc.x.root {
                let root = inst.vals.lock()[0].clone();
                return dump(&root, DumpOpts { bytes: inst.desc.ser_bytes, ..o });
            }
            let vals = inst.vals.lock().clone();
            let set = inst.set.lock().clone();
            if inst.raw {
                if let Some((f, v)) = inst.desc.fields.iter().zip(&vals).find(|(f, v)| !fits(v, f.td)) {
                    return Err(Exc::type_error(format!(
                        "py2axum: {}.{} holds a {} given to model_construct(): pydantic would serialize it with a warning (not supported)",
                        inst.desc.name, f.name, v.type_name()
                    )));
                }
            }
            let mut items = Vec::new();
            let o = DumpOpts { bytes: inst.desc.ser_bytes, in_model: !inst.desc.open, ..o };
            for (i, f) in inst.desc.fields.iter().enumerate() {
                if (o.exclude_unset && !set[i]) || inst.desc.x.exclude.contains(&f.name) {
                    continue;
                }
                if o.exclude_none && vals[i].is_none() {
                    continue;
                }
                if o.exclude_defaults && !inst.desc.x.field_ser.iter().any(|s| s.deco && s.field == f.name) {
                    if let Dflt::Value(d) | Dflt::Factory(d) = f.default {
                        if ops::eq_bool(&d(), &vals[i]) {
                            continue;
                        }
                    }
                }
                if let Some((_, g)) = inst.desc.x.exclude_if.iter().find(|(n, _)| *n == f.name) {
                    if ops::truthy(&sync_call(*g, V::None, vec![vals[i].clone()], "an exclude_if function")?)? {
                        continue;
                    }
                }
                let k = if o.by_alias || (o.alias_cfg && inst.desc.x.ser_by_alias) { f.ser_key() } else { f.name };
                if let Some(s) = inst.desc.x.field_ser.iter().find(|s| s.field == f.name && ser_applies(s, &vals[i], &o)) {
                    items.push((V::str(k), run_ser(s, v, &vals[i], o, Some(f.name))?));
                    continue;
                }
                let fv = if o.json { float_ser(&vals[i], f.td) } else { None };
                items.push((V::str(k), dump(fv.as_ref().unwrap_or(&vals[i]), o)?));
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
        V::Set(s) | V::Frozen(s) if o.json => V::list(s.lock().values().map(|x| dump(x, o)).collect::<R<Vec<_>>>()?),
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
        V::Native(n) if o.json && matches!(&**n, Native::PydUrl(..) | Native::Uuid(_) | Native::Secret(_) | Native::Path(_)) => V::str(ops::str_(v)?),
        V::Native(n) if o.json && matches!(&**n, Native::Complex(..)) => match &**n {
            Native::Complex(re, im) => V::str(super::complex::json(*re, *im)),
            _ => unreachable!(),
        },
        // pydantic-core dumps a Fraction as its str() in both modes
        V::Native(n) if matches!(&**n, Native::Fraction(_)) => V::str(ops::str_(v)?),
        // an Iterable field: its items validated as the dump reaches them (python mode keeps pydantic-core's
        // SerializationIterator, not reproduced)
        V::Native(n) if matches!(&**n, Native::ValIter(_)) => match &**n {
            Native::ValIter(it) if o.json => V::list(it.drain()?.iter().map(|x| dump(x, o)).collect::<R<Vec<_>>>()?),
            _ => return Err(Exc::type_error("py2axum: model_dump() in python mode of an Iterable field (pydantic's SerializationIterator) is not supported: use mode=\"json\"")),
        },
        V::Native(n) if o.json && matches!(&**n, Native::Deque(_)) => match &**n {
            Native::Deque(d) => V::list(d.items.lock().iter().map(|x| dump(x, o)).collect::<R<Vec<_>>>()?),
            _ => unreachable!(),
        },
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
    if let Some(s) = super::pgpy::as_str(v) {
        return jsonable(&s);
    }
    if let Some(t) = ops::seq_tuple(v) {
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
        V::Set(s) | V::Frozen(s) => V::list(s.lock().values().map(jsonable).collect::<R<Vec<_>>>()?),
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
        V::Native(n) if matches!(&**n, Native::PydUrl(..) | Native::Uuid(_) | Native::Secret(_) | Native::Fraction(_) | Native::Path(_)) => V::str(ops::str_(v)?),
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
                return Err(Exc::value_error(format!("Out of range float values are not JSON compliant: {}", ops::float_repr(*f))));
            }
            out.push_str(&ops::float_repr(*f))
        }
        V::Str(s) => write_str(out, s, st),
        V::List(_) | V::Tuple(_) | V::Inst(_) if !matches!(v, V::Inst(i) if !i.desc.x.ntuple) => {
            // a NamedTuple too (a tuple subclass: an array, its notes naming its class)
            out.push('[');
            for (i, x) in ops::iter(v)?.iter().enumerate() {
                if i > 0 {
                    out.push_str(st.item_sep);
                }
                write(out, x, st, default_str).map_err(|e| noted(e, st, || format!("when serializing {} item {i}", v.type_name())))?;
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
                write(out, x, st, default_str).map_err(|e| noted(e, st, || format!("when serializing dict item {}", ops::repr(k).unwrap_or_default())))?;
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

/// Python 3.14's json encoder notes where a nested value failed (`when serializing list item 1`), innermost
/// first; pydantic's serializer (`nan_null`) adds none
fn noted(e: Exc, st: &JsonStyle, note: impl FnOnce() -> String) -> Exc {
    if !st.nan_null && super::python() >= (3, 14) {
        e.add_note(&note());
    }
    e
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
    V::Inst(Arc::new(Inst { raw: false, desc, vals: Slots::new(vec![V::None; n]), set: Mutex::new(vec![false; n]), extra: Slots::new(IndexMap::new()) }))
}

/// `object.__new__(cls)`: a bare instance of a plain class held as a value
pub fn object_new_of(cls: &V) -> R {
    match cls {
        V::Class(c) => match &c.kind {
            ClassKind::Schema(d) if d.open => Ok(object_new(d)),
            _ => Err(Exc::type_error(format!("py2axum: object.__new__({}) of a class that is not a plain class", c.name))),
        },
        o => Err(Exc::type_error(format!("object.__new__(X): X is not a type object ({})", o.type_name()))),
    }
}

/// `Model.model_construct(_fields_set=None, **values)` (pydantic 2.12-2.14, `BaseModel.model_construct`): no
/// validation; each field takes its alias, then its validation aliases, then its name, else its default (a
/// `default_factory` taking data gets the fields taken so far); the keys left are the extras with
/// `extra="allow"` (ignored otherwise) and set the private attributes they name. A required field left out
/// (pydantic leaves it out of `__dict__`), a `_fields_set` that is not a set of field names, and a RootModel are
/// refused at run time.
pub async fn model_construct(cx: &super::Cx, desc: &'static SchemaDesc, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if desc.open || desc.dataclass {
        return Err(Exc::attr_error(format!("type object '{}' has no attribute 'model_construct'", desc.name)));
    }
    if desc.x.root {
        return Err(Exc::type_error(format!("py2axum: {}.model_construct() of a RootModel is not supported", desc.name)));
    }
    if args.len() > 1 {
        return Err(Exc::type_error(format!("BaseModel.model_construct() takes from 1 to 2 positional arguments but {} were given", args.len() + 1)));
    }
    let mut fields_set = args.into_iter().next();
    let mut values: IndexMap<Key, (V, V)> = IndexMap::new();
    for (k, v) in kwargs {
        if k == "_fields_set" {
            if fields_set.is_some() {
                return Err(Exc::type_error("BaseModel.model_construct() got multiple values for argument '_fields_set'"));
            }
            fields_set = Some(v);
            continue;
        }
        values.insert(Key::Str(Arc::from(k.as_str())), (V::str(&k), v));
    }
    let n = desc.fields.len();
    let mut vals: Vec<Option<V>> = Vec::with_capacity(n);
    let mut set = vec![false; n];
    let key = |s: &str| Key::Str(Arc::from(s));
    for (fi, f) in desc.fields.iter().enumerate() {
        let mut got = f.alias.and_then(|a| values.shift_remove(&key(a))).map(|(_, v)| v);
        if got.is_none() {
            for p in f.paths {
                // a plain key of AliasChoices is taken out of the values, an AliasPath is only read
                if let [PK::S(k)] = p {
                    if let Some((_, v)) = values.shift_remove(&key(k)) {
                        got = Some(v);
                        break;
                    }
                } else if let Some(v) = path_lookup(&values, p) {
                    got = Some(v);
                    break;
                }
            }
        }
        if got.is_none() {
            got = values.shift_remove(&key(f.name)).map(|(_, v)| v);
        }
        if got.is_some() {
            set[fi] = true;
            vals.push(got);
            continue;
        }
        vals.push(match &f.default {
            Dflt::Required => None,
            Dflt::Value(d) | Dflt::Factory(d) => Some(d()),
            Dflt::Dyn(d) => Some(d(cx, V::None, vec![]).await?),
            Dflt::Data(d) => {
                let taken = desc.fields[..fi].iter().zip(&vals).filter_map(|(fd, v)| v.clone().map(|v| (V::str(fd.name), v))).collect();
                Some(d(cx, V::None, vec![V::dict_from(taken)?]).await?)
            }
        });
    }
    if let Some(i) = vals.iter().position(|v| v.is_none()) {
        return Err(Exc::type_error(format!(
            "py2axum: {}.model_construct() without the required field '{}' is not supported (pydantic leaves it unset)",
            desc.name, desc.fields[i].name
        )));
    }
    if let Some(fs) = fields_set.filter(|v| !v.is_none()) {
        let V::Set(names) = &fs else {
            return Err(Exc::type_error(format!("py2axum: model_construct(_fields_set=) must be a set, not {}", fs.type_name())));
        };
        set = vec![false; n];
        for x in names.lock().values() {
            let name = ops::str_(x)?;
            match desc.field_index(&name) {
                Some(i) => set[i] = true,
                None => return Err(Exc::type_error(format!("py2axum: model_construct(_fields_set=) with {name:?}, which is not a field of {}", desc.name))),
            }
        }
    }
    let mut extra = IndexMap::new();
    desc.private_defaults(&mut extra);
    for (_, (k, v)) in values {
        let k = ops::str_(&k)?;
        if desc.is_private(&k) || desc.extra == Extra::Allow {
            extra.insert(k, v);
        }
    }
    let vals = vals.into_iter().map(Option::unwrap).collect();
    Ok(V::Inst(Arc::new(Inst { desc, vals: Slots::new(vals), set: Mutex::new(set), extra: Slots::new(extra), raw: true })))
}

/// a value of an instance built by `model_construct()` that its field serializes as `dump` does (by its own
/// type), with no warning from pydantic's serializer: the same Python type (an int in a float field too)
fn fits(v: &V, td: &TD) -> bool {
    match (td, v) {
        (TD::Any, _) => true,
        (TD::NoneT, V::None) => true,
        (TD::Optional(t), _) => v.is_none() || fits(v, t),
        (TD::Strict(t, _) | TD::StrictTop(t) | TD::Len(t, _, _) | TD::DtC(t, _), _) => fits(v, t),
        (TD::Union(ts) | TD::UnionLtr(ts), _) => ts.iter().any(|t| fits(v, t)),
        (TD::Bool, V::Bool(_)) => true,
        (TD::Int(_), V::Int(_)) => true,
        (TD::Float(_), V::Int(_) | V::Float(_)) => true,
        (TD::Str(_) | TD::Email(_), V::Str(_)) => true,
        (TD::DateTime, V::DateTime(_)) | (TD::Date, V::Date(_)) | (TD::Time, V::Time(_)) | (TD::Delta, V::Delta(_)) => true,
        (TD::Decimal(_), V::Decimal(_)) => true,
        (TD::Uuid, V::Native(n)) => matches!(&**n, Native::Uuid(_)),
        (TD::Path, V::Native(n)) => matches!(&**n, Native::Path(_)),
        (TD::Enum(d, _), V::Enum(e, _)) => std::ptr::eq(*d, *e),
        (TD::Schema(d), V::Inst(i)) => std::ptr::eq(*d, i.desc),
        (TD::List(t), V::List(l)) => t.is_none_or(|t| l.lock().iter().all(|x| fits(x, t))),
        (TD::Set(t), V::Set(s) | V::Frozen(s)) => t.is_none_or(|t| s.lock().values().all(|x| fits(x, t))),
        (TD::Tuple(t), V::Tuple(xs)) => t.is_none_or(|t| xs.iter().all(|x| fits(x, t))),
        (TD::Dict(kv), V::Dict(d)) => kv.is_none_or(|(kt, vt)| d.lock().values().all(|(k, x)| fits(k, kt) && fits(x, vt))),
        _ => false,
    }
}

pub fn object_new(desc: &'static SchemaDesc) -> V {
    V::Inst(Arc::new(Inst { raw: false, desc, vals: Slots::new(vec![]), set: Mutex::new(vec![]), extra: Slots::new(IndexMap::new()) }))
}


/// the name pydantic-core gives a validator, which titles a TypeAdapter's ValidationError (`1 validation error for
/// list[int]`): None for a type whose name this does not reproduce (url, secret, deque... compose their names)
pub fn td_title(td: &TD) -> Option<String> {
    let num = |c: &NumC| c.ge.is_none() && c.gt.is_none() && c.le.is_none() && c.lt.is_none() && c.mul.is_none();
    let join = |ts: &[&'static TD], sep: &str| ts.iter().map(|t| td_title(t)).collect::<Option<Vec<_>>>().map(|v| v.join(sep));
    Some(match td {
        TD::Any => "any".into(),
        TD::NoneT => "none".into(),
        TD::Bool => "bool".into(),
        TD::Int(c) => if num(c) { "int" } else { "constrained-int" }.into(),
        TD::Float(c) => if num(c) { "float" } else { "constrained-float" }.into(),
        TD::Str(c) => if c.min.is_none() && c.max.is_none() && c.pattern.is_none() && !c.strip && !c.lower && !c.upper && !c.ascii { "str" } else { "constrained-str" }.into(),
        TD::DateTime => "datetime".into(),
        TD::Date => "date".into(),
        TD::Time => "time".into(),
        TD::Delta => "timedelta".into(),
        TD::Uuid => "uuid".into(),
        TD::Path => "path".into(),
        TD::Complex => "complex".into(),
        TD::Dict(None) => "dict[any,any]".into(),
        TD::Dict(Some((k, v))) => format!("dict[{},{}]", td_title(k)?, td_title(v)?),
        TD::List(None) => "list[any]".into(),
        TD::List(Some(t)) => format!("list[{}]", td_title(t)?),
        TD::Set(None) => "set[any]".into(),
        TD::Set(Some(t)) => format!("set[{}]", td_title(t)?),
        TD::Coll(inner, Coll::Frozen) => match &**inner {
            TD::Set(None) => "frozenset[any]".into(),
            TD::Set(Some(t)) => format!("frozenset[{}]", td_title(t)?),
            _ => return None,
        },
        TD::Tuple(None) => "tuple[any, ...]".into(),
        TD::Tuple(Some(t)) => format!("tuple[{}, ...]", td_title(t)?),
        TD::TupleFix(ts) => format!("tuple[{}]", join(ts, ", ")?),
        TD::Optional(t) => format!("nullable[{}]", td_title(t)?),
        TD::Union(ts) => format!("union[{}]", join(ts, ",")?),
        TD::Literal(ls) => format!(
            "literal[{}]",
            ls.iter()
                .map(|l| match l {
                    Lit::Str(x) => ops::repr(&V::str(*x)).unwrap_or_default(),
                    Lit::Int(i) => i.to_string(),
                    Lit::Bool(b) => if *b { "True" } else { "False" }.into(),
                    Lit::None => "None".into(),
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        TD::Schema(d) => d.name.to_string(),
        TD::Strict(t, _) | TD::StrictTop(t) | TD::Len(t, _, _) | TD::DtC(t, _) => td_title(t)?,
        _ => return None,
    })
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
            let json = name == "validate_json";
            let input = if json {
                match json_input(obj) {
                    Ok(v) => v,
                    Err(JsonIn::Invalid(kind, msg, ctx)) => {
                        let d = ErrDetail { kind, loc: vec![], msg, input: obj.clone(), ctx, custom: false };
                        return Err(Exc::validation(&VALIDATION_ERROR, vec![d]));
                    }
                    Err(JsonIn::Raise(x)) => return Err(x),
                }
            } else {
                obj.clone()
            };
            let mut errs = Vec::new();
            match validate_mode(cx, &input, td, &[], json, true, &mut errs).await? {
                Some(v) if errs.is_empty() => Ok(v),
                _ => match td_title(td) {
                    Some(t) => Err(Exc::validation_titled(&VALIDATION_ERROR, errs, &t)),
                    // `str(exc)` would name the validator: refused rather than printed wrong
                    None => Err(Exc::type_error("py2axum: the title of this TypeAdapter's ValidationError (its validator's name) is not reproduced for this type")),
                },
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


/// `dataclasses.astuple(obj)` / `dataclasses.asdict(obj)`: the fields, dataclasses, lists, tuples and dicts
/// inside converted recursively (CPython deep-copies the other values: immutable ones are the same here)
pub fn dataclass_convert(v: &V, dict: bool) -> R {
    match v {
        V::Inst(i) if i.desc.dataclass => {}
        _ => return Err(Exc::type_error(format!("{}() should be called on dataclass instances", if dict { "asdict" } else { "astuple" }))),
    }
    dc_inner(v, dict)
}

fn dc_inner(v: &V, dict: bool) -> R {
    Ok(match v {
        V::Inst(i) if i.desc.dataclass => {
            let vals = i.vals.read().clone();
            if dict {
                let mut items = Vec::with_capacity(vals.len());
                for (f, x) in i.desc.fields.iter().zip(vals.iter()) {
                    items.push((V::str(f.name), dc_inner(x, dict)?));
                }
                V::dict_from(items)?
            } else {
                V::tuple(vals.iter().map(|x| dc_inner(x, dict)).collect::<R<Vec<_>>>()?)
            }
        }
        V::List(l) => V::list(l.lock().iter().map(|x| dc_inner(x, dict)).collect::<R<Vec<_>>>()?),
        V::Tuple(t) => V::tuple(t.iter().map(|x| dc_inner(x, dict)).collect::<R<Vec<_>>>()?),
        V::Dict(d) => {
            let items: Vec<(V, V)> = d.lock().iter().map(|(_, (k, x))| (k.clone(), x.clone())).collect();
            V::dict_from(items.iter().map(|(k, x)| Ok((dc_inner(k, dict)?, dc_inner(x, dict)?))).collect::<R<Vec<_>>>()?)?
        }
        other => other.clone(),
    })
}
