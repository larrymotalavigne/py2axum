//! loguru 0.7 (`from loguru import logger`), the part applications use: the default handler on `sys.stderr`
//! that `import loguru` adds, `logger.trace/debug/info/success/warning/error/critical(message, *args,
//! **kwargs)` and `logger.log(level, ...)`, `logger.add(sys.stderr | sys.stdout, level=, format=, colorize=)`,
//! `logger.remove([id])`. A record is written as loguru writes it: its format with the call site's module
//! (`__name__`), function and line, the local time to the microsecond, and the colours (`<green>`, `<level>`...)
//! when the stream is a terminal (or `CI` with a CI's variable set, as loguru decides).
//!
//! The formats are compiled at translation (py2axum/logurufmt.py, a port of loguru's colorizer): one text without
//! colours and one per level with them, each a list of literal text and record fields. Everything else of loguru
//! (exceptions, `bind`, `opt`, file sinks, `serialize=`...) is refused at translation.
use std::io::Write;
use std::sync::LazyLock;

use parking_lot::RwLock;

use super::logging::Loc;
use super::methods::call_method;
use super::ops;
use super::v::*;
use super::Cx;

/// a record field of a format (`{name}`, `{level.no}`...)
#[derive(Clone, Copy)]
pub enum F {
    Level,
    LevelName,
    LevelNo,
    LevelIcon,
    Name,
    Function,
    Line,
    Message,
    Module,
    File,
    FilePath,
    Exception,
}

