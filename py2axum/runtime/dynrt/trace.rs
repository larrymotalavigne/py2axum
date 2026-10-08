//! `sys.settrace(f)` / `threading.settrace(f)` / `threading.settrace_all_threads(f)` for the project's
//! own functions: when the project calls one of them, every compiled function reports a `call` event
//! (the trace function's result becomes the frame's local trace function), then `return` with the
//! returned value, or `exception` with `(type, value, traceback)` when an exception enters the frame
//! (raised there, propagated from a callee, or caught there) followed by `return` with None. The trace
//! function is process-wide (all threads) and is not traced itself.
//!
//! Differences with CPython: one `call`/`return` pair per invocation (a coroutine suspended by `await`
//! does not report `return`/`call` at each suspension), no `line`/`opcode` events, library frames do
//! not exist (tracebacks list the project frames only), a frame's line is the one where its current
//! statement starts.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use super::methods::call_value;
use super::v::*;
use super::Cx;

/// A project function's frame while it runs (`frame`, `frame.f_code`).
pub struct Frame {
    pub module: &'static str,
    pub qualname: &'static str,
    pub file: &'static str,
    pub first: u32,
    line: AtomicU32,
    local: Mutex<Option<V>>,
    /// exceptions already reported in this frame
    reported: Mutex<Vec<usize>>,
}

static TRACER: RwLock<Option<V>> = RwLock::new(None);
static ON: AtomicBool = AtomicBool::new(false);

/// `sys.settrace(f)` and the `threading` variants (None stops tracing)
pub fn settrace(args: &[V]) -> R {
    let [f] = args else {
        return Err(Exc::type_error(format!("settrace() takes exactly one argument ({} given)", args.len())));
    };
    *TRACER.write() = if f.is_none() { None } else { Some(f.clone()) };
    ON.store(!f.is_none(), Ordering::Relaxed);
    Ok(V::None)
}

/// `sys.gettrace()`
pub fn gettrace() -> R {
    Ok(TRACER.read().clone().unwrap_or(V::None))
}

fn frame_value(f: &Arc<Frame>) -> V {
    V::native(Native::TraceFrame(f.clone()))
}

/// calls a trace function with the guard set (the tracer's own calls are not traced)
async fn call_tracer(cx: &Cx, t: &V, f: &Arc<Frame>, event: &str, arg: V) -> R {
    cx.in_trace.store(true, Ordering::Relaxed);
    let r = call_value(cx, t, vec![frame_value(f), V::str(event), arg], vec![]).await;
    cx.in_trace.store(false, Ordering::Relaxed);
    if r.is_err() {
        // CPython: an exception in a trace function turns tracing off
        *TRACER.write() = None;
        ON.store(false, Ordering::Relaxed);
    }
    r
}

/// a function starts: the `call` event, the frame kept while it has a local trace function
pub async fn enter(cx: &Cx, module: &'static str, qualname: &'static str, file: &'static str, first: u32) -> R<Option<Arc<Frame>>> {
    if !ON.load(Ordering::Relaxed) || cx.in_trace.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let Some(t) = TRACER.read().clone() else { return Ok(None) };
    let f = Arc::new(Frame { module, qualname, file, first, line: AtomicU32::new(first), local: Mutex::new(None), reported: Mutex::new(vec![]) });
    let local = call_tracer(cx, &t, &f, "call", V::None).await?;
    if local.is_none() {
        return Ok(None);
    }
    *f.local.lock() = Some(local);
    Ok(Some(f))
}

fn exc_id(e: &Exc) -> usize {
    Arc::as_ptr(&e.0) as usize
}

/// `(type, value, traceback)` of an exception seen in frame `f` at `line`
fn exc_arg(f: &Arc<Frame>, e: &Exc, line: u32) -> V {
    let mut chain = vec![(f.clone(), line)];
    chain.extend(e.0.tb.lock().iter().rev().cloned());
    V::tuple(vec![V::Class(e.0.class), V::Exc(e.clone()), V::native(Native::Traceback(Arc::new(chain), 0))])
}

/// the `exception` event, once per exception and frame; the local trace function it returns is kept
async fn exception_event(cx: &Cx, f: &Arc<Frame>, e: &Exc, line: u32) -> R<()> {
    {
        let mut seen = f.reported.lock();
        if seen.contains(&exc_id(e)) {
            return Ok(());
        }
        seen.push(exc_id(e));
    }
    f.line.store(line, Ordering::Relaxed);
    let local = f.local.lock().clone();
    if let Some(l) = local {
        let next = call_tracer(cx, &l, f, "exception", exc_arg(f, e, line)).await?;
        *f.local.lock() = if next.is_none() { None } else { Some(next) };
    }
    Ok(())
}

/// an exception reaches a `try` of the frame (before its handlers)
pub async fn caught(cx: &Cx, f: &Option<Arc<Frame>>, e: &Exc, line: u32) -> R<()> {
    match f {
        Some(f) => exception_event(cx, f, e, line).await,
        None => Ok(()),
    }
}

/// the function ends: `exception` if it raises, then `return` (with None after an exception)
pub async fn leave(cx: &Cx, f: Option<Arc<Frame>>, r: R, line: u32) -> R {
    let Some(f) = f else { return r };
    f.line.store(line, Ordering::Relaxed);
    let arg = match &r {
        Ok(v) => v.clone(),
        Err(e) => {
            exception_event(cx, &f, e, line).await?;
            e.0.tb.lock().push((f.clone(), line));
            V::None
        }
    };
    let local = f.local.lock().clone();
    if let Some(l) = local {
        call_tracer(cx, &l, &f, "return", arg).await?;
    }
    r
}

