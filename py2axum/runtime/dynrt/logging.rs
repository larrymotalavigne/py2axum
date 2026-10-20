//! The `logging` module of CPython 3.12–3.14, as the project uses it: the logger hierarchy (levels,
//! effective levels, `propagate`, `disabled`, placeholders), handlers (`StreamHandler` on stdout/stderr,
//! `FileHandler`, `NullHandler`, `lastResort`), `Formatter` (styles `%`, `{`, `$`, every `LogRecord`
//! attribute, `datefmt`, `defaults`, exceptions), filters (`logging.Filter`, a project subclass, a
//! callable), `LoggerAdapter`, `basicConfig`, `dictConfig`, python-json-logger's `JsonFormatter`, and the
//! logging uvicorn configures before importing the application (`LOGGING_CONFIG`, its formatters, the
//! server's messages and access log: see `uvicorn_*`).
//!
//! A record's `pathname`/`filename`/`module`/`lineno`/`funcName` are the Python call site's, known at
//! translation (`Loc`). Formatting, filters and handlers run only for a record that passes the logger's
//! level: a disabled call costs the level check, as in CPython.
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};

use super::methods::{call_method, call_value};
use super::ops;
use super::v::*;
use super::Cx;

// ---------------------------------------------------------------- source locations

/// A logging call site in the Python source (CPython's `findCaller`): file relative to the source root,
/// line, function name (`co_name`: `<module>` at module level).
pub struct Loc {
    pub file: &'static str,
    pub line: u32,
    pub func: &'static str,
    /// the call's source and carets, as a traceback prints this frame (py2axum/tbfmt.py)
    pub text: &'static str,
}

/// a record made outside project code (CPython: no caller frame found)
pub static NO_LOC: Loc = Loc { file: "", line: 0, func: "", text: "" };

static SOURCE_ROOT: OnceLock<String> = OnceLock::new();

/// The directory `pathname` is built from: PY2AXUM_SOURCE_ROOT, else the project root at translation
/// (where the Python process imports the project from).
pub fn set_source_root(default: &str) {
    let root = std::env::var("PY2AXUM_SOURCE_ROOT").unwrap_or_else(|_| default.to_string());
    let _ = SOURCE_ROOT.set(root.trim_end_matches('/').to_string());
}

/// a project file's path as the Python process sees it (the source root + its path relative to --root)
pub fn source_path(file: &str) -> String {
    if file.starts_with('/') {
        return file.to_string();
    }
    match SOURCE_ROOT.get() {
        Some(r) if !r.is_empty() => format!("{r}/{file}"),
        _ => file.to_string(),
    }
}

fn pathname(loc: &Loc) -> String {
    if loc.file.is_empty() {
        return "(unknown file)".into();
    }
    if loc.file.starts_with('/') {
        return loc.file.to_string();
    }
    match SOURCE_ROOT.get() {
        Some(r) if !r.is_empty() => format!("{r}/{}", loc.file),
        _ => loc.file.to_string(),
    }
}

// ---------------------------------------------------------------- levels

pub const NOTSET: i64 = 0;
pub const DEBUG: i64 = 10;
pub const INFO: i64 = 20;
pub const WARNING: i64 = 30;
pub const ERROR: i64 = 40;
pub const CRITICAL: i64 = 50;

static LEVEL_NAMES: LazyLock<RwLock<Vec<(i64, String)>>> = LazyLock::new(|| {
    RwLock::new(
        [(CRITICAL, "CRITICAL"), (ERROR, "ERROR"), (WARNING, "WARNING"), (INFO, "INFO"), (DEBUG, "DEBUG"), (NOTSET, "NOTSET")]
            .iter()
            .map(|(l, n)| (*l, n.to_string()))
            .collect(),
    )
});
/// `_nameToLevel` extras: WARN, FATAL (and names added by addLevelName)
static NAME_LEVELS: LazyLock<RwLock<Vec<(String, i64)>>> = LazyLock::new(|| {
    RwLock::new(
        [("CRITICAL", CRITICAL), ("FATAL", 50), ("ERROR", ERROR), ("WARN", 30), ("WARNING", WARNING), ("INFO", INFO), ("DEBUG", DEBUG), ("NOTSET", NOTSET)]
            .iter()
            .map(|(n, l)| (n.to_string(), *l))
            .collect(),
    )
});

pub fn level_name(level: i64) -> String {
    LEVEL_NAMES.read().iter().find(|(l, _)| *l == level).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("Level {level}"))
}

fn level_of_name(name: &str) -> Option<i64> {
    NAME_LEVELS.read().iter().find(|(n, _)| n == name).map(|(_, l)| *l)
}

/// `logging.addLevelName(level, name)`
pub fn add_level_name(level: i64, name: &str) {
    let mut ln = LEVEL_NAMES.write();
    match ln.iter_mut().find(|(l, _)| *l == level) {
        Some(e) => e.1 = name.to_string(),
        None => ln.push((level, name.to_string())),
    }
    let mut nl = NAME_LEVELS.write();
    match nl.iter_mut().find(|(n, _)| n == name) {
        Some(e) => e.1 = level,
        None => nl.push((name.to_string(), level)),
    }
}

/// `logging._checkLevel`: an int, or a registered level name
fn check_level(v: &V) -> R<i64> {
    match v {
        V::Int(i) => Ok(*i),
        V::Bool(b) => Ok(*b as i64),
        V::Str(s) => level_of_name(s).ok_or_else(|| Exc::value_error(format!("Unknown level: {}", ops::str_repr(s)))),
        o => Err(Exc::type_error(format!("Level not an integer or a valid string: {}", ops::repr(o)?))),
    }
}

/// `logging.getLevelName(level)`: a name for a number, a number for a name, else "Level %s"
pub fn get_level_name(v: &V) -> R {
    if let V::Str(s) = v {
        if let Some(l) = level_of_name(s) {
            return Ok(V::Int(l));
        }
    }
    if let V::Int(i) = v {
        if let Some((_, n)) = LEVEL_NAMES.read().iter().find(|(l, _)| l == i) {
            return Ok(V::str(n));
        }
    }
    Ok(V::str(format!("Level {}", ops::str_(v)?)))
}

// ---------------------------------------------------------------- loggers

/// incremented whenever a level changes (CPython's `Manager._clear_cache`)
static GENERATION: AtomicU64 = AtomicU64::new(1);
/// `logging.disable(level)`
static DISABLE: AtomicI64 = AtomicI64::new(0);

pub struct Logger {
    pub name: Arc<str>,
    level: AtomicI64,
    /// (generation << 32) | effective level (cache of `getEffectiveLevel`, levels fit in 32 bits)
    eff: AtomicU64,
    propagate: AtomicBool,
    disabled: AtomicBool,
    /// created only as the ancestor of a named logger (CPython's PlaceHolder): not a logger of its own yet
    placeholder: AtomicBool,
    /// lists, shared with the code that reads them (`logger.handlers.clear()` affects the logger)
    handlers: RwLock<V>,
    filters: RwLock<V>,
    parent: Option<Arc<Logger>>,
}

impl Logger {
    fn new(name: &str, level: i64, parent: Option<Arc<Logger>>) -> Arc<Logger> {
        Arc::new(Logger {
            name: Arc::from(name),
            level: AtomicI64::new(level),
            eff: AtomicU64::new(0),
            propagate: AtomicBool::new(true),
            disabled: AtomicBool::new(false),
            placeholder: AtomicBool::new(false),
            handlers: RwLock::new(V::list(vec![])),
            filters: RwLock::new(V::list(vec![])),
            parent,
        })
    }

    pub fn level(&self) -> i64 {
        self.level.load(Ordering::Relaxed)
    }

    pub fn set_level(&self, level: i64) {
        self.level.store(level, Ordering::Relaxed);
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    /// `getEffectiveLevel()`, cached until a level changes anywhere
    pub fn effective_level(&self) -> i64 {
        let g = GENERATION.load(Ordering::Relaxed);
        let c = self.eff.load(Ordering::Relaxed);
        if c >> 32 == g & 0xffff_ffff {
            return (c as u32 as i32) as i64;
        }
        let mut l: Option<&Logger> = Some(self);
        let mut lvl = NOTSET;
        while let Some(x) = l {
            let v = x.level();
            if v != NOTSET {
                lvl = v;
                break;
            }
            l = x.parent.as_deref();
        }
        self.eff.store(((g & 0xffff_ffff) << 32) | (lvl as i32 as u32 as u64), Ordering::Relaxed);
        lvl
    }

    /// `isEnabledFor(level)`: the cheap check every logging call starts with
    #[inline]
    pub fn enabled_for(&self, level: i64) -> bool {
        if self.disabled.load(Ordering::Relaxed) || DISABLE.load(Ordering::Relaxed) >= level {
            return false;
        }
        level >= self.effective_level()
    }

    fn handler_list(&self) -> Vec<Arc<Handler>> {
        let h = self.handlers.read().clone();
        items(&h).iter().filter_map(as_handler).collect()
    }

    pub fn has_handlers(&self) -> bool {
        let mut l: Option<&Logger> = Some(self);
        while let Some(x) = l {
            if !items(&x.handlers.read()).is_empty() {
                return true;
            }
            if !x.propagate.load(Ordering::Relaxed) {
                return false;
            }
            l = x.parent.as_deref();
        }
        false
    }

    fn add_handler(&self, h: V) {
        let hs = self.handlers.read().clone();
        if let V::List(l) = &hs {
            let mut l = l.lock();
            if !l.iter().any(|x| same(x, &h)) {
                l.push(h);
            }
        } else {
            let mut v = items(&hs);
            if !v.iter().any(|x| same(x, &h)) {
                v.push(h);
            }
            *self.handlers.write() = V::list(v);
        }
    }

    fn remove_handler(&self, h: &V) {
        let hs = self.handlers.read().clone();
        if let V::List(l) = &hs {
            l.lock().retain(|x| !same(x, h));
        }
    }
}

fn same(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Native(x), V::Native(y)) => Arc::ptr_eq(x, y),
        (V::Inst(x), V::Inst(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

fn items(v: &V) -> Vec<V> {
    match v {
        V::List(l) => l.lock().clone(),
        V::Tuple(t) => t.to_vec(),
        _ => vec![],
    }
}

fn as_handler(v: &V) -> Option<Arc<Handler>> {
    match v {
        V::Native(n) => match &**n {
            Native::Log(LObj::Handler(h)) => Some(h.clone()),
            _ => None,
        },
        _ => None,
    }
}

struct Registry {
    loggers: RwLock<HashMap<String, Arc<Logger>>>,
}

static ROOT: LazyLock<Arc<Logger>> = LazyLock::new(|| Logger::new("root", WARNING, None));
static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Registry { loggers: RwLock::new(HashMap::new()) });

pub fn root() -> Arc<Logger> {
    ROOT.clone()
}

/// `logging.getLogger(name)`: the root for no name or "root"; ancestors ("a" and "a.b" for "a.b.c")
/// exist as placeholders until asked for themselves
pub fn get_logger(name: &str) -> Arc<Logger> {
    if name.is_empty() || name == "root" {
        return root();
    }
    if let Some(l) = REGISTRY.loggers.read().get(name) {
        if l.placeholder.load(Ordering::Relaxed) {
            l.placeholder.store(false, Ordering::Relaxed);
        }
        return l.clone();
    }
    let l = make_logger(name);
    l.placeholder.store(false, Ordering::Relaxed);
    l
}

fn make_logger(name: &str) -> Arc<Logger> {
    let parent = match name.rfind('.') {
        Some(i) if i > 0 => {
            let pn = &name[..i];
            let existing = REGISTRY.loggers.read().get(pn).cloned();
            match existing {
                Some(p) => p,
                None => {
                    let p = make_logger(pn);
                    p
                }
            }
        }
        _ => root(),
    };
    let mut reg = REGISTRY.loggers.write();
    if let Some(l) = reg.get(name) {
        return l.clone();
    }
    let l = Logger::new(name, NOTSET, Some(parent));
    l.placeholder.store(true, Ordering::Relaxed);
    reg.insert(name.to_string(), l.clone());
    l
}

pub fn logger_value(l: Arc<Logger>) -> V {
    V::native(Native::Log(LObj::Logger(l)))
}

/// `logging.getLogger(name=None)` from Python
pub fn get_logger_v(args: &[V], kwargs: &[(String, V)]) -> R {
    let name = args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "name").map(|(_, v)| v));
    match name {
        None | Some(V::None) => Ok(logger_value(root())),
        Some(V::Str(s)) => Ok(logger_value(get_logger(s))),
        Some(_) => Err(Exc::type_error("A logger name must be a string")),
    }
}

// ---------------------------------------------------------------- objects

/// The logging objects as Python values.
pub enum LObj {
    Logger(Arc<Logger>),
    Handler(Arc<Handler>),
    Formatter(Arc<Formatter>),
    Record(Arc<Record>),
    Filter(Arc<str>),
    Adapter(Arc<Adapter>),
    /// `sys.stdout` (1) / `sys.stderr` (2)
    Stream(u8),
}

pub fn type_name(o: &LObj) -> &'static str {
    match o {
        LObj::Logger(l) if Arc::ptr_eq(l, &ROOT) => "RootLogger",
        LObj::Logger(_) => "Logger",
        LObj::Handler(h) => h.kind.class_name(),
        LObj::Formatter(f) => match f.kind {
            FKind::Std => "Formatter",
            FKind::UvDefault(_) => "DefaultFormatter",
            FKind::UvAccess(_) => "AccessFormatter",
            FKind::Json(_) => "JsonFormatter",
            FKind::Ecs(_) => "StdlibFormatter",
        },
        LObj::Record(_) => "LogRecord",
        LObj::Filter(_) => "Filter",
        LObj::Adapter(_) => "LoggerAdapter",
        LObj::Stream(_) => "TextIOWrapper",
    }
}

pub fn repr(o: &LObj) -> String {
    match o {
        LObj::Logger(l) if Arc::ptr_eq(l, &ROOT) => format!("<RootLogger root ({})>", level_name(l.level())),
        LObj::Logger(l) => format!("<Logger {} ({})>", l.name, level_name(l.effective_level())),
        LObj::Handler(h) => match &h.kind {
            HKind::Stream(s) => format!("<StreamHandler {} ({})>", if *s == 1 { "<stdout>" } else { "<stderr>" }, level_name(h.level())),
            HKind::File(f) => format!("<FileHandler {} ({})>", f.path, level_name(h.level())),
            HKind::Null => format!("<NullHandler ({})>", level_name(h.level())),
        },
        LObj::Record(r) => {
            let g = r.d.lock();
            format!(
                "<LogRecord: {}, {}, {}, {}, \"{}\">",
                g.get("name").map(|v| ops::str_(v).unwrap_or_default()).unwrap_or_default(),
                g.get("levelno").map(|v| ops::str_(v).unwrap_or_default()).unwrap_or_default(),
                g.get("pathname").map(|v| ops::str_(v).unwrap_or_default()).unwrap_or_default(),
                g.get("lineno").map(|v| ops::str_(v).unwrap_or_default()).unwrap_or_default(),
                g.get("msg").map(|v| ops::str_(v).unwrap_or_default()).unwrap_or_default()
            )
        }
        LObj::Adapter(a) => format!("<LoggerAdapter {} ({})>", a.logger.name, level_name(a.logger.effective_level())),
        LObj::Stream(1) => "<_io.TextIOWrapper name='<stdout>' mode='w' encoding='utf-8'>".into(),
        LObj::Stream(_) => "<_io.TextIOWrapper name='<stderr>' mode='w' encoding='utf-8'>".into(),
        o => format!("<{} object>", type_name(o)),
    }
}

