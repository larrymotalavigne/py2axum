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
        V::Exc(e) => exc_line(e),
        o => return Err(Exc::type_error(format!("py2axum: format_exception() of a {} is not supported", o.type_name()))),
    };
    // the notes (`add_note`, 3.14's json/dict notes), one line each after the message
    let mut out = vec![V::str(&line)];
    if let V::Exc(e) = value {
        out.extend(e.notes().iter().map(|n| V::str(format!("{n}\n"))));
    }
    Ok(V::list(out))
}

/// an exception's last traceback line: `module.QualName: message`
pub fn format_exc_only(e: &Exc) -> String {
    let mut out = exc_line(e);
    // its notes (`add_note`, json's from 3.14), a line each
    for n in e.notes() {
        out.push_str(&n);
        out.push('\n');
    }
    out
}

/// `type: message` of an exception, without its notes
fn exc_line(e: &Exc) -> String {
    let c = e.0.class;
    // a project exception's qualname already carries its module
    let ty = match super::sentry::exc_module(c) {
        Some(m) if m != "builtins" && m != "__main__" && !c.qualname.starts_with(&format!("{m}.")) => format!("{m}.{}", c.qualname),
        _ => c.qualname.to_string(),
    };
    let msg = super::ops::str_(&V::Exc(e.clone())).unwrap_or_default();
    if msg.is_empty() { format!("{ty}\n") } else { format!("{ty}: {msg}\n") }
}

/// `traceback.print_exception(exc)` of a raised exception: its chain (`__cause__`, else `__context__`),
/// each with "Traceback (most recent call last):", its project frames, and its own line
pub fn format_exc(e: &Exc) -> String {
    let mut out = String::new();
    let mut seen: Vec<*const ExcObj> = vec![];
    chain(e, &mut out, &mut seen);
    out
}

/// anyio's TaskGroup: `ExceptionGroup("unhandled errors in a TaskGroup", [e])`
pub fn task_group_error(e: &Exc) -> Exc {
    let g = Exc::msg(&EXCEPTION_GROUP, "unhandled errors in a TaskGroup (1 sub-exception)");
    g.0.attrs.lock().insert("exceptions".into(), V::tuple(vec![V::Exc(e.clone())]));
    g
}

/// An exception escaping Starlette's StreamingResponse while it streams: before Starlette 1.3 the body runs
/// in an anyio TaskGroup (with the disconnect listener) and Starlette re-raises the single exception out of
/// its `ExceptionGroup`, which becomes its `__context__`; from 1.3 (`create_collapsing_task_group`) it
/// comes out alone.
pub fn streaming_error(e: &Exc) -> Exc {
    if super::web::starlette() < (1, 3) {
        let g = task_group_error(e);
        e.0.attrs.lock().insert("__context__".into(), V::Exc(g));
    }
    e.clone()
}

/// an exception group as `traceback` prints it: its own traceback and line behind `  | `, then each
/// sub-exception boxed under its number (the exceptions already printed are shared with the group's: a
/// sub-exception whose `__context__` is the group itself, as Starlette re-raises it, is not followed back)
fn group(g: &Exc, subs: &[Exc], out: &mut String, seen: &mut Vec<*const ExcObj>) {
    seen.push(Arc::as_ptr(&g.0));
    out.push_str("  + Exception Group Traceback (most recent call last):\n");
    for l in format_frames(g).lines() {
        out.push_str(&format!("  | {l}\n"));
    }
    for l in format_exc_only(g).lines() {
        out.push_str(&format!("  | {l}\n"));
    }
    for (i, x) in subs.iter().enumerate() {
        let head = if i == 0 { "  +-+" } else { "    +" };
        out.push_str(&format!("{head}---------------- {} ----------------\n", i + 1));
        let mut sub = String::new();
        chain(x, &mut sub, seen);
        for l in sub.lines() {
            out.push_str(&format!("    | {l}\n"));
        }
    }
    out.push_str("    +------------------------------------\n");
}

fn chain(e: &Exc, out: &mut String, seen: &mut Vec<*const ExcObj>) {
    if e.isinstance(&EXCEPTION_GROUP) {
        let subs: Vec<Exc> = match e.0.attrs.lock().get("exceptions") {
            Some(V::Tuple(t)) => t.iter().filter_map(|v| if let V::Exc(x) = v { Some(x.clone()) } else { None }).collect(),
            _ => vec![],
        };
        return group(e, &subs, out, seen);
    }
    seen.push(Arc::as_ptr(&e.0));
    let attrs = e.0.attrs.lock();
    let cause = attrs.get("__cause__").cloned();
    let context = attrs.get("__context__").cloned();
    let suppress = matches!(attrs.get("__suppress_context__"), Some(V::Bool(true)));
    drop(attrs);
    match (cause, context) {
        (Some(V::Exc(c)), _) if !seen.contains(&Arc::as_ptr(&c.0)) => {
            chain(&c, out, seen);
            out.push_str("\nThe above exception was the direct cause of the following exception:\n\n");
        }
        (_, Some(V::Exc(c))) if !suppress && !seen.contains(&Arc::as_ptr(&c.0)) => {
            chain(&c, out, seen);
            out.push_str("\nDuring handling of the above exception, another exception occurred:\n\n");
        }
        _ => {}
    }
    // an exception never raised (no `__traceback__`: the context `Enum.__new__` sets) has no header
    if !matches!(e.0.attrs.lock().get("__unraised__"), Some(V::Bool(true))) {
        out.push_str("Traceback (most recent call last):\n");
    }
    out.push_str(&format_frames(e));
    out.push_str(&format_exc_only(e));
}

