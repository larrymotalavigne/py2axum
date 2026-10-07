//! tenacity 9.x: `@retry(stop=, wait=, retry=, before=, after=, before_sleep=, reraise=,
//! retry_error_callback=)` on async and sync functions, the usual stop/wait/retry strategies and their
//! `|` / `&` / `+` combinations, `before_sleep_log`, `RetryError`. The loop follows `BaseRetrying.iter`:
//! retry predicate, `after`, wait computed, then stop checked, then `before_sleep` and the sleep.
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use super::methods::{call_value, getattr};
use super::ops;
use super::v::*;
use super::{Cx, KwFn};

#[derive(Clone)]
pub enum Stop {
    Never,
    Attempt(i64),
    Delay(f64),
    Any(Vec<Stop>),
    All(Vec<Stop>),
}

#[derive(Clone)]
pub enum Wait {
    Fixed(f64),
    Random(f64, f64),
    Exp { mult: f64, min: f64, max: f64, base: f64 },
    ExpJitter { initial: f64, max: f64, base: f64, jitter: f64 },
    Incrementing { start: f64, inc: f64, max: f64 },
    Combine(Vec<Wait>),
    Chain(Vec<Wait>),
}

#[derive(Clone)]
pub enum Retry {
    ExcType(V),
    NotExcType(V),
    Exc(V),
    Result(V),
    Always,
    Never,
    Any(Vec<Retry>),
    All(Vec<Retry>),
}

pub struct Cfg {
    stop: Stop,
    wait: Wait,
    retry: Retry,
    before: Option<V>,
    after: Option<V>,
    before_sleep: Option<V>,
    reraise: bool,
    error_callback: Option<V>,
}

/// `RetryCallState`, passed to the callbacks
pub struct State {
    pub fn_: V,
    pub args: Vec<V>,
    pub kwargs: Vec<(String, V)>,
    start: Instant,
    attempt: Mutex<i64>,
    outcome: Mutex<Option<Result<V, Exc>>>,
    outcome_at: Mutex<f64>,
    idle_for: Mutex<f64>,
    next_sleep: Mutex<Option<f64>>,
    upcoming: Mutex<f64>,
}

pub enum Ten {
    Stop(Stop),
    Wait(Wait),
    Retry(Retry),
    SleepLog { logger: V, level: i64, sec_format: String },
    Decorator(Arc<Cfg>),
    State(Arc<State>),
    Outcome(Result<V, Exc>),
    Action(f64),
}

fn ten(t: Ten) -> V {
    V::native(Native::Tenacity(Arc::new(t)))
}

fn secs(v: &V) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Float(f) => Ok(*f),
        V::Delta(d) => Ok(d.num_microseconds().map(|u| u as f64 / 1e6).unwrap_or(f64::INFINITY)),
        V::Bool(b) => Ok(*b as i64 as f64),
        o => Err(Exc::type_error(format!("py2axum: tenacity expects a number of seconds, not {}", o.type_name()))),
    }
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn arg<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kw(kwargs, name))
}

fn check_kwargs(fname: &str, kwargs: &[(String, V)], allowed: &[&str]) -> R<()> {
    match kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(Exc::type_error(format!("{fname}.__init__() got an unexpected keyword argument '{k}'"))),
        None => Ok(()),
    }
}