// ---------------------------------------------------------------- records

/// A `LogRecord`: its `__dict__`, in CPython's order (the standard attributes, then `extra`, then what
/// filters and formatters add).
pub struct Record {
    pub d: Mutex<IndexMap<&'static str, V>>,
    /// attributes named at run time (`extra`, a filter's `record.x = ...`)
    pub more: Mutex<IndexMap<String, V>>,
    /// the exception of `exc_info` (formatted with its traceback)
    pub exc: Option<Exc>,
    /// the call site's source (`handleError`'s call stack)
    pub loc_text: Option<&'static str>,
}

/// the standard attributes a record always has (`extra` may not overwrite them)
const STD_ATTRS: &[&str] = &[
    "name", "msg", "args", "levelname", "levelno", "pathname", "filename", "module", "exc_info", "exc_text", "stack_info", "lineno",
    "funcName", "created", "msecs", "relativeCreated", "thread", "threadName", "processName", "process", "taskName",
];

static START: LazyLock<f64> = LazyLock::new(now);

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `logging._startTime` (the module's import: the binary's start)
pub fn init_clock() {
    LazyLock::force(&START);
}

fn thread_ident() -> i64 {
    thread_local! {
        static ID: i64 = {
            static NEXT: AtomicI64 = AtomicI64::new(0);
            // a stable, large id per OS thread (`threading.get_ident()` returns a pthread address)
            0x7000_0000_0000 + NEXT.fetch_add(1, Ordering::Relaxed) * 0x1000
        };
    }
    ID.with(|i| *i)
}

impl Record {
    fn get(&self, name: &str) -> Option<V> {
        if let Some(v) = self.d.lock().get(name) {
            return Some(v.clone());
        }
        self.more.lock().get(name).cloned()
    }

    pub fn set(&self, name: &str, v: V) {
        {
            let mut d = self.d.lock();
            if let Some(slot) = d.get_mut(name) {
                *slot = v;
                return;
            }
        }
        self.more.lock().insert(name.to_string(), v);
    }

    fn has(&self, name: &str) -> bool {
        self.d.lock().contains_key(name) || self.more.lock().contains_key(name)
    }

    /// `record.__dict__` as (name, value) pairs, in order
    fn dict_items(&self) -> Vec<(String, V)> {
        let mut out: Vec<(String, V)> = self.d.lock().iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        out.extend(self.more.lock().iter().map(|(k, v)| (k.clone(), v.clone())));
        out
    }

    fn levelno(&self) -> i64 {
        match self.get("levelno") {
            Some(V::Int(i)) => i,
            _ => 0,
        }
    }

    fn name(&self) -> String {
        self.get("name").map(|v| ops::str_(&v).unwrap_or_default()).unwrap_or_default()
    }

    /// `getMessage()`: `str(msg) % args` when there are args
    pub fn get_message(&self) -> R<String> {
        let msg = self.get("msg").unwrap_or(V::None);
        let s = ops::str_(&msg)?;
        match self.get("args") {
            Some(a) if ops::truthy(&a)? => ops::percent_format(&s, &a),
            _ => Ok(s),
        }
    }
}

/// `Logger.makeRecord(...)` (LogRecord.__init__ + the `extra` mapping)
#[allow(clippy::too_many_arguments)]
fn make_record(cx: &Cx, name: &str, level: i64, loc: &'static Loc, msg: V, args: Vec<V>, exc_info: V, exc: Option<Exc>, extra: Option<V>, sinfo: V) -> R<Arc<Record>> {
    let created = now();
    let path = pathname(loc);
    let filename = path.rsplit('/').next().unwrap_or(&path).to_string();
    let module = filename.strip_suffix(".py").unwrap_or(&filename).to_string();
    // a single non-empty mapping is the args (`log.info("%(a)s", {"a": 1})`)
    let args = match args.as_slice() {
        [d @ V::Dict(m)] if !m.lock().is_empty() => d.clone(),
        _ => V::tuple(args),
    };
    let nanos = (created * 1e9) as i128;
    let msecs = ((nanos % 1_000_000_000) / 1_000_000) as f64;
    let mut d: IndexMap<&'static str, V> = IndexMap::with_capacity(24);
    d.insert("name", V::str(name));
    d.insert("msg", msg);
    d.insert("args", args);
    d.insert("levelname", V::str(level_name(level)));
    d.insert("levelno", V::Int(level));
    d.insert("pathname", V::str(&path));
    d.insert("filename", V::str(&filename));
    d.insert("module", V::str(&module));
    d.insert("exc_info", exc_info);
    d.insert("exc_text", V::None);
    d.insert("stack_info", sinfo);
    d.insert("lineno", V::Int(loc.line as i64));
    d.insert("funcName", if loc.func.is_empty() { V::str("(unknown function)") } else { V::str(loc.func) });
    d.insert("created", V::Float(created));
    d.insert("msecs", V::Float(msecs));
    d.insert("relativeCreated", V::Float((created - *START) * 1000.0));
    d.insert("thread", V::Int(thread_ident()));
    d.insert("threadName", V::str(thread_name(cx)));
    d.insert("processName", V::str("MainProcess"));
    d.insert("process", V::Int(std::process::id() as i64));
    if super::python().1 >= 12 {
        d.insert("taskName", task_name(cx));
    }
    let rec = Record { d: Mutex::new(d), more: Mutex::new(IndexMap::new()), exc, loc_text: Some(loc.text).filter(|t| !t.is_empty()) };
    if let Some(e) = extra {
        if !e.is_none() {
            for (k, v) in mapping_items(&e)? {
                let k = ops::str_(&k)?;
                if k == "message" || k == "asctime" || rec.has(&k) {
                    return Err(Exc::new(&KEY_ERROR, vec![V::str(format!("Attempt to overwrite {} in LogRecord", ops::str_repr(&k)))]));
                }
                rec.more.lock().insert(k, v);
            }
        }
    }
    Ok(Arc::new(rec))
}

fn mapping_items(v: &V) -> R<Vec<(V, V)>> {
    match v {
        V::Dict(m) => Ok(m.lock().values().cloned().collect()),
        o => Err(Exc::type_error(format!("'{}' object is not a mapping", o.type_name()))),
    }
}

/// `threading.current_thread().name`: uvicorn's event loop runs in the main thread; a `def` endpoint in
/// AnyIO's worker threads
fn thread_name(cx: &Cx) -> &'static str {
    if cx.in_worker_thread() {
        "AnyIO worker thread"
    } else {
        "MainThread"
    }
}

/// `asyncio.current_task().get_name()` (3.12+): the request's task under uvicorn, None at import
fn task_name(cx: &Cx) -> V {
    if cx.is_root() || cx.in_worker_thread() {
        V::None
    } else {
        V::str(format!("Task-{}", cx.task_number()))
    }
}

pub fn record_value(r: Arc<Record>) -> V {
    V::native(Native::Log(LObj::Record(r)))
}