/// A traceback entry: a position in the project's code an exception went through (the file relative to
/// the source root, the instruction's line, the function, and the source and carets CPython prints under
/// the `File` line, computed at translation by py2axum/tbfmt.py).
pub struct Pos {
    pub file: &'static str,
    pub line: u32,
    pub func: &'static str,
    pub text: &'static str,
}

/// `r`, its exception recording the position it passes (once per frame invocation: `fid`), and the exception
/// being handled as its `__context__` when it starts there
#[inline]
pub fn at<T>(cx: &Cx, r: R<T>, p: &'static Pos, fid: usize) -> R<T> {
    if let Err(e) = &r {
        passed(cx, e, p, fid, false);
    }
    r
}

/// `at` for a method call: a missing attribute (`'X' object has no attribute 'name'`) passes at `obj.name`,
/// anything else at the call
#[inline]
pub fn at_attr<T>(cx: &Cx, r: R<T>, call: &'static Pos, attr: &'static Pos, name: &str, fid: usize) -> R<T> {
    if let Err(e) = &r {
        // raised here (no project frame yet) by the lookup of this very name
        let missing = e.0.ptb.lock().is_empty() && e.isinstance(&ATTRIBUTE_ERROR) && e.message().ends_with(&format!("has no attribute '{name}'"));
        passed(cx, e, if missing { attr } else { call }, fid, false);
    }
    r
}

#[cold]
fn passed(cx: &Cx, e: &Exc, p: &'static Pos, fid: usize, force: bool) {
    let mut tb = e.0.ptb.lock();
    if tb.is_empty() {
        drop(tb);
        set_context(cx, e);
        tb = e.0.ptb.lock();
    } else if !force && fid != 0 && tb.last().is_some_and(|(_, f)| *f == fid) {
        return;
    }
    tb.push((p, fid));
}

/// the exception being handled (`sys.exc_info()`) becomes `__context__` of one raised then, as in CPython
fn set_context(cx: &Cx, e: &Exc) {
    let handled = cx.handling.lock().last().cloned();
    if let Some(h) = handled {
        if !Arc::ptr_eq(&h.0, &e.0) && !e.0.attrs.lock().contains_key("__context__") {
            // no cycle: an exception is never in its own context chain
            let mut c = Some(h.clone());
            let mut n = 0;
            while let Some(x) = c {
                if Arc::ptr_eq(&x.0, &e.0) || n > 100 {
                    return;
                }
                n += 1;
                c = match x.0.attrs.lock().get("__context__") {
                    Some(V::Exc(y)) => Some(y.clone()),
                    _ => None,
                };
            }
            e.0.attrs.lock().insert("__context__".into(), V::Exc(h));
        }
    }
}

/// an exception raised by the runtime while `handled` is being handled: its `__context__`, as in CPython
pub fn with_context(e: Exc, handled: &Exc) -> Exc {
    if !Arc::ptr_eq(&e.0, &handled.0) && !e.0.attrs.lock().contains_key("__context__") {
        e.0.attrs.lock().insert("__context__".into(), V::Exc(handled.clone()));
    }
    e
}

/// `raise X [from Y]`: the raise statement's entry (always: `raise e` again in the same frame adds a line)
pub fn raised(cx: &Cx, e: Exc, p: &'static Pos, cause: Option<&V>) -> Exc {
    if let Some(c) = cause {
        let mut a = e.0.attrs.lock();
        match c {
            V::Exc(c) => {
                a.insert("__cause__".into(), V::Exc(c.clone()));
            }
            _ => {
                a.shift_remove("__cause__");
            }
        }
        a.insert("__suppress_context__".into(), V::Bool(true));
    }
    passed(cx, &e, p, 0, true);
    e
}

/// the project frames an exception went through, outermost first, as `traceback` prints them
pub fn format_frames(e: &Exc) -> String {
    format_frames_limit(e, None)
}

/// `traceback.format_tb(tb, limit)` joined: the first `limit` frames (outermost first)
pub fn format_frames_limit(e: &Exc, limit: Option<usize>) -> String {
    let tb = e.0.ptb.lock();
    let mut out = String::new();
    for (p, _) in tb.iter().rev().take(limit.unwrap_or(usize::MAX)) {
        out += &format!("  File \"{}\", line {}, in {}\n{}", super::logging::source_path(p.file), p.line, p.func, p.text);
    }
    out
}