fn name_of(q: &str) -> &str {
    q.rsplit('.').next().unwrap_or(q)
}

pub fn frame_attr(f: &Arc<Frame>, name: &str) -> R {
    match name {
        "f_globals" => V::dict_from(vec![(V::str("__name__"), V::str(f.module)), (V::str("__file__"), V::str(f.file))]),
        "f_code" => Ok(V::native(Native::TraceCode(f.clone()))),
        "f_lineno" => Ok(V::Int(f.line.load(Ordering::Relaxed) as i64)),
        "f_back" => Ok(V::None),
        _ => Err(Exc::attr_error(format!("py2axum: frame.{name} is not available"))),
    }
}

pub fn code_attr(f: &Arc<Frame>, name: &str) -> R {
    match name {
        "co_qualname" => Ok(V::str(f.qualname)),
        "co_name" => Ok(V::str(name_of(f.qualname))),
        "co_filename" => Ok(V::str(f.file)),
        "co_firstlineno" => Ok(V::Int(f.first as i64)),
        _ => Err(Exc::attr_error(format!("py2axum: code.{name} is not available"))),
    }
}

pub fn tb_attr(chain: &Arc<Vec<(Arc<Frame>, u32)>>, i: usize, name: &str) -> R {
    let (f, line) = &chain[i];
    match name {
        "tb_lineno" => Ok(V::Int(*line as i64)),
        "tb_frame" => Ok(frame_value(f)),
        "tb_next" => Ok(if i + 1 < chain.len() { V::native(Native::Traceback(chain.clone(), i + 1)) } else { V::None }),
        _ => Err(Exc::attr_error(format!("'traceback' object has no attribute '{name}'"))),
    }
}

/// `traceback.extract_tb(tb, limit=None)`: the project frames from `tb` down
pub fn extract_tb(args: &[V], kwargs: &[(String, V)]) -> R {
    let tb = args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "tb").map(|(_, v)| v));
    let limit = args.get(1).or_else(|| kwargs.iter().find(|(k, _)| k == "limit").map(|(_, v)| v));
    let mut out = Vec::new();
    match tb {
        None | Some(V::None) => {}
        Some(V::Native(n)) => match &**n {
            Native::Traceback(chain, i) => {
                for (f, line) in &chain[*i..] {
                    out.push(V::native(Native::FrameSummary(f.clone(), *line)));
                }
            }
            _ => return Err(Exc::attr_error(format!("'{}' object has no attribute 'tb_frame'", n_type(n)))),
        },
        Some(o) => return Err(Exc::attr_error(format!("'{}' object has no attribute 'tb_frame'", o.type_name()))),
    }
    if let Some(V::Int(l)) = limit {
        let l = (*l).max(0) as usize;
        out.truncate(l);
    }
    Ok(V::list(out))
}

fn n_type(n: &Native) -> &'static str {
    match n {
        Native::TraceFrame(_) => "frame",
        Native::TraceCode(_) => "code",
        _ => "object",
    }
}

pub fn summary_attr(f: &Arc<Frame>, line: u32, name: &str) -> R {
    match name {
        "filename" => Ok(V::str(f.file)),
        "lineno" => Ok(V::Int(line as i64)),
        "name" => Ok(V::str(name_of(f.qualname))),
        "line" => Ok(V::None),
        _ => Err(Exc::attr_error(format!("'FrameSummary' object has no attribute '{name}'"))),
    }
}

/// `traceback.format_exception(exc)` / `(type, value, tb)`: the exception's own line(s), as CPython formats
/// them for a traceback of None (`module.QualName: message`); frames and chained exceptions are not
/// rendered (the binary has no Python frames, a documented difference)
pub fn format_exception(cx: &Cx, args: &[V], kwargs: &[(String, V)]) -> R {
    let _ = cx;
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !["value", "tb", "limit", "chain"].contains(&k.as_str())) {
        return Err(Exc::type_error(format!("format_exception() got an unexpected keyword argument '{k}'")));
    }
    let value = match (args.first(), args.get(1).or_else(|| kwargs.iter().find(|(k, _)| k == "value").map(|(_, v)| v))) {
        (Some(V::Class(_)), Some(v)) => v.clone(),
        (Some(v), _) => v.clone(),
        (None, _) => return Err(Exc::type_error("format_exception() missing required argument 'exc' (pos 1)")),
    };
    format_exception_only(&value)
}

/// `traceback.format_exception_only(exc)`
pub fn format_exception_only(value: &V) -> R {
    let line = match value {
        V::None => "NoneType: None\n".to_string(),
        V::Exc(e) => {
            let c = e.0.class;
            // a project exception's qualname already carries its module
            let ty = match super::sentry::exc_module(c) {
                Some(m) if m != "builtins" && m != "__main__" && !c.qualname.starts_with(&format!("{m}.")) => format!("{m}.{}", c.qualname),
                _ => c.qualname.to_string(),
            };
            let msg = super::ops::str_(value)?;
            if msg.is_empty() { format!("{ty}\n") } else { format!("{ty}: {msg}\n") }
        }
        o => return Err(Exc::type_error(format!("py2axum: format_exception() of a {} is not supported", o.type_name()))),
    };
    Ok(V::list(vec![V::str(&line)]))
}