fn as_record(v: &V) -> Option<Arc<Record>> {
    match v {
        V::Native(n) => match &**n {
            Native::Log(LObj::Record(r)) => Some(r.clone()),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------- formatters

#[derive(Clone, Copy, PartialEq)]
pub enum Style {
    Percent,
    Brace,
    Dollar,
}

/// One piece of a `%`-style format: literal text, or `%(name)spec`
enum Seg {
    Lit(String),
    Field(String, String),
}

pub enum FKind {
    Std,
    /// uvicorn.logging.DefaultFormatter(use_colors)
    UvDefault(bool),
    /// uvicorn.logging.AccessFormatter(use_colors)
    UvAccess(bool),
    /// pythonjsonlogger's JsonFormatter
    Json(Box<JsonOpts>),
    /// ecs_logging's StdlibFormatter
    Ecs(Box<EcsOpts>),
}

pub struct EcsOpts {
    extra: Option<V>,
    exclude: Vec<String>,
    stack_trace_limit: Option<i64>,
    ensure_ascii: bool,
}

pub struct JsonOpts {
    required: Vec<String>,
    rename: Vec<(String, String)>,
    static_fields: Vec<(String, V)>,
    defaults: Vec<(String, V)>,
    reserved: Vec<String>,
    prefix: String,
    timestamp: Option<String>,
    keep_missing: bool,
    ensure_ascii: bool,
    indent: V,
}

pub struct Formatter {
    pub kind: FKind,
    fmt: String,
    style: Style,
    segs: Option<Vec<Seg>>,
    datefmt: RwLock<Option<String>>,
    defaults: Option<V>,
    uses_time: bool,
    default_time_format: RwLock<String>,
    default_msec_format: RwLock<Option<String>>,
}

const BASIC_FORMAT: &str = "%(levelname)s:%(name)s:%(message)s";

fn default_fmt(style: Style) -> &'static str {
    match style {
        Style::Percent => "%(message)s",
        Style::Brace => "{message}",
        Style::Dollar => "${message}",
    }
}

fn parse_style(v: Option<&V>) -> R<Style> {
    match v.map(|v| ops::str_(v)).transpose()?.as_deref() {
        None | Some("%") => Ok(Style::Percent),
        Some("{") => Ok(Style::Brace),
        Some("$") => Ok(Style::Dollar),
        Some(_) => Err(Exc::value_error("Style must be one of: %,{,$")),
    }
}

/// `%(name)` followed by `[#0+ -]*(\*|\d+)?(\.(\*|\d+))?[diouxefgcrsa%]` (case-insensitive)
fn percent_spec(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < b.len() && b"#0+ -".contains(&b[i]) {
        i += 1;
    }
    if i < b.len() && b[i] == b'*' {
        i += 1;
    } else {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if i < b.len() && b[i] == b'*' {
            i += 1;
        } else {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    if i < b.len() && b"diouxefgcrsaDIOUXEFGCRSA%".contains(&b[i]) {
        Some(i + 1)
    } else {
        None
    }
}

/// the segments of a `%` format, or None when it holds a directive other than `%(name)...` and `%%`
/// (formatted then by `str % dict` as a whole, with its errors)
fn parse_percent(fmt: &str) -> Option<Vec<Seg>> {
    let b = fmt.as_bytes();
    let mut out = vec![];
    let mut lit = String::new();
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        if b[i] != b'%' {
            i += 1;
            continue;
        }
        lit += &fmt[last..i];
        if b.get(i + 1) == Some(&b'%') {
            lit.push('%');
            i += 2;
            last = i;
            continue;
        }
        if b.get(i + 1) != Some(&b'(') {
            return None;
        }
        let close = fmt[i + 2..].find(')')? + i + 2;
        let name = fmt[i + 2..close].to_string();
        let n = percent_spec(&b[close + 1..])?;
        let spec = fmt[close + 1..close + 1 + n].to_string();
        if spec.contains('*') {
            return None;
        }
        if !lit.is_empty() {
            out.push(Seg::Lit(std::mem::take(&mut lit)));
        }
        out.push(Seg::Field(name, spec));
        i = close + 1 + n;
        last = i;
    }
    lit += &fmt[last..];
    if !lit.is_empty() {
        out.push(Seg::Lit(lit));
    }
    Some(out)
}

impl Formatter {
    /// `Formatter(fmt=None, datefmt=None, style='%', validate=True, *, defaults=None)`
    pub fn new(kind: FKind, fmt: Option<String>, datefmt: Option<String>, style: Style, validate: bool, defaults: Option<V>) -> R<Formatter> {
        let fmt = fmt.unwrap_or_else(|| default_fmt(style).to_string());
        if validate {
            validate_fmt(&fmt, style)?;
        }
        let uses_time = match style {
            Style::Percent => fmt.contains("%(asctime)"),
            Style::Brace => fmt.contains("{asctime"),
            Style::Dollar => fmt.contains("$asctime") || fmt.contains("${asctime}"),
        };
        let segs = if style == Style::Percent { parse_percent(&fmt) } else { None };
        Ok(Formatter {
            kind,
            fmt,
            style,
            segs,
            datefmt: RwLock::new(datefmt),
            defaults,
            uses_time,
            default_time_format: RwLock::new("%Y-%m-%d %H:%M:%S".into()),
            default_msec_format: RwLock::new(Some("%s,%03d".into())),
        })
    }

    /// `formatTime(record, datefmt)`: `time.localtime(created)` through strftime, `,mmm` by default
    fn format_time(&self, created: f64, msecs: f64, datefmt: Option<&str>) -> String {
        let (wall, off, zone) = localtime(created);
        match datefmt {
            Some(f) => super::libs::strftime_lenient(&wall, Some(off), Some(zone), f),
            None => {
                let s = super::libs::strftime_lenient(&wall, Some(off), Some(zone), &self.default_time_format.read());
                match &*self.default_msec_format.read() {
                    Some(m) => ops::percent_format(m, &V::tuple(vec![V::str(&s), V::Float(msecs)])).unwrap_or(s),
                    None => s,
                }
            }
        }
    }

    /// `formatMessage(record)`: the style applied to the record's attributes (plus `defaults`)
    fn format_message(&self, rec: &Record, extra: &[(&str, V)]) -> R<String> {
        let get = |n: &str| -> Option<V> {
            if let Some((_, v)) = extra.iter().find(|(k, _)| *k == n) {
                return Some(v.clone());
            }
            rec.get(n).or_else(|| self.defaults.as_ref().and_then(|d| ops::getitem(d, &V::str(n)).ok()))
        };
        match (self.style, &self.segs) {
            (Style::Percent, Some(segs)) => {
                let mut out = String::with_capacity(self.fmt.len() + 64);
                for s in segs {
                    match s {
                        Seg::Lit(l) => out += l,
                        Seg::Field(n, spec) => {
                            let v = get(n).ok_or_else(|| Exc::new(&KEY_ERROR, vec![V::str(n)]))?;
                            if spec == "s" {
                                out += &ops::str_(&v)?;
                            } else {
                                out += &ops::percent_format(&format!("%{spec}"), &V::tuple(vec![v]))?;
                            }
                        }
                    }
                }
                Ok(out)
            }
            (Style::Brace, _) => super::methods::format_with(
                &self.fmt,
                &|i| Err(Exc::msg(&INDEX_ERROR, format!("Replacement index {i} out of range for positional args tuple"))),
                &|n: &str| get(n).ok_or_else(|| Exc::new(&KEY_ERROR, vec![V::str(n)])),
                false,
            ),
            _ => {
                let mut items: Vec<(V, V)> = vec![];
                if let Some(d) = &self.defaults {
                    items.extend(mapping_items(d)?);
                }
                items.extend(rec.dict_items().into_iter().map(|(k, v)| (V::str(k), v)));
                items.extend(extra.iter().map(|(k, v)| (V::str(k), v.clone())));
                let d = V::dict_from(items)?;
                if self.style == Style::Dollar {
                    ops::str_(&super::stdlib::template_method(&self.fmt, "substitute", &[d], &[])?)
                } else {
                    ops::percent_format(&self.fmt, &d)
                }
            }
        }
    }

    /// `format(record)`
    pub fn format(&self, rec: &Record) -> R<String> {
        match &self.kind {
            FKind::Json(o) => return self.format_json(o, rec),
            FKind::Ecs(o) => return self.format_ecs(o, rec),
            FKind::UvDefault(_) | FKind::UvAccess(_) | FKind::Std => {}
        }
        let msg = rec.get_message()?;
        rec.set("message", V::str(&msg));
        if self.uses_time {
            let t = self.time_of(rec);
            rec.set("asctime", V::str(t));
        }
        let mut s = match &self.kind {
            FKind::UvDefault(colors) => self.uv_message(rec, *colors, None)?,
            FKind::UvAccess(colors) => {
                let args = items(&rec.get("args").unwrap_or(V::None));
                let [client, method, path, version, status] = args.as_slice() else {
                    return Err(Exc::value_error(format!("not enough values to unpack (expected 5, got {})", args.len())));
                };
                let code = match status {
                    V::Int(i) => *i,
                    o => ops::str_(o)?.parse().map_err(|_| Exc::value_error("invalid literal for int()"))?,
                };
                let phrase = super::uvicorn::http_phrase(code);
                let mut status = format!("{code} {phrase}");
                let mut line = format!("{} {} HTTP/{}", ops::str_(method)?, ops::str_(path)?, ops::str_(version)?);
                if *colors {
                    let fg = match code / 100 {
                        1 => Some("97"),
                        2 => Some("32"),
                        3 => Some("33"),
                        4 => Some("31"),
                        5 => Some("91"),
                        _ => None,
                    };
                    if let Some(fg) = fg {
                        status = format!("\x1b[{fg}m{status}\x1b[0m");
                    }
                    line = format!("\x1b[1m{line}\x1b[0m");
                }
                let extra = [("client_addr", client.clone()), ("request_line", V::str(line)), ("status_code", V::str(status))];
                self.uv_message(rec, *colors, Some(&extra))?
            }
            _ => self.format_message(rec, &[])?,
        };
        let exc_text = self.exc_text(rec)?;
        if let Some(t) = exc_text {
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s += &t;
        }
        if let Some(V::Str(si)) = rec.get("stack_info") {
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s += &si;
        }
        Ok(s)
    }

    fn time_of(&self, rec: &Record) -> String {
        let created = match rec.get("created") {
            Some(V::Float(f)) => f,
            Some(V::Int(i)) => i as f64,
            _ => now(),
        };
        let msecs = match rec.get("msecs") {
            Some(V::Float(f)) => f,
            Some(V::Int(i)) => i as f64,
            _ => 0.0,
        };
        let df = self.datefmt.read().clone();
        self.format_time(created, msecs, df.as_deref())
    }

    /// `exc_text`, computed once per record (`formatException`), shared by the handlers
    fn exc_text(&self, rec: &Record) -> R<Option<String>> {
        let info = rec.get("exc_info").unwrap_or(V::None);
        if ops::truthy(&info)? {
            if matches!(rec.get("exc_text"), None | Some(V::None)) {
                let t = format_exc_info(rec, &info);
                rec.set("exc_text", V::str(&t));
            }
        }
        Ok(match rec.get("exc_text") {
            Some(V::Str(t)) if !t.is_empty() => Some(t.to_string()),
            _ => None,
        })
    }

    /// uvicorn's ColourizedFormatter.formatMessage: `levelprefix` (the level name, colon, padding to 9
    /// columns), the level name coloured and `color_message` used when colours are on
    fn uv_message(&self, rec: &Record, colors: bool, extra: Option<&[(&str, V)]>) -> R<String> {
        let levelno = rec.levelno();
        let name = ops::str_(&rec.get("levelname").unwrap_or(V::None))?;
        let pad = " ".repeat(8usize.saturating_sub(name.chars().count()));
        let mut lname = name.clone();
        let mut message = None;
        if colors {
            let fg = match levelno {
                5 => Some("34"),
                10 => Some("36"),
                20 => Some("32"),
                30 => Some("33"),
                40 => Some("31"),
                50 => Some("91"),
                _ => None,
            };
            if let Some(fg) = fg {
                lname = format!("\x1b[{fg}m{name}\x1b[0m");
            }
            if let Some(cm) = rec.get("color_message") {
                // the record copy's message: color_message % args
                let s = ops::str_(&cm)?;
                message = Some(match rec.get("args") {
                    Some(a) if ops::truthy(&a)? => ops::percent_format(&s, &a)?,
                    _ => s,
                });
            }
        }
        let mut ex: Vec<(&str, V)> = vec![("levelprefix", V::str(format!("{lname}:{pad}")))];
        if let Some(m) = message {
            ex.push(("message", V::str(m)));
        }
        if let Some(e) = extra {
            ex.extend(e.iter().cloned());
        }
        self.format_message(rec, &ex)
    }

    /// python-json-logger's `BaseJsonFormatter.format` + `JsonFormatter.jsonify_log_record`
    fn format_json(&self, o: &JsonOpts, rec: &Record) -> R<String> {
        let mut message_dict: Vec<(String, V)> = vec![];
        let msg = rec.get("msg").unwrap_or(V::None);
        if let V::Dict(m) = &msg {
            for (k, v) in m.lock().values() {
                message_dict.push((ops::str_(k)?, v.clone()));
            }
            rec.set("message", V::str(""));
        } else {
            let m = rec.get_message()?;
            rec.set("message", V::str(m));
        }
        if o.required.iter().any(|f| f == "asctime") {
            let t = self.time_of(rec);
            rec.set("asctime", V::str(t));
        }
        let md_has = |md: &Vec<(String, V)>, k: &str| md.iter().any(|(n, v)| n == k && ops::truthy(v).unwrap_or(false));
        let info = rec.get("exc_info").unwrap_or(V::None);
        if ops::truthy(&info)? && !md_has(&message_dict, "exc_info") {
            message_dict.push(("exc_info".into(), V::str(format_exc_info(rec, &info))));
        }
        if !md_has(&message_dict, "exc_info") {
            if let Some(V::Str(t)) = rec.get("exc_text") {
                message_dict.push(("exc_info".into(), V::Str(t)));
            }
        }
        if let Some(V::Str(si)) = rec.get("stack_info") {
            if !md_has(&message_dict, "stack_info") {
                message_dict.push(("stack_info".into(), V::Str(si)));
            }
        }
        let rename = |k: &str| -> String { o.rename.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone()).unwrap_or_else(|| k.to_string()) };
        let mut data: IndexMap<String, V> = IndexMap::new();
        for (k, v) in &o.defaults {
            data.insert(rename(k), v.clone());
        }
        for f in &o.required {
            data.insert(rename(f), rec.get(f).unwrap_or(V::None));
        }
        for (k, v) in o.static_fields.iter().chain(message_dict.iter()) {
            data.insert(rename(k), v.clone());
        }
        for (k, v) in rec.dict_items() {
            if o.required.contains(&k) || o.reserved.contains(&k) || k.starts_with('_') {
                continue;
            }
            data.insert(rename(&k), v);
        }
        if let Some(t) = &o.timestamp {
            let created = match rec.get("created") {
                Some(V::Float(f)) => f,
                _ => now(),
            };
            let secs = created.floor() as i64;
            let micros = ((created - created.floor()) * 1e6).round() as u32;
            let dt = chrono::DateTime::from_timestamp(secs, micros * 1000).map(|d| d.naive_utc()).unwrap_or_default();
            data.insert(rename(t), V::DateTime(super::dt::DateTime::aware(dt, super::dt::Tz::Utc)));
        }
        if o.keep_missing {
            for (_, b) in &o.rename {
                if !data.contains_key(b) {
                    data.insert(b.clone(), V::None);
                }
            }
        }
        let items: Vec<(V, V)> = data.into_iter().map(|(k, v)| Ok((V::str(k), json_default(&v)?))).collect::<R<_>>()?;
        let d = V::dict_from(items)?;
        let mut kw = vec![("ensure_ascii".to_string(), V::Bool(o.ensure_ascii))];
        if !o.indent.is_none() {
            kw.push(("indent".to_string(), o.indent.clone()));
        }
        let s = ops::str_(&super::libs::json_dumps(&d, &kw)?)?;
        Ok(format!("{}{s}", o.prefix))
    }
}

impl Formatter {
    /// ecs_logging 2.x `StdlibFormatter.format`: `format_to_ecs` then its `json_dumps`
    fn format_ecs(&self, o: &EcsOpts, rec: &Record) -> R<String> {
        let excluded = |field: &str| -> bool {
            let mut path = String::new();
            for (i, part) in field.split('.').enumerate() {
                if i > 0 {
                    path.push('.');
                }
                path.push_str(part);
                if o.exclude.contains(&path) {
                    return true;
                }
            }
            false
        };
        let result = V::empty_dict();
        // the extractors (their order does not matter: they never collide)
        let exc = rec.exc.clone().or_else(|| match rec.get("exc_info") {
            Some(V::Tuple(t)) if t.len() == 3 => match &t[1] {
                V::Exc(e) => Some(e.clone()),
                _ => None,
            },
            Some(V::Exc(e)) => Some(e),
            _ => None,
        });
        let has_exc = rec.get("exc_info").map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false);
        let created = match rec.get("created") {
            Some(V::Float(f)) => f,
            Some(V::Int(i)) => i as f64,
            _ => now(),
        };
        let msecs = match rec.get("msecs") {
            Some(V::Float(f)) => f,
            Some(V::Int(i)) => i as f64,
            _ => 0.0,
        };
        let secs = created.floor() as i64;
        let utc = chrono::DateTime::from_timestamp(secs, 0).map(|d| d.naive_utc()).unwrap_or_default();
        let stamp = format!("{}.{:03}Z", super::libs::strftime_lenient(&utc, Some(0), Some("UTC".into()), "%Y-%m-%dT%H:%M:%S"), msecs.trunc() as i64);
        let level = rec.get("levelname").map(|v| ops::truthy(&v).map(|t| (t, v))).transpose()?
            .and_then(|(t, v)| t.then(|| ops::str_(&v).map(|s| V::str(s.to_lowercase())))).transpose()?;
        let stack = if has_exc && exc.is_some() && o.stack_trace_limit.is_none_or(|l| l > 0) {
            let t = super::trace::format_frames_limit(exc.as_ref().unwrap(), o.stack_trace_limit.map(|l| l as usize));
            if t.is_empty() { None } else { Some(V::str(t)) }
        } else {
            match rec.get("stack_info") {
                Some(si) if ops::truthy(&si)? => Some(V::str(ops::str_(&si)?)),
                _ => None,
            }
        };
        let extractors: Vec<(&str, Option<V>)> = vec![
            ("@timestamp", Some(V::str(stamp))),
            ("ecs.version", Some(V::str("1.6.0"))),
            ("log.level", level),
            ("log.origin.function", rec.get("funcName")),
            ("log.origin.file.line", rec.get("lineno")),
            ("log.origin.file.name", rec.get("filename")),
            ("log.original", Some(V::str(rec.get_message()?))),
            ("log.logger", rec.get("name")),
            ("process.pid", rec.get("process")),
            ("process.name", rec.get("processName")),
            ("process.thread.id", rec.get("thread")),
            ("process.thread.name", rec.get("threadName")),
            ("error.type", if has_exc { exc.as_ref().map(|e| V::str(e.0.class.name)) } else { None }),
            ("error.message", if has_exc { exc.as_ref().map(|e| V::str(e.message())) } else { None }),
            ("error.stack_trace", stack),
        ];
        for (field, value) in extractors {
            if excluded(field) {
                continue;
            }
            let Some(value) = value.filter(|v| !v.is_none()) else { continue };
            let d = if field == "ecs.version" { V::dict_from(vec![(V::str(field), value)])? } else { ecs_de_dot(field, value)? };
            ecs_merge(&d, &result)?;
        }
        let message = rec.get_message()?;
        rec.set("message", V::str(&message));
        // the attributes LogRecord does not define: `extra=`, attributes set by filters
        let known = |k: &str| ECS_RECORD_ATTRS.contains(&k) || (k.starts_with("__") && k.ends_with("__")) || (k == "taskName" && super::python().1 >= 12);
        let extra_items: Vec<(V, V)> = rec.dict_items().into_iter().filter(|(k, _)| !known(k)).map(|(k, v)| (V::str(k), v)).collect();
        let extras = ecs_flatten(&V::dict_from(extra_items)?)?;
        if let Some(e) = &o.extra {
            for (field, value) in mapping_items(e)? {
                ecs_merge(&ecs_de_dot(&ops::str_(&field)?, value)?, &extras)?;
            }
        }
        {
            let V::Dict(m) = &extras else { unreachable!() };
            let mut m = m.lock();
            for (to, from) in [("span.id", "elasticapm_span_id"), ("transaction.id", "elasticapm_transaction_id"),
                               ("trace.id", "elasticapm_trace_id"), ("service.name", "elasticapm_service_name"),
                               ("service.environment", "elasticapm_service_environment")] {
                let popped = m.shift_remove(&Key::Str(from.into())).map(|(_, v)| v).unwrap_or(V::None);
                m.entry(Key::Str(to.into())).or_insert((V::str(to), popped));
            }
        }
        let entries: Vec<(V, V)> = match &extras {
            V::Dict(m) => m.lock().values().cloned().collect(),
            _ => vec![],
        };
        for (field, value) in entries {
            let f = ops::str_(&field)?;
            if f.starts_with("elasticapm_labels.") || value.is_none() || excluded(&f) {
                continue;
            }
            ecs_merge(&ecs_de_dot(&f, value)?, &result)?;
        }
        if !excluded("message") {
            let V::Dict(m) = &result else { unreachable!() };
            m.lock().entry(Key::Str("message".into())).or_insert((V::str("message"), V::str(&message)));
        }
        ecs_json_dumps(&result, o.ensure_ascii)
    }
}

/// `LogRecord`'s own names (`StdlibFormatter._LOGRECORD_DICT`, without the dunders)
const ECS_RECORD_ATTRS: &[&str] = &[
    "name", "msg", "args", "asctime", "levelname", "levelno", "pathname", "filename", "module", "exc_info", "exc_text", "stack_info",
    "lineno", "funcName", "created", "msecs", "relativeCreated", "thread", "threadName", "processName", "process", "message", "getMessage",
];

/// `de_dot("a.b", v)` = `{"a": {"b": v}}`
fn ecs_de_dot(field: &str, value: V) -> R {
    let mut out = value;
    for part in field.rsplit('.') {
        out = V::dict_from(vec![(V::str(part), out)])?;
    }
    Ok(out)
}

/// ecs_logging's `merge_dicts(from_, into)`
fn ecs_merge(from: &V, into: &V) -> R<()> {
    let (V::Dict(f), V::Dict(i)) = (from, into) else { return Ok(()) };
    let pairs: Vec<(V, V)> = f.lock().values().cloned().collect();
    for (k, value) in pairs {
        let key = Key::dict_key(&k)?;
        let cur = {
            let mut m = i.lock();
            m.entry(key.clone()).or_insert_with(|| (k.clone(), V::empty_dict())).1.clone()
        };
        match (&value, &cur) {
            (V::Dict(_), V::Dict(_)) => ecs_merge(&value, &cur)?,
            (_, V::Dict(c)) if c.lock().is_empty() => {
                if let Some(slot) = i.lock().get_mut(&key) {
                    slot.1 = value;
                }
            }
            _ => {
                return Err(Exc::type_error(format!(
                    "Type mismatch at key `{}`: merging dicts would replace value `{}` with `{}`. This is likely due to dotted keys in the \
                     event dict being turned into nested dictionaries, causing a conflict.",
                    ops::str_(&k)?, ops::str_(&cur)?, ops::str_(&value)?)));
            }
        }
    }
    Ok(())
}

/// ecs_logging's `flatten_dict`: nested mappings as dotted keys
fn ecs_flatten(value: &V) -> R {
    let top = V::empty_dict();
    let V::Dict(t) = &top else { unreachable!() };
    for (key, val) in mapping_items(value)? {
        let k = ops::str_(&key)?;
        if !matches!(val, V::Dict(_)) {
            if t.lock().contains_key(&Key::dict_key(&key)?) {
                return Err(Exc::value_error(format!("Duplicate entry for '{k}' with different nesting")));
            }
            t.lock().insert(Key::dict_key(&key)?, (key, val));
        } else {
            for (vk, vv) in mapping_items(&ecs_flatten(&val)?)? {
                let name = format!("{k}.{}", ops::str_(&vk)?);
                let nk = Key::Str(name.as_str().into());
                if t.lock().contains_key(&nk) {
                    return Err(Exc::value_error(format!("Duplicate entry for '{name}' with different nesting")));
                }
                t.lock().insert(nk, (V::str(&name), vv));
            }
        }
    }
    Ok(top)
}

/// json.dumps's `default=_json_dumps_fallback` (`repr()`), applied ahead (what the encoder cannot write)
fn ecs_default(v: &V) -> R {
    Ok(match v {
        V::None | V::Bool(_) | V::Int(_) | V::Float(_) | V::Str(_) => v.clone(),
        V::List(l) => V::list(l.lock().iter().map(ecs_default).collect::<R<_>>()?),
        V::Tuple(t) => V::list(t.iter().map(ecs_default).collect::<R<_>>()?),
        V::Dict(m) => V::dict_from(m.lock().values().map(|(k, x)| Ok((k.clone(), ecs_default(x)?))).collect::<R<_>>()?)?,
        V::Enum(d, i) if matches!(d.kind, EnumKind::Str | EnumKind::StrEnum | EnumKind::Int | EnumKind::IntEnum) => d.value(*i),
        o => V::str(ops::repr(o)?),
    })
}

/// ecs_logging's `json_dumps`: `@timestamp`, `log.level` and `message` first, the rest sorted, compact
fn ecs_json_dumps(value: &V, ensure_ascii: bool) -> R<String> {
    let V::Dict(m) = value else { return Ok(String::new()) };
    let dump = |v: &V| -> R<String> {
        let kw = vec![("sort_keys".to_string(), V::Bool(true)),
                      ("separators".to_string(), V::tuple(vec![V::str(","), V::str(":")])),
                      ("ensure_ascii".to_string(), V::Bool(ensure_ascii))];
        ops::str_(&super::libs::json_dumps(&ecs_default(v)?, &kw)?)
    };
    let mut ordered: Vec<(&str, V)> = vec![];
    {
        let mut d = m.lock();
        if let Some((_, v)) = d.shift_remove(&Key::Str("@timestamp".into())) {
            ordered.push(("@timestamp", v));
        }
        let log_level = match d.get(&Key::Str("log".into())) {
            Some((_, V::Dict(l))) => {
                let lv = l.lock().shift_remove(&Key::Str("level".into())).map(|(_, v)| v);
                if lv.is_some() && l.lock().is_empty() {
                    d.shift_remove(&Key::Str("log".into()));
                }
                lv
            }
            _ => d.shift_remove(&Key::Str("log.level".into())).map(|(_, v)| v),
        };
        if let Some(v) = log_level {
            ordered.push(("log.level", v));
        }
        if let Some((_, v)) = d.shift_remove(&Key::Str("message".into())) {
            ordered.push(("message", v));
        }
    }
    if ordered.is_empty() {
        return dump(value);
    }
    let head = ordered.iter().map(|(k, v)| Ok(format!("\"{k}\":{}", dump(v)?))).collect::<R<Vec<_>>>()?.join(",");
    if m.lock().is_empty() {
        Ok(format!("{{{head}}}"))
    } else {
        Ok(format!("{{{head},{}", &dump(value)?[1..]))
    }
}

/// `ecs_logging.StdlibFormatter(fmt=None, datefmt=None, style='%', validate=None, stack_trace_limit=None,
/// extra=None, exclude_fields=(), ensure_ascii=True)` (2.x)
pub fn ecs_formatter_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("StdlibFormatter.__init__", args, kwargs,
                 &["fmt", "datefmt", "style", "validate", "stack_trace_limit", "extra", "exclude_fields", "ensure_ascii"], &[])?;
    let style = parse_style(a[2])?;
    let validate = match a[3] {
        Some(v) if !v.is_none() => ops::truthy(v)?,
        _ => true,
    };
    let base = Formatter::new(FKind::Std, opt_str(a[0])?, opt_str(a[1])?, style, validate, None)?;
    let stack_trace_limit = match a[4] {
        None | Some(V::None) => None,
        Some(V::Int(i)) if *i < 0 => return Err(Exc::value_error("'stack_trace_limit' must be None, or a non-negative integer")),
        Some(V::Int(i)) => Some(*i),
        Some(V::Bool(b)) => Some(*b as i64),
        Some(_) => return Err(Exc::type_error("'stack_trace_limit' must be None, or a non-negative integer")),
    };
    let exclude = match a[6] {
        None => vec![],
        Some(v @ (V::List(_) | V::Tuple(_))) => {
            let xs = items(v);
            if xs.iter().any(|x| !matches!(x, V::Str(_))) {
                return Err(Exc::type_error("'exclude_fields' must be a sequence of strings"));
            }
            xs.iter().map(ops::str_).collect::<R<_>>()?
        }
        Some(_) => return Err(Exc::type_error("'exclude_fields' must be a sequence of strings")),
    };
    let opts = EcsOpts {
        extra: a[5].filter(|v| !v.is_none()).cloned(),
        exclude,
        stack_trace_limit,
        ensure_ascii: a[7].map(ops::truthy).transpose()?.unwrap_or(true),
    };
    Ok(formatter_value(Formatter { kind: FKind::Ecs(Box::new(opts)), ..base }))
}

