//! `threading` and the event-loop parts of `asyncio` a project uses to run work beside its requests,
//! emulated on tokio:
//! - "threads": the event loop is one thread (every coroutine shares its `get_ident()`, like CPython's
//!   loop thread); `threading.Thread` and `run_in_executor` run on their own OS threads with their own ident;
//! - `threading.Lock` / `RLock` / `Event` block the calling thread like CPython's (a coroutine of the loop
//!   blocks its worker; tokio moves the other tasks away);
//! - `asyncio.new_event_loop()` + `run_forever()` in a thread is a loop object that is "running";
//!   `run_coroutine_threadsafe(coro, loop)` runs the coroutine as a task of the process (the loop object only
//!   tracks its state), `wrap_future` awaits it.
//! - `with` statements: their protocol for runtime values and project classes (`__enter__`/`__exit__`).
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::v::*;
use super::Cx;

const MAIN_IDENT: u64 = 140_000_000_000_001;
static NEXT_IDENT: AtomicU64 = AtomicU64::new(MAIN_IDENT + 1);

thread_local! {
    /// the ident of an emulated thread (0: the event loop's)
    static IDENT: Cell<u64> = const { Cell::new(0) };
}

/// `threading.get_ident()`
pub fn get_ident() -> u64 {
    match IDENT.with(|c| c.get()) {
        0 => MAIN_IDENT,
        i => i,
    }
}

/// runs `f`, which may block the OS thread: on a loop worker, tokio is told first
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    if IDENT.with(|c| c.get()) == 0 && tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

// ---------------------------------------------------------------- locks, events

pub struct TLock {
    pub reentrant: bool,
    /// (owner ident, depth)
    st: Mutex<(u64, u32)>,
    cv: Condvar,
}

fn timeout_of(blocking_: bool, timeout: f64) -> R<Option<Duration>> {
    if timeout < 0.0 {
        if timeout != -1.0 {
            return Err(Exc::value_error("timeout value must be a non-negative number"));
        }
        return Ok(None);
    }
    if !blocking_ {
        return Err(Exc::value_error("can't specify a timeout for a non-blocking call"));
    }
    Ok(Some(Duration::from_secs_f64(timeout)))
}

impl TLock {
    pub fn acquire(&self, block: bool, timeout: f64) -> R<bool> {
        let me = get_ident();
        let limit = timeout_of(block, timeout)?;
        let mut g = self.st.lock();
        if self.reentrant && g.1 > 0 && g.0 == me {
            g.1 += 1;
            return Ok(true);
        }
        if g.1 > 0 && !block {
            return Ok(false);
        }
        if g.1 > 0 {
            let deadline = limit.map(|d| Instant::now() + d);
            let got = blocking(|| {
                while g.1 > 0 {
                    match deadline {
                        Some(d) => {
                            if self.cv.wait_until(&mut g, d).timed_out() && g.1 > 0 {
                                return false;
                            }
                        }
                        None => self.cv.wait(&mut g),
                    }
                }
                true
            });
            if !got {
                return Ok(false);
            }
        }
        *g = (me, 1);
        Ok(true)
    }

    pub fn release(&self) -> R<()> {
        let mut g = self.st.lock();
        if self.reentrant {
            if g.1 == 0 || g.0 != get_ident() {
                return Err(Exc::runtime("cannot release un-acquired lock"));
            }
            g.1 -= 1;
        } else {
            if g.1 == 0 {
                return Err(Exc::runtime("release unlocked lock"));
            }
            g.1 = 0;
        }
        if g.1 == 0 {
            g.0 = 0;
            self.cv.notify_one();
        }
        Ok(())
    }

    pub fn locked(&self) -> bool {
        self.st.lock().1 > 0
    }
}

pub fn lock(reentrant: bool, args: &[V]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("Lock() takes no arguments"));
    }
    Ok(V::native(Native::TLock(Arc::new(TLock { reentrant, st: Mutex::new((0, 0)), cv: Condvar::new() }))))
}

fn flag(args: &[V], kwargs: &[(String, V)], pos: usize, name: &str, dflt: V) -> V {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).or_else(|| args.get(pos).cloned()).unwrap_or(dflt)
}

fn float_of(v: &V) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Float(f) => Ok(*f),
        V::Bool(b) => Ok(*b as i64 as f64),
        other => Err(Exc::type_error(format!("'{}' object cannot be interpreted as a number", other.type_name()))),
    }
}

