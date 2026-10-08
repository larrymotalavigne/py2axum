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
            _ => return Ok(v),
        },
        _ => return Ok(v),
    };
    fut.await
}

/// The call in `await obj.method(...)` on a value known only at run time: a synchronous `Session`'s
/// methods return plain values, so CPython runs the call, then fails on the `await` (the effects stay).
pub async fn call_method_awaited(cx: &Cx, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let sync = matches!(recv, V::Session(s) if s.is_sync());
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
            Native::Sem(s) => sem_method(s, "__aenter__").await,
            Native::Acm(g) => super::agen::acm_enter(g).await,
            Native::McpRun => Ok(V::None),
            _ => Err(no_acm(v)),
        },
        V::Session(_) => Ok(v.clone()),
        _ => Err(no_acm(v)),
    }
}

fn no_acm(v: &V) -> Exc {
    Exc::type_error(format!("'{}' object does not support the asynchronous context manager protocol", v.type_name()))
}

/// leaving `async with v`: `exc` when the body raised; the result suppresses it when true
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
        V::Native(n) if matches!(&**n, Native::McpRun) => Ok(V::Bool(false)),
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