/// the strategy constructors (`tenacity.stop_after_attempt(3)`, ...)
pub fn make(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let f = |i: usize, n: &str, d: f64| -> R<f64> { arg(args, kwargs, i, n).map(secs).transpose().map(|o| o.unwrap_or(d)) };
    Ok(match name {
        "stop_after_attempt" => {
            check_kwargs(name, kwargs, &["max_attempt_number"])?;
            let n = arg(args, kwargs, 0, "max_attempt_number").ok_or_else(|| Exc::type_error("stop_after_attempt.__init__() missing 1 required positional argument: 'max_attempt_number'"))?;
            match n {
                V::Int(i) => ten(Ten::Stop(Stop::Attempt(*i))),
                o => return Err(Exc::type_error(format!("py2axum: stop_after_attempt expects an int, not {}", o.type_name()))),
            }
        }
        "stop_after_delay" => {
            check_kwargs(name, kwargs, &["max_delay"])?;
            ten(Ten::Stop(Stop::Delay(f(0, "max_delay", f64::NAN)?)))
        }
        "stop_any" | "stop_all" => {
            let mut v = Vec::new();
            for a in args {
                v.push(stop_of(a)?);
            }
            ten(Ten::Stop(if name == "stop_any" { Stop::Any(v) } else { Stop::All(v) }))
        }
        "wait_fixed" => {
            check_kwargs(name, kwargs, &["wait"])?;
            ten(Ten::Wait(Wait::Fixed(f(0, "wait", f64::NAN)?)))
        }
        "wait_none" => ten(Ten::Wait(Wait::Fixed(0.0))),
        "wait_random" => {
            check_kwargs(name, kwargs, &["min", "max"])?;
            ten(Ten::Wait(Wait::Random(f(0, "min", 0.0)?, f(1, "max", 1.0)?)))
        }
        "wait_exponential" => {
            check_kwargs(name, kwargs, &["multiplier", "max", "exp_base", "min"])?;
            ten(Ten::Wait(Wait::Exp { mult: f(0, "multiplier", 1.0)?, max: f(1, "max", f64::INFINITY)?, base: f(2, "exp_base", 2.0)?, min: f(3, "min", 0.0)? }))
        }
        "wait_exponential_jitter" => {
            check_kwargs(name, kwargs, &["initial", "max", "exp_base", "jitter"])?;
            ten(Ten::Wait(Wait::ExpJitter { initial: f(0, "initial", 1.0)?, max: f(1, "max", f64::INFINITY)?, base: f(2, "exp_base", 2.0)?, jitter: f(3, "jitter", 1.0)? }))
        }
        "wait_incrementing" => {
            check_kwargs(name, kwargs, &["start", "increment", "max"])?;
            ten(Ten::Wait(Wait::Incrementing { start: f(0, "start", 0.0)?, inc: f(1, "increment", 100.0)?, max: f(2, "max", f64::INFINITY)? }))
        }
        "wait_combine" | "wait_chain" => {
            let mut v = Vec::new();
            for a in args {
                v.push(wait_of(a)?);
            }
            ten(Ten::Wait(if name == "wait_combine" { Wait::Combine(v) } else { Wait::Chain(v) }))
        }
        "retry_if_exception_type" | "retry_if_not_exception_type" => {
            check_kwargs(name, kwargs, &["exception_types"])?;
            let t = arg(args, kwargs, 0, "exception_types").cloned().unwrap_or(V::Class(&EXCEPTION));
            ten(Ten::Retry(if name == "retry_if_exception_type" { Retry::ExcType(t) } else { Retry::NotExcType(t) }))
        }
        "retry_if_exception" | "retry_if_result" => {
            check_kwargs(name, kwargs, &["predicate"])?;
            let p = arg(args, kwargs, 0, "predicate").cloned().ok_or_else(|| Exc::type_error(format!("{name}.__init__() missing 1 required positional argument: 'predicate'")))?;
            ten(Ten::Retry(if name == "retry_if_exception" { Retry::Exc(p) } else { Retry::Result(p) }))
        }
        "retry_always" => ten(Ten::Retry(Retry::Always)),
        "retry_never" => ten(Ten::Retry(Retry::Never)),
        "retry_any" | "retry_all" => {
            let mut v = Vec::new();
            for a in args {
                v.push(retry_of(a)?);
            }
            ten(Ten::Retry(if name == "retry_any" { Retry::Any(v) } else { Retry::All(v) }))
        }
        "before_sleep_log" => {
            check_kwargs(name, kwargs, &["logger", "log_level", "exc_info", "sec_format"])?;
            let logger = arg(args, kwargs, 0, "logger").cloned().ok_or_else(|| Exc::type_error("before_sleep_log() missing 2 required positional arguments: 'logger' and 'log_level'"))?;
            let level = match arg(args, kwargs, 1, "log_level") {
                Some(V::Int(l)) => *l,
                _ => return Err(Exc::type_error("before_sleep_log() missing 1 required positional argument: 'log_level'")),
            };
            if arg(args, kwargs, 2, "exc_info").map(ops::truthy).transpose()?.unwrap_or(false) {
                return Err(Exc::type_error("py2axum: before_sleep_log(exc_info=True) is not supported"));
            }
            let sec_format = match arg(args, kwargs, 3, "sec_format") {
                Some(v) => ops::str_(v)?,
                None => "%.3g".into(),
            };
            ten(Ten::SleepLog { logger, level, sec_format })
        }
        _ => return Err(Exc::type_error(format!("py2axum: tenacity.{name} is not supported"))),
    })
}