/// python-json-logger's `JsonEncoder.default`, applied ahead of `json.dumps` (recursively)
fn json_default(v: &V) -> R {
    Ok(match v {
        V::None | V::Bool(_) | V::Int(_) | V::Float(_) | V::Str(_) => v.clone(),
        V::List(l) => V::list(l.lock().iter().map(json_default).collect::<R<_>>()?),
        V::Tuple(t) => V::list(t.iter().map(json_default).collect::<R<_>>()?),
        V::Dict(m) => {
            let items: Vec<(V, V)> = m.lock().values().map(|(k, v)| Ok((k.clone(), json_default(v)?))).collect::<R<_>>()?;
            V::dict_from(items)?
        }
        V::DateTime(_) | V::Date(_) | V::Time(_) => call_isoformat(v)?,
        V::Exc(e) => V::str(format!("{}: {}", e.0.class.name, ops::str_(v)?)),
        V::Enum(d, i) => json_default(&d.value(*i))?,
        V::Bytes(b) => {
            use base64::Engine;
            V::str(base64::engine::general_purpose::URL_SAFE.encode(b))
        }
        V::Class(c) => V::str(c.name),
        _ => match ops::str_(v) {
            Ok(s) => V::str(s),
            Err(_) => match ops::repr(v) {
                Ok(s) => V::str(s),
                Err(_) => V::str("__could_not_encode__"),
            },
        },
    })
}

fn call_isoformat(v: &V) -> R {
    Ok(V::str(match v {
        V::DateTime(d) => d.isoformat('T', "auto"),
        V::Date(d) => super::dt::date_iso(d),
        V::Time(t) => super::dt::time_iso(t),
        _ => ops::str_(v)?,
    }))
}

/// `raise ValueError(...) from e` (dictConfig's errors name the failing entry)
fn chained(e: Exc, cause: Exc) -> Exc {
    e.0.attrs.lock().insert("__cause__".into(), V::Exc(cause));
    e
}

fn validate_fmt(fmt: &str, style: Style) -> R<()> {
    let ok = match style {
        Style::Percent => {
            let b = fmt.as_bytes();
            let mut found = false;
            let mut i = 0;
            while let Some(p) = fmt[i..].find("%(") {
                let s = i + p;
                if let Some(c) = fmt[s + 2..].find(')') {
                    let name = &fmt[s + 2..s + 2 + c];
                    if !name.is_empty() && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') && percent_spec(&b[s + 3 + c..]).is_some() {
                        found = true;
                        break;
                    }
                }
                i = s + 2;
            }
            found
        }
        Style::Brace => {
            // a `{field...}` with a valid identifier (CPython also checks the conversions and specs)
            let mut found = false;
            let mut it = fmt.char_indices().peekable();
            while let Some((i, c)) = it.next() {
                if c == '{' {
                    if fmt[i + 1..].starts_with('{') {
                        it.next();
                        continue;
                    }
                    let end = fmt[i + 1..].find('}').map(|e| i + 1 + e);
                    match end {
                        Some(e) => {
                            let field = &fmt[i + 1..e];
                            let name = field.split(['!', ':', '.', '[']).next().unwrap_or("");
                            if name.is_empty() || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
                                return Err(Exc::value_error(format!("invalid field name/expression: {}", ops::str_repr(name))));
                            }
                            found = true;
                        }
                        None => return Err(Exc::value_error("invalid format: expected '}' before end of string")),
                    }
                }
            }
            found
        }
        Style::Dollar => fmt.contains('$'),
    };
    if !ok {
        let d = match style {
            Style::Percent => "%(message)s",
            Style::Brace => "{message}",
            Style::Dollar => "${message}",
        };
        return Err(Exc::value_error(format!("Invalid format '{fmt}' for '{}' style", &d[..1])));
    }
    Ok(())
}

/// `time.localtime(t)`: the wall time, its UTC offset and zone abbreviation
fn localtime(t: f64) -> (chrono::NaiveDateTime, i32, String) {
    let secs = t.floor() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        let d = chrono::DateTime::from_timestamp(secs as i64, 0).map(|d| d.naive_utc()).unwrap_or_default();
        return (d, 0, "UTC".into());
    }
    let date = chrono::NaiveDate::from_ymd_opt(tm.tm_year + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32).unwrap_or_default();
    let time = chrono::NaiveTime::from_hms_opt(tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec.min(59) as u32).unwrap_or_default();
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned()
    };
    (date.and_time(time), tm.tm_gmtoff as i32, zone)
}

/// `Formatter.formatException(exc_info)`: the traceback as `traceback.print_exception` writes it, without
/// its last newline; `(None, None, None)` (logged outside an `except`) is "NoneType: None"
fn format_exc_info(rec: &Record, info: &V) -> String {
    let exc = rec.exc.clone().or_else(|| match info {
        V::Tuple(t) if t.len() == 3 => match &t[1] {
            V::Exc(e) => Some(e.clone()),
            _ => None,
        },
        V::Exc(e) => Some(e.clone()),
        _ => None,
    });
    let s = match exc {
        Some(e) => super::trace::format_exc(&e),
        None => "NoneType: None\n".to_string(),
    };
    s.strip_suffix('\n').unwrap_or(&s).to_string()
}

pub fn formatter_value(f: Formatter) -> V {
    V::native(Native::Log(LObj::Formatter(Arc::new(f))))
}

