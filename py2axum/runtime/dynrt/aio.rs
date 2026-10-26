//! Coroutine objects and the asyncio functions that take them: a call of an async function that is not
//! awaited is a `Native::Coro` (its arguments already evaluated, like CPython), run once by `await`, by a
//! task, or by `asyncio.gather`.
use std::sync::Arc;

use parking_lot::Mutex;

use super::methods::{call_method, call_value};
use super::v::*;
use super::{BoxFut, Cx};

/// A coroutine value from a future owning everything it needs.
pub fn coro(f: BoxFut<'static>) -> V {
    V::native(Native::Coro(Mutex::new(Some(f))))
}

/// `await v`: runs a coroutine (once), waits for a task; other values pass through (an awaited call of a
/// project async function already produced its result).
pub async fn await_value(v: V) -> R {
    let fut = match &v {
        V::Native(n) => match &**n {
            Native::Coro(c) => match c.lock().take() {
                Some(f) => f,
                None => return Err(Exc::runtime("cannot reuse already awaited coroutine")),
            },
            Native::Task(t) => return super::web::task_result(t).await,
            Native::AFuture(f) => return future_wait(f).await,
            _ => return Ok(v),
        },
        _ => return Ok(v),
    };
    fut.await
}

/// The call in `await obj.method(...)` on a value known only at run time: a synchronous `Session`'s
/// methods return plain values, so CPython runs the call, then fails on the `await` (the effects stay).
pub async fn call_method_awaited(cx: &Cx, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    // `await self.app(scope, receive, send)` in a raw middleware: the rest of the stack, run in place (no
    // coroutine object in between)
    if let V::Inst(i) = recv {
        if kwargs.is_empty() && super::methods::find_method(i.desc.methods, name).is_none() {
            if let Some(V::Native(n)) = i.field(name) {
                if let Native::AsgiApp(slot) = &*n {
                    return super::asgi::call_app(cx, slot, args).await;
                }
            }
        }
    }
    let sync = matches!(recv, V::Session(s) if s.is_sync())
        || matches!(recv, V::Native(n) if matches!(&**n, Native::Redis(c) if c.sync))
        || matches!(recv, V::Native(n) if matches!(&**n, Native::RPipe(p) if p.client.sync));
    let v = call_method(cx, recv, name, args, kwargs).await?;
    if !sync {
        return Ok(v); // awaited by the caller (`await_value`)
    }
    let ty = match &v {
        V::Result(_) => "ChunkedIteratorResult",
        other => other.type_name(),
    };
    Err(Exc::type_error(if super::python() >= (3, 14) {
        format!("'{ty}' object can't be awaited")
    } else {
        format!("object {ty} can't be used in 'await' expression")
    }))
}

pub fn is_async_fn(f: &V) -> bool {
    match f {
        V::Native(n) => match &**n {
            Native::PyFn(p) => p.is_async,
            Native::Bound(m, recv) => bound_is_async(*m, recv),
            _ => false,
        },
        _ => false,
    }
}

/// is the method `m` bound to `recv` one of its class's `async def`s?
fn bound_is_async(m: super::pyd::MethodFn, recv: &V) -> bool {
    let (methods, asyncs): (&[(&str, bool, super::pyd::MethodFn)], &[&str]) = match recv {
        V::Inst(i) => (i.desc.methods, i.desc.async_methods),
        V::Obj(o) => (o.desc.methods, o.desc.async_methods),
        _ => return false,
    };
    methods.iter().any(|(n, _, f)| std::ptr::fn_addr_eq(*f, m) && asyncs.contains(n))
}

/// `f(args)` not awaited: a coroutine when `f` is an async function, else the call itself
pub async fn call_value_lazy(cx: &Cx, f: &V, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if !is_async_fn(f) {
        return call_value(cx, f, args, kwargs).await;
    }
    let (cx2, f2) = (cx.clone(), f.clone());
    Ok(coro(Box::pin(async move { call_value(&cx2, &f2, args, kwargs).await })))
}