/// a part of `{time:...}`: loguru's tokens (YYYY, MM, SSS...) and strftime's directives, the same values
#[derive(Clone, Copy)]
pub enum T {
    Lit(&'static str),
    Year,
    Year2,
    Quarter,
    MonthName,
    MonthAbbr,
    Month2,
    Month,
    YDay3,
    YDay,
    Day2,
    Day,
    WeekdayName,
    WeekdayAbbr,
    Weekday,
    WeekdayIso,
    Hour2,
    Hour,
    Hour12x2,
    Hour12,
    Min2,
    Min,
    Sec2,
    Sec,
    /// the first n digits of the microseconds (S..SSSSSS, %f = 6)
    Frac(u8),
    AmPm,
    /// `Z` (+01:00) / `ZZ` and `%z` (+0100)
    Tz(bool),
    TzName,
    Timestamp,
    TimestampMicro,
}

#[derive(Clone, Copy)]
pub enum Seg {
    Lit(&'static str),
    /// a field, `!r` (repr) or not, its format spec
    Field(F, bool, &'static str),
    /// `{time:spec}`: the parts, in UTC (`!UTC`) or local time
    Time(&'static [T], bool),
}

/// a compiled format: the text without colours, and one per level with them (TRACE ... CRITICAL, then any other
/// level: "Level 25")
#[derive(Clone, Copy)]
pub struct Format {
    pub plain: &'static [Seg],
    pub colored: &'static [&'static [Seg]; 8],
}

const LEVELS: [(&str, i64, &str); 7] = [
    ("TRACE", 5, "\u{270F}\u{FE0F}"),
    ("DEBUG", 10, "\u{1F41E}"),
    ("INFO", 20, "\u{2139}\u{FE0F}"),
    ("SUCCESS", 25, "\u{2705}"),
    ("WARNING", 30, "\u{26A0}\u{FE0F}"),
    ("ERROR", 40, "\u{274C}"),
    ("CRITICAL", 50, "\u{2620}\u{FE0F}"),
];

struct Handler {
    id: i64,
    levelno: i64,
    fd: u8,
    colorize: bool,
    format: Format,
}

struct Core {
    handlers: Vec<Handler>,
    count: i64,
    /// the lowest level of a handler (None: no handler)
    min_level: Option<i64>,
}

static CORE: LazyLock<RwLock<Core>> = LazyLock::new(|| RwLock::new(Core { handlers: vec![], count: 0, min_level: None }));
static DEFAULTS: RwLock<Option<Defaults>> = RwLock::new(None);

/// the LOGURU_* variables loguru reads at import (`_defaults.py`) that the binary reproduces
struct Defaults {
    level: V,
    colorize: Option<bool>,
}

/// loguru's `env(key, bool)` (`_defaults.py`)
fn env(key: &str) -> Option<bool> {
    let v = std::env::var(key).ok()?;
    match v.to_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "ok" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "nok" | "off" => Some(false),
        _ => fail(format!("ValueError: Invalid environment variable '{key}' (expected a boolean): '{v}'")),
    }
}

fn fail(msg: String) -> ! {
    eprintln!("py2axum: {msg}");
    std::process::exit(1);
}

/// `import loguru`: its defaults from the environment, and `logger.add(sys.stderr)` unless LOGURU_AUTOINIT is false.
/// A variable whose effect the binary does not reproduce (another format, a filter, serialisation, colours or
/// numbers of the levels) stops the server at startup, with its name.
pub fn init(default: Format) {
    // LOGURU_BACKTRACE, _DIAGNOSE and _CATCH change only what is refused (exceptions) or a failing sink's report
    const KEPT: [&str; 8] = ["LOGURU_AUTOINIT", "LOGURU_LEVEL", "LOGURU_COLORIZE", "LOGURU_BACKTRACE", "LOGURU_DIAGNOSE",
                             "LOGURU_CATCH", "LOGURU_ENQUEUE", "LOGURU_SERIALIZE"];
    let mut other: Vec<String> = std::env::vars_os().filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("LOGURU_") && !KEPT.contains(&k.as_str())).collect();
    other.sort();
    if let Some(k) = other.first() {
        fail(format!("{k} is set: of loguru's environment variables, the binary reproduces LOGURU_LEVEL, LOGURU_COLORIZE and LOGURU_AUTOINIT"));
    }
    let autoinit = env("LOGURU_AUTOINIT");
    let colorize = env("LOGURU_COLORIZE");
    let _ = (env("LOGURU_BACKTRACE"), env("LOGURU_DIAGNOSE"), env("LOGURU_CATCH"));
    if env("LOGURU_ENQUEUE") == Some(true) {
        fail("LOGURU_ENQUEUE is true: the binary writes each message at once".into());
    }
    if env("LOGURU_SERIALIZE") == Some(true) {
        fail("LOGURU_SERIALIZE is true: the binary does not serialise the records".into());
    }
    let level = std::env::var("LOGURU_LEVEL").map(V::str).unwrap_or_else(|_| V::str("DEBUG"));
    *DEFAULTS.write() = Some(Defaults { level: level.clone(), colorize });
    if autoinit.unwrap_or(true) {
        if let Err(e) = add_handler(2, &level, colorize.map(V::Bool).unwrap_or(V::None), default) {
            fail(super::trace::format_exc_only(&e).trim_end().to_string());
        }
    }
}

/// loguru's `_colorama.should_colorize(stream)` for sys.stdout / sys.stderr
fn should_colorize(fd: u8) -> bool {
    let ci = [std::env::var_os("TRAVIS"), std::env::var_os("CIRCLECI"), std::env::var_os("APPVEYOR"), std::env::var_os("GITLAB_CI"), std::env::var_os("GITHUB_ACTIONS")];
    if std::env::var_os("CI").is_some() && ci.iter().any(|v| v.is_some()) {
        return true;
    }
    if std::env::var_os("PYCHARM_HOSTED").is_some() {
        return true;
    }
    if fd == 1 {
        std::io::IsTerminal::is_terminal(&std::io::stdout())
    } else {
        std::io::IsTerminal::is_terminal(&std::io::stderr())
    }
}

fn level_of(level: &V) -> R<i64> {
    match level {
        V::Str(_) => {
            let name = ops::str_(level)?;
            match LEVELS.iter().find(|l| l.0 == name) {
                Some(l) => Ok(l.1),
                None => Err(Exc::value_error(format!("Level '{name}' does not exist"))),
            }
        }
        V::Int(i) => check_no(*i),
        V::Bool(b) => check_no(*b as i64),
        _ => Err(Exc::type_error(format!("Invalid level, it should be an integer or a string, not: '{}'", level.type_name()))),
    }
}

fn check_no(n: i64) -> R<i64> {
    if n < 0 {
        return Err(Exc::value_error(format!("Invalid level value, it should be a positive integer, not: {n}")));
    }
    Ok(n)
}

fn add_handler(fd: u8, level: &V, colorize: V, format: Format) -> R {
    let mut core = CORE.write();
    let id = core.count;
    core.count += 1;
    drop(core);
    let colorize = match colorize {
        V::None => should_colorize(fd),
        v => ops::truthy(&v)?,
    };
    let levelno = level_of(level)?;
    let mut core = CORE.write();
    core.handlers.push(Handler { id, levelno, fd, colorize, format });
    core.min_level = Some(core.min_level.map_or(levelno, |m| m.min(levelno)));
    Ok(V::Int(id))
}

/// `logger.add(sys.stderr | sys.stdout, level=, format=, colorize=)` (the format compiled at translation);
/// `level` and `colorize` left out are loguru's defaults (LOGURU_LEVEL, LOGURU_COLORIZE)
pub fn add(fd: u8, level: Option<V>, colorize: Option<V>, format: Format) -> R {
    let (dl, dc) = match &*DEFAULTS.read() {
        Some(d) => (d.level.clone(), d.colorize),
        None => (V::str("DEBUG"), None),
    };
    let colorize = colorize.unwrap_or_else(|| dc.map(V::Bool).unwrap_or(V::None));
    add_handler(fd, &level.unwrap_or(dl), colorize, format)
}

/// `logger.remove(handler_id=None)`
pub fn remove(id: Option<V>) -> R {
    let id = match id {
        None | Some(V::None) => None,
        Some(V::Int(i)) => Some(i),
        Some(V::Bool(b)) => Some(b as i64),
        Some(v) => {
            return Err(Exc::type_error(format!(
                "Invalid handler id, it should be an integer as returned by the 'add()' method (or None), not: '{}'",
                v.type_name()
            )))
        }
    };
    let mut core = CORE.write();
    match id {
        Some(i) => {
            let Some(pos) = core.handlers.iter().position(|h| h.id == i) else {
                return Err(Exc::value_error(format!("There is no existing handler with id {i}")));
            };
            core.handlers.remove(pos);
        }
        None => core.handlers.clear(),
    }
    core.min_level = core.handlers.iter().map(|h| h.levelno).min();
    Ok(V::None)
}

/// `logger.<method>(message, *args, **kwargs)` (`method`: trace ... critical) or `logger.log(level, message, *args,
/// **kwargs)`, at a call site of the module `name`
pub async fn log(cx: &Cx, method: &str, mut args: Vec<V>, kwargs: Vec<(String, V)>, loc: &'static Loc, name: &'static str) -> R {
    let level = if method == "log" {
        if args.len() < 2 {
            return Err(Exc::type_error(if args.is_empty() {
                "Logger.log() missing 2 required positional arguments: '_Logger__level' and '_Logger__message'".to_string()
            } else {
                "Logger.log() missing 1 required positional argument: '_Logger__message'".to_string()
            }));
        }
        Some(args.remove(0))
    } else {
        if args.is_empty() {
            return Err(Exc::type_error(format!("Logger.{method}() missing 1 required positional argument: '_Logger__message'")));
        }
        None
    };
    let message = args.remove(0);
    let method = method.to_uppercase();
    let method = method.as_str();
    let min = {
        let core = CORE.read();
        if core.handlers.is_empty() {
            return Ok(V::None);
        }
        core.min_level.unwrap_or(i64::MAX)
    };
    // (index in LEVELS or 7, name, no)
    let (idx, lname, no) = match &level {
        None => {
            let i = LEVELS.iter().position(|l| l.0 == method).unwrap_or(2);
            (i, LEVELS[i].0.to_string(), LEVELS[i].1)
        }
        Some(V::Str(_)) => {
            let n = ops::str_(level.as_ref().unwrap())?;
            match LEVELS.iter().position(|l| l.0 == n) {
                Some(i) => (i, n, LEVELS[i].1),
                None => return Err(Exc::value_error(format!("Level '{n}' does not exist"))),
            }
        }
        Some(V::Int(i)) => (7, format!("Level {}", check_no(*i)?), *i),
        Some(V::Bool(b)) => (7, format!("Level {}", *b as i64), *b as i64),
        Some(v) => return Err(Exc::type_error(format!("Invalid level, it should be an integer or a string, not: '{}'", v.type_name()))),
    };
    if no < min {
        return Ok(V::None);
    }
    let now = Now::local();
    let mut text = ops::str_(&message)?;
    if !args.is_empty() || !kwargs.is_empty() {
        let r = call_method(cx, &message, "format", args, kwargs).await?;
        text = ops::str_(&r)?;
    }
    let file = loc.file.rsplit('/').next().unwrap_or(loc.file);
    let rec = Rec {
        level: &lname,
        no,
        icon: LEVELS.get(idx).map(|l| l.2).unwrap_or(" "),
        name,
        function: loc.func,
        line: loc.line as i64,
        message: &text,
        file,
        module: file.strip_suffix(".py").unwrap_or(file),
        path: super::logging::source_path(loc.file),
        now,
    };
    let core = CORE.read();
    let mut out: Vec<(u8, String)> = vec![];
    for h in &core.handlers {
        if h.levelno > no {
            continue;
        }
        let segs = if h.colorize { h.format.colored[idx] } else { h.format.plain };
        out.push((h.fd, render(segs, &rec)?));
    }
    drop(core);
    for (fd, s) in out {
        if fd == 1 {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(s.as_bytes());
            let _ = o.flush();
        } else {
            let mut o = std::io::stderr().lock();
            let _ = o.write_all(s.as_bytes());
            let _ = o.flush();
        }
    }
    Ok(V::None)
}

struct Rec<'a> {
    level: &'a str,
    no: i64,
    icon: &'static str,
    name: &'static str,
    function: &'static str,
    line: i64,
    message: &'a str,
    file: &'static str,
    module: &'static str,
    path: String,
    now: Now,
}