fn as_formatter(v: &V) -> Option<Arc<Formatter>> {
    match v {
        V::Native(n) => match &**n {
            Native::Log(LObj::Formatter(f)) => Some(f.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn opt_str(v: Option<&V>) -> R<Option<String>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(v) => Ok(Some(ops::str_(v)?)),
    }
}

/// binds (args, kwargs) to the named parameters, CPython's TypeErrors for the rest
fn bind<'a>(func: &str, args: &'a [V], kwargs: &'a [(String, V)], names: &[&str], kwonly: &[&str]) -> R<Vec<Option<&'a V>>> {
    if args.len() > names.len() {
        return Err(Exc::type_error(format!("{func}() takes from 1 to {} positional arguments but {} were given", names.len() + 1, args.len() + 1)));
    }
    let mut out: Vec<Option<&V>> = args.iter().map(Some).collect();
    out.resize(names.len() + kwonly.len(), None);
    for (k, v) in kwargs {
        match names.iter().chain(kwonly).position(|n| n == k) {
            Some(i) if out[i].is_some() => return Err(Exc::type_error(format!("{func}() got multiple values for argument '{k}'"))),
            Some(i) => out[i] = Some(v),
            None => return Err(Exc::type_error(format!("{func}() got an unexpected keyword argument '{k}'"))),
        }
    }
    Ok(out)
}

/// `logging.Formatter(...)`
pub fn formatter_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("Formatter.__init__", args, kwargs, &["fmt", "datefmt", "style", "validate"], &["defaults"])?;
    let style = parse_style(a[2])?;
    let validate = match a[3] {
        Some(v) => ops::truthy(v)?,
        None => true,
    };
    let defaults = a[4].filter(|v| !v.is_none()).cloned();
    Ok(formatter_value(Formatter::new(FKind::Std, opt_str(a[0])?, opt_str(a[1])?, style, validate, defaults)?))
}

/// `uvicorn.logging.DefaultFormatter` / `AccessFormatter(fmt, datefmt, style, use_colors)`
pub fn uv_formatter_new(access: bool, args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("ColourizedFormatter.__init__", args, kwargs, &["fmt", "datefmt", "style", "use_colors"], &[])?;
    let colors = match a[3] {
        Some(V::Bool(b)) => *b,
        _ => std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    let kind = if access { FKind::UvAccess(colors) } else { FKind::UvDefault(colors) };
    Ok(formatter_value(Formatter::new(kind, opt_str(a[0])?, opt_str(a[1])?, parse_style(a[2])?, true, None)?))
}

const JSON_RESERVED: &[&str] = &[
    "args", "asctime", "created", "exc_info", "exc_text", "filename", "funcName", "levelname", "levelno", "lineno", "module", "msecs", "message",
    "msg", "name", "pathname", "process", "processName", "relativeCreated", "stack_info", "thread", "threadName",
];

/// `pythonjsonlogger.json.JsonFormatter(fmt=None, datefmt=None, style='%', validate=True, *, prefix, rename_fields,
/// rename_fields_keep_missing, static_fields, reserved_attrs, timestamp, defaults, exc_info_as_array,
/// stack_info_as_array, json_default, json_encoder, json_serializer, json_indent, json_ensure_ascii)` (4.0)
pub fn json_formatter_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind(
        "JsonFormatter.__init__",
        args,
        kwargs,
        &["fmt", "datefmt", "style", "validate"],
        &[
            "prefix", "rename_fields", "rename_fields_keep_missing", "static_fields", "reserved_attrs", "timestamp", "defaults", "exc_info_as_array",
            "stack_info_as_array", "json_default", "json_encoder", "json_serializer", "json_indent", "json_ensure_ascii",
        ],
    )?;
    for (i, n) in [(11, "exc_info_as_array"), (12, "stack_info_as_array")] {
        if let Some(v) = a[i] {
            if ops::truthy(v)? {
                return Err(Exc::type_error(format!("py2axum: JsonFormatter({n}=True) is not supported")));
            }
        }
    }
    for (i, n) in [(13, "json_default"), (14, "json_encoder"), (15, "json_serializer")] {
        if a[i].is_some_and(|v| !v.is_none()) {
            return Err(Exc::type_error(format!("py2axum: JsonFormatter({n}=...) is not supported")));
        }
    }
    let style_s = opt_str(a[2])?.unwrap_or_else(|| "%".into());
    let validate = match a[3] {
        Some(v) => ops::truthy(v)?,
        None => true,
    };
    let (style, required, fmt) = match a[0] {
        Some(V::List(_)) | Some(V::Tuple(_)) => {
            let f = a[0].unwrap();
            let names: Vec<String> = items(f).iter().map(ops::str_).collect::<R<_>>()?;
            (Style::Percent, names, Some(ops::repr(f)?))
        }
        f => {
            let fmt = opt_str(f)?;
            let (style, req) = match style_s.as_str() {
                "," => (Style::Percent, fmt.as_deref().map(|f| f.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default()),
                s => {
                    let st = parse_style(Some(&V::str(s))).map_err(|_| Exc::value_error("Style must be one of: '%{$,'"))?;
                    let f = fmt.clone().unwrap_or_else(|| default_fmt(st).to_string());
                    if validate {
                        validate_fmt(&f, st)?;
                    }
                    let req = match st {
                        Style::Percent => find_fields(&f, "%(", ")"),
                        Style::Brace => find_fields(&f, "{", "}"),
                        Style::Dollar => find_fields(&f, "${", "}"),
                    };
                    (st, if fmt.is_none() { vec![] } else { req })
                }
            };
            (style, req, fmt)
        }
    };
    let pairs = |v: Option<&V>| -> R<Vec<(String, V)>> {
        match v {
            None | Some(V::None) => Ok(vec![]),
            Some(d) => mapping_items(d)?.into_iter().map(|(k, v)| Ok((ops::str_(&k)?, v))).collect(),
        }
    };
    let rename = pairs(a[5])?.into_iter().map(|(k, v)| Ok((k, ops::str_(&v)?))).collect::<R<Vec<_>>>()?;
    let reserved = match a[8] {
        None | Some(V::None) => {
            let mut r: Vec<String> = JSON_RESERVED.iter().map(|s| s.to_string()).collect();
            if super::python().1 >= 12 {
                r.push("taskName".into());
            }
            r
        }
        Some(v) => items(v).iter().map(ops::str_).collect::<R<_>>()?,
    };
    let timestamp = match a[9] {
        None | Some(V::None) | Some(V::Bool(false)) => None,
        Some(V::Bool(true)) => Some("timestamp".to_string()),
        Some(v) => Some(ops::str_(v)?),
    };
    let opts = JsonOpts {
        required,
        rename,
        static_fields: pairs(a[7])?,
        defaults: pairs(a[10])?,
        reserved,
        prefix: opt_str(a[4])?.unwrap_or_default(),
        timestamp,
        keep_missing: a[6].map(ops::truthy).transpose()?.unwrap_or(false),
        ensure_ascii: a[17].map(ops::truthy).transpose()?.unwrap_or(true),
        indent: a[16].cloned().unwrap_or(V::None),
    };
    let datefmt = opt_str(a[1])?;
    let f = Formatter {
        kind: FKind::Json(Box::new(opts)),
        fmt: fmt.unwrap_or_default(),
        style,
        segs: None,
        datefmt: RwLock::new(datefmt),
        defaults: None,
        uses_time: false,
        default_time_format: RwLock::new("%Y-%m-%d %H:%M:%S".into()),
        default_msec_format: RwLock::new(Some("%s,%03d".into())),
    };
    Ok(formatter_value(f))
}

/// python-json-logger's field regexes: `%\((.+?)\)`, `\{(.+?)\}`, `\$\{(.+?)\}`
fn find_fields(fmt: &str, open: &str, close: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = fmt;
    while let Some(i) = rest.find(open) {
        let after = &rest[i + open.len()..];
        match after.get(1..).and_then(|a| a.find(close)) {
            Some(j) => {
                out.push(after[..j + 1].to_string());
                rest = &after[j + 1 + close.len()..];
            }
            None => break,
        }
    }
    out
}

// ---------------------------------------------------------------- handlers

pub struct FileTarget {
    path: String,
    mode: String,
    file: Mutex<Option<std::fs::File>>,
}

pub enum HKind {
    /// `StreamHandler`: 1 = stdout, 2 = stderr
    Stream(u8),
    File(FileTarget),
    Null,
}

impl HKind {
    fn class_name(&self) -> &'static str {
        match self {
            HKind::Stream(_) => "StreamHandler",
            HKind::File(_) => "FileHandler",
            HKind::Null => "NullHandler",
        }
    }
}

pub struct Handler {
    pub kind: HKind,
    level: AtomicI64,
    formatter: RwLock<V>,
    filters: RwLock<V>,
    name: RwLock<V>,
    terminator: RwLock<String>,
    lock: Mutex<()>,
}

/// `logging._handlers`: the named handlers (`getHandlerByName`)
static NAMED: LazyLock<Mutex<Vec<(String, Arc<Handler>)>>> = LazyLock::new(|| Mutex::new(vec![]));

impl Handler {
    fn new(kind: HKind, level: i64) -> Arc<Handler> {
        Arc::new(Handler {
            kind,
            level: AtomicI64::new(level),
            formatter: RwLock::new(V::None),
            filters: RwLock::new(V::list(vec![])),
            name: RwLock::new(V::None),
            terminator: RwLock::new("\n".into()),
            lock: Mutex::new(()),
        })
    }

    pub fn level(&self) -> i64 {
        self.level.load(Ordering::Relaxed)
    }

    fn set_formatter(&self, f: V) {
        *self.formatter.write() = f;
    }

    /// `format(record)`: its formatter, or the module's default one (`%(message)s`)
    fn format(&self, rec: &Record) -> R<String> {
        static DEFAULT: LazyLock<Formatter> = LazyLock::new(|| Formatter::new(FKind::Std, None, None, Style::Percent, false, None).expect("formatter"));
        let f = self.formatter.read().clone();
        match as_formatter(&f) {
            Some(f) => f.format(rec),
            None => DEFAULT.format(rec),
        }
    }

    /// `emit(record)`: a formatting error goes to `handleError`, like CPython
    fn emit(&self, rec: &Record) {
        let out = match &self.kind {
            HKind::Null => return,
            _ => self.format(rec),
        };
        let s = match out {
            Ok(s) => s,
            Err(e) => return handle_error(rec, &e),
        };
        let line = format!("{s}{}", self.terminator.read());
        let _g = self.lock.lock();
        match &self.kind {
            HKind::Stream(1) => {
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(line.as_bytes());
                let _ = o.flush();
            }
            HKind::Stream(_) => {
                let mut o = std::io::stderr().lock();
                let _ = o.write_all(line.as_bytes());
                let _ = o.flush();
            }
            HKind::File(f) => {
                let mut g = f.file.lock();
                if g.is_none() {
                    *g = open_file(&f.path, &f.mode).ok();
                }
                if let Some(file) = g.as_mut() {
                    let _ = file.write_all(line.as_bytes());
                    let _ = file.flush();
                }
            }
            HKind::Null => {}
        }
    }
}

fn open_file(path: &str, mode: &str) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    if mode.starts_with('w') {
        o.write(true).create(true).truncate(true);
    } else {
        o.append(true).create(true);
    }
    o.open(path)
}

/// `Handler.handleError(record)` with `raiseExceptions` (CPython's default): the report on stderr, the
/// program goes on. The binary has no library frames: the traceback holds the exception line only.
fn handle_error(rec: &Record, e: &Exc) {
    let mut s = String::from("--- Logging error ---\nTraceback (most recent call last):\n");
    s += &super::trace::format_exc_only(e);
    s += "Call stack:\n";
    let path = ops::str_(&rec.get("pathname").unwrap_or(V::None)).unwrap_or_default();
    let line = ops::str_(&rec.get("lineno").unwrap_or(V::None)).unwrap_or_default();
    let func = ops::str_(&rec.get("funcName").unwrap_or(V::None)).unwrap_or_default();
    s += &format!("  File \"{path}\", line {line}, in {func}\n");
    if let Some(t) = rec.loc_text {
        s += t;
    }
    match (ops::repr(&rec.get("msg").unwrap_or(V::None)), ops::str_(&rec.get("args").unwrap_or(V::None))) {
        (Ok(m), Ok(a)) => s += &format!("Message: {m}\nArguments: {a}\n"),
        _ => s += "Unable to print the message and arguments - possible formatting error.\nUse the traceback above to help find the error.\n",
    }
    let mut o = std::io::stderr().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

pub fn handler_value(h: Arc<Handler>) -> V {
    V::native(Native::Log(LObj::Handler(h)))
}

fn stream_of(v: Option<&V>) -> R<u8> {
    match v {
        None | Some(V::None) => Ok(2),
        Some(V::Native(n)) => match &**n {
            Native::Log(LObj::Stream(s)) => Ok(*s),
            _ => Err(Exc::type_error("py2axum: a logging stream other than sys.stdout / sys.stderr is not supported")),
        },
        Some(_) => Err(Exc::type_error("py2axum: a logging stream other than sys.stdout / sys.stderr is not supported")),
    }
}

/// `logging.StreamHandler(stream=None)`
pub fn stream_handler_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("StreamHandler.__init__", args, kwargs, &["stream"], &[])?;
    Ok(handler_value(Handler::new(HKind::Stream(stream_of(a[0])?), NOTSET)))
}

/// `logging.FileHandler(filename, mode='a', encoding=None, delay=False, errors=None)`
pub fn file_handler_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("FileHandler.__init__", args, kwargs, &["filename", "mode", "encoding", "delay", "errors"], &[])?;
    let Some(path) = a[0] else {
        return Err(Exc::type_error("FileHandler.__init__() missing 1 required positional argument: 'filename'"));
    };
    let path = ops::str_(path)?;
    let path = if path.starts_with('/') {
        path
    } else {
        std::env::current_dir().map(|d| d.join(&path).to_string_lossy().into_owned()).unwrap_or(path)
    };
    let mode = opt_str(a[1])?.unwrap_or_else(|| "a".into());
    if let Some(enc) = opt_str(a[2])? {
        if !["utf-8", "utf8", "UTF-8"].contains(&enc.as_str()) {
            return Err(Exc::type_error(format!("py2axum: FileHandler(encoding={}) is not supported (UTF-8 only)", ops::str_repr(&enc))));
        }
    }
    let delay = a[3].map(ops::truthy).transpose()?.unwrap_or(false);
    let file = if delay { None } else { Some(open_file(&path, &mode).map_err(|e| super::pathio::os_err(e, &path))?) };
    Ok(handler_value(Handler::new(HKind::File(FileTarget { path, mode, file: Mutex::new(file) }), NOTSET)))
}

/// `logging.NullHandler()`
pub fn null_handler_new(args: &[V], kwargs: &[(String, V)]) -> R {
    bind("NullHandler.__init__", args, kwargs, &["level"], &[])?;
    Ok(handler_value(Handler::new(HKind::Null, NOTSET)))
}

/// `logging.lastResort`: WARNING and above on stderr, `%(message)s`, when no handler is found
static LAST_RESORT: LazyLock<Arc<Handler>> = LazyLock::new(|| Handler::new(HKind::Stream(2), WARNING));
static LAST_RESORT_ON: AtomicBool = AtomicBool::new(true);

// ---------------------------------------------------------------- filters

/// `Filterer.filter(record)` (3.12+): every filter in turn; a falsy result drops the record, a returned
/// LogRecord replaces it
async fn run_filters(cx: &Cx, filters: &V, rec: Arc<Record>) -> R<Option<Arc<Record>>> {
    let fs = items(filters);
    if fs.is_empty() {
        return Ok(Some(rec));
    }
    let mut rec = rec;
    for f in fs {
        let rv = record_value(rec.clone());
        let result = match &f {
            V::Native(n) => match &**n {
                Native::Log(LObj::Filter(name)) => V::Bool(name_matches(name, &rec.name())),
                _ => call_value(cx, &f, vec![rv], vec![]).await?,
            },
            V::Inst(_) | V::Obj(_) => call_method(cx, &f, "filter", vec![rv], vec![]).await?,
            _ => call_value(cx, &f, vec![rv], vec![]).await?,
        };
        if !ops::truthy(&result)? {
            return Ok(None);
        }
        if let Some(r) = as_record(&result) {
            rec = r;
        }
    }
    Ok(Some(rec))
}

/// `logging.Filter(name).filter(record)`: the logger and its descendants
fn name_matches(name: &str, rec_name: &str) -> bool {
    name.is_empty() || name == rec_name || (rec_name.starts_with(name) && rec_name.as_bytes().get(name.len()) == Some(&b'.'))
}

/// `logging.Filter(name='')`
pub fn filter_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("Filter.__init__", args, kwargs, &["name"], &[])?;
    Ok(V::native(Native::Log(LObj::Filter(Arc::from(opt_str(a[0])?.unwrap_or_default().as_str())))))
}

fn add_to(list: &RwLock<V>, v: V) {
    let l = list.read().clone();
    if let V::List(l) = &l {
        let mut g = l.lock();
        if !g.iter().any(|x| same(x, &v)) {
            g.push(v);
        }
    }
}

fn remove_from(list: &RwLock<V>, v: &V) {
    let l = list.read().clone();
    if let V::List(l) = &l {
        l.lock().retain(|x| !same(x, v));
    }
}

// ---------------------------------------------------------------- emitting