pub fn lock_method(l: &TLock, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "acquire" | "__enter__" => {
            let block = super::ops::truthy(&flag(args, kwargs, 0, "blocking", V::Bool(true)))?;
            let timeout = match flag(args, kwargs, 1, "timeout", V::Int(-1)) {
                V::None => -1.0,
                v => float_of(&v)?,
            };
            let got = l.acquire(block, timeout)?;
            Ok(if name == "acquire" { V::Bool(got) } else { V::Bool(true) })
        }
        "release" => l.release().map(|_| V::None),
        "__exit__" => l.release().map(|_| V::Bool(false)),
        "locked" => Ok(V::Bool(l.locked())),
        _ => Err(Exc::attr_error(format!("'_thread.{}' object has no attribute '{name}'", if l.reentrant { "RLock" } else { "lock" }))),
    }
}

pub struct TEvent {
    set: Mutex<bool>,
    cv: Condvar,
}

pub fn event(args: &[V]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("Event() takes no arguments"));
    }
    Ok(V::native(Native::TEvent(Arc::new(TEvent { set: Mutex::new(false), cv: Condvar::new() }))))
}

pub fn event_method(e: &TEvent, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "set" => {
            *e.set.lock() = true;
            e.cv.notify_all();
            Ok(V::None)
        }
        "clear" => {
            *e.set.lock() = false;
            Ok(V::None)
        }
        "is_set" => Ok(V::Bool(*e.set.lock())),
        "wait" => {
            let timeout = match flag(args, kwargs, 0, "timeout", V::None) {
                V::None => None,
                v => Some(Duration::from_secs_f64(float_of(&v)?.max(0.0))),
            };
            let mut g = e.set.lock();
            let deadline = timeout.map(|d| Instant::now() + d);
            let ok = blocking(|| {
                while !*g {
                    match deadline {
                        Some(d) => {
                            if e.cv.wait_until(&mut g, d).timed_out() {
                                return *g;
                            }
                        }
                        None => e.cv.wait(&mut g),
                    }
                }
                true
            });
            Ok(V::Bool(ok))
        }
        _ => Err(Exc::attr_error(format!("'Event' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- threads

pub struct TThread {
    target: V,
    args: Vec<V>,
    kwargs: Vec<(String, V)>,
    pub name: String,
    pub daemon: bool,
    started: AtomicBool,
    alive: Arc<AtomicBool>,
    ident: AtomicU64,
}

/// `threading.Thread(target=, args=, kwargs=, name=, daemon=)`
pub fn thread_new(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: Thread() takes keyword arguments only"));
    }
    let mut t = TThread {
        target: V::None,
        args: vec![],
        kwargs: vec![],
        name: format!("Thread-{}", NEXT_IDENT.load(Ordering::Relaxed) - MAIN_IDENT),
        daemon: false,
        started: AtomicBool::new(false),
        alive: Arc::new(AtomicBool::new(false)),
        ident: AtomicU64::new(0),
    };
    for (k, v) in kwargs {
        match k.as_str() {
            "target" => t.target = v.clone(),
            "args" => t.args = super::ops::iter(v)?,
            "kwargs" => {
                if let V::Dict(d) = v {
                    t.kwargs = d.lock().values().map(|(k, x)| Ok((super::ops::str_(k)?, x.clone()))).collect::<R<Vec<_>>>()?;
                }
            }
            "name" => t.name = super::ops::str_(v)?,
            "daemon" => t.daemon = super::ops::truthy(v)?,
            other => return Err(Exc::type_error(format!("Thread.__init__() got an unexpected keyword argument '{other}'"))),
        }
    }
    Ok(V::native(Native::TThread(Arc::new(t))))
}

/// runs a project callable on its own OS thread (own ident), inside the tokio runtime
fn spawn_os(cx: &Cx, f: V, args: Vec<V>, kwargs: Vec<(String, V)>, alive: Arc<AtomicBool>) -> R<u64> {
    let ident = NEXT_IDENT.fetch_add(1, Ordering::Relaxed);
    let handle = tokio::runtime::Handle::current();
    let cx2 = cx.clone();
    alive.store(true, Ordering::SeqCst);
    std::thread::Builder::new()
        .spawn(move || {
            IDENT.with(|c| c.set(ident));
            let r = handle.block_on(super::methods::call_value(&cx2, &f, args, kwargs));
            if let Err(e) = r {
                eprintln!("Exception in thread: {:?}", e);
            }
            alive.store(false, Ordering::SeqCst);
        })
        .map_err(|e| Exc::runtime(format!("can't start new thread: {e}")))?;
    Ok(ident)
}

pub fn thread_method(cx: &Cx, t: &Arc<TThread>, name: &str, args: &[V]) -> R {
    match name {
        "start" => {
            if t.started.swap(true, Ordering::SeqCst) {
                return Err(Exc::runtime("threads can only be started once"));
            }
            let ident = spawn_os(cx, t.target.clone(), t.args.clone(), t.kwargs.clone(), t.alive.clone())?;
            t.ident.store(ident, Ordering::SeqCst);
            Ok(V::None)
        }
        "is_alive" => Ok(V::Bool(t.alive.load(Ordering::SeqCst))),
        "join" => {
            let timeout = match args.first() {
                None | Some(V::None) => None,
                Some(v) => Some(Instant::now() + Duration::from_secs_f64(float_of(v)?.max(0.0))),
            };
            blocking(|| {
                while t.alive.load(Ordering::SeqCst) {
                    if matches!(timeout, Some(d) if Instant::now() >= d) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'Thread' object has no attribute '{name}'"))),
    }
}

pub fn thread_attr(t: &TThread, name: &str) -> R {
    Ok(match name {
        "name" => V::str(&t.name),
        "daemon" => V::Bool(t.daemon),
        "ident" => match t.ident.load(Ordering::SeqCst) {
            0 => V::None,
            i => V::Int(i as i64),
        },
        _ => return Err(Exc::attr_error(format!("'Thread' object has no attribute '{name}'"))),
    })
}

// ---------------------------------------------------------------- event loops

pub struct ELoop {
    pub id: u64,
    running: AtomicBool,
    closed: AtomicBool,
    pending: Mutex<Vec<(V, Vec<V>)>>,
}

fn main_loop() -> V {
    static MAIN: std::sync::OnceLock<V> = std::sync::OnceLock::new();
    MAIN.get_or_init(|| {
        V::native(Native::ELoop(Arc::new(ELoop { id: 0, running: AtomicBool::new(true), closed: AtomicBool::new(false), pending: Mutex::new(vec![]) })))
    })
    .clone()
}

thread_local! {
    static THREAD_LOOP: std::cell::RefCell<Option<V>> = const { std::cell::RefCell::new(None) };
}

/// `asyncio.get_running_loop()` / `get_event_loop()`: the loop of the calling thread
pub fn running_loop() -> R {
    if IDENT.with(|c| c.get()) == 0 {
        return Ok(main_loop());
    }
    THREAD_LOOP
        .with(|l| l.borrow().clone())
        .ok_or_else(|| Exc::runtime("no running event loop"))
}

/// `asyncio.new_event_loop()`
pub fn new_loop() -> R {
    let id = NEXT_IDENT.fetch_add(1, Ordering::Relaxed);
    Ok(V::native(Native::ELoop(Arc::new(ELoop { id, running: AtomicBool::new(false), closed: AtomicBool::new(false), pending: Mutex::new(vec![]) }))))
}

pub async fn loop_method(cx: &Cx, recv: &V, l: &Arc<ELoop>, name: &str, args: Vec<V>) -> R {
    match name {
        "is_running" => Ok(V::Bool(l.running.load(Ordering::SeqCst))),
        "is_closed" => Ok(V::Bool(l.closed.load(Ordering::SeqCst))),
        "call_soon" | "call_soon_threadsafe" => {
            let Some((cb, rest)) = args.split_first() else {
                return Err(Exc::type_error(format!("{name}() missing 1 required positional argument: 'callback'")));
            };
            if l.running.load(Ordering::SeqCst) {
                super::web::spawn_task(cx, cb.clone(), rest.to_vec(), vec![])?;
            } else {
                l.pending.lock().push((cb.clone(), rest.to_vec()));
            }
            Ok(V::None)
        }
        "run_forever" => {
            if l.id == 0 {
                return Err(Exc::runtime("This event loop is already running"));
            }
            THREAD_LOOP.with(|t| *t.borrow_mut() = Some(recv.clone()));
            l.running.store(true, Ordering::SeqCst);
            let pending = std::mem::take(&mut *l.pending.lock());
            for (cb, a) in pending {
                if let Err(e) = Box::pin(super::methods::call_value(cx, &cb, a, vec![])).await {
                    eprintln!("ERROR:asyncio:Exception in callback: {:?}", e);
                }
            }
            // the loop "runs" the coroutines submitted to it (as tasks of the process) until stopped
            while l.running.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(V::None)
        }
        "stop" => {
            l.running.store(false, Ordering::SeqCst);
            Ok(V::None)
        }
        "close" => {
            l.closed.store(true, Ordering::SeqCst);
            Ok(V::None)
        }
        "create_task" => match args.first() {
            Some(c) => super::aio::spawn_coro(cx, c.clone()),
            None => Err(Exc::type_error("create_task() missing 1 required positional argument: 'coro'")),
        },
        "run_in_executor" => {
            let mut it = args.into_iter();
            let executor = it.next().unwrap_or(V::None);
            if !executor.is_none() {
                return Err(Exc::type_error("py2axum: run_in_executor() with an executor is not supported (pass None)"));
            }
            let f = it.next().ok_or_else(|| Exc::type_error("run_in_executor() missing 1 required positional argument: 'func'"))?;
            let rest: Vec<V> = it.collect();
            Ok(executor_call(cx, f, rest))
        }
        _ => Err(Exc::attr_error(format!("'_UnixSelectorEventLoop' object has no attribute '{name}'"))),
    }
}

/// `loop.run_in_executor(None, f, *args)`: an awaitable running `f` on a worker thread (its own ident)
fn executor_call(cx: &Cx, f: V, args: Vec<V>) -> V {
    let cx2 = cx.clone();
    super::aio::coro(Box::pin(async move {
        let handle = tokio::runtime::Handle::current();
        let ident = NEXT_IDENT.fetch_add(1, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            // blocking-pool threads later serve as loop workers: the ident is the executor call's only
            IDENT.with(|c| c.set(ident));
            let r = handle.block_on(super::methods::call_value(&cx2, &f, args, vec![]));
            IDENT.with(|c| c.set(0));
            r
        })
        .await
        .map_err(|e| Exc::runtime(format!("executor task failed: {e}")))?
    }))
}

/// `asyncio.run_coroutine_threadsafe(coro, loop)`: the coroutine runs as a task; the returned future is
/// awaited through `asyncio.wrap_future`
pub fn run_coroutine_threadsafe(cx: &Cx, args: &[V]) -> R {
    let [c, l] = args else { return Err(Exc::type_error("run_coroutine_threadsafe() takes 2 positional arguments")) };
    if !matches!(c, V::Native(n) if matches!(&**n, Native::Coro(_))) {
        return Err(Exc::type_error("A coroutine object is required"));
    }
    if !matches!(l, V::Native(n) if matches!(&**n, Native::ELoop(_))) {
        return Err(Exc::type_error("py2axum: run_coroutine_threadsafe() needs an event loop"));
    }
    super::aio::spawn_coro(cx, c.clone())
}

// ---------------------------------------------------------------- `with`

/// `with v`: the value entered (project `__enter__`, locks, files)
pub async fn enter(cx: &Cx, v: &V) -> R {
    if let Some(f) = super::ops::dunder(v, "__enter__") {
        return f(cx, v.clone(), vec![]).await;
    }
    if let V::Native(n) = v {
        if let Native::TLock(l) = &**n {
            lock_method(l, "__enter__", &[], &[])?;
            return Ok(V::Bool(true));
        }
        if let Native::Prom(p) = &**n {
            return super::prom::enter(v, p);
        }
        if let Native::Sentry(o) = &**n {
            return super::sentry::enter(o);
        }
        if let Native::Suppress(_) = &**n {
            return Ok(V::None);
        }
    }
    super::pathio::ctx_enter(v)
}

/// leaving `with v`: `exc` when the body raised; a true result suppresses it
pub async fn exit(cx: &Cx, v: &V, exc: Option<Exc>) -> R {
    if let Some(f) = super::ops::dunder(v, "__exit__") {
        let args = match exc {
            Some(e) => vec![V::Class(e.0.class), V::Exc(e), V::None],
            None => vec![V::None, V::None, V::None],
        };
        return f(cx, v.clone(), args).await;
    }
    if let V::Native(n) = v {
        if let Native::TLock(l) = &**n {
            return lock_method(l, "__exit__", &[], &[]);
        }
        if let Native::Prom(p) = &**n {
            return super::prom::exit(p, exc.as_ref());
        }
        if let Native::Sentry(o) = &**n {
            return super::sentry::exit(o);
        }
        if let Native::Suppress(classes) = &**n {
            // contextlib.suppress: true when the exception is an instance of one of its classes
            let hit = exc.is_some_and(|e| classes.iter().any(|c| matches!(c, V::Class(k) if e.isinstance(k))));
            return Ok(V::Bool(hit));
        }
    }
    super::pathio::ctx_exit(v)?;
    Ok(V::Bool(false))
}