fn render(segs: &[Seg], r: &Rec) -> R<String> {
    let mut s = String::new();
    for seg in segs {
        match seg {
            Seg::Lit(t) => s.push_str(t),
            Seg::Time(parts, utc) => {
                let t = if *utc { r.now.utc() } else { r.now.clone() };
                time_text(&mut s, parts, &t);
            }
            Seg::Field(f, repr, spec) => {
                let v = match f {
                    F::Level | F::LevelName => V::str(r.level),
                    F::LevelNo => V::Int(r.no),
                    F::LevelIcon => V::str(r.icon),
                    F::Name => V::str(r.name),
                    F::Function => V::str(r.function),
                    F::Line => V::Int(r.line),
                    F::Message => V::str(r.message),
                    F::Module => V::str(r.module),
                    F::File => V::str(r.file),
                    F::FilePath => V::str(&r.path),
                    F::Exception => V::str(""),
                };
                let v = if *repr { V::str(ops::repr(&v)?) } else { v };
                if spec.is_empty() {
                    s.push_str(&ops::str_(&v)?);
                } else {
                    s.push_str(&ops::format_spec(&v, spec)?);
                }
            }
        }
    }
    Ok(s)
}

// ---------------------------------------------------------------- time

/// loguru's `aware_now()`: the local time to the microsecond, its UTC offset and zone name
#[derive(Clone)]
struct Now {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    micro: i64,
    /// Monday = 0
    wday: i64,
    yday: i64,
    offset: i64,
    zone: String,
    ts: f64,
}