/// `stop_never`, `retry_always`, `retry_never`: instances in tenacity
pub fn value(name: &str) -> V {
    ten(match name {
        "stop_never" => Ten::Stop(Stop::Never),
        "retry_always" => Ten::Retry(Retry::Always),
        _ => Ten::Retry(Retry::Never),
    })
}

fn native(v: &V) -> Option<&Ten> {
    match v {
        V::Native(n) => match &**n {
            Native::Tenacity(t) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

fn stop_of(v: &V) -> R<Stop> {
    match native(v) {
        Some(Ten::Stop(s)) => Ok(s.clone()),
        _ => Err(Exc::type_error(format!("py2axum: tenacity stop= expects a stop strategy, not {}", v.type_name()))),
    }
}

fn wait_of(v: &V) -> R<Wait> {
    match (native(v), v) {
        (Some(Ten::Wait(w)), _) => Ok(w.clone()),
        // a number is a fixed wait (tenacity accepts plain numbers)
        (None, V::Int(_) | V::Float(_)) => Ok(Wait::Fixed(secs(v)?)),
        _ => Err(Exc::type_error(format!("py2axum: tenacity wait= expects a wait strategy, not {}", v.type_name()))),
    }
}

fn retry_of(v: &V) -> R<Retry> {
    match native(v) {
        Some(Ten::Retry(r)) => Ok(r.clone()),
        _ => Err(Exc::type_error(format!("py2axum: tenacity retry= expects a retry strategy, not {}", v.type_name()))),
    }
}

/// `stop_a | stop_b`, `wait_a + wait_b`, `retry_a & retry_b`... (None: not tenacity objects)
pub fn binop(a: &V, op: &str, b: &V) -> Option<R> {
    let (x, y) = (native(a), native(b));
    if x.is_none() && y.is_none() {
        return None;
    }
    Some(match (x, op, y) {
        (Some(Ten::Stop(p)), "|", Some(Ten::Stop(q))) => Ok(ten(Ten::Stop(Stop::Any(vec![p.clone(), q.clone()])))),
        (Some(Ten::Stop(p)), "&", Some(Ten::Stop(q))) => Ok(ten(Ten::Stop(Stop::All(vec![p.clone(), q.clone()])))),
        (Some(Ten::Retry(p)), "|", Some(Ten::Retry(q))) => Ok(ten(Ten::Retry(Retry::Any(vec![p.clone(), q.clone()])))),
        (Some(Ten::Retry(p)), "&", Some(Ten::Retry(q))) => Ok(ten(Ten::Retry(Retry::All(vec![p.clone(), q.clone()])))),
        (Some(Ten::Wait(p)), "+", Some(Ten::Wait(q))) => Ok(ten(Ten::Wait(Wait::Combine(vec![p.clone(), q.clone()])))),
        // sum(waits): 0 + wait is the wait
        (None, "+", Some(Ten::Wait(_))) if matches!(a, V::Int(0)) => Ok(b.clone()),
        _ => Err(Exc::type_error(format!("unsupported operand type(s) for {op}: '{}' and '{}'", a.type_name(), b.type_name()))),
    })
}

pub fn type_name(t: &Ten) -> &'static str {
    match t {
        Ten::Stop(_) => "stop_base",
        Ten::Wait(_) => "wait_base",
        Ten::Retry(_) => "retry_base",
        Ten::SleepLog { .. } => "function",
        Ten::Decorator(_) => "function",
        Ten::State(_) => "RetryCallState",
        Ten::Outcome(_) => "Future",
        Ten::Action(_) => "RetryAction",
    }
}

impl Stop {
    fn check(&self, st: &State) -> bool {
        match self {
            Stop::Never => false,
            Stop::Attempt(n) => *st.attempt.lock() >= *n,
            Stop::Delay(d) => *st.outcome_at.lock() >= *d,
            Stop::Any(v) => v.iter().any(|s| s.check(st)),
            Stop::All(v) => v.iter().all(|s| s.check(st)),
        }
    }
}

impl Wait {
    fn compute(&self, st: &State) -> f64 {
        use rand::Rng;
        let n = *st.attempt.lock();
        match self {
            Wait::Fixed(w) => *w,
            Wait::Random(lo, hi) => lo + rand::thread_rng().gen_range(0.0..1.0) * (hi - lo),
            Wait::Exp { mult, min, max, base } => {
                let r = mult * base.powf((n - 1) as f64);
                if r.is_infinite() {
                    return *max;
                }
                min.max(0.0).max(r.min(*max))
            }
            Wait::ExpJitter { initial, max, base, jitter } => {
                let j = rand::thread_rng().gen_range(0.0..=jitter.max(0.0));
                let r = initial * base.powf((n - 1) as f64) + j;
                if r.is_infinite() {
                    return *max;
                }
                0.0f64.max(max.min(r))
            }
            Wait::Incrementing { start, inc, max } => 0.0f64.max((start + inc * (n - 1) as f64).min(*max)),
            Wait::Combine(v) => v.iter().map(|w| w.compute(st)).sum(),
            Wait::Chain(v) => {
                let i = (n.max(1) as usize).min(v.len());
                v.get(i.wrapping_sub(1)).map(|w| w.compute(st)).unwrap_or(0.0)
            }
        }
    }
}

impl Retry {
    fn check<'a>(&'a self, cx: &'a Cx, out: &'a Result<V, Exc>) -> super::BoxFut<'a> {
        Box::pin(async move {
            let b = match (self, out) {
                (Retry::Always, _) => true,
                (Retry::Never, _) => false,
                (Retry::ExcType(t), Err(e)) => super::types::isinstance(&V::Exc(e.clone()), t)?,
                (Retry::NotExcType(t), Err(e)) => !super::types::isinstance(&V::Exc(e.clone()), t)?,
                (Retry::Exc(p), Err(e)) => ops::truthy(&call(cx, p, vec![V::Exc(e.clone())]).await?)?,
                (Retry::Result(p), Ok(v)) => ops::truthy(&call(cx, p, vec![v.clone()]).await?)?,
                (Retry::ExcType(_) | Retry::NotExcType(_) | Retry::Exc(_), Ok(_)) | (Retry::Result(_), Err(_)) => false,
                (Retry::Any(v), _) => {
                    let mut r = false;
                    for x in v {
                        if ops::truthy(&x.check(cx, out).await?)? {
                            r = true;
                            break;
                        }
                    }
                    r
                }
                (Retry::All(v), _) => {
                    let mut r = true;
                    for x in v {
                        if !ops::truthy(&x.check(cx, out).await?)? {
                            r = false;
                            break;
                        }
                    }
                    r
                }
            };
            Ok(V::Bool(b))
        })
    }
}

