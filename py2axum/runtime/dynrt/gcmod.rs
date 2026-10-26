//! `gc`: the binary has no cycle collector (Rust frees values by ownership and reference counts), so the module
//! keeps only its observable state: the thresholds (`set_threshold`/`get_threshold`, with each CPython version's
//! defaults and its rules), the enabled flag and the frozen count. `collect()` returns 0 (CPython: the number of
//! unreachable objects found, which depends on the interpreter's heap and is not part of a response); `get_count()`
//! is `(0, 0, 0)`. Documented in docs/reference/stdlib.md.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::v::*;

static ENABLED: AtomicBool = AtomicBool::new(true);
static THRESHOLDS: Mutex<Option<[i64; 3]>> = Mutex::new(None);

fn defaults() -> [i64; 3] {
    // CPython 3.14's incremental collector: (2000, 10, 0), the third threshold always 0
    if super::python() >= (3, 14) { [2000, 10, 0] } else { [700, 10, 10] }
}

fn int_arg(name: &str, v: &V) -> R<i64> {
    match v {
        V::Int(i) => Ok(*i),
        V::Bool(b) => Ok(*b as i64),
        other => {
            let _ = name;
            Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", other.type_name())))
        }
    }
}

/// `gc.<name>(*args, **kwargs)`
pub fn call(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !(name == "collect" && k == "generation")) {
        return Err(Exc::type_error(format!("{name}() takes no keyword arguments ('{k}' given)")));
    }
    let nargs = |lo: usize, hi: usize| -> R<()> {
        if args.len() < lo || args.len() > hi {
            let what = if lo == hi && lo == 0 { "takes no arguments".to_string() } else { format!("expected at most {hi} arguments") };
            return Err(Exc::type_error(format!("{name}() {what} ({} given)", args.len())));
        }
        Ok(())
    };
    match name {
        "collect" => {
            let g = args.first().or(kwargs.first().map(|(_, v)| v));
            if args.len() + kwargs.len() > 1 {
                return Err(Exc::type_error(format!("collect() takes at most 1 argument ({} given)", args.len() + kwargs.len())));
            }
            if let Some(g) = g {
                let g = int_arg("collect", g)?;
                if !(0..=2).contains(&g) {
                    return Err(Exc::value_error("invalid generation"));
                }
            }
            Ok(V::Int(0))
        }
        "enable" | "disable" => {
            nargs(0, 0)?;
            ENABLED.store(name == "enable", Ordering::Relaxed);
            Ok(V::None)
        }
        "isenabled" => {
            nargs(0, 0)?;
            Ok(V::Bool(ENABLED.load(Ordering::Relaxed)))
        }
        "freeze" | "unfreeze" => {
            nargs(0, 0)?;
            Ok(V::None)
        }
        "get_freeze_count" => {
            nargs(0, 0)?;
            Ok(V::Int(0))
        }
        "get_count" => {
            nargs(0, 0)?;
            Ok(V::tuple(vec![V::Int(0), V::Int(0), V::Int(0)]))
        }
        "get_threshold" => {
            nargs(0, 0)?;
            let t = THRESHOLDS.lock().unwrap().unwrap_or_else(defaults);
            Ok(V::tuple(t.iter().map(|x| V::Int(*x)).collect()))
        }
        "set_threshold" => {
            if args.is_empty() || args.len() > 3 {
                return Err(Exc::type_error(format!(
                    "set_threshold() takes {} ({} given)",
                    if args.is_empty() { "at least 1 argument" } else { "at most 3 arguments" },
                    args.len()
                )));
            }
            let mut g = THRESHOLDS.lock().unwrap();
            let mut t = g.unwrap_or_else(defaults);
            for (i, a) in args.iter().enumerate() {
                t[i] = int_arg("set_threshold", a)?;
            }
            if super::python() >= (3, 14) {
                t[2] = 0;
            }
            *g = Some(t);
            Ok(V::None)
        }
        _ => Err(Exc::type_error(format!("py2axum: gc.{name}() is not supported"))),
    }
}