/// `obj.name(args)` not awaited: a coroutine when that method is an `async def` of the object's class
pub async fn call_method_lazy(cx: &Cx, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let asyncs: &[&str] = match recv {
        V::Inst(i) => i.desc.async_methods,
        V::Obj(o) => o.desc.async_methods,
        V::Exc(e) => match &e.0.class.kind {
            ClassKind::UserException(d) => d.async_methods,
            _ => &[],
        },
        _ => &[],
    };
    if !asyncs.contains(&name) {
        return call_method(cx, recv, name, args, kwargs).await;
    }
    let (cx2, recv2, name2) = (cx.clone(), recv.clone(), name.to_string());
    Ok(coro(Box::pin(async move { call_method(&cx2, &recv2, &name2, args, kwargs).await })))
}

/// `asyncio.gather(*aws, return_exceptions=False)`: each awaitable runs as a task; results in argument
/// order; without return_exceptions the first exception (in argument order) propagates and the other
/// tasks keep running, like asyncio.
pub async fn gather(args: Vec<V>, kwargs: &[(String, V)]) -> R {
    let mut return_exceptions = false;
    for (k, v) in kwargs {
        match k.as_str() {
            "return_exceptions" => return_exceptions = super::ops::truthy(v)?,
            _ => return Err(Exc::type_error(format!("gather() got an unexpected keyword argument '{k}'"))),
        }
    }
    for a in &args {
        let ok = matches!(a, V::Native(n) if matches!(&**n, Native::Coro(_) | Native::Task(_)));
        if !ok {
            return Err(Exc::type_error("An asyncio.Future, a coroutine or an awaitable is required"));
        }
    }
    let handles: Vec<_> = args.into_iter().map(|a| tokio::spawn(await_value(a))).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(Ok(v)) => out.push(v),
            Ok(Err(e)) if return_exceptions => out.push(V::Exc(e)),
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(Exc::runtime(format!("py2axum: gathered task failed: {e}"))),
        }
    }
    Ok(V::list(out))
}

/// `asyncio.as_completed(fs)`: the awaitables are scheduled at once (`ensure_future`, a task given twice once); the
/// iterator gives as many awaitables, the k-th one resolving with the k-th to complete (its result, or its
/// exception raised where it is awaited)
pub fn as_completed(cx: &Cx, args: Vec<V>, kwargs: &[(String, V)]) -> R {
    for (k, v) in kwargs {
        match k.as_str() {
            "timeout" if v.is_none() => {}
            "timeout" => return Err(Exc::type_error("py2axum: asyncio.as_completed(timeout=) is not supported")),
            _ => return Err(Exc::type_error(format!("as_completed() got an unexpected keyword argument '{k}'"))),
        }
    }
    let [fs] = &args[..] else {
        return Err(Exc::type_error(format!("as_completed() takes 1 positional argument but {} were given", args.len())));
    };
    if matches!(fs, V::Native(n) if matches!(&**n, Native::Coro(_) | Native::Task(_) | Native::AFuture(_))) {
        return Err(Exc::type_error(format!("expect an iterable of futures, not {}", fs.type_name())));
    }
    let mut items: Vec<V> = Vec::new();
    for a in super::ops::iter(fs)? {
        if !matches!(&a, V::Native(n) if matches!(&**n, Native::Coro(_) | Native::Task(_) | Native::AFuture(_))) {
            return Err(Exc::type_error("An asyncio.Future, a coroutine or an awaitable is required"));
        }
        if !items.iter().any(|x| super::ops::is(x, &a)) {
            items.push(a);
        }
    }
    let outs: Vec<V> = items.iter().map(|_| future_new(cx)).collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<R>();
    for a in items {
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(await_value(a).await);
        });
    }
    drop(tx);
    let slots = outs.clone();
    tokio::spawn(async move {
        for out in &slots {
            let Some(r) = rx.recv().await else { break };
            let V::Native(n) = out else { continue };
            let Native::AFuture(f) = &**n else { continue };
            let st = match r {
                Ok(v) => FutState::Done(v),
                Err(e) => FutState::Failed(e),
            };
            let _ = settle(out, f, st);
        }
    });
    Ok(V::native(Native::Iter(Mutex::new(outs.into()))))
}

/// `asyncio.Semaphore(value=1)` / `asyncio.Lock()`
pub struct Sem {
    pub sem: tokio::sync::Semaphore,
    pub held: Mutex<usize>,
}