/// a callback or predicate: called, then awaited when it returned a coroutine (tenacity's asyncio
/// support awaits awaitables)
async fn call(cx: &Cx, f: &V, args: Vec<V>) -> R {
    super::aio::await_value(call_value(cx, f, args, vec![]).await?).await
}

async fn callback(cx: &Cx, f: &V, st: &Arc<State>) -> R {
    if let Some(Ten::SleepLog { logger, level, sec_format }) = native(f) {
        return sleep_log(cx, logger, *level, sec_format, st).await;
    }
    call(cx, f, vec![ten(Ten::State(st.clone()))]).await
}

/// `before_sleep_log`: "Retrying module.qualname in 0.5 seconds as it raised ValueError: boom."
async fn sleep_log(cx: &Cx, logger: &V, level: i64, sec_format: &str, st: &Arc<State>) -> R {
    let (verb, value) = match st.outcome.lock().clone() {
        Some(Err(e)) => ("raised", format!("{}: {}", e.0.class.name, e.message())),
        Some(Ok(v)) => ("returned", ops::str_(&v)?),
        None => return Err(Exc::runtime("log_it() called before outcome was set")),
    };
    let module = getattr(cx, &st.fn_, "__module__").await.ok().filter(|m| !m.is_none());
    let name = match getattr(cx, &st.fn_, "__qualname__").await {
        Ok(q) => {
            let q = ops::str_(&q)?;
            match module {
                Some(m) => format!("{}.{q}", ops::str_(&m)?),
                None => q,
            }
        }
        Err(_) => ops::repr(&st.fn_)?,
    };
    let sleep = st.next_sleep.lock().unwrap_or(0.0);
    let msg = format!("Retrying {name} in {} seconds as it {verb} {value}.", ops::percent_format(sec_format, &V::Float(sleep))?);
    let method = match level {
        10 => "debug",
        20 => "info",
        30 => "warning",
        40 => "error",
        50 => "critical",
        _ => return Err(Exc::type_error(format!("py2axum: before_sleep_log with log level {level} is not supported"))),
    };
    super::methods::call_method(cx, logger, method, vec![V::str(msg)], vec![]).await
}

