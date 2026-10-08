//! Async generators in lockstep with their consumer, like CPython: the body starts at the first
//! `__anext__`, stops at each `yield` until the next `__anext__`/`athrow`/`aclose`, and a generator
//! dropped while suspended sees `GeneratorExit` at its `yield` (its `finally` blocks run).
//! `contextlib.asynccontextmanager` (`_AsyncGeneratorContextManager`) is built on it.
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::v::*;
use super::Cx;

pub enum GenIn {
    Next(V),
    Throw(Exc),
    Close,
}

enum GenOut {
    Yield(V),
    Done(R),
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Created,
    Suspended,
    Done,
}

pub struct AGen {
    tx: mpsc::UnboundedSender<GenIn>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<GenOut>>,
    st: Mutex<St>,
}

/// The generator side: `yield v` hands `v` over, then waits to be resumed.
pub struct Yielder {
    out: mpsc::UnboundedSender<GenOut>,
    inp: tokio::sync::Mutex<mpsc::UnboundedReceiver<GenIn>>,
}

impl Yielder {
    /// `yield v`: the value sent by `asend` (None for `__anext__`), or the exception thrown in
    pub async fn send(&self, v: V) -> R {
        if self.out.send(GenOut::Yield(v)).is_err() {
            return Err(Exc::new(&GENERATOR_EXIT, vec![]));
        }
        match self.inp.lock().await.recv().await {
            Some(GenIn::Next(x)) => Ok(x),
            Some(GenIn::Throw(e)) => Err(e),
            Some(GenIn::Close) | None => Err(Exc::new(&GENERATOR_EXIT, vec![])),
        }
    }
}

/// Calling an `async def` generator: the body waits for the first `__anext__` (or never runs).
pub fn spawn_gen<F>(f: F) -> V
where
    F: FnOnce(Yielder) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'static>> + Send + 'static,
{
    V::native(Native::Gen(Arc::new(new_gen(f))))
}

fn new_gen<F>(f: F) -> AGen
where
    F: FnOnce(Yielder) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'static>> + Send + 'static,
{
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<GenIn>();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<GenOut>();
    tokio::spawn(async move {
        let first = in_rx.recv().await;
        let r = match first {
            Some(GenIn::Next(_)) => {
                let y = Yielder { out: out_tx.clone(), inp: tokio::sync::Mutex::new(in_rx) };
                f(y).await
            }
            // thrown into / closed before it started: the body never runs
            Some(GenIn::Throw(e)) => Err(e),
            Some(GenIn::Close) | None => Ok(V::None),
        };
        let _ = out_tx.send(GenOut::Done(r));
    });
    AGen { tx: in_tx, rx: tokio::sync::Mutex::new(out_rx), st: Mutex::new(St::Created) }
}

fn stop() -> Exc {
    Exc::new(&STOP_ASYNC_ITERATION, vec![])
}

impl AGen {
    async fn resume(&self, msg: GenIn) -> Option<GenOut> {
        let mut rx = self.rx.lock().await;
        if self.tx.send(msg).is_err() {
            return None;
        }
        rx.recv().await
    }

    /// `await gen.asend(v)` / `await anext(gen)` (v = None)
    pub async fn asend(&self, v: V) -> R {
        let st = *self.st.lock();
        if st == St::Done {
            return Err(stop());
        }
        if st == St::Created && !v.is_none() {
            return Err(Exc::type_error("can't send non-None value to a just-started async generator"));
        }
        match self.resume(GenIn::Next(v)).await {
            Some(GenOut::Yield(x)) => {
                *self.st.lock() = St::Suspended;
                Ok(x)
            }
            Some(GenOut::Done(r)) => {
                *self.st.lock() = St::Done;
                match r {
                    Ok(_) => Err(stop()),
                    Err(e) if e.isinstance(&STOP_ASYNC_ITERATION) => Err(Exc::runtime("async generator raised StopAsyncIteration")),
                    Err(e) => Err(e),
                }
            }
            None => {
                *self.st.lock() = St::Done;
                Err(stop())
            }
        }
    }

    /// `await gen.athrow(exc)`
    pub async fn athrow(&self, e: Exc) -> R {
        let st = *self.st.lock();
        if st == St::Done {
            return Err(e);
        }
        match self.resume(GenIn::Throw(e)).await {
            Some(GenOut::Yield(x)) => {
                *self.st.lock() = St::Suspended;
                Ok(x)
            }
            Some(GenOut::Done(r)) => {
                *self.st.lock() = St::Done;
                match r {
                    Ok(_) => Err(stop()),
                    Err(e) => Err(e),
                }
            }
            None => {
                *self.st.lock() = St::Done;
                Err(stop())
            }
        }
    }