pub fn semaphore(args: &[V], kwargs: &[(String, V)]) -> R {
    let n = match (args, kwargs) {
        ([], []) => 1,
        ([V::Int(i)], []) => *i,
        ([], [(k, V::Int(i))]) if k == "value" => *i,
        _ => return Err(Exc::type_error("py2axum: Semaphore(value) takes one int")),
    };
    if n < 0 {
        return Err(Exc::value_error("Semaphore initial value must be >= 0"));
    }
    Ok(V::native(Native::Sem(Arc::new(Sem { sem: tokio::sync::Semaphore::new(n as usize), held: Mutex::new(0) }))))
}

pub async fn sem_method(s: &Arc<Sem>, name: &str) -> R {
    match name {
        "acquire" | "__aenter__" => {
            let p = s.sem.acquire().await.map_err(|e| Exc::runtime(e.to_string()))?;
            p.forget();
            *s.held.lock() += 1;
            Ok(if name == "acquire" { V::Bool(true) } else { V::None })
        }
        "release" | "__aexit__" => {
            s.sem.add_permits(1);
            let mut h = s.held.lock();
            *h = h.saturating_sub(1);
            Ok(if name == "release" { V::None } else { V::Bool(false) })
        }
        "locked" => Ok(V::Bool(s.sem.available_permits() == 0)),
        _ => Err(Exc::attr_error(format!("'Semaphore' object has no attribute '{name}'"))),
    }
}

/// the future of a spawned coroutine value (`create_task(coro)`)
pub fn spawn_coro(cx: &Cx, c: V) -> R {
    super::web::spawn_future(cx, Box::pin(await_value(c)))
}

/// a project class's async dunder method (`__aenter__`, `__aexit__`)
fn project_dunder(v: &V, name: &str) -> Option<super::pyd::MethodFn> {
    super::ops::dunder(v, name)
}

/// `async with v`: the value entered
pub async fn aenter(cx: &Cx, v: &V) -> R {
    if let Some(f) = project_dunder(v, "__aenter__") {
        return f(cx, v.clone(), vec![]).await;
    }
    match v {
        V::Native(n) => match &**n {
            Native::HttpClient(_) | Native::HttpResp(_) | Native::AmqpConn(_) => Ok(v.clone()),
            // `async with session.begin_nested() [as sp]` / `session.begin()`: the AsyncSessionTransaction
            Native::Savepoint(..) => Ok(v.clone()),
            Native::Sem(s) => sem_method(s, "__aenter__").await,
            // `async with Redis(...) as r` / `async with r.pipeline() as pipe` (redis.asyncio)
            Native::Redis(c) if !c.sync => Ok(v.clone()),
            Native::RPipe(p) if !p.client.sync => Ok(v.clone()),
            Native::RPubSub(_) => Ok(v.clone()),
            Native::AmqpIter(it) => {
                super::rmq::iter_enter(it).await?;
                Ok(v.clone())
            }
            Native::AmqpProcess(m, ..) => Ok(V::native(Native::AmqpIncoming(m.clone()))),
            Native::Acm(g) => super::agen::acm_enter(g).await,
            Native::McpRun => {
                super::mcp::log("mcp.server.streamable_http_manager", "StreamableHTTP session manager started", vec![], None);
                Ok(V::None)
            }
            _ => Err(no_acm(v)),
        },
        V::Session(_) => Ok(v.clone()),
        _ => Err(no_acm(v)),
    }
}

fn no_acm(v: &V) -> Exc {
    cm_protocol_error(v, true)
}