/// `Logger.handle(record)` then `callHandlers`: the logger's own filters, then every handler up the
/// hierarchy while `propagate` holds, `lastResort` when none was found; then Sentry's logging integration
async fn handle(cx: &Cx, lg: &Arc<Logger>, rec: Arc<Record>) -> R<()> {
    if lg.disabled.load(Ordering::Relaxed) {
        return Ok(());
    }
    let filters = lg.filters.read().clone();
    let Some(rec) = run_filters(cx, &filters, rec).await? else { return Ok(()) };
    let levelno = rec.levelno();
    let mut found = 0;
    let mut cur: Option<&Arc<Logger>> = Some(lg);
    while let Some(l) = cur {
        for h in l.handler_list() {
            found += 1;
            if levelno >= h.level() {
                handler_handle(cx, &h, rec.clone()).await?;
            }
        }
        if !l.propagate.load(Ordering::Relaxed) {
            break;
        }
        cur = l.parent.as_ref();
    }
    if found == 0 && LAST_RESORT_ON.load(Ordering::Relaxed) && levelno >= LAST_RESORT.level() {
        LAST_RESORT.emit(&rec);
    }
    if super::sentry::active() && !NOT_TO_SENTRY.with(|n| n.get()) {
        Box::pin(to_sentry(cx, &rec)).await;
    }
    Ok(())
}

thread_local! {
    static NOT_TO_SENTRY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `emit_sync` for a record the logging integration sees elsewhere, with the right scope
/// (`sentry::task_never_retrieved`)
pub fn emit_sync_not_to_sentry(logger: &str, level: i64, msg: &str, exc: Option<Exc>, loc: &'static Loc) {
    let prev = NOT_TO_SENTRY.with(|n| n.replace(true));
    emit_sync(logger, level, msg, vec![], vec![], exc, loc);
    NOT_TO_SENTRY.with(|n| n.set(prev));
}

async fn handler_handle(cx: &Cx, h: &Arc<Handler>, rec: Arc<Record>) -> R<()> {
    let filters = h.filters.read().clone();
    if let Some(rec) = run_filters(cx, &filters, rec).await? {
        h.emit(&rec);
    }
    Ok(())
}

async fn to_sentry(cx: &Cx, rec: &Record) {
    let msg = rec.get("msg").unwrap_or(V::None);
    let args = match rec.get("args") {
        Some(V::Tuple(t)) => t.to_vec(),
        Some(d @ V::Dict(_)) => vec![d],
        _ => vec![],
    };
    let formatted = rec.get_message().unwrap_or_default();
    // `_extra_from_record`: the record's attributes but `COMMON_RECORD_ATTRS` and private ones (`message`, set
    // by a Formatter that ran first, stays out; `asctime` or uvicorn's `color_message` go in)
    let more = rec.more.lock().clone();
    let extra = if more.is_empty() {
        None
    } else {
        let keep = |k: &str| !k.starts_with('_') && !STD_ATTRS.contains(&k) && !["linenno", "message", "stack", "tags"].contains(&k);
        V::dict_from(more.into_iter().filter(|(k, _)| keep(k)).map(|(k, v)| (V::str(k), v)).collect()).ok()
    };
    let exc = rec.exc.clone();
    super::sentry::log_record(cx, &rec.name(), rec.levelno(), &msg, &args, &formatted, exc, extra).await;
}

/// `Logger._log(level, msg, args, exc_info=None, extra=None, stack_info=False, stacklevel=1)` for an
/// enabled level
async fn log_enabled(cx: &Cx, lg: &Arc<Logger>, level: i64, method: &str, args: Vec<V>, kwargs: Vec<(String, V)>, loc: &'static Loc, exception: bool) -> R {
    let mut exc_info = if exception { Some(V::Bool(true)) } else { None };
    let mut extra = None;
    let mut stack_info = false;
    for (k, v) in kwargs {
        match k.as_str() {
            "exc_info" => exc_info = Some(v),
            "extra" => extra = Some(v),
            "stack_info" => stack_info = ops::truthy(&v)?,
            "stacklevel" => {
                if !matches!(v, V::Int(1)) {
                    return Err(Exc::type_error("py2axum: Logger.log(stacklevel=) other than 1 is not supported"));
                }
            }
            _ => return Err(Exc::type_error(format!("Logger._log() got an unexpected keyword argument '{k}'"))),
        }
    }
    let mut it = args.into_iter();
    let Some(msg) = it.next() else {
        return Err(Exc::type_error(format!("Logger.{method}() missing 1 required positional argument: 'msg'")));
    };
    let (info, exc) = match exc_info {
        None => (V::None, None),
        Some(v) if !ops::truthy(&v)? => (V::None, None),
        Some(V::Exc(e)) => (exc_tuple(&e), Some(e)),
        Some(V::Tuple(t)) => {
            let e = match t.get(1) {
                Some(V::Exc(e)) => Some(e.clone()),
                _ => None,
            };
            (V::Tuple(t), e)
        }
        Some(_) => match cx.handling.lock().last().cloned() {
            Some(e) => (exc_tuple(&e), Some(e)),
            None => (V::tuple(vec![V::None, V::None, V::None]), None),
        },
    };
    let sinfo = if stack_info { V::str(format!("Stack (most recent call last):\n{}", stack_line(loc))) } else { V::None };
    let rec = make_record(cx, &lg.name, level, loc, msg, it.collect(), info, exc, extra, sinfo)?;
    handle(cx, lg, rec).await?;
    Ok(V::None)
}

/// the logging call's frame, as `traceback.print_stack` writes it (the binary has no library frames)
fn stack_line(loc: &Loc) -> String {
    let s = format!("  File \"{}\", line {}, in {}\n{}", pathname(loc), loc.line, if loc.func.is_empty() { "<module>" } else { loc.func }, loc.text);
    s.strip_suffix('\n').unwrap_or(&s).to_string()
}

fn exc_tuple(e: &Exc) -> V {
    V::tuple(vec![V::Class(e.0.class), V::Exc(e.clone()), V::None])
}

fn method_level(method: &str) -> Option<i64> {
    Some(match method {
        "debug" => DEBUG,
        "info" => INFO,
        "warning" | "warn" => WARNING,
        "error" | "exception" => ERROR,
        "critical" | "fatal" => CRITICAL,
        _ => return None,
    })
}

/// A logging call (`logger.info(...)`, `logging.warning(...)`, `adapter.error(...)`) at a known call site.
pub async fn log_call(cx: &Cx, lg: &Arc<Logger>, method: &str, mut args: Vec<V>, kwargs: Vec<(String, V)>, loc: &'static Loc) -> R {
    let level = match method_level(method) {
        Some(l) => l,
        None => {
            // log(level, msg, *args)
            if args.is_empty() {
                return Err(Exc::type_error("Logger.log() missing 2 required positional arguments: 'level' and 'msg'"));
            }
            let l = args.remove(0);
            match l {
                V::Int(i) => i,
                V::Bool(b) => b as i64,
                _ => return Err(Exc::type_error("level must be an integer")),
            }
        }
    };
    if !lg.enabled_for(level) {
        return Ok(V::None);
    }
    Box::pin(log_enabled(cx, lg, level, method, args, kwargs, loc, method == "exception")).await
}

/// `logging.debug(...)` & co.: the root logger, configured by `basicConfig()` first when it has no handler
pub async fn module_log(cx: &Cx, method: &str, args: Vec<V>, kwargs: Vec<(String, V)>, loc: &'static Loc) -> R {
    let r = root();
    if items(&r.handlers.read()).is_empty() {
        basic_config(&[], &[])?;
    }
    log_call(cx, &r, method, args, kwargs, loc).await
}

/// a record of the runtime itself (uvicorn's messages): a logger, a level, `msg % args`, extra attributes
pub async fn emit(cx: &Cx, logger: &str, level: i64, msg: &str, args: Vec<V>, extra: Vec<(&str, V)>, exc: Option<Exc>, loc: &'static Loc) {
    let lg = get_logger(logger);
    if !lg.enabled_for(level) {
        return;
    }
    // `logger.error(msg, exc_info=exc)` (uvicorn): an exception whose `__bool__` is false stays as given,
    // and no handler formats it (Logger._log and Formatter.format test `if exc_info`)
    let (info, exc) = match exc {
        Some(e) if !ops::truthy(&V::Exc(e.clone())).unwrap_or(true) => (V::Exc(e), None),
        Some(e) => (exc_tuple(&e), Some(e)),
        None => (V::None, None),
    };
    let extra = if extra.is_empty() { None } else { V::dict_from(extra.into_iter().map(|(k, v)| (V::str(k), v)).collect()).ok() };
    let Ok(rec) = make_record(cx, logger, level, loc, V::str(msg), args, info, exc, extra, V::None) else { return };
    let _ = handle(cx, &lg, rec).await;
}

/// the synchronous variant for code outside a request (no filters but the native ones are expected)
pub fn emit_sync(logger: &str, level: i64, msg: &str, args: Vec<V>, extra: Vec<(&str, V)>, exc: Option<Exc>, loc: &'static Loc) {
    let lg = get_logger(logger);
    if !lg.enabled_for(level) {
        return;
    }
    let cx = super::root_cx();
    let fut = emit(&cx, logger, level, msg, args, extra, exc, loc);
    // native filters and handlers never wait: one poll completes it
    let waker = std::task::Waker::noop();
    let mut c = std::task::Context::from_waker(waker);
    let mut fut = std::pin::pin!(fut);
    let _ = std::future::Future::poll(fut.as_mut(), &mut c);
}

// ---------------------------------------------------------------- configuration

/// `logging.basicConfig(**kwargs)` (3.8+)
pub fn basic_config(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error(format!("basicConfig() takes 0 positional arguments but {} were given", args.len())));
    }
    let mut kw: Vec<(String, V)> = kwargs.to_vec();
    let mut pop = |n: &str| -> Option<V> { kw.iter().position(|(k, _)| k == n).map(|i| kw.remove(i).1) };
    let force = pop("force").map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false);
    let encoding = pop("encoding");
    let _errors = pop("errors");
    let r = root();
    if force {
        *r.handlers.write() = V::list(vec![]);
    }
    if !items(&r.handlers.read()).is_empty() {
        return Ok(V::None);
    }
    let handlers = pop("handlers");
    let has_stream = kw.iter().any(|(k, _)| k == "stream");
    let has_file = kw.iter().any(|(k, _)| k == "filename");
    match &handlers {
        None if has_stream && has_file => return Err(Exc::value_error("'stream' and 'filename' should not be specified together")),
        Some(_) if has_stream || has_file => return Err(Exc::value_error("'stream' or 'filename' should not be specified together with 'handlers'")),
        _ => {}
    }
    let mut kw2 = kw;
    let mut pop = |n: &str| -> Option<V> { kw2.iter().position(|(k, _)| k == n).map(|i| kw2.remove(i).1) };
    let hs: Vec<V> = match handlers {
        Some(h) => items(&h),
        None => {
            let filename = pop("filename");
            let mode = pop("filemode");
            match filename {
                Some(f) if ops::truthy(&f)? => {
                    let mut fk = vec![];
                    if let Some(m) = mode {
                        fk.push(("mode".to_string(), m));
                    }
                    if let Some(e) = encoding {
                        fk.push(("encoding".to_string(), e));
                    }
                    vec![file_handler_new(&[f], &fk)?]
                }
                _ => {
                    let stream = pop("stream");
                    vec![stream_handler_new(&stream.into_iter().collect::<Vec<_>>(), &[])?]
                }
            }
        }
    };
    let dfs = opt_str(pop("datefmt").as_ref())?;
    let style_v = pop("style");
    let style = parse_style(style_v.as_ref())?;
    let fs = match pop("format") {
        Some(f) => Some(ops::str_(&f)?),
        None => Some(match style {
            Style::Percent => BASIC_FORMAT.to_string(),
            Style::Brace => "{levelname}:{name}:{message}".to_string(),
            Style::Dollar => "${levelname}:${name}:${message}".to_string(),
        }),
    };
    let fmt = formatter_value(Formatter::new(FKind::Std, fs, dfs, style, true, None)?);
    for h in &hs {
        let Some(hh) = as_handler(h) else {
            return Err(Exc::type_error("py2axum: basicConfig(handlers=) takes logging handlers only"));
        };
        if hh.formatter.read().is_none() {
            hh.set_formatter(fmt.clone());
        }
        r.add_handler(h.clone());
    }
    if let Some(l) = pop("level") {
        r.set_level(check_level(&l)?);
    }
    if !kw2.is_empty() {
        let keys: Vec<&str> = kw2.iter().map(|(k, _)| k.as_str()).collect();
        return Err(Exc::value_error(format!("Unrecognised argument(s): {}", keys.join(", "))));
    }
    Ok(V::None)
}

/// `logging.disable(level=CRITICAL)`
pub fn disable(args: &[V], kwargs: &[(String, V)]) -> R {
    let a = bind("disable", args, kwargs, &["level"], &[])?;
    let l = match a[0] {
        Some(v) => check_level(v)?,
        None => CRITICAL,
    };
    DISABLE.store(l, Ordering::Relaxed);
    GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(V::None)
}

fn get_str(d: &V, k: &str) -> R<Option<V>> {
    match d {
        V::Dict(m) => {
            let key = Key::Str(Arc::from(k));
            Ok(m.lock().get(&key).map(|(_, v)| v.clone()))
        }
        _ => Err(Exc::value_error("dictionary expected")),
    }
}

/// `ext://sys.stdout` / `ext://sys.stderr`, or the stream object itself
fn cfg_stream(v: Option<V>) -> R<Option<V>> {
    match v {
        None => Ok(None),
        Some(V::Str(s)) => match &*s {
            "ext://sys.stdout" => Ok(Some(V::native(Native::Log(LObj::Stream(1))))),
            "ext://sys.stderr" => Ok(Some(V::native(Native::Log(LObj::Stream(2))))),
            o => Err(Exc::value_error(format!("py2axum: logging stream {} is not supported (ext://sys.stdout or ext://sys.stderr)", ops::str_repr(o)))),
        },
        Some(v) => Ok(Some(v)),
    }
}