    /// `await gen.aclose()`
    pub async fn aclose(&self) -> R {
        let st = *self.st.lock();
        if st == St::Done {
            return Ok(V::None);
        }
        let out = self.resume(GenIn::Close).await;
        *self.st.lock() = St::Done;
        match out {
            Some(GenOut::Yield(_)) => Err(Exc::runtime("async generator ignored GeneratorExit")),
            Some(GenOut::Done(Err(e))) if !e.isinstance(&GENERATOR_EXIT) => Err(e),
            _ => Ok(V::None),
        }
    }
}

/// The items of a generator as a channel (StreamingResponse): pulled one `__anext__` at a time.
pub fn into_channel(g: Arc<AGen>) -> mpsc::Receiver<V> {
    let (tx, rx) = mpsc::channel::<V>(1);
    tokio::spawn(async move {
        loop {
            match g.asend(V::None).await {
                Ok(v) => {
                    if tx.send(v).await.is_err() {
                        let _ = g.aclose().await;
                        break;
                    }
                }
                Err(e) => {
                    if !e.isinstance(&STOP_ASYNC_ITERATION) && !e.isinstance(&GENERATOR_EXIT) {
                        eprintln!("ERROR:py2axum:exception in async generator: {:?}", e);
                    }
                    break;
                }
            }
        }
    });
    rx
}