/// `async with v` (`asynchronous`) or `with v` on a value without that protocol: CPython names the type
/// (`_thread.lock`), and from 3.14 the missing method and, when the value has the other protocol, the hint
pub fn cm_protocol_error(v: &V, asynchronous: bool) -> Exc {
    let name = match v {
        V::Native(n) => match &**n {
            Native::TLock(l) => if l.reentrant { "_thread.RLock" } else { "_thread.lock" },
            // a class written in Python: its module and qualified name
            // (3.14's message names it with its module)
            Native::Sem(_) => if super::python() >= (3, 14) { "asyncio.locks.Semaphore" } else { "Semaphore" },
            _ => v.type_name(),
        },
        _ => v.type_name(),
    };
    // the protocol the value has
    let other = match v {
        V::Session(s) => s.is_sync() == asynchronous,
        V::Native(n) => if asynchronous {
            matches!(&**n, Native::TLock(_) | Native::Gcm(_) | Native::File(_) | Native::BytesIO(_) | Native::Socket(_) | Native::ZipW(_)
                | Native::Prom(_) | Native::Sentry(_) | Native::Suppress(_) | Native::CtxToken(..))
                || matches!(&**n, Native::Redis(c) if c.sync) || matches!(&**n, Native::RPipe(p) if p.client.sync)
        } else {
            matches!(&**n, Native::HttpClient(_) | Native::HttpResp(_) | Native::AmqpConn(_) | Native::Sem(_) | Native::Acm(_))
                || matches!(&**n, Native::Redis(c) if !c.sync) || matches!(&**n, Native::RPipe(p) if !p.client.sync)
        },
        // a project class: its own methods
        V::Inst(_) | V::Obj(_) => super::ops::dunder(v, if asynchronous { "__exit__" } else { "__aexit__" }).is_some(),
        _ => false,
    };
    cm_error(name, asynchronous, other)
}

/// The message of `cm_protocol_error` for a type name, and whether the value has the other protocol
pub fn cm_error(name: &str, asynchronous: bool, other: bool) -> Exc {
    // 3.14 qualifies the type by its module (`contextlib._GeneratorContextManager`)
    let name = if name == "_GeneratorContextManager" && super::python() >= (3, 14) { "contextlib._GeneratorContextManager" } else { name };
    let (kind, missed) = if asynchronous { ("asynchronous context manager", "__aexit__") } else { ("context manager", "__exit__") };
    let mut msg = format!("'{name}' object does not support the {kind} protocol");
    if super::python() >= (3, 14) {
        msg += &format!(" (missed {missed} method)");
        if other && asynchronous {
            msg += " but it supports the context manager protocol. Did you mean to use 'with'?";
        } else if other {
            msg += " but it supports the asynchronous context manager protocol. Did you mean to use 'async with'?";
        }
    }
    Exc::type_error(msg)
}

/// leaving `async with v`: `exc` when the body raised; the result suppresses it when true
/// The end of `async with session.begin_nested()` (`name`, its SAVEPOINT) or `session.begin()` (`""`), as
/// SQLAlchemy's SessionTransaction.__exit__: nothing when the block already ended it (`await sp.commit()`); a
/// commit (flush, RELEASE / COMMIT) after a normal exit, rolled back if it fails (an IntegrityError of the flush:
/// ROLLBACK TO SAVEPOINT, the outer transaction stays usable) and its error raised; a rollback when the body
/// raised (its exception then propagates)
async fn savepoint_exit(sess: &super::orm::Session, name: &str, raised: bool) -> R<()> {
    if name.is_empty() {
        if raised {
            return sess.rollback().await;
        }
        if let Err(e) = sess.commit().await {
            let _ = sess.rollback().await;
            return Err(e);
        }
        return Ok(());
    }
    if !sess.has_savepoint(name).await {
        return Ok(());
    }
    if raised {
        return sess.end_savepoint(name, false).await;
    }
    if let Err(e) = sess.end_savepoint(name, true).await {
        // SessionTransaction.__exit__: rollback() after a failed commit (a failed flush already rolled the
        // savepoint back: this ends its PendingRollbackError state)
        let _ = sess.end_savepoint(name, false).await;
        return Err(e);
    }
    Ok(())
}