/// `logging.config.dictConfig(config)` (version 1, not incremental): formatters (`format`/`fmt`,
/// `datefmt`, `style`, `validate`, `defaults`, `class` or `()` naming a known formatter), filters
/// (`name`), handlers (`class` StreamHandler/FileHandler/NullHandler, `level`, `formatter`, `filters`,
/// `stream`, `filename`, `mode`), loggers and root (`level`, `handlers`, `propagate`, `filters`),
/// `disable_existing_loggers`
pub fn dict_config(cfg: &V) -> R {
    let ver = get_str(cfg, "version")?;
    if !matches!(ver, Some(V::Int(1))) {
        return Err(Exc::value_error(match ver {
            None => "dictionary doesn't specify a version".to_string(),
            Some(v) => format!("Unsupported version: {}", ops::str_(&v)?),
        }));
    }
    if get_str(cfg, "incremental")?.map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false) {
        return Err(Exc::value_error("py2axum: dictConfig(incremental=True) is not supported"));
    }
    let disable_existing = get_str(cfg, "disable_existing_loggers")?.map(|v| ops::truthy(&v)).transpose()?.unwrap_or(true);
    let section = |n: &str| -> R<Vec<(String, V)>> {
        match get_str(cfg, n)? {
            None => Ok(vec![]),
            Some(d) => mapping_items(&d)?.into_iter().map(|(k, v)| Ok((ops::str_(&k)?, v))).collect(),
        }
    };
    // formatters
    let mut formatters: HashMap<String, V> = HashMap::new();
    for (name, c) in section("formatters")? {
        let f = (|| -> R<V> {
            let factory = get_str(&c, "()")?.or(get_str(&c, "class")?);
            let fmt = get_str(&c, "format")?.or(get_str(&c, "fmt")?);
            let mut kw: Vec<(String, V)> = vec![];
            if let Some(f) = fmt {
                kw.push(("fmt".into(), f));
            }
            for k in ["datefmt", "style", "validate", "defaults"] {
                if let Some(v) = get_str(&c, k)? {
                    kw.push((k.into(), v));
                }
            }
            let fac = factory.map(|f| ops::str_(&f)).transpose()?;
            match fac.as_deref() {
                None | Some("logging.Formatter") => formatter_new(&[], &kw),
                Some("uvicorn.logging.DefaultFormatter") | Some("uvicorn.logging.AccessFormatter") => {
                    kw.retain(|(k, _)| k != "validate" && k != "defaults");
                    if let Some(u) = get_str(&c, "use_colors")? {
                        kw.push(("use_colors".into(), u));
                    }
                    uv_formatter_new(fac.as_deref() == Some("uvicorn.logging.AccessFormatter"), &[], &kw)
                }
                Some("pythonjsonlogger.json.JsonFormatter") | Some("pythonjsonlogger.jsonlogger.JsonFormatter") => {
                    for k in ["rename_fields", "static_fields", "reserved_attrs", "timestamp", "prefix", "json_ensure_ascii", "json_indent"] {
                        if let Some(v) = get_str(&c, k)? {
                            kw.push((k.into(), v));
                        }
                    }
                    json_formatter_new(&[], &kw)
                }
                Some("ecs_logging.StdlibFormatter") => {
                    kw.retain(|(k, _)| k != "defaults");
                    for k in ["stack_trace_limit", "extra", "exclude_fields", "ensure_ascii"] {
                        if let Some(v) = get_str(&c, k)? {
                            kw.push((k.into(), v));
                        }
                    }
                    ecs_formatter_new(&[], &kw)
                }
                Some(o) => Err(Exc::value_error(format!("py2axum: formatter class {} is not supported", ops::str_repr(o)))),
            }
        })()
        .map_err(|e| chained(Exc::value_error(format!("Unable to configure formatter {}", ops::str_repr(&name))), e))?;
        formatters.insert(name, f);
    }
    // filters
    let mut filters: HashMap<String, V> = HashMap::new();
    for (name, c) in section("filters")? {
        if get_str(&c, "()")?.is_some() {
            return Err(Exc::value_error(format!("py2axum: filter {} with a factory is not supported", ops::str_repr(&name))));
        }
        let n = get_str(&c, "name")?.unwrap_or(V::str(""));
        filters.insert(name, filter_new(&[n], &[])?);
    }
    let filter_list = |c: &V| -> R<Vec<V>> {
        match get_str(c, "filters")? {
            None => Ok(vec![]),
            Some(l) => items(&l)
                .iter()
                .map(|n| {
                    let n = ops::str_(n)?;
                    filters.get(&n).cloned().ok_or_else(|| Exc::value_error(format!("Unable to add filter {}", ops::str_repr(&n))))
                })
                .collect(),
        }
    };
    // handlers: the previous ones are closed (`_clearExistingHandlers`)
    NAMED.lock().clear();
    let mut handlers: HashMap<String, V> = HashMap::new();
    for (name, c) in section("handlers")? {
        let h = (|| -> R<V> {
            if get_str(&c, "()")?.is_some() {
                return Err(Exc::value_error("py2axum: a handler factory is not supported"));
            }
            let class = get_str(&c, "class")?.map(|v| ops::str_(&v)).transpose()?.unwrap_or_default();
            let h = match class.as_str() {
                "logging.StreamHandler" => stream_handler_new(&cfg_stream(get_str(&c, "stream")?)?.into_iter().collect::<Vec<_>>(), &[])?,
                "logging.FileHandler" => {
                    let mut kw = vec![];
                    for k in ["filename", "mode", "encoding", "delay"] {
                        if let Some(v) = get_str(&c, k)? {
                            kw.push((k.to_string(), v));
                        }
                    }
                    file_handler_new(&[], &kw)?
                }
                "logging.NullHandler" => null_handler_new(&[], &[])?,
                o => return Err(Exc::value_error(format!("py2axum: handler class {} is not supported", ops::str_repr(o)))),
            };
            let hh = as_handler(&h).expect("handler");
            if let Some(f) = get_str(&c, "formatter")? {
                let n = ops::str_(&f)?;
                let f = formatters.get(&n).cloned().ok_or_else(|| Exc::value_error(format!("Unable to set formatter {}", ops::str_repr(&n))))?;
                hh.set_formatter(f);
            }
            if let Some(l) = get_str(&c, "level")? {
                hh.level.store(check_level(&l)?, Ordering::Relaxed);
            }
            for f in filter_list(&c)? {
                add_to(&hh.filters, f);
            }
            *hh.name.write() = V::str(&name);
            NAMED.lock().push((name.clone(), hh.clone()));
            Ok(h)
        })()
        .map_err(|e| chained(Exc::value_error(format!("Unable to configure handler {}", ops::str_repr(&name))), e))?;
        handlers.insert(name, h);
    }
    let configure = |lg: &Arc<Logger>, c: &V, is_root: bool| -> R<()> {
        if let Some(l) = get_str(c, "level")? {
            lg.set_level(check_level(&l)?);
        }
        *lg.handlers.write() = V::list(vec![]);
        if let Some(hl) = get_str(c, "handlers")? {
            for n in items(&hl) {
                let n = ops::str_(&n)?;
                let h = handlers.get(&n).cloned().ok_or_else(|| Exc::value_error(format!("Unable to add handler {}", ops::str_repr(&n))))?;
                lg.add_handler(h);
            }
        }
        if !is_root {
            if let Some(p) = get_str(c, "propagate")? {
                lg.propagate.store(ops::truthy(&p)?, Ordering::Relaxed);
            }
        }
        *lg.filters.write() = V::list(filter_list(c)?);
        Ok(())
    };
    // loggers: the existing ones not configured are disabled (or reset, for children of configured ones)
    let mut existing: Vec<String> = REGISTRY.loggers.read().keys().cloned().collect();
    existing.sort();
    let mut children: Vec<String> = vec![];
    for (name, c) in section("loggers")? {
        if let Some(i) = existing.iter().position(|e| *e == name) {
            let prefixed = format!("{name}.");
            children.extend(existing[i + 1..].iter().filter(|e| e.starts_with(&prefixed)).cloned());
            existing.remove(i);
        }
        let lg = get_logger(&name);
        configure(&lg, &c, false).map_err(|e| chained(Exc::value_error(format!("Unable to configure logger {}", ops::str_repr(&name))), e))?;
    }
    for name in existing {
        let Some(lg) = REGISTRY.loggers.read().get(&name).cloned() else { continue };
        if children.contains(&name) {
            if !lg.placeholder.load(Ordering::Relaxed) {
                lg.set_level(NOTSET);
                *lg.handlers.write() = V::list(vec![]);
                lg.propagate.store(true, Ordering::Relaxed);
            }
        } else if !lg.placeholder.load(Ordering::Relaxed) {
            lg.disabled.store(disable_existing, Ordering::Relaxed);
        }
    }
    if let Some(c) = get_str(cfg, "root")? {
        configure(&root(), &c, true).map_err(|e| chained(Exc::value_error("Unable to configure root logger"), e))?;
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(V::None)
}

// ---------------------------------------------------------------- LoggerAdapter

pub struct Adapter {
    pub logger: Arc<Logger>,
    extra: RwLock<V>,
    merge: bool,
}

/// `logging.LoggerAdapter(logger, extra=None, merge_extra=False)`
pub fn adapter_new(args: &[V], kwargs: &[(String, V)]) -> R {
    // merge_extra: 3.13
    let names: &[&str] = if super::python().1 >= 13 { &["logger", "extra", "merge_extra"] } else { &["logger", "extra"] };
    let a = bind("LoggerAdapter.__init__", args, kwargs, names, &[])?;
    let logger = match a[0] {
        Some(V::Native(n)) => match &**n {
            Native::Log(LObj::Logger(l)) => l.clone(),
            Native::Log(LObj::Adapter(ad)) => ad.logger.clone(),
            _ => return Err(Exc::type_error("py2axum: LoggerAdapter of something else than a logger")),
        },
        _ => return Err(Exc::type_error("LoggerAdapter.__init__() missing 1 required positional argument: 'logger'")),
    };
    let merge = a.get(2).copied().flatten().map(ops::truthy).transpose()?.unwrap_or(false);
    Ok(V::native(Native::Log(LObj::Adapter(Arc::new(Adapter { logger, extra: RwLock::new(a[1].cloned().unwrap_or(V::None)), merge })))))
}

impl Adapter {
    /// `process(msg, kwargs)`: the adapter's `extra` replaces (or, merge_extra, completes) the call's
    fn process(&self, kwargs: &mut Vec<(String, V)>) -> R<()> {
        let own = self.extra.read().clone();
        let pos = kwargs.iter().position(|(k, _)| k == "extra");
        match pos {
            Some(i) if self.merge => {
                let mut items = if own.is_none() { vec![] } else { mapping_items(&own)? };
                items.extend(mapping_items(&kwargs[i].1)?);
                kwargs[i].1 = V::dict_from(items)?;
            }
            Some(i) => kwargs[i].1 = own,
            None => kwargs.push(("extra".into(), own)),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- Python surface

const LOGGER_METHODS: &[&str] = &[
    "debug", "info", "warning", "warn", "error", "exception", "critical", "fatal", "log", "setLevel", "getEffectiveLevel", "isEnabledFor",
    "getChild", "getChildren", "addHandler", "removeHandler", "hasHandlers", "addFilter", "removeFilter", "filter", "handle",
];

/// a method call on a logging object at a known call site (the logging methods record it)
pub async fn call_at(cx: &Cx, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>, loc: &'static Loc) -> R {
    if let V::Native(n) = recv {
        if let Native::Log(o) = &**n {
            return method(cx, o, recv, name, args, kwargs, loc).await;
        }
    }
    call_method(cx, recv, name, args, kwargs).await
}

pub async fn method(cx: &Cx, o: &LObj, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>, loc: &'static Loc) -> R {
    match o {
        LObj::Logger(l) => logger_method(cx, l, recv, name, args, kwargs, loc).await,
        LObj::Adapter(a) => {
            if let Some(level) = method_level(name).or(if name == "log" { Some(-1) } else { None }) {
                let mut args = args;
                let level = if level == -1 {
                    match args.first() {
                        Some(V::Int(i)) => *i,
                        _ => return Err(Exc::type_error("level must be an integer")),
                    }
                } else {
                    level
                };
                if !a.logger.enabled_for(level) {
                    return Ok(V::None);
                }
                let mut kwargs = kwargs;
                if name == "exception" && !kwargs.iter().any(|(k, _)| k == "exc_info") {
                    kwargs.push(("exc_info".into(), V::Bool(true)));
                }
                a.process(&mut kwargs)?;
                if name != "log" {
                    args.insert(0, V::Int(level));
                }
                return log_call(cx, &a.logger, "log", args, kwargs, loc).await;
            }
            match name {
                "process" => {
                    let msg = args.first().cloned().unwrap_or(V::None);
                    let mut kw = match args.get(1) {
                        Some(d) => mapping_items(d)?.into_iter().map(|(k, v)| Ok((ops::str_(&k)?, v))).collect::<R<Vec<_>>>()?,
                        None => vec![],
                    };
                    a.process(&mut kw)?;
                    Ok(V::tuple(vec![msg, V::dict_from(kw.into_iter().map(|(k, v)| (V::str(k), v)).collect())?]))
                }
                "isEnabledFor" | "getEffectiveLevel" | "setLevel" | "hasHandlers" => {
                    logger_method(cx, &a.logger, &logger_value(a.logger.clone()), name, args, kwargs, loc).await
                }
                _ => Err(Exc::attr_error(format!("'LoggerAdapter' object has no attribute '{name}'"))),
            }
        }
        LObj::Handler(h) => handler_method(cx, h, name, args, kwargs).await,
        LObj::Formatter(f) => formatter_method(f, name, &args, &kwargs),
        LObj::Record(r) => match name {
            "getMessage" => Ok(V::str(r.get_message()?)),
            _ => Err(Exc::attr_error(format!("'LogRecord' object has no attribute '{name}'"))),
        },
        LObj::Filter(n) => match name {
            "filter" => {
                let rec = args.first().and_then(as_record).ok_or_else(|| Exc::type_error("filter() takes a LogRecord"))?;
                Ok(V::Bool(name_matches(n, &rec.name())))
            }
            _ => Err(Exc::attr_error(format!("'Filter' object has no attribute '{name}'"))),
        },
        LObj::Stream(s) => match name {
            "write" => {
                let t = ops::str_(args.first().unwrap_or(&V::None))?;
                if *s == 1 {
                    let _ = std::io::stdout().lock().write_all(t.as_bytes());
                } else {
                    let _ = std::io::stderr().lock().write_all(t.as_bytes());
                }
                Ok(V::Int(t.chars().count() as i64))
            }
            "flush" => {
                let _ = if *s == 1 { std::io::stdout().flush() } else { std::io::stderr().flush() };
                Ok(V::None)
            }
            "isatty" => Ok(V::Bool(if *s == 1 { std::io::IsTerminal::is_terminal(&std::io::stdout()) } else { std::io::IsTerminal::is_terminal(&std::io::stderr()) })),
            "fileno" => Ok(V::Int(*s as i64)),
            _ => Err(Exc::attr_error(format!("'_io.TextIOWrapper' object has no attribute '{name}'"))),
        },
    }
    .map(|v| {
        let _ = recv;
        v
    })
}

async fn logger_method(cx: &Cx, l: &Arc<Logger>, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>, loc: &'static Loc) -> R {
    if method_level(name).is_some() || name == "log" {
        return log_call(cx, l, name, args, kwargs, loc).await;
    }
    let one = |what: &str| -> R<V> {
        if !kwargs.is_empty() {
            return Err(Exc::type_error(format!("Logger.{what}() got an unexpected keyword argument '{}'", kwargs[0].0)));
        }
        args.first().cloned().ok_or_else(|| Exc::type_error(format!("Logger.{what}() missing 1 required positional argument")))
    };
    match name {
        "setLevel" => {
            l.set_level(check_level(&one("setLevel")?)?);
            Ok(V::None)
        }
        "getEffectiveLevel" => Ok(V::Int(l.effective_level())),
        "isEnabledFor" => {
            let lv = one("isEnabledFor")?;
            let lv = match lv {
                V::Int(i) => i,
                o => return Err(Exc::type_error(format!("'>=' not supported between instances of '{}' and 'int'", o.type_name()))),
            };
            Ok(V::Bool(l.enabled_for(lv)))
        }
        "getChild" => {
            let s = ops::str_(&one("getChild")?)?;
            let n = if Arc::ptr_eq(l, &ROOT) { s } else { format!("{}.{s}", l.name) };
            Ok(logger_value(get_logger(&n)))
        }
        "getChildren" => {
            let reg = REGISTRY.loggers.read();
            let out: Vec<V> = reg
                .values()
                .filter(|c| !c.placeholder.load(Ordering::Relaxed) && c.parent.as_ref().is_some_and(|p| Arc::ptr_eq(p, l)))
                .map(|c| logger_value(c.clone()))
                .collect();
            Ok(V::Set(Arc::new(Mutex::new(out.into_iter().map(|v| (Key::Ptr(key_id(&v)), v)).collect()))))
        }
        "addHandler" => {
            let h = one("addHandler")?;
            l.add_handler(h);
            Ok(V::None)
        }
        "removeHandler" => {
            l.remove_handler(&one("removeHandler")?);
            Ok(V::None)
        }
        "hasHandlers" => Ok(V::Bool(l.has_handlers())),
        "addFilter" => {
            add_to(&l.filters, one("addFilter")?);
            Ok(V::None)
        }
        "removeFilter" => {
            remove_from(&l.filters, &one("removeFilter")?);
            Ok(V::None)
        }
        "filter" => {
            let rec = as_record(&one("filter")?).ok_or_else(|| Exc::type_error("filter() takes a LogRecord"))?;
            let f = l.filters.read().clone();
            Ok(match run_filters(cx, &f, rec).await? {
                Some(r) => record_value(r),
                None => V::Bool(false),
            })
        }
        "handle" => {
            let rec = as_record(&one("handle")?).ok_or_else(|| Exc::type_error("handle() takes a LogRecord"))?;
            handle(cx, l, rec).await?;
            Ok(V::None)
        }
        _ => {
            let _ = recv;
            Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if Arc::ptr_eq(l, &ROOT) { "RootLogger" } else { "Logger" })))
        }
    }
}

