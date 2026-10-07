//! The dynamic Python value, classes and exceptions.
//!
//! Generated code manipulates `V` the way CPython manipulates `PyObject*`: lists and dicts are
//! shared mutable references (aliasing behaves like Python), everything else is cheap to clone.
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;

use super::dt::{DateTime, Tz};
use super::orm;
use super::pyd;
use super::web;

pub type R<T = V> = Result<T, Exc>;

#[derive(Clone)]
pub enum V {
    /// A local read before assignment (UnboundLocalError).
    Unbound,
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Arc<str>),
    Bytes(Arc<[u8]>),
    List(Arc<Mutex<Vec<V>>>),
    Tuple(Arc<Vec<V>>),
    Dict(Arc<Mutex<IndexMap<Key, (V, V)>>>),
    Set(Arc<Mutex<IndexMap<Key, V>>>),
    DateTime(DateTime),
    Date(chrono::NaiveDate),
    Time(chrono::NaiveTime),
    Delta(chrono::TimeDelta),
    Tz(Tz),
    /// A project class: SQLAlchemy model, Pydantic model, exception class.
    Class(&'static Class),
    /// An instance of a Pydantic model (or BaseSettings).
    Inst(Arc<pyd::Inst>),
    /// An instance of a SQLAlchemy model.
    Obj(Arc<orm::ObjCell>),
    /// A member of a Python `enum.Enum` class.
    Enum(&'static EnumDesc, u16),
    /// An instrumented column attribute `Model.col`.
    Col(&'static orm::ModelDesc, usize),
    /// SQL expression or statement (sqlalchemy core).
    Sql(Arc<orm::Sql>),
    Session(orm::Session),
    Result(Arc<Mutex<orm::QResult>>),
    Exc(Exc),
    /// `decimal.Decimal`
    Decimal(Arc<super::decimal::Dec>),
    Native(Arc<Native>),
}

/// Values without a natural representation elsewhere.
/// A project module as a value: its top-level names, read by a generated function.
pub struct ModDesc {
    pub name: &'static str,
    pub attr: for<'a> fn(&'a super::Cx, &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'a>>,
}

pub enum Native {
    Request(Arc<web::ReqCell>),
    Headers(Arc<web::ReqCell>),
    State(Arc<web::ReqCell>),
    Url(Arc<web::ReqCell>),
    Response(Arc<web::RespCell>),
    Logger(Arc<str>),
    Queue(Arc<web::AQueue>),
    Hash(Mutex<super::libs::Hasher>),
    Serializer(super::itsd::Serializer),
    Fernet(super::fernet::Fernet),
    RespObj(Arc<super::resp::RespObj>),
    /// `response.headers` of a Response object (Starlette's MutableHeaders, live)
    RespHeaders(Arc<super::resp::RespObj>),
    /// `response.headers` of the injected `response: Response` parameter
    CellHeaders(Arc<web::RespCell>),
    /// `call_next` of a BaseHTTPMiddleware: the rest of the middleware stack
    CallNext(Arc<super::asgi::Next>),
    /// `request.client`: (host, port)
    Address(String, u16),
    Totp(super::auth::Totp),
    /// `asyncio.create_task(coro)`: runs on its own; `add_done_callback`
    Task(Arc<super::web::Task>),
    HttpClient(Arc<super::http::Client>),
    HttpResp(Arc<super::http::Resp>),
    HttpTimeout(super::http::Timeout),
    /// `httpx.BasicAuth(user, password)`: its `Authorization` header value
    HttpBasicAuth(String),
    HttpUrl(String),
    /// a result row (`execute(select(...)).all()`): a tuple whose columns are also attributes
    Row(Arc<Vec<Arc<str>>>, Arc<Vec<V>>),
    /// `await session.begin_nested()`
    Savepoint(orm::Session, String),
    /// `urllib.parse.urlparse(...)` / `urlsplit(...)`
    UrlParts(Arc<super::stdlib::UrlParts>),
    /// a pydantic URL (`RedisDsn`...): (type name, parsed URL)
    PydUrl(&'static str, Arc<url::Url>),
    /// `alembic.config.Config(file)`
    IniConfig(Arc<super::ini::IniConfig>),
    /// `create_async_engine(...)`: the binary's own pool (DATABASE_URL, DB_POOL_SIZE)
    Engine,
    /// a named tuple of a library (`psutil.virtual_memory()`): (type name, fields)
    Record(&'static str, Arc<Vec<(&'static str, V)>>),
    /// a method of a builtin value read as a value (`_pending.discard`)
    MethodOf(V, &'static str),
    Pattern(Arc<super::stdlib::Pattern>),
    Match(Arc<super::stdlib::Match>),
    StringIO(super::stdlib::StringIO),
    CsvWriter(super::stdlib::CsvWriter),
    /// csv.DictReader: (fieldnames, rows)
    CsvRows(V, V),
    SnifferObj,
    Sniffed(char),
    Hmac(Mutex<super::stdlib::Hmac>),
    /// a stateful iterator (csv.reader...): next() pops, iteration drains
    Iter(Mutex<std::collections::VecDeque<V>>),
    /// hashlib.sha256 & co used as a value (hmac digestmod)
    HashCtor(&'static str),
    Jinja(Arc<super::mail::Jinja>),
    JinjaTpl(super::mail::JinjaTpl),
    Mime(Arc<super::mail::Mime>),
    Tasks(super::resp::Tasks),
    BytesIO(super::files::BytesIO),
    Path(String),
    File(super::pathio::File),
    Uuid(u128),
    Upload(Arc<super::files::Upload>),
    /// a method bound to its receiver (`obj.method` read as a value)
    Bound(super::pyd::MethodFn, V),
    /// keyword arguments riding at the end of a method's argument vector (see `pack`)
    Kwargs(Vec<(String, V)>),
    /// a `mode="before"` validator failed on this input: (type, msg, input), reported in place by the
    /// synchronous validation
    ValErr(&'static str, String, V, V),
    Streaming(Mutex<Option<web::Streaming>>),
    Gen(Arc<super::agen::AGen>),
    /// `@asynccontextmanager` called: `_AsyncGeneratorContextManager`
    Acm(Arc<super::agen::AGen>),
    /// `contextlib.suppress(*excs)`
    Suppress(Vec<V>),
    /// `contextvars.ContextVar(name, default=...)`
    CtxVar(Arc<super::agen::CtxVar>),
    /// the `Token` of `var.set(v)`: (variable, previous value)
    CtxToken(Arc<super::agen::CtxVar>, Option<V>),
    /// the `receive` / `send` callables given to a raw ASGI app
    AsgiReceive(Arc<super::rawasgi::Chan>),
    AsgiSend(Arc<super::rawasgi::Chan>),
    /// `mcp.server.mcpserver.MCPServer`, its `@tool(...)` decorator, `session_manager`, `session_manager.run()`
    McpServer(Arc<super::mcp::Server>),
    McpToolDeco(Arc<super::mcp::Server>, &'static super::mcp::ToolSpec),
    McpManager(Arc<super::mcp::Server>),
    McpRun,
    /// `func` of sqlalchemy, `status` of fastapi...: a namespace of attributes.
    Namespace(&'static str),
    /// Bound builtin type used as a value (`dict`, `str`) e.g. in isinstance.
    Type(&'static str),
    FieldInfo,
    /// A first-class function value (lambda, bound method, function passed as argument).
    Func(super::FnVal),
    /// A project `def` as a value (see `pyfn`).
    PyFn(PyFn),
    /// `pydantic.TypeAdapter(T)`: (its type, `repr` of the type for messages)
    Adapter(&'static super::pyd::TD, &'static str),
    /// a library class used as a value (`pydantic.BaseModel`, `types.GenericAlias`...), see `types`
    ExtType(&'static str),
    /// a generic alias or union written in the source (`list[Schema]`): its validator and its text
    TypeExpr(&'static super::pyd::TD, &'static str),
    /// `threading.Lock` / `RLock`, `Event`, `Thread`, an asyncio event loop (see `thread`)
    TLock(Arc<super::thread::TLock>),
    TEvent(Arc<super::thread::TEvent>),
    TThread(Arc<super::thread::TThread>),
    ELoop(Arc<super::thread::ELoop>),
    /// `async_sessionmaker(...)`/`sessionmaker(...)` built at run time: (expire_on_commit, autoflush, sync: `sessionmaker` rather than `async_sessionmaker`)
    Maker(bool, bool, bool),
    /// `session.query(...)`: the session and the `select()` it runs
    Query(super::orm::Session, V),
    /// a `redis.asyncio` client (see `rds`), a `Retry` policy (its number of retries)
    Redis(Arc<super::rds::RClient>),
    RRetry(usize),
    /// the items of an async iterator produced by the runtime (`scan_iter`), read by `async for`
    AsyncItems(Mutex<Option<Vec<V>>>),
    /// `yarl.URL`
    YarlUrl(String),
    /// aio_pika objects (see `rmq`)
    AmqpConn(Arc<super::rmq::AConn>),
    AmqpChan(Arc<super::rmq::AChan>),
    AmqpQueue(Arc<super::rmq::AChan>, String),
    AmqpExchange(Arc<super::rmq::AChan>, String),
    AmqpMsg(Arc<super::rmq::AMsg>),
    AmqpIncoming(Arc<super::rmq::AIncoming>),
    /// a project module returned by `importlib.import_module("literal")`
    Module(&'static ModDesc),
    Tenacity(Arc<super::tenacity::Ten>),
    /// `sys.settrace`: a frame, its code, a traceback (frames, index), a FrameSummary (see `trace`)
    TraceFrame(Arc<super::trace::Frame>),
    TraceCode(Arc<super::trace::Frame>),
    Traceback(Arc<Vec<(Arc<super::trace::Frame>, u32)>>, usize),
    FrameSummary(Arc<super::trace::Frame>, u32),
    /// Starlette / FastAPI routing objects (see `routing`)
    Routing(Arc<super::routing::RObj>),
    /// prometheus_client objects (see `prom`)
    Prom(Arc<super::prom::Prom>),
    /// a coroutine object (see `aio`)
    Coro(Mutex<Option<super::BoxFut<'static>>>),
    /// `asyncio.Semaphore` / `asyncio.Lock`
    Sem(Arc<super::aio::Sem>),
}

pub struct PyFn {
    pub call: super::KwFn,
    pub is_async: bool,
    pub attrs: Mutex<Vec<(String, V)>>,
}

impl PyFn {
    /// attributes of the function object itself, not of its `__dict__`
    pub const SLOTS: [&'static str; 5] = ["__module__", "__name__", "__qualname__", "__doc__", "__annotations__"];
}

impl V {
    pub fn str(s: impl AsRef<str>) -> V {
        V::Str(Arc::from(s.as_ref()))
    }
    pub fn list(v: Vec<V>) -> V {
        V::List(Arc::new(Mutex::new(v)))
    }
    pub fn tuple(v: Vec<V>) -> V {
        V::Tuple(Arc::new(v))
    }
    pub fn dict_from(items: Vec<(V, V)>) -> R {
        let mut m = IndexMap::new();
        for (k, v) in items {
            m.insert(Key::of(&k)?, (k, v));
        }
        Ok(V::Dict(Arc::new(Mutex::new(m))))
    }
    pub fn empty_dict() -> V {
        V::Dict(Arc::new(Mutex::new(IndexMap::new())))
    }
    pub fn native(n: Native) -> V {
        V::Native(Arc::new(n))
    }
    pub fn is_none(&self) -> bool {
        matches!(self, V::None)
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            V::Unbound => "unbound",
            V::None => "NoneType",
            V::Bool(_) => "bool",
            V::Int(_) => "int",
            V::Float(_) => "float",
            V::Str(_) => "str",
            V::Bytes(_) => "bytes",
            V::List(_) => "list",
            V::Tuple(_) => "tuple",
            V::Dict(_) => "dict",
            V::Set(_) => "set",
            V::DateTime(_) => "datetime",
            V::Date(_) => "date",
            V::Time(_) => "time",
            V::Delta(_) => "timedelta",
            V::Tz(_) => "timezone",
            V::Class(c) => c.name,
            V::Inst(i) => i.desc.name,
            V::Obj(o) => o.desc.name,
            V::Enum(e, _) => e.name,
            V::Col(..) => "InstrumentedAttribute",
            V::Sql(_) => "ClauseElement",
            V::Session(_) => "AsyncSession",
            V::Result(_) => "Result",
            V::Exc(e) => e.0.class.name,
            V::Decimal(_) => "decimal.Decimal",
            V::Native(n) => match &**n {
                Native::Request(_) => "Request",
                Native::Headers(_) => "Headers",
                Native::State(_) => "State",
                Native::Url(_) => "URL",
                Native::Response(_) => "Response",
                Native::Logger(_) => "Logger",
                Native::Queue(_) => "Queue",
                Native::Hash(_) => "HASH",
                Native::Serializer(_) => "URLSafeTimedSerializer",
                Native::Fernet(_) => "Fernet",
                Native::RespObj(_) => "Response",
                Native::RespHeaders(_) | Native::CellHeaders(_) => "MutableHeaders",
                Native::CallNext(_) => "function",
                Native::Address(..) => "Address",
                Native::Totp(_) => "TOTP",
                Native::Task(_) => "Task",
                Native::HttpClient(_) => "AsyncClient",
                Native::HttpResp(_) => "Response",
                Native::HttpTimeout(_) => "Timeout",
                Native::HttpBasicAuth(_) => "BasicAuth",
                Native::HttpUrl(_) => "URL",
                Native::Savepoint(..) => "AsyncSessionTransaction",
                Native::Row(..) => "Row",
                Native::UrlParts(u) => if u.parse { "ParseResult" } else { "SplitResult" },
                Native::Record(n, _) => n,
                Native::Engine => "AsyncEngine",
                Native::IniConfig(_) => "Config",
                Native::PydUrl(n, _) => n,
                Native::MethodOf(..) => "builtin_function_or_method",
                Native::Pattern(_) => "re.Pattern",
                Native::Match(_) => "re.Match",
                Native::StringIO(_) => "StringIO",
                Native::CsvWriter(_) => "_csv.writer",
                Native::CsvRows(..) => "DictReader",
                Native::SnifferObj => "Sniffer",
                Native::Sniffed(_) => "Dialect",
                Native::Hmac(_) => "HMAC",
                Native::HashCtor(_) => "builtin_function_or_method",
                Native::Iter(_) => "iterator",
                Native::Jinja(_) => "Environment",
                Native::JinjaTpl(_) => "Template",
                Native::Mime(_) => "MIMEBase",
                Native::Tasks(_) => "BackgroundTasks",
                Native::BytesIO(_) => "BytesIO",
                Native::Path(_) => "PosixPath",
                Native::File(_) => "TextIOWrapper",
                Native::Uuid(_) => "UUID",
                Native::Upload(_) => "UploadFile",
                Native::Bound(..) => "method",
                Native::Kwargs(_) => "kwargs",
                Native::ValErr(..) => "ValErr",
                Native::Streaming(_) => "StreamingResponse",
                Native::Gen(_) => "async_generator",
                Native::Acm(_) => "_AsyncGeneratorContextManager",
                Native::Suppress(_) => "suppress",
                Native::CtxVar(_) => "ContextVar",
                Native::CtxToken(..) => "Token",
                Native::AsgiReceive(_) | Native::AsgiSend(_) => "function",
                Native::McpServer(_) => "MCPServer",
                Native::McpToolDeco(..) => "function",
                Native::McpManager(_) => "StreamableHTTPSessionManager",
                Native::McpRun => "_AsyncGeneratorContextManager",
                Native::Namespace(_) => "module",
                Native::Type(_) => "type",
                Native::FieldInfo => "FieldInfo",
                Native::Func(_) | Native::PyFn(_) => "function",
                Native::Coro(_) => "coroutine",
                Native::YarlUrl(_) => "URL",
                Native::AmqpConn(_) => "RobustConnection",
                Native::AmqpChan(_) => "RobustChannel",
                Native::AmqpQueue(..) => "RobustQueue",
                Native::AmqpExchange(..) => "Exchange",
                Native::AmqpMsg(_) => "Message",
                Native::AmqpIncoming(_) => "IncomingMessage",
                Native::Module(_) => "module",
                Native::Tenacity(t) => super::tenacity::type_name(t),
                Native::Prom(p) => super::prom::type_name(p),
                Native::Routing(o) => super::routing::type_name(o),
                Native::TraceFrame(_) => "frame",
                Native::TraceCode(_) => "code",
                Native::Traceback(..) => "traceback",
                Native::FrameSummary(..) => "FrameSummary",
                Native::Redis(_) => "Redis",
                Native::RRetry(_) => "Retry",
                Native::AsyncItems(_) => "async_generator",
                Native::Maker(_, _, false) => "async_sessionmaker",
                Native::Maker(_, _, true) => "sessionmaker",
                Native::Query(..) => "Query",
                Native::TLock(l) => if l.reentrant { "RLock" } else { "lock" },
                Native::TEvent(_) => "Event",
                Native::TThread(_) => "Thread",
                Native::ELoop(_) => "_UnixSelectorEventLoop",
                Native::Adapter(..) => "TypeAdapter",
                Native::ExtType(_) => "type",
                Native::TypeExpr(..) => "GenericAlias",
                Native::Sem(_) => "Semaphore",
            },
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            V::Str(s) => Some(s),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- hashable keys

/// The hashable projection of a value (dict keys, set members).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    None,
    Int(i64),
    Str(Arc<str>),
    Tuple(Vec<Key>),
    /// floats hash by bit pattern after normalising integral values to Int
    Float(u64),
    Date(chrono::NaiveDate),
    /// aware datetimes hash by UTC instant, naive ones by wall time
    DateTime(i64, bool),
    Ptr(usize),
}

impl Key {
    pub fn of(v: &V) -> R<Key> {
        Ok(match v {
            V::None => Key::None,
            V::Bool(b) => Key::Int(*b as i64),
            V::Int(i) => Key::Int(*i),
            V::Float(f) => {
                if f.fract() == 0.0 && f.abs() < 9.0e18 {
                    Key::Int(*f as i64)
                } else {
                    Key::Float(f.to_bits())
                }
            }
            V::Str(s) => Key::Str(s.clone()),
            V::Tuple(t) => Key::Tuple(t.iter().map(Key::of).collect::<R<Vec<_>>>()?),
            V::Date(d) => Key::Date(*d),
            V::DateTime(d) => Key::DateTime(d.key_micros(), d.tz.is_some()),
            V::Obj(o) => Key::Ptr(Arc::as_ptr(o) as *const () as usize),
            V::Enum(e, i) => match e.kind {
                EnumKind::Plain => Key::Ptr((*e as *const EnumDesc as usize).wrapping_add(*i as usize + 1)),
                _ => Key::of(&e.value(*i))?,
            },
            V::Inst(o) => match o.desc.hash {
                super::pyd::HashKind::Value => Key::Tuple(o.vals.lock().iter().map(Key::of).collect::<R<Vec<_>>>()?),
                super::pyd::HashKind::Id => Key::Ptr(Arc::as_ptr(o) as *const () as usize),
                super::pyd::HashKind::Unhashable => return Err(Exc::type_error(format!("unhashable type: '{}'", o.desc.name))),
            },
            V::Class(c) => Key::Ptr(*c as *const Class as usize),
            V::Native(n) if matches!(&**n, Native::Row(..)) => match &**n {
                Native::Row(_, vals) => Key::Tuple(vals.iter().map(Key::of).collect::<R<Vec<_>>>()?),
                _ => unreachable!(),
            },
            // hash(Decimal(2)) == hash(2)
            V::Decimal(d) if d.is_integral() => match num_traits::ToPrimitive::to_i64(&d.to_int()) {
                Some(i) => Key::Int(i),
                None => Key::Str(Arc::from(format!("\u{0}dec:{}", d))),
            },
            V::Decimal(d) => Key::Float(d.to_f64().to_bits()),
            V::Native(n) => match &**n {
                // value semantics (a Path equals a Path, never a str)
                Native::Path(p) => Key::Str(Arc::from(format!("\u{0}path:{p}"))),
                Native::Uuid(u) => Key::Str(Arc::from(format!("\u{0}uuid:{u:032x}"))),
                _ => Key::Ptr(Arc::as_ptr(n) as *const () as usize),
            },
            V::Col(m, i) => Key::Ptr((*m as *const orm::ModelDesc as usize) ^ (*i << 1)),
            other => return Err(Exc::type_error(format!("unhashable type: '{}'", other.type_name()))),
        })
    }
}

// ---------------------------------------------------------------- classes

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EnumKind {
    /// `class X(Enum)`: members are distinct from their values
    Plain,
    /// `class X(str, Enum)`: equal to their value, str() is `X.NAME`
    Str,
    /// `class X(StrEnum)`: equal to their value, str() is the value
    StrEnum,
    /// `class X(int, Enum)`
    Int,
    /// `class X(IntEnum)`: str() is the value
    IntEnum,
}

pub enum EV {
    Str(&'static str),
    Int(i64),
    Float(f64),
    Bool(bool),
    None,
}

pub struct EnumDesc {
    pub name: &'static str,
    pub class: &'static Class,
    pub kind: EnumKind,
    pub members: &'static [(&'static str, EV)],
    /// methods defined on the enum: (name, property, wrapper)
    pub methods: &'static [(&'static str, bool, super::pyd::MethodFn)],
    /// a custom `_missing_` classmethod
    pub missing: Option<super::pyd::MethodFn>,
}

impl EnumDesc {
    pub fn value(&self, i: u16) -> V {
        match &self.members[i as usize].1 {
            EV::Str(s) => V::str(*s),
            EV::Int(n) => V::Int(*n),
            EV::Float(f) => V::Float(*f),
            EV::Bool(b) => V::Bool(*b),
            EV::None => V::None,
        }
    }
    pub fn member_name(&self, i: u16) -> &'static str {
        self.members[i as usize].0
    }
    pub fn by_value(&'static self, v: &V) -> Option<V> {
        (0..self.members.len() as u16).find(|i| super::ops::eq_bool(&self.value(*i), v) && same_kind(&self.value(*i), v)).map(|i| V::Enum(self, i))
    }
    pub fn by_name(&'static self, n: &str) -> Option<V> {
        self.members.iter().position(|(m, _)| *m == n).map(|i| V::Enum(self, i as u16))
    }
    pub fn all(&'static self) -> Vec<V> {
        (0..self.members.len() as u16).map(|i| V::Enum(self, i)).collect()
    }
}

/// Enum lookup by value is type-strict: `2` does not match `"2"`.
fn same_kind(a: &V, b: &V) -> bool {
    matches!((a, b), (V::Str(_), V::Str(_)) | (V::Int(_), V::Int(_)) | (V::Float(_), V::Float(_)) | (V::Bool(_), V::Bool(_)) | (V::None, V::None) | (V::Int(_), V::Float(_)) | (V::Float(_), V::Int(_)))
}

pub enum ClassKind {
    /// Builtin exception class.
    Exception,
    /// Project exception class (`class Conflict(Exception)`): its body's attributes and methods.
    UserException(&'static ExcDesc),
    Model(&'static orm::ModelDesc),
    Schema(&'static pyd::SchemaDesc),
    Enum(&'static EnumDesc),
}

/// What a project exception class body defines.
pub struct ExcDesc {
    /// class attributes, evaluated once (at startup, like the class body at import)
    pub attrs: &'static [(&'static str, ClsAttr)],
    /// (name, is_property, function)
    pub methods: &'static [(&'static str, bool, super::pyd::MethodFn)],
    /// the methods that are `async def`s (a call not awaited is a coroutine)
    pub async_methods: &'static [&'static str],
}

pub type ClsAttr = for<'a> fn(&'a super::Cx) -> super::BoxFut<'a>;

impl Class {
    /// a class attribute or method of a project exception class, along the MRO (depth-first bases)
    pub fn exc_lookup(&'static self, name: &str) -> Option<ExcMember> {
        if let ClassKind::UserException(d) = &self.kind {
            if let Some((_, f)) = d.attrs.iter().find(|(n, _)| *n == name) {
                return Some(ExcMember::Attr(*f));
            }
            if let Some((_, prop, f)) = d.methods.iter().find(|(n, _, _)| *n == name) {
                return Some(ExcMember::Method(*prop, *f));
            }
        }
        self.bases.iter().find_map(|b| b.exc_lookup(name))
    }
}

pub enum ExcMember {
    Attr(ClsAttr),
    Method(bool, super::pyd::MethodFn),
}

pub struct Class {
    pub name: &'static str,
    pub qualname: &'static str,
    pub bases: &'static [&'static Class],
    pub kind: ClassKind,
}

impl Class {
    pub fn is_subclass(&'static self, other: &'static Class) -> bool {
        if std::ptr::eq(self, other) {
            return true;
        }
        self.bases.iter().any(|b| b.is_subclass(other))
    }
}

// ---------------------------------------------------------------- exceptions

pub struct ExcObj {
    pub class: &'static Class,
    pub args: Vec<V>,
    /// HTTPException(status_code, detail, headers)
    pub http: Option<(u16, V, Vec<(String, String)>)>,
    /// RequestValidationError / pydantic ValidationError
    pub errors: Option<Vec<pyd::ErrDetail>>,
    /// instance attributes set by a project exception's `__init__` (`self.code = ...`)
    pub attrs: Mutex<indexmap::IndexMap<String, V>>,
    /// `super().__init__(...)` in a project exception's `__init__` rebinds `args`
    pub new_args: Mutex<Option<Vec<V>>>,
    /// HTTPException.__init__ run by a project subclass's `__init__` (`super().__init__(status_code=...)`)
    pub http_late: Mutex<Option<(u16, V, Vec<(String, String)>)>>,
    /// the traced project frames it has left, innermost first (`sys.settrace`, see `trace`)
    pub tb: Mutex<Vec<(Arc<super::trace::Frame>, u32)>>,
}

/// A raised exception. Cheap to clone.
#[derive(Clone)]
pub struct Exc(pub Arc<ExcObj>);

impl std::fmt::Debug for Exc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0.class.name, self.message())?;
        if let Some(errs) = &self.0.errors {
            for e in errs {
                let loc: Vec<String> = e.loc.iter().map(|l| super::ops::str_(l).unwrap_or_default()).collect();
                write!(f, "\n  {}: {} [type={}]", loc.join("."), e.msg, e.kind)?;
            }
        }
        Ok(())
    }
}

macro_rules! builtin_exc {
    ($id:ident, $name:expr, [$($base:ident),*]) => {
        pub static $id: Class = Class {
            name: $name,
            qualname: $name,
            bases: &[$(&$base),*],
            kind: ClassKind::Exception,
        };
    };
}

builtin_exc!(BASE_EXCEPTION, "BaseException", []);
builtin_exc!(GENERATOR_EXIT, "GeneratorExit", [BASE_EXCEPTION]);
builtin_exc!(CANCELLED_ERROR, "CancelledError", [BASE_EXCEPTION]);
builtin_exc!(MCP_TOOL_ERROR, "ToolError", [EXCEPTION]);
builtin_exc!(EXCEPTION, "Exception", [BASE_EXCEPTION]);
builtin_exc!(VALUE_ERROR, "ValueError", [EXCEPTION]);
builtin_exc!(TYPE_ERROR, "TypeError", [EXCEPTION]);
builtin_exc!(ATTRIBUTE_ERROR, "AttributeError", [EXCEPTION]);
builtin_exc!(PICKLE_ERROR, "PickleError", [EXCEPTION]);
builtin_exc!(REDIS_ERROR, "RedisError", [EXCEPTION]);
builtin_exc!(AMQP_ERROR, "AMQPError", [EXCEPTION]);
builtin_exc!(TENACITY_RETRY_ERROR, "RetryError", [EXCEPTION]);
builtin_exc!(DUPLICATE_TIMESERIES, "DuplicateTimeseries", [VALUE_ERROR]);
builtin_exc!(AMQP_CONNECTION_ERROR, "AMQPConnectionError", [AMQP_ERROR]);
builtin_exc!(AMQP_QUEUE_EMPTY, "QueueEmpty", [AMQP_ERROR]);
builtin_exc!(REDIS_CONNECTION_ERROR, "ConnectionError", [REDIS_ERROR]);
builtin_exc!(REDIS_TIMEOUT_ERROR, "TimeoutError", [REDIS_ERROR]);
builtin_exc!(REDIS_DATA_ERROR, "DataError", [REDIS_ERROR]);
builtin_exc!(REDIS_RESPONSE_ERROR, "ResponseError", [REDIS_ERROR]);
builtin_exc!(EOF_ERROR, "EOFError", [EXCEPTION]);
builtin_exc!(PICKLING_ERROR, "PicklingError", [PICKLE_ERROR]);
builtin_exc!(UNPICKLING_ERROR, "UnpicklingError", [PICKLE_ERROR]);
builtin_exc!(LOOKUP_ERROR, "LookupError", [EXCEPTION]);
builtin_exc!(KEY_ERROR, "KeyError", [LOOKUP_ERROR]);
builtin_exc!(INDEX_ERROR, "IndexError", [LOOKUP_ERROR]);
builtin_exc!(NAME_ERROR, "NameError", [EXCEPTION]);
builtin_exc!(UNBOUND_LOCAL_ERROR, "UnboundLocalError", [NAME_ERROR]);
builtin_exc!(ARITHMETIC_ERROR, "ArithmeticError", [EXCEPTION]);
builtin_exc!(ZERO_DIVISION_ERROR, "ZeroDivisionError", [ARITHMETIC_ERROR]);
builtin_exc!(OVERFLOW_ERROR, "OverflowError", [ARITHMETIC_ERROR]);
builtin_exc!(RUNTIME_ERROR, "RuntimeError", [EXCEPTION]);
builtin_exc!(ASSERTION_ERROR, "AssertionError", [EXCEPTION]);
builtin_exc!(NOT_IMPLEMENTED_ERROR, "NotImplementedError", [RUNTIME_ERROR]);
builtin_exc!(OS_ERROR, "OSError", [EXCEPTION]);
builtin_exc!(TIMEOUT_ERROR, "TimeoutError", [OS_ERROR]);
builtin_exc!(CONNECTION_ERROR, "ConnectionError", [OS_ERROR]);
builtin_exc!(CONNECTION_REFUSED_ERROR, "ConnectionRefusedError", [CONNECTION_ERROR]);
builtin_exc!(CONNECTION_RESET_ERROR, "ConnectionResetError", [CONNECTION_ERROR]);
builtin_exc!(CONNECTION_ABORTED_ERROR, "ConnectionAbortedError", [CONNECTION_ERROR]);
builtin_exc!(BROKEN_PIPE_ERROR, "BrokenPipeError", [CONNECTION_ERROR]);
builtin_exc!(STOP_ITERATION, "StopIteration", [EXCEPTION]);
builtin_exc!(STOP_ASYNC_ITERATION, "StopAsyncIteration", [EXCEPTION]);
builtin_exc!(QUEUE_FULL, "QueueFull", [EXCEPTION]);
builtin_exc!(QUEUE_EMPTY, "QueueEmpty", [EXCEPTION]);
builtin_exc!(HTTP_EXCEPTION, "HTTPException", [EXCEPTION]);
builtin_exc!(REQUEST_VALIDATION_ERROR, "RequestValidationError", [VALUE_ERROR]);
builtin_exc!(VALIDATION_ERROR, "ValidationError", [VALUE_ERROR]);
builtin_exc!(SQLALCHEMY_ERROR, "SQLAlchemyError", [EXCEPTION]);
builtin_exc!(DBAPI_ERROR, "DBAPIError", [SQLALCHEMY_ERROR]);
builtin_exc!(INTEGRITY_ERROR, "IntegrityError", [DBAPI_ERROR]);
builtin_exc!(STATEMENT_ERROR, "StatementError", [SQLALCHEMY_ERROR]);
builtin_exc!(COMPILE_ERROR, "CompileError", [SQLALCHEMY_ERROR]);
builtin_exc!(OPERATIONAL_ERROR, "OperationalError", [DBAPI_ERROR]);
builtin_exc!(DATA_ERROR, "DataError", [DBAPI_ERROR]);
builtin_exc!(PROGRAMMING_ERROR, "ProgrammingError", [DBAPI_ERROR]);
builtin_exc!(INTERNAL_ERROR, "InternalError", [DBAPI_ERROR]);
builtin_exc!(NOT_SUPPORTED_ERROR, "NotSupportedError", [DBAPI_ERROR]);
builtin_exc!(NO_RESULT_FOUND, "NoResultFound", [SQLALCHEMY_ERROR]);
builtin_exc!(MULTIPLE_RESULTS_FOUND, "MultipleResultsFound", [SQLALCHEMY_ERROR]);
builtin_exc!(MISSING_GREENLET, "MissingGreenlet", [SQLALCHEMY_ERROR]);
builtin_exc!(INVALID_REQUEST_ERROR, "InvalidRequestError", [SQLALCHEMY_ERROR]);
builtin_exc!(OBJECT_DELETED_ERROR, "ObjectDeletedError", [INVALID_REQUEST_ERROR]);
builtin_exc!(ARGUMENT_ERROR, "ArgumentError", [SQLALCHEMY_ERROR]);
builtin_exc!(JOSE_ERROR, "JOSEError", [EXCEPTION]);
builtin_exc!(JWS_ERROR, "JWSError", [JOSE_ERROR]);
builtin_exc!(JWT_ERROR, "JWTError", [JOSE_ERROR]);
builtin_exc!(JWT_CLAIMS_ERROR, "JWTClaimsError", [JWT_ERROR]);
builtin_exc!(EXPIRED_SIGNATURE_ERROR, "ExpiredSignatureError", [JWT_ERROR]);
builtin_exc!(JWK_ERROR, "JWKError", [JOSE_ERROR]);
builtin_exc!(BAD_DATA, "BadData", [EXCEPTION]);
builtin_exc!(BAD_SIGNATURE, "BadSignature", [BAD_DATA]);
builtin_exc!(BAD_TIME_SIGNATURE, "BadTimeSignature", [BAD_SIGNATURE]);
builtin_exc!(SIGNATURE_EXPIRED, "SignatureExpired", [BAD_TIME_SIGNATURE]);
builtin_exc!(BAD_HEADER, "BadHeader", [BAD_SIGNATURE]);
builtin_exc!(BAD_PAYLOAD, "BadPayload", [BAD_DATA]);
builtin_exc!(INVALID_TOKEN, "InvalidToken", [EXCEPTION]);
builtin_exc!(TEMPLATE_NOT_FOUND, "TemplateNotFound", [OS_ERROR, LOOKUP_ERROR]);
builtin_exc!(SMTP_EXCEPTION, "SMTPException", [EXCEPTION]);
builtin_exc!(RE_ERROR, "error", [EXCEPTION]);
builtin_exc!(IMPORT_ERROR, "ImportError", [EXCEPTION]);
builtin_exc!(MODULE_NOT_FOUND_ERROR, "ModuleNotFoundError", [IMPORT_ERROR]);
builtin_exc!(FROZEN_INSTANCE_ERROR, "FrozenInstanceError", [ATTRIBUTE_ERROR]);
builtin_exc!(CSV_ERROR, "Error", [EXCEPTION]);
builtin_exc!(BINASCII_ERROR, "Error", [VALUE_ERROR]);
builtin_exc!(JSON_DECODE_ERROR, "JSONDecodeError", [VALUE_ERROR]);
// decimal
builtin_exc!(DECIMAL_EXCEPTION, "DecimalException", [ARITHMETIC_ERROR]);
builtin_exc!(DECIMAL_INVALID_OPERATION, "InvalidOperation", [DECIMAL_EXCEPTION]);
builtin_exc!(DECIMAL_CONVERSION_SYNTAX, "ConversionSyntax", [DECIMAL_INVALID_OPERATION]);
builtin_exc!(DECIMAL_DIVISION_BY_ZERO, "DivisionByZero", [DECIMAL_EXCEPTION, ZERO_DIVISION_ERROR]);
builtin_exc!(DECIMAL_DIVISION_UNDEFINED, "DivisionUndefined", [DECIMAL_INVALID_OPERATION, ZERO_DIVISION_ERROR]);
// httpx
builtin_exc!(HTTPX_HTTP_ERROR, "HTTPError", [EXCEPTION]);
builtin_exc!(HTTPX_REQUEST_ERROR, "RequestError", [HTTPX_HTTP_ERROR]);
builtin_exc!(HTTPX_TRANSPORT_ERROR, "TransportError", [HTTPX_REQUEST_ERROR]);
builtin_exc!(HTTPX_TIMEOUT_EXCEPTION, "TimeoutException", [HTTPX_TRANSPORT_ERROR]);
builtin_exc!(HTTPX_CONNECT_TIMEOUT, "ConnectTimeout", [HTTPX_TIMEOUT_EXCEPTION]);
builtin_exc!(HTTPX_READ_TIMEOUT, "ReadTimeout", [HTTPX_TIMEOUT_EXCEPTION]);
builtin_exc!(HTTPX_NETWORK_ERROR, "NetworkError", [HTTPX_TRANSPORT_ERROR]);
builtin_exc!(HTTPX_CONNECT_ERROR, "ConnectError", [HTTPX_NETWORK_ERROR]);
builtin_exc!(HTTPX_UNSUPPORTED_PROTOCOL, "UnsupportedProtocol", [HTTPX_TRANSPORT_ERROR]);
builtin_exc!(HTTPX_TOO_MANY_REDIRECTS, "TooManyRedirects", [HTTPX_REQUEST_ERROR]);
builtin_exc!(HTTPX_DECODING_ERROR, "DecodingError", [HTTPX_REQUEST_ERROR]);
builtin_exc!(HTTPX_STATUS_ERROR, "HTTPStatusError", [HTTPX_HTTP_ERROR]);
builtin_exc!(HTTPX_INVALID_URL, "InvalidURL", [EXCEPTION]);
// requests, pywebpush, py-vapid
builtin_exc!(REQUESTS_EXCEPTION, "RequestException", [OS_ERROR]);
builtin_exc!(REQUESTS_CONNECTION_ERROR, "ConnectionError", [REQUESTS_EXCEPTION]);
builtin_exc!(WEBPUSH_EXCEPTION, "WebPushException", [EXCEPTION]);
builtin_exc!(VAPID_EXCEPTION, "VapidException", [EXCEPTION]);
// google-auth
builtin_exc!(GOOGLE_AUTH_ERROR, "GoogleAuthError", [EXCEPTION]);
builtin_exc!(GOOGLE_TRANSPORT_ERROR, "TransportError", [GOOGLE_AUTH_ERROR]);
builtin_exc!(GOOGLE_DEFAULT_CREDENTIALS_ERROR, "DefaultCredentialsError", [GOOGLE_AUTH_ERROR]);
builtin_exc!(GOOGLE_MALFORMED_ERROR, "MalformedError", [GOOGLE_DEFAULT_CREDENTIALS_ERROR, VALUE_ERROR]);
builtin_exc!(GOOGLE_INVALID_VALUE, "InvalidValue", [GOOGLE_DEFAULT_CREDENTIALS_ERROR, VALUE_ERROR]);
// aiohttp
builtin_exc!(AIO_CLIENT_ERROR, "ClientError", [EXCEPTION]);
builtin_exc!(AIO_RESPONSE_ERROR, "ClientResponseError", [AIO_CLIENT_ERROR]);
builtin_exc!(AIO_CONTENT_TYPE_ERROR, "ContentTypeError", [AIO_RESPONSE_ERROR]);
builtin_exc!(AIO_TOO_MANY_REDIRECTS, "TooManyRedirects", [AIO_RESPONSE_ERROR]);
builtin_exc!(AIO_CONNECTION_ERROR, "ClientConnectionError", [AIO_CLIENT_ERROR]);
builtin_exc!(AIO_OS_ERROR, "ClientOSError", [AIO_CONNECTION_ERROR, OS_ERROR]);
builtin_exc!(AIO_CONNECTOR_ERROR, "ClientConnectorError", [AIO_OS_ERROR]);
builtin_exc!(AIO_SERVER_CONNECTION_ERROR, "ServerConnectionError", [AIO_CONNECTION_ERROR]);
builtin_exc!(AIO_SERVER_TIMEOUT, "ServerTimeoutError", [AIO_SERVER_CONNECTION_ERROR, TIMEOUT_ERROR]);
builtin_exc!(AIO_CONNECTION_TIMEOUT, "ConnectionTimeoutError", [AIO_SERVER_TIMEOUT]);
builtin_exc!(AIO_INVALID_URL, "InvalidURL", [AIO_CLIENT_ERROR, VALUE_ERROR]);
builtin_exc!(UNICODE_DECODE_ERROR, "UnicodeDecodeError", [VALUE_ERROR]);
builtin_exc!(FILE_NOT_FOUND_ERROR, "FileNotFoundError", [OS_ERROR]);
builtin_exc!(FILE_EXISTS_ERROR, "FileExistsError", [OS_ERROR]);
builtin_exc!(PERMISSION_ERROR, "PermissionError", [OS_ERROR]);
builtin_exc!(IS_A_DIRECTORY_ERROR, "IsADirectoryError", [OS_ERROR]);

impl Exc {
    pub fn new(class: &'static Class, args: Vec<V>) -> Exc {
        Exc(Arc::new(ExcObj { class, args, http: None, errors: None, attrs: Default::default(), new_args: Default::default(), http_late: Default::default(), tb: Default::default() }))
    }
    pub fn msg(class: &'static Class, msg: impl AsRef<str>) -> Exc {
        Exc::new(class, vec![V::str(msg)])
    }
    pub fn type_error(msg: impl AsRef<str>) -> Exc {
        Exc::msg(&TYPE_ERROR, msg)
    }
    pub fn value_error(msg: impl AsRef<str>) -> Exc {
        Exc::msg(&VALUE_ERROR, msg)
    }
    pub fn attr_error(msg: impl AsRef<str>) -> Exc {
        Exc::msg(&ATTRIBUTE_ERROR, msg)
    }
    pub fn runtime(msg: impl AsRef<str>) -> Exc {
        Exc::msg(&RUNTIME_ERROR, msg)
    }
    pub fn http(status: u16, detail: V, headers: Vec<(String, String)>) -> Exc {
        Exc(Arc::new(ExcObj {
            class: &HTTP_EXCEPTION,
            args: vec![V::Int(status as i64), detail.clone()],
            http: Some((status, detail, headers)),
            errors: None,
            attrs: Default::default(),
            new_args: Default::default(),
            http_late: Default::default(),
            tb: Default::default(),
        }))
    }
    pub fn validation(class: &'static Class, errors: Vec<pyd::ErrDetail>) -> Exc {
        Exc(Arc::new(ExcObj { class, args: vec![], http: None, errors: Some(errors), attrs: Default::default(), new_args: Default::default(), http_late: Default::default(), tb: Default::default() }))
    }
    /// `exc.args` (rebound by `super().__init__(...)` in a project exception)
    pub fn args(&self) -> Vec<V> {
        self.0.new_args.lock().clone().unwrap_or_else(|| self.0.args.clone())
    }
    pub fn isinstance(&self, class: &'static Class) -> bool {
        self.0.class.is_subclass(class)
    }
    /// HTTPException's (status_code, detail, headers), set at construction or by `super().__init__`
    pub fn http_info(&self) -> Option<(u16, V, Vec<(String, String)>)> {
        self.0.http.clone().or_else(|| self.0.http_late.lock().clone())
    }

    /// `str(exc)`: the single argument, or the tuple of arguments, like CPython.
    pub fn message(&self) -> String {
        if let Some(errs) = &self.0.errors {
            // the model's name, when the raiser knew it (pydantic's `ValidationError.title`)
            if let Some(V::Str(t)) = self.0.attrs.lock().get("title") {
                return super::pyd::error_str(t, errs);
            }
            return format!("{} validation errors", errs.len());
        }
        if let Some((code, detail, _)) = &self.http_info() {
            return format!("{}: {}", code, super::ops::str_(detail).unwrap_or_default());
        }
        let args = self.args();
        if std::ptr::eq(self.0.class, &WEBPUSH_EXCEPTION) {
            // WebPushException.__str__
            let msg = args.first().map(|a| super::ops::str_(a).unwrap_or_default()).unwrap_or_default();
            let extra = match self.0.attrs.lock().get("response") {
                Some(r) if !r.is_none() => format!(", Response {}", super::http::text_of(r)),
                _ => String::new(),
            };
            return format!("WebPushException: {msg}{extra}");
        }
        match args.len() {
            0 => String::new(),
            // KeyError.__str__ is the repr of its single argument
            1 if self.0.class.is_subclass(&KEY_ERROR) => super::ops::repr(&args[0]).unwrap_or_default(),
            1 => super::ops::str_(&args[0]).unwrap_or_default(),
            _ => super::ops::repr(&V::tuple(args)).unwrap_or_default(),
        }
    }
}

impl From<sqlx::Error> for Exc {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            // psycopg's SQLSTATE class -> DBAPI exception, as SQLAlchemy wraps it
            let code = db.code().map(|c| c.to_string()).unwrap_or_default();
            let (class, name): (&'static Class, &str) = match code.get(..2).unwrap_or("") {
                "23" => (&INTEGRITY_ERROR, "IntegrityError"),
                "22" => (&DATA_ERROR, "DataError"),
                "42" | "2B" | "2D" | "2F" | "34" | "3D" | "3F" | "44" => (&PROGRAMMING_ERROR, "ProgrammingError"),
                "0A" => (&NOT_SUPPORTED_ERROR, "NotSupportedError"),
                "XX" | "2C" | "39" | "3B" => (&INTERNAL_ERROR, "InternalError"),
                _ => (&OPERATIONAL_ERROR, "OperationalError"),
            };
            return Exc::msg(class, format!("(psycopg.errors.{name}) {}", db.message()));
        }
        Exc::msg(&OPERATIONAL_ERROR, format!("(psycopg.OperationalError) {e}"))
    }
}