pub async fn aexit(cx: &Cx, v: &V, exc: Option<Exc>) -> R {
    if let Some(f) = project_dunder(v, "__aexit__") {
        let args = match exc {
            Some(e) => vec![V::Class(e.0.class), V::Exc(e), V::None],
            None => vec![V::None, V::None, V::None],
        };
        return f(cx, v.clone(), args).await;
    }
    match v {
        V::Native(n) if matches!(&**n, Native::Sem(_)) => {
            let Native::Sem(s) = &**n else { unreachable!() };
            sem_method(s, "__aexit__").await
        }
        V::Native(n) if matches!(&**n, Native::AmqpIter(_) | Native::AmqpProcess(..)) => {
            match &**n {
                Native::AmqpIter(it) => super::rmq::iter_close(it).await?,
                Native::AmqpProcess(m, requeue, redelivered, ignore) => super::rmq::process_exit(m, *requeue, *redelivered, *ignore, exc.as_ref()).await?,
                _ => unreachable!(),
            }
            Ok(V::Bool(false))
        }
        V::Native(n) if matches!(&**n, Native::Savepoint(..)) => {
            let Native::Savepoint(sess, name) = &**n else { unreachable!() };
            savepoint_exit(sess, name, exc.is_some()).await?;
            Ok(V::Bool(false))
        }
        V::Native(n) if matches!(&**n, Native::McpRun) => {
            super::mcp::log("mcp.server.streamable_http_manager", "StreamableHTTP session manager shutting down", vec![], None);
            Ok(V::Bool(false))
        }
        V::Native(n) if matches!(&**n, Native::Redis(_) | Native::RPipe(_) | Native::RPubSub(_)) => {
            // the client or pubsub closed (`aclose()`), the pipeline reset
            let name = if matches!(&**n, Native::RPipe(_)) { "reset" } else { "aclose" };
            call_method(cx, v, name, vec![], vec![]).await?;
            Ok(V::Bool(false))
        }
        V::Native(n) if matches!(&**n, Native::Acm(_)) => {
            let Native::Acm(g) = &**n else { unreachable!() };
            super::agen::acm_exit(g, exc).await
        }
        V::Session(s) if exc.is_none() && s.commits_on_exit() => {
            s.commit().await?;
            Ok(V::Bool(false))
        }
        V::Native(n) if matches!(&**n, Native::AmqpConn(_)) => {
            let Native::AmqpConn(c) = &**n else { unreachable!() };
            super::rmq::conn_method(c, "close", &[]).await?;
            Ok(V::Bool(false))
        }
        _ => {
            super::http::aexit(v).await?;
            Ok(V::Bool(false))
        }
    }
}

/// the items of an async iterator (`[x async for x in it]`): a project async generator, a runtime one
pub async fn collect(cx: &Cx, it: V) -> R {
    if let V::Native(n) = &it {
        if let Native::AsyncItems(items) = &**n {
            return Ok(V::list(items.lock().take().unwrap_or_default()));
        }
    }
    let ai = super::agen::aiter(cx, &it).await?;
    let mut out = Vec::new();
    while let Some(v) = super::agen::anext_opt(cx, &ai).await? {
        out.push(v);
    }
    Ok(V::list(out))
}

// ---------------------------------------------------------------- asyncio.Future, asyncio.Event

pub enum FutState {
    Pending,
    Done(V),
    Failed(Exc),
    Cancelled,
}

/// `asyncio.Future()` / `loop.create_future()`: a result set by someone else, awaited
pub struct AFut {
    pub state: parking_lot::Mutex<FutState>,
    notify: tokio::sync::Notify,
    callbacks: parking_lot::Mutex<Vec<V>>,
    cx: Option<Cx>,
}

pub fn future_new(cx: &Cx) -> V {
    V::native(Native::AFuture(Arc::new(AFut { state: parking_lot::Mutex::new(FutState::Pending), notify: tokio::sync::Notify::new(),
                                              callbacks: parking_lot::Mutex::new(vec![]), cx: Some(cx.clone()) })))
}

fn invalid_state(msg: &str) -> Exc {
    Exc::msg(&INVALID_STATE_ERROR, msg)
}

/// the state reached: wakes the waiters, schedules the done callbacks (`call_soon`)
fn settle(me: &V, f: &Arc<AFut>, st: FutState) -> R<()> {
    {
        let mut s = f.state.lock();
        if !matches!(*s, FutState::Pending) {
            return Err(invalid_state("invalid state"));
        }
        *s = st;
    }
    f.notify.notify_waiters();
    let cbs = std::mem::take(&mut *f.callbacks.lock());
    if let Some(cx) = &f.cx {
        for cb in cbs {
            super::web::spawn_task(cx, cb, vec![me.clone()], vec![])?;
        }
    }
    Ok(())
}

fn fut_result(f: &AFut) -> R {
    match &*f.state.lock() {
        FutState::Pending => Err(invalid_state("Result is not set.")),
        FutState::Done(v) => Ok(v.clone()),
        FutState::Failed(e) => Err(e.clone()),
        FutState::Cancelled => Err(Exc::new(&CANCELLED_ERROR, vec![])),
    }
}