impl Now {
    fn local() -> Now {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let secs = d.as_secs() as libc::time_t;
        let micro = d.subsec_micros() as i64;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
        if !ok {
            unsafe { libc::gmtime_r(&secs, &mut tm) };
        }
        let zone = if !ok || tm.tm_zone.is_null() {
            "UTC".to_string()
        } else {
            unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned()
        };
        Now {
            year: tm.tm_year as i64 + 1900,
            month: tm.tm_mon as i64 + 1,
            day: tm.tm_mday as i64,
            hour: tm.tm_hour as i64,
            minute: tm.tm_min as i64,
            second: (tm.tm_sec as i64).min(59),
            micro,
            wday: (tm.tm_wday as i64 + 6) % 7,
            yday: tm.tm_yday as i64 + 1,
            offset: if ok { tm.tm_gmtoff as i64 } else { 0 },
            zone,
            ts: secs as f64 + micro as f64 / 1e6,
        }
    }

    /// `dt.astimezone(timezone.utc)`
    fn utc(&self) -> Now {
        let secs = self.ts.floor() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::gmtime_r(&secs, &mut tm) };
        Now {
            year: tm.tm_year as i64 + 1900,
            month: tm.tm_mon as i64 + 1,
            day: tm.tm_mday as i64,
            hour: tm.tm_hour as i64,
            minute: tm.tm_min as i64,
            second: tm.tm_sec as i64,
            micro: self.micro,
            wday: (tm.tm_wday as i64 + 6) % 7,
            yday: tm.tm_yday as i64 + 1,
            offset: 0,
            zone: "UTC".into(),
            ts: self.ts,
        }
    }
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// `_format_timezone(tzinfo, sep=...)`
fn tz_text(offset: i64, sep: &str) -> String {
    let sign = if offset >= 0 { '+' } else { '-' };
    let a = offset.abs();
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    let mut z = format!("{sign}{h:02}{sep}{m:02}");
    if s > 0 {
        z.push_str(&format!("{sep}{s:02}"));
    }
    z
}