pub fn as_gen(v: &V) -> Option<Arc<AGen>> {
    match v {
        V::Native(n) => match &**n {
            Native::Gen(g) => Some(g.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `anext(it)` (awaited)
pub async fn anext(it: &V) -> R {
    if let V::Native(n) = it {
        if let Native::WsIter(s, kind) = &**n {
            return super::ws::iter_next(s, kind).await?.ok_or_else(stop);
        }
    }
    match as_gen(it) {
        Some(g) => g.asend(V::None).await,
        None => Err(Exc::type_error(format!("'{}' object is not an async iterator", it.type_name()))),
    }
}

/// `async for x in it`: `aiter(it)`; an async generator (and `websocket.iter_*()`) is its own iterator,
/// a project object calls its `__aiter__`
pub async fn aiter(cx: &Cx, it: &V) -> R {
    match it {
        V::Native(n) if matches!(&**n, Native::Gen(_) | Native::WsIter(..) | Native::AsyncItems(_)) => Ok(it.clone()),
        V::Inst(_) => super::methods::call_method(cx, it, "__aiter__", vec![], vec![]).await,
        o => Err(Exc::type_error(format!("'async for' requires an object with __aiter__ method, got {}", o.type_name()))),
    }
}

/// the next item of an `async for` (None once StopAsyncIteration is raised)
pub async fn anext_opt(cx: &Cx, it: &V) -> R<Option<V>> {
    let r = match it {
        V::Native(n) if matches!(&**n, Native::WsIter(..)) => {
            let Native::WsIter(s, kind) = &**n else { unreachable!() };
            return super::ws::iter_next(s, kind).await;
        }
        V::Native(n) if matches!(&**n, Native::Gen(_)) => anext(it).await,
        V::Native(n) if matches!(&**n, Native::AsyncItems(_)) => {
            let Native::AsyncItems(items) = &**n else { unreachable!() };
            let mut g = items.lock();
            return Ok(match g.as_mut() {
                Some(v) if !v.is_empty() => Some(v.remove(0)),
                _ => None,
            });
        }
        _ => super::methods::call_method(cx, it, "__anext__", vec![], vec![]).await,
    };
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.isinstance(&STOP_ASYNC_ITERATION) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `agen.asend(v)`, `agen.athrow(e)`, `agen.aclose()`, `agen.__anext__()` (all awaited)
pub async fn method(g: &Arc<AGen>, recv: &V, name: &str, args: &[V]) -> R {
    match name {
        "__anext__" => g.asend(V::None).await,
        "asend" => g.asend(args.first().cloned().unwrap_or(V::None)).await,
        "aclose" => g.aclose().await,
        "athrow" => {
            let e = match args.first() {
                Some(V::Exc(e)) => e.clone(),
                Some(V::Class(c)) => Exc::new(c, vec![]),
                _ => return Err(Exc::type_error("exceptions must be classes deriving BaseException")),
            };
            g.athrow(e).await
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", recv.type_name()))),
    }
}

// ---------------------------------------------------------------- contextlib.asynccontextmanager

/// `@asynccontextmanager`: the decorated function, called, gives an `_AsyncGeneratorContextManager`.
pub fn asynccontextmanager(f: &V) -> R {
    let (module, qual, doc) = fn_names(f);
    let f = f.clone();
    Ok(super::pyfn(&module, &qual, doc.as_deref(), false, Arc::new(move |cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| {
        let f = f.clone();
        Box::pin(async move {
            let g = super::methods::call_value(cx, &f, args, kwargs).await?;
            match as_gen(&g) {
                Some(g) => Ok(V::native(Native::Acm(g))),
                None => Err(Exc::type_error(format!("'{}' object is not an async iterator", g.type_name()))),
            }
        })
    })))
}

fn fn_names(f: &V) -> (String, String, Option<String>) {
    let attr = |k: &str| -> Option<String> {
        match f {
            V::Native(n) => match &**n {
                Native::PyFn(p) => p.attrs.lock().iter().find(|(n, _)| n == k).and_then(|(_, v)| match v {
                    V::Str(s) => Some(s.to_string()),
                    _ => None,
                }),
                _ => None,
            },
            _ => None,
        }
    };
    (attr("__module__").unwrap_or_default(), attr("__qualname__").unwrap_or_else(|| "<function>".into()), attr("__doc__"))
}

/// `async with cm`: the value of the generator's first `yield`
pub async fn acm_enter(g: &Arc<AGen>) -> R {
    match g.asend(V::None).await {
        Err(e) if e.isinstance(&STOP_ASYNC_ITERATION) => Err(Exc::runtime("generator didn't yield")),
        r => r,
    }
}

/// leaving `async with cm`: true suppresses the body's exception
pub async fn acm_exit(g: &Arc<AGen>, exc: Option<Exc>) -> R {
    match exc {
        None => match g.asend(V::None).await {
            Ok(_) => Err(Exc::runtime("generator didn't stop")),
            Err(e) if e.isinstance(&STOP_ASYNC_ITERATION) => Ok(V::Bool(false)),
            Err(e) => Err(e),
        },
        Some(e) => match g.athrow(e.clone()).await {
            Ok(_) => Err(Exc::runtime("generator didn't stop after athrow()")),
            // the exception was swallowed by the generator: suppressed
            Err(x) if x.isinstance(&STOP_ASYNC_ITERATION) && !e.isinstance(&STOP_ASYNC_ITERATION) => Ok(V::Bool(true)),
            // re-raised as is: not suppressed (the `async with` raises it)
            Err(x) if Arc::ptr_eq(&x.0, &e.0) => Ok(V::Bool(false)),
            Err(x) => Err(x),
        },
    }
}

// ---------------------------------------------------------------- contextvars

/// `contextvars.ContextVar`: its values live in the request's context (`Cx`), shared with the tasks the
/// request creates (CPython copies the context into a new task: a value set inside the task stays there)
pub struct CtxVar {
    id: usize,
    pub name: String,
    default: Option<V>,
}

static NEXT_VAR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

pub fn context_var(args: &[V], kwargs: &[(String, V)]) -> R {
    let name = match args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "name").map(|(_, v)| v)) {
        Some(V::Str(s)) => s.to_string(),
        Some(_) => return Err(Exc::type_error("context variable name must be a str")),
        None => return Err(Exc::type_error("ContextVar() missing required argument 'name' (pos 1)")),
    };
    let default = kwargs.iter().find(|(k, _)| k == "default").map(|(_, v)| v.clone());
    let id = NEXT_VAR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(V::native(Native::CtxVar(Arc::new(CtxVar { id, name, default }))))
}

pub fn ctxvar_method(cx: &Cx, var: &Arc<CtxVar>, name: &str, args: &[V]) -> R {
    match name {
        "get" => {
            if let Some(v) = cx.ctxvars.lock().get(&var.id) {
                return Ok(v.clone());
            }
            if let Some(d) = args.first() {
                return Ok(d.clone());
            }
            match &var.default {
                Some(d) => Ok(d.clone()),
                None => Err(Exc::msg(&LOOKUP_ERROR, format!("<ContextVar name='{}'>", var.name))),
            }
        }
        "set" => {
            let v = args.first().cloned().ok_or_else(|| Exc::type_error("ContextVar.set() takes exactly one argument (0 given)"))?;
            let old = cx.ctxvars.lock().insert(var.id, v);
            Ok(V::native(Native::CtxToken(var.clone(), old)))
        }
        "reset" => {
            let Some(V::Native(n)) = args.first() else {
                return Err(Exc::type_error("ContextVar.reset() takes exactly one argument"));
            };
            let Native::CtxToken(tv, old) = &**n else {
                return Err(Exc::type_error("an instance of Token was expected"));
            };
            if tv.id != var.id {
                return Err(Exc::value_error("Token was created by a different ContextVar"));
            }
            let mut m = cx.ctxvars.lock();
            match old {
                Some(o) => {
                    m.insert(var.id, o.clone());
                }
                None => {
                    m.remove(&var.id);
                }
            }
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'_contextvars.ContextVar' object has no attribute '{name}'"))),
    }
}