/// `await fut`
pub async fn future_wait(f: &Arc<AFut>) -> R {
    loop {
        // registered before the check: a `set_result` in between still wakes this waiter
        let notified = f.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !matches!(*f.state.lock(), FutState::Pending) {
            return fut_result(f);
        }
        notified.await;
    }
}

pub fn future_method(recv: &V, f: &Arc<AFut>, name: &str, args: &[V]) -> R {
    let one = || args.first().cloned().ok_or_else(|| Exc::type_error(format!("{name}() takes exactly one argument (0 given)")));
    match name {
        "set_result" => settle(recv, f, FutState::Done(one()?)).map(|_| V::None),
        "set_exception" => match one()? {
            V::Exc(e) => settle(recv, f, FutState::Failed(e)).map(|_| V::None),
            V::Class(c) => settle(recv, f, FutState::Failed(Exc::new(c, vec![]))).map(|_| V::None),
            _ => Err(Exc::type_error("exception must derive from BaseException")),
        },
        "result" => fut_result(f),
        "exception" => match &*f.state.lock() {
            FutState::Pending => Err(invalid_state("Exception is not set.")),
            FutState::Done(_) => Ok(V::None),
            FutState::Failed(e) => Ok(V::Exc(e.clone())),
            FutState::Cancelled => Err(Exc::new(&CANCELLED_ERROR, vec![])),
        },
        "done" => Ok(V::Bool(!matches!(*f.state.lock(), FutState::Pending))),
        "cancelled" => Ok(V::Bool(matches!(*f.state.lock(), FutState::Cancelled))),
        "cancel" => Ok(V::Bool(settle(recv, f, FutState::Cancelled).is_ok())),
        "add_done_callback" => {
            let cb = one()?;
            if matches!(*f.state.lock(), FutState::Pending) {
                f.callbacks.lock().push(cb);
            } else if let Some(cx) = &f.cx {
                super::web::spawn_task(cx, cb, vec![recv.clone()], vec![])?;
            }
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'_asyncio.Future' object has no attribute '{name}'"))),
    }
}

/// `repr(fut)` (`_asyncio.Future`'s, without the creation's source line)
pub fn future_repr(f: &AFut) -> R<String> {
    Ok(match &*f.state.lock() {
        FutState::Pending => "<Future pending>".into(),
        FutState::Done(v) => format!("<Future finished result={}>", super::ops::repr(v)?),
        FutState::Failed(e) => format!("<Future finished exception={}>", super::ops::repr(&V::Exc(e.clone()))?),
        FutState::Cancelled => "<Future cancelled>".into(),
    })
}

/// `asyncio.Event()`
pub struct AEvt {
    set: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

pub fn event_new(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() || !kwargs.is_empty() {
        return Err(Exc::type_error("Event.__init__() takes 1 positional argument"));
    }
    Ok(V::native(Native::AEvent(Arc::new(AEvt { set: std::sync::atomic::AtomicBool::new(false), notify: tokio::sync::Notify::new() }))))
}

pub fn event_is_set(e: &AEvt) -> bool {
    e.set.load(std::sync::atomic::Ordering::SeqCst)
}

pub async fn event_method(e: &Arc<AEvt>, name: &str, args: &[V]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error(format!("Event.{name}() takes 1 positional argument but {} were given", args.len() + 1)));
    }
    use std::sync::atomic::Ordering::SeqCst;
    match name {
        "is_set" => Ok(V::Bool(e.set.load(SeqCst))),
        "set" => {
            e.set.store(true, SeqCst);
            e.notify.notify_waiters();
            Ok(V::None)
        }
        "clear" => {
            e.set.store(false, SeqCst);
            Ok(V::None)
        }
        // a coroutine: `await ev.wait()` (or a task made of it)
        "wait" => {
            let e = e.clone();
            Ok(coro(Box::pin(async move {
                loop {
                    let notified = e.notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if e.set.load(SeqCst) {
                        return Ok(V::Bool(true));
                    }
                    notified.await;
                }
            })))
        }
        _ => Err(Exc::attr_error(format!("'Event' object has no attribute '{name}'"))),
    }
}