/// `tenacity.retry(...)`: the decorator; `@retry` without arguments decorates directly.
pub fn retry(args: &[V], kwargs: &[(String, V)]) -> R {
    if let ([f], true) = (args, kwargs.is_empty()) {
        if native(f).is_none() {
            let cfg = Arc::new(parse(&[])?);
            return Ok(wrap(f.clone(), cfg));
        }
    }
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: tenacity.retry() takes keyword arguments only"));
    }
    Ok(ten(Ten::Decorator(Arc::new(parse(kwargs)?))))
}

fn parse(kwargs: &[(String, V)]) -> R<Cfg> {
    let mut cfg = Cfg { stop: Stop::Never, wait: Wait::Fixed(0.0), retry: Retry::ExcType(V::Class(&EXCEPTION)), before: None, after: None, before_sleep: None, reraise: false, error_callback: None };
    let opt = |v: &V| if v.is_none() { None } else { Some(v.clone()) };
    for (k, v) in kwargs {
        match k.as_str() {
            "stop" => cfg.stop = stop_of(v)?,
            "wait" => cfg.wait = wait_of(v)?,
            "retry" => cfg.retry = retry_of(v)?,
            "before" => cfg.before = opt(v),
            "after" => cfg.after = opt(v),
            "before_sleep" => cfg.before_sleep = opt(v),
            "reraise" => cfg.reraise = ops::truthy(v)?,
            "retry_error_callback" => cfg.error_callback = opt(v),
            "sleep" | "retry_error_cls" => return Err(Exc::type_error(format!("py2axum: tenacity.retry({k}=) is not supported"))),
            _ => return Err(Exc::type_error(format!("retry() got an unexpected keyword argument '{k}'"))),
        }
    }
    Ok(cfg)
}

/// calling the decorator object on a function
pub fn decorate(t: &Ten, args: &[V]) -> R {
    match (t, args) {
        (Ten::Decorator(cfg), [f]) => Ok(wrap(f.clone(), cfg.clone())),
        _ => Err(Exc::type_error("py2axum: this tenacity object is not callable")),
    }
}

/// the wrapped function: `functools.wraps(f)` attributes, `__wrapped__`, CPython's async-ness of `f`
fn wrap(f: V, cfg: Arc<Cfg>) -> V {
    let is_async = super::aio::is_async_fn(&f);
    let target = f.clone();
    let call: KwFn = Arc::new(move |cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| {
        let (f, cfg) = (target.clone(), cfg.clone());
        Box::pin(async move { run(cx, f, cfg, args, kwargs).await })
    });
    let attrs: Vec<(String, V)> = match &f {
        V::Native(n) => match &**n {
            Native::PyFn(p) => p.attrs.lock().iter().filter(|(k, _)| k != "__wrapped__").cloned().collect(),
            _ => vec![],
        },
        _ => vec![],
    };
    let mut attrs = attrs;
    attrs.push(("__wrapped__".into(), f));
    V::native(Native::PyFn(PyFn { call, is_async, attrs: Mutex::new(attrs) }))
}