fn key_id(v: &V) -> usize {
    match v {
        V::Native(n) => Arc::as_ptr(n) as *const () as usize,
        _ => 0,
    }
}

async fn handler_method(cx: &Cx, h: &Arc<Handler>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let cls = h.kind.class_name();
    let one = |what: &str| -> R<V> {
        if !kwargs.is_empty() {
            return Err(Exc::type_error(format!("{cls}.{what}() got an unexpected keyword argument '{}'", kwargs[0].0)));
        }
        args.first().cloned().ok_or_else(|| Exc::type_error(format!("{cls}.{what}() missing 1 required positional argument")))
    };
    match name {
        "setLevel" => {
            h.level.store(check_level(&one("setLevel")?)?, Ordering::Relaxed);
            Ok(V::None)
        }
        "setFormatter" => {
            let f = one("setFormatter")?;
            if !f.is_none() && as_formatter(&f).is_none() {
                return Err(Exc::type_error("py2axum: setFormatter() takes a logging.Formatter (a project subclass is not supported)"));
            }
            h.set_formatter(f);
            Ok(V::None)
        }
        "addFilter" => {
            add_to(&h.filters, one("addFilter")?);
            Ok(V::None)
        }
        "removeFilter" => {
            remove_from(&h.filters, &one("removeFilter")?);
            Ok(V::None)
        }
        "format" => {
            let rec = as_record(&one("format")?).ok_or_else(|| Exc::type_error("format() takes a LogRecord"))?;
            Ok(V::str(h.format(&rec)?))
        }
        "handle" | "emit" => {
            let rec = as_record(&one(name)?).ok_or_else(|| Exc::type_error("handle() takes a LogRecord"))?;
            if name == "emit" {
                h.emit(&rec);
            } else {
                handler_handle(cx, h, rec).await?;
            }
            Ok(V::None)
        }
        "flush" | "acquire" | "release" => Ok(V::None),
        "close" => {
            if let HKind::File(f) = &h.kind {
                *f.file.lock() = None;
            }
            NAMED.lock().retain(|(_, x)| !Arc::ptr_eq(x, h));
            Ok(V::None)
        }
        "get_name" => Ok(h.name.read().clone()),
        "set_name" => {
            set_handler_name(h, one("set_name")?);
            Ok(V::None)
        }
        "setStream" => {
            Err(Exc::type_error("py2axum: StreamHandler.setStream() is not supported"))
        }
        _ => Err(Exc::attr_error(format!("'{cls}' object has no attribute '{name}'"))),
    }
}

fn set_handler_name(h: &Arc<Handler>, name: V) {
    let mut named = NAMED.lock();
    named.retain(|(_, x)| !Arc::ptr_eq(x, h));
    if let V::Str(s) = &name {
        named.retain(|(n, _)| n != &**s);
        named.push((s.to_string(), h.clone()));
    }
    *h.name.write() = name;
}

fn formatter_method(f: &Arc<Formatter>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let rec = || args.first().and_then(as_record).ok_or_else(|| Exc::type_error(format!("{name}() takes a LogRecord")));
    match name {
        "format" => {
            let r = rec()?;
            Ok(V::str(f.format(&r)?))
        }
        "formatMessage" => {
            let r = rec()?;
            Ok(V::str(f.format_message(&r, &[])?))
        }
        "formatTime" => {
            let r = rec()?;
            let df = match args.get(1).or_else(|| kw(kwargs, "datefmt")) {
                Some(v) => opt_str(Some(v))?,
                None => None,
            };
            let created = match r.get("created") {
                Some(V::Float(x)) => x,
                _ => now(),
            };
            let msecs = match r.get("msecs") {
                Some(V::Float(x)) => x,
                _ => 0.0,
            };
            Ok(V::str(f.format_time(created, msecs, df.as_deref())))
        }
        "usesTime" => Ok(V::Bool(f.uses_time)),
        "formatException" => {
            let info = args.first().cloned().unwrap_or(V::None);
            let e = match &info {
                V::Tuple(t) if t.len() == 3 => match &t[1] {
                    V::Exc(e) => Some(e.clone()),
                    _ => None,
                },
                _ => None,
            };
            let s = match e {
                Some(e) => super::trace::format_exc(&e),
                None => "NoneType: None\n".into(),
            };
            Ok(V::str(s.strip_suffix('\n').unwrap_or(&s)))
        }
        _ => Err(Exc::attr_error(format!("'Formatter' object has no attribute '{name}'"))),
    }
}

/// attributes of the logging objects
pub fn attr(o: &LObj, recv: &V, name: &str) -> R {
    match o {
        LObj::Logger(l) => match name {
            "name" => Ok(V::str(&*l.name)),
            "level" => Ok(V::Int(l.level())),
            "propagate" => Ok(V::Bool(l.propagate.load(Ordering::Relaxed))),
            "disabled" => Ok(V::Bool(l.disabled.load(Ordering::Relaxed))),
            "handlers" => Ok(l.handlers.read().clone()),
            "filters" => Ok(l.filters.read().clone()),
            "parent" => Ok(match &l.parent {
                Some(p) => {
                    // the nearest logger that is not a placeholder
                    let mut p = p.clone();
                    while p.placeholder.load(Ordering::Relaxed) {
                        match &p.parent {
                            Some(q) => p = q.clone(),
                            None => break,
                        }
                    }
                    logger_value(p)
                }
                None => V::None,
            }),
            "root" => Ok(logger_value(root())),
            m if LOGGER_METHODS.contains(&m) => Ok(bound(recv, m)),
            _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(o)))),
        },
        LObj::Adapter(a) => match name {
            "logger" => Ok(logger_value(a.logger.clone())),
            "extra" => Ok(a.extra.read().clone()),
            "name" => Ok(V::str(&*a.logger.name)),
            "manager" => Err(Exc::attr_error("py2axum: LoggerAdapter.manager is not supported")),
            m if LOGGER_METHODS.contains(&m) || m == "process" => Ok(bound(recv, m)),
            _ => Err(Exc::attr_error(format!("'LoggerAdapter' object has no attribute '{name}'"))),
        },
        LObj::Handler(h) => match name {
            "level" => Ok(V::Int(h.level())),
            "formatter" => Ok(h.formatter.read().clone()),
            "filters" => Ok(h.filters.read().clone()),
            "name" => Ok(h.name.read().clone()),
            "terminator" => Ok(V::str(&*h.terminator.read())),
            "stream" => match h.kind {
                HKind::Stream(s) => Ok(V::native(Native::Log(LObj::Stream(s)))),
                _ => Err(Exc::attr_error(format!("'{}' object has no attribute 'stream'", h.kind.class_name()))),
            },
            "baseFilename" => match &h.kind {
                HKind::File(f) => Ok(V::str(&f.path)),
                _ => Err(Exc::attr_error(format!("'{}' object has no attribute 'baseFilename'", h.kind.class_name()))),
            },
            "mode" => match &h.kind {
                HKind::File(f) => Ok(V::str(&f.mode)),
                _ => Err(Exc::attr_error(format!("'{}' object has no attribute 'mode'", h.kind.class_name()))),
            },
            m if ["setLevel", "setFormatter", "addFilter", "removeFilter", "format", "handle", "emit", "flush", "close"].contains(&m) => Ok(bound(recv, m)),
            _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", h.kind.class_name()))),
        },
        LObj::Formatter(f) => match name {
            "_fmt" => Ok(V::str(&f.fmt)),
            "datefmt" => Ok(f.datefmt.read().as_deref().map(V::str).unwrap_or(V::None)),
            "default_time_format" => Ok(V::str(&*f.default_time_format.read())),
            "default_msec_format" => Ok(f.default_msec_format.read().as_deref().map(V::str).unwrap_or(V::None)),
            m if ["format", "formatTime", "formatException", "formatMessage", "usesTime"].contains(&m) => Ok(bound(recv, m)),
            _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(o)))),
        },
        LObj::Record(r) => match name {
            "__dict__" => V::dict_from(r.dict_items().into_iter().map(|(k, v)| (V::str(k), v)).collect()),
            "getMessage" => Ok(bound(recv, "getMessage")),
            _ => r.get(name).ok_or_else(|| Exc::attr_error(format!("'LogRecord' object has no attribute '{name}'"))),
        },
        LObj::Filter(n) => match name {
            "name" => Ok(V::str(&**n)),
            "nlen" => Ok(V::Int(n.len() as i64)),
            "filter" => Ok(bound(recv, "filter")),
            _ => Err(Exc::attr_error(format!("'Filter' object has no attribute '{name}'"))),
        },
        LObj::Stream(s) => match name {
            "name" => Ok(V::str(if *s == 1 { "<stdout>" } else { "<stderr>" })),
            "encoding" => Ok(V::str("utf-8")),
            m if ["write", "flush", "isatty", "fileno"].contains(&m) => Ok(bound(recv, m)),
            _ => Err(Exc::attr_error(format!("'_io.TextIOWrapper' object has no attribute '{name}'"))),
        },
    }
}

fn bound(recv: &V, m: &str) -> V {
    // a method read as a value (`log = logger.info`): `MethodOf` dispatches the call back here
    let name: &'static str = BOUND_NAMES.iter().find(|n| **n == m).copied().unwrap_or("info");
    V::native(Native::MethodOf(recv.clone(), name))
}

const BOUND_NAMES: &[&str] = &[
    "debug", "info", "warning", "warn", "error", "exception", "critical", "fatal", "log", "setLevel", "getEffectiveLevel", "isEnabledFor",
    "getChild", "getChildren", "addHandler", "removeHandler", "hasHandlers", "addFilter", "removeFilter", "filter", "handle", "process",
    "setFormatter", "format", "emit", "flush", "close", "formatTime", "formatException", "formatMessage", "usesTime", "getMessage", "write",
    "isatty", "fileno",
];

/// `obj.attr = value` on the logging objects
pub fn setattr(o: &LObj, name: &str, val: V) -> R<()> {
    match o {
        LObj::Logger(l) => match name {
            "level" => {
                l.set_level(check_level(&val)?);
                Ok(())
            }
            "propagate" => {
                l.propagate.store(ops::truthy(&val)?, Ordering::Relaxed);
                Ok(())
            }
            "disabled" => {
                l.disabled.store(ops::truthy(&val)?, Ordering::Relaxed);
                Ok(())
            }
            "handlers" => {
                *l.handlers.write() = val;
                Ok(())
            }
            "filters" => {
                *l.filters.write() = val;
                Ok(())
            }
            _ => Err(Exc::attr_error(format!("py2axum: setting Logger.{name} is not supported"))),
        },
        LObj::Handler(h) => match name {
            "level" => {
                h.level.store(check_level(&val)?, Ordering::Relaxed);
                Ok(())
            }
            "formatter" => {
                h.set_formatter(val);
                Ok(())
            }
            "filters" => {
                *h.filters.write() = val;
                Ok(())
            }
            "name" => {
                set_handler_name(h, val);
                Ok(())
            }
            "terminator" => {
                *h.terminator.write() = ops::str_(&val)?;
                Ok(())
            }
            _ => Err(Exc::attr_error(format!("py2axum: setting {}.{name} is not supported", h.kind.class_name()))),
        },
        LObj::Formatter(f) => match name {
            "datefmt" => {
                *f.datefmt.write() = opt_str(Some(&val))?;
                Ok(())
            }
            "default_time_format" => {
                *f.default_time_format.write() = ops::str_(&val)?;
                Ok(())
            }
            "default_msec_format" => {
                *f.default_msec_format.write() = opt_str(Some(&val))?;
                Ok(())
            }
            _ => Err(Exc::attr_error(format!("py2axum: setting Formatter.{name} is not supported"))),
        },
        LObj::Record(r) => {
            r.set(name, val);
            Ok(())
        }
        LObj::Adapter(a) if name == "extra" => {
            *a.extra.write() = val;
            Ok(())
        }
        _ => Err(Exc::attr_error(format!("'{}' object attribute '{name}' is read-only", type_name(o)))),
    }
}

/// `logging.lastResort`
pub fn last_resort() -> V {
    if LAST_RESORT_ON.load(Ordering::Relaxed) {
        handler_value(LAST_RESORT.clone())
    } else {
        V::None
    }
}

/// `sys.stdout` (1) / `sys.stderr` (2)
pub fn stream(fd: u8) -> V {
    V::native(Native::Log(LObj::Stream(fd)))
}

/// `logging.getHandlerByName(name)` (3.12)
pub fn handler_by_name(args: &[V]) -> R {
    let n = ops::str_(args.first().unwrap_or(&V::None))?;
    Ok(NAMED.lock().iter().find(|(k, _)| *k == n).map(|(_, h)| handler_value(h.clone())).unwrap_or(V::None))
}

/// `logging.getHandlerNames()` (3.12)
pub fn handler_names() -> R {
    let names: Vec<V> = NAMED.lock().iter().map(|(n, _)| V::str(n)).collect();
    Ok(V::Set(Arc::new(Mutex::new(names.into_iter().map(|v| (Key::Str(Arc::from(ops::str_(&v).unwrap_or_default().as_str())), v)).collect()))))
}

/// `logging.getLevelNamesMapping()` (3.11)
pub fn level_names_mapping() -> R {
    V::dict_from(NAME_LEVELS.read().iter().map(|(n, l)| (V::str(n), V::Int(*l))).collect())
}

/// `logging.shutdown()`: flushes the handlers
pub fn shutdown() -> R {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    Ok(V::None)
}