fn time_text(s: &mut String, parts: &[T], t: &Now) {
    use std::fmt::Write as _;
    for p in parts {
        let _ = match p {
            T::Lit(x) => write!(s, "{x}"),
            T::Year => write!(s, "{:04}", t.year),
            T::Year2 => write!(s, "{:02}", t.year % 100),
            T::Quarter => write!(s, "{}", (t.month - 1) / 3 + 1),
            T::MonthName => write!(s, "{}", MONTHS[(t.month - 1) as usize]),
            T::MonthAbbr => write!(s, "{}", &MONTHS[(t.month - 1) as usize][..3]),
            T::Month2 => write!(s, "{:02}", t.month),
            T::Month => write!(s, "{}", t.month),
            T::YDay3 => write!(s, "{:03}", t.yday),
            T::YDay => write!(s, "{}", t.yday),
            T::Day2 => write!(s, "{:02}", t.day),
            T::Day => write!(s, "{}", t.day),
            T::WeekdayName => write!(s, "{}", DAYS[t.wday as usize]),
            T::WeekdayAbbr => write!(s, "{}", &DAYS[t.wday as usize][..3]),
            T::Weekday => write!(s, "{}", t.wday),
            T::WeekdayIso => write!(s, "{}", t.wday + 1),
            T::Hour2 => write!(s, "{:02}", t.hour),
            T::Hour => write!(s, "{}", t.hour),
            T::Hour12x2 => write!(s, "{:02}", (t.hour + 11) % 12 + 1),
            T::Hour12 => write!(s, "{}", (t.hour + 11) % 12 + 1),
            T::Min2 => write!(s, "{:02}", t.minute),
            T::Min => write!(s, "{}", t.minute),
            T::Sec2 => write!(s, "{:02}", t.second),
            T::Sec => write!(s, "{}", t.second),
            T::Frac(n) => {
                let v = t.micro / 10i64.pow(6 - *n as u32);
                write!(s, "{:0w$}", v, w = *n as usize)
            }
            T::AmPm => write!(s, "{}", if t.hour < 12 { "AM" } else { "PM" }),
            T::Tz(colon) => write!(s, "{}", tz_text(t.offset, if *colon { ":" } else { "" })),
            T::TzName => write!(s, "{}", t.zone),
            T::Timestamp => write!(s, "{}", t.ts.trunc() as i64),
            T::TimestampMicro => write!(s, "{}", (t.ts * 1_000_000.0 + t.micro as f64) as i64),
        };
    }
}