async fn run(cx: &Cx, f: V, cfg: Arc<Cfg>, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let st = Arc::new(State {
        fn_: f.clone(),
        args: args.clone(),
        kwargs: kwargs.clone(),
        start: Instant::now(),
        attempt: Mutex::new(1),
        outcome: Mutex::new(None),
        outcome_at: Mutex::new(0.0),
        idle_for: Mutex::new(0.0),
        next_sleep: Mutex::new(None),
        upcoming: Mutex::new(0.0),
    });
    loop {
        if let Some(b) = &cfg.before {
            callback(cx, b, &st).await?;
        }
        let out = match call_value(cx, &f, args.clone(), kwargs.clone()).await {
            Ok(v) => super::aio::await_value(v).await,
            Err(e) => Err(e),
        };
        *st.outcome_at.lock() = st.start.elapsed().as_secs_f64();
        *st.outcome.lock() = Some(out.clone());
        if !ops::truthy(&cfg.retry.check(cx, &out).await?)? {
            return out;
        }
        if let Some(a) = &cfg.after {
            callback(cx, a, &st).await?;
        }
        let sleep = cfg.wait.compute(&st);
        *st.upcoming.lock() = sleep;
        if cfg.stop.check(&st) {
            if let Some(cb) = &cfg.error_callback {
                return callback(cx, cb, &st).await;
            }
            return match (cfg.reraise, out) {
                (true, Err(e)) => Err(e),
                (_, out) => Err(retry_error(&out)),
            };
        }
        *st.next_sleep.lock() = Some(sleep);
        *st.idle_for.lock() += sleep;
        if let Some(b) = &cfg.before_sleep {
            callback(cx, b, &st).await?;
        }
        if sleep > 0.0 {
            tokio::time::sleep(std::time::Duration::from_secs_f64(sleep)).await;
        }
        *st.attempt.lock() += 1;
        *st.outcome.lock() = None;
        *st.next_sleep.lock() = None;
    }
}

/// `RetryError[<Future at 0x... state=finished raised ValueError>]`: the address is CPython's object
/// id, not reproducible (documented); the binary prints 0x0.
fn retry_error(out: &Result<V, Exc>) -> Exc {
    let what = match out {
        Err(e) => format!("raised {}", e.0.class.name),
        Ok(v) => format!("returned {}", v.type_name()),
    };
    let e = Exc::new(&TENACITY_RETRY_ERROR, vec![V::str(format!("RetryError[<Future at 0x0 state=finished {what}>]"))]);
    e.0.attrs.lock().insert("last_attempt".into(), ten(Ten::Outcome(out.clone())));
    e
}

pub fn attr(t: &Ten, name: &str) -> R {
    let none = || Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(t))));
    match t {
        Ten::State(s) => Ok(match name {
            "attempt_number" => V::Int(*s.attempt.lock()),
            "outcome" => s.outcome.lock().clone().map(|o| ten(Ten::Outcome(o))).unwrap_or(V::None),
            "idle_for" => V::Float(*s.idle_for.lock()),
            "seconds_since_start" => {
                if s.outcome.lock().is_none() {
                    V::None
                } else {
                    V::Float(*s.outcome_at.lock())
                }
            }
            "upcoming_sleep" => V::Float(*s.upcoming.lock()),
            "next_action" => s.next_sleep.lock().map(|x| ten(Ten::Action(x))).unwrap_or(V::None),
            "fn" => s.fn_.clone(),
            "args" => V::tuple(s.args.clone()),
            "kwargs" => V::dict_from(s.kwargs.iter().map(|(k, v)| (V::str(k), v.clone())).collect())?,
            _ => return none(),
        }),
        Ten::Outcome(o) => match name {
            "failed" => Ok(V::Bool(o.is_err())),
            _ => none(),
        },
        Ten::Action(s) => match name {
            "sleep" => Ok(V::Float(*s)),
            _ => none(),
        },
        _ => none(),
    }
}

pub fn method(t: &Ten, name: &str, args: &[V]) -> R {
    match (t, name, args) {
        (Ten::Outcome(o), "result", []) => o.clone(),
        (Ten::Outcome(o), "exception", []) => Ok(match o {
            Err(e) => V::Exc(e.clone()),
            Ok(_) => V::None,
        }),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(t)))),
    }
}
