//! Attribute access, method calls and builtins on dynamic values.
use std::sync::Arc;

use chrono::{Datelike, Duration, Timelike};
use indexmap::IndexMap;
use parking_lot::Mutex;

use super::dt::{self, DateTime, Tz};
use super::orm;
use super::pyd;
use super::v::*;
use super::{libs, ops, web, Cx, FnVal};

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn arg<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kw(kwargs, name))
}

fn no_attr(v: &V, name: &str) -> Exc {
    Exc::attr_error(format!("'{}' object has no attribute '{}'", v.type_name(), name))
}

fn find_method(table: &'static [(&'static str, bool, pyd::MethodFn)], name: &str) -> Option<(bool, pyd::MethodFn)> {
    table.iter().find(|(n, _, _)| *n == name).map(|(_, p, f)| (*p, *f))
}

fn bound(f: pyd::MethodFn, recv: V) -> V {
    V::native(Native::Bound(f, recv))
}

/// `copy.deepcopy` of validated data (lists, dicts, sets, model instances; immutable values shared).
fn deepcopy(v: &V) -> R {
    Ok(match v {
        V::List(l) => V::list(l.lock().iter().map(deepcopy).collect::<R<Vec<_>>>()?),
        V::Dict(d) => {
            let mut out = Vec::new();
            for (_, (k, x)) in d.lock().iter() {
                out.push((k.clone(), deepcopy(x)?));
            }
            V::dict_from(out)?
        }
        V::Inst(i) => V::Inst(Arc::new(pyd::Inst {
            desc: i.desc,
            vals: Mutex::new(i.vals.lock().iter().map(deepcopy).collect::<R<Vec<_>>>()?),
            set: Mutex::new(i.set.lock().clone()),
            extra: Mutex::new(i.extra.lock().clone()),
        })),
        V::Obj(_) => return Err(Exc::type_error("py2axum: deepcopy of a mapped object is not supported")),
        V::Set(_) => return Err(Exc::type_error("py2axum: deepcopy of a set is not supported")),
        other => other.clone(),
    })
}

// ---------------------------------------------------------------- getattr / setattr

/// methods of the runtime's lock / event / loop / semaphore objects that may be read as values
const NATIVE_METHODS: &[&str] = &["acquire", "release", "locked", "set", "clear", "is_set", "wait", "is_running", "is_closed",
    "call_soon", "call_soon_threadsafe", "run_forever", "stop", "close", "create_task", "run_in_executor"];

pub async fn getattr(cx: &Cx, v: &V, name: &str) -> R {
    if let V::Native(n) = v {
        match &**n {
            Native::Module(m) => return (m.attr)(cx, name).await,
            Native::Tenacity(t) => return super::tenacity::attr(t, name),
            Native::Prom(p) => return super::prom::attr(p, name),
            Native::Routing(o) => return super::routing::attr(o, name),
            Native::TraceFrame(f) => return super::trace::frame_attr(f, name),
            Native::TraceCode(f) => return super::trace::code_attr(f, name),
            Native::Traceback(c, i) => return super::trace::tb_attr(c, *i, name),
            Native::FrameSummary(f, l) => return super::trace::summary_attr(f, *l, name),
            _ => {}
        }
    }
    if let Some(r) = orm::sql_attr(v, name) {
        return r;
    }
    match v {
        V::Obj(o) => {
            if o.desc.col_index(name).is_some() || o.desc.rel_index(name).is_some() {
                return o.get_attr(name).await;
            }
            if let Some((prop, f)) = find_method(o.desc.methods, name) {
                return if prop { f(cx, v.clone(), vec![]).await } else { Ok(bound(f, v.clone())) };
            }
            o.get_attr_sync(name)
        }
        V::Inst(i) => {
            if let Some(x) = i.field(name) {
                return Ok(x);
            }
            if let Some((prop, f)) = find_method(i.desc.methods, name) {
                return if prop { f(cx, v.clone(), vec![]).await } else { Ok(bound(f, v.clone())) };
            }
            match name {
                "model_fields_set" => {
                    let set = i.set.lock().clone();
                    let names: Vec<V> = i.desc.fields.iter().zip(set).filter(|(_, s)| *s).map(|(f, _)| V::str(f.name)).collect();
                    set_of(names)
                }
                "model_fields" => schema_fields(i.desc),
                // a snapshot of the instance dict (fields, then the free attributes)
                "__dict__" if i.desc.slots.is_empty() => {
                    let vals = i.vals.lock().clone();
                    let mut items: Vec<(V, V)> = i.desc.fields.iter().zip(vals).map(|(f, x)| (V::str(f.name), x)).collect();
                    items.extend(i.extra.lock().iter().map(|(k, x)| (V::str(k), x.clone())));
                    V::dict_from(items)
                }
                _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{}'", i.desc.name, name))),
            }
        }
        V::Class(c) if matches!(c.kind, ClassKind::UserException(_)) && c.exc_lookup(name).is_some() => match c.exc_lookup(name).unwrap() {
            ExcMember::Attr(f) => f(cx).await,
            ExcMember::Method(..) => Err(Exc::type_error(format!("py2axum: {}.{name} read on the class is not supported", c.name))),
        },
        V::Class(c) => match (&c.kind, name) {
            (ClassKind::Model(m), _) if m.col_index(name).is_some() => Ok(V::Col(m, m.col_index(name).unwrap())),
            (ClassKind::Model(m), _) if m.rel_index(name).is_some() => Ok(orm::sql(orm::Sql::Rel(m, m.rel_index(name).unwrap()))),
            (ClassKind::Model(m), "__tablename__") => Ok(V::str(m.table)),
            (ClassKind::Model(m), _) if m.class_methods.contains(&name) => match find_method(m.methods, name) {
                Some((_, f)) => Ok(bound(f, v.clone())),
                None => unreachable!("class method {name} not in the method table"),
            },
            (ClassKind::Schema(s), "model_fields") => schema_fields(s),
            (ClassKind::Enum(e), _) if e.by_name(name).is_some() => Ok(e.by_name(name).unwrap()),
            (ClassKind::Enum(e), _) if find_method(e.methods, name).is_some() => Ok(bound(find_method(e.methods, name).unwrap().1, v.clone())),
            (_, "__name__") => Ok(V::str(c.name)),
            (_, "__qualname__") => Ok(V::str(c.qualname)),
            _ => Err(Exc::attr_error(format!("type object '{}' has no attribute '{}'", c.name, name))),
        },
        V::Enum(e, i) => match name {
            "name" | "_name_" => Ok(V::str(e.member_name(*i))),
            "value" | "_value_" => Ok(e.value(*i)),
            _ => match find_method(e.methods, name) {
                Some((true, f)) => f(cx, v.clone(), vec![]).await,
                Some((false, f)) => Ok(bound(f, v.clone())),
                None => Err(no_attr(v, name)),
            },
        },
        V::Exc(e) if e.0.attrs.lock().contains_key(name) => Ok(e.0.attrs.lock()[name].clone()),
        V::Exc(e) => match name {
            "args" => Ok(V::tuple(e.args())),
            "status_code" if e.http_info().is_some() => Ok(V::Int(e.http_info().unwrap().0 as i64)),
            "detail" if e.http_info().is_some() => Ok(e.http_info().unwrap().1),
            "headers" if e.http_info().is_some() => {
                let h = e.http_info().unwrap().2;
                if h.is_empty() { Ok(V::None) } else { V::dict_from(h.into_iter().map(|(k, v)| (V::str(k), V::str(v))).collect()) }
            }
            _ => match e.0.class.exc_lookup(name) {
                Some(ExcMember::Attr(f)) => f(cx).await,
                Some(ExcMember::Method(true, f)) => f(cx, v.clone(), vec![]).await,
                Some(ExcMember::Method(false, f)) => Ok(bound(f, v.clone())),
                None => Err(no_attr(v, name)),
            },
        },
        V::DateTime(d) => Ok(match name {
            "year" => V::Int(d.wall.year() as i64),
            "month" => V::Int(d.wall.month() as i64),
            "day" => V::Int(d.wall.day() as i64),
            "hour" => V::Int(d.wall.hour() as i64),
            "minute" => V::Int(d.wall.minute() as i64),
            "second" => V::Int(d.wall.second() as i64),
            "microsecond" => V::Int((d.wall.nanosecond() / 1000) as i64),
            "tzinfo" => d.tz.map(V::Tz).unwrap_or(V::None),
            "fold" => V::Int(d.fold as i64),
            _ => return Err(no_attr(v, name)),
        }),
        V::Date(d) => Ok(match name {
            "year" => V::Int(d.year() as i64),
            "month" => V::Int(d.month() as i64),
            "day" => V::Int(d.day() as i64),
            _ => return Err(no_attr(v, name)),
        }),
        V::Delta(d) => {
            let us = dt::micros(d);
            Ok(match name {
                "days" => V::Int(us.div_euclid(86_400_000_000)),
                "seconds" => V::Int(us.rem_euclid(86_400_000_000) / 1_000_000),
                "microseconds" => V::Int(us.rem_euclid(1_000_000)),
                _ => return Err(no_attr(v, name)),
            })
        }
        V::Tz(t) => match name {
            "key" => Ok(V::str(t.name())),
            _ => Err(no_attr(v, name)),
        },
        V::Result(r) => orm::result_attr(r, name),
        V::Col(m, i) => match name {
            "key" | "name" => Ok(V::str(m.cols[*i].name)),
            _ => Err(no_attr(v, name)),
        },
        V::Native(n) if matches!(&**n, Native::McpServer(_)) => match &**n {
            Native::McpServer(s) => super::mcp::attr(s, name),
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::Upload(_) | Native::Path(_) | Native::Uuid(_) | Native::RespObj(_) | Native::Pattern(_) | Native::Match(_) | Native::CsvRows(..) | Native::Sniffed(_)) => match &**n {
            Native::Pattern(p) => super::stdlib::pattern_attr(p, name),
            Native::Match(m) => super::stdlib::match_attr(m, name),
            Native::CsvRows(f, _) if name == "fieldnames" => Ok(f.clone()),
            Native::Sniffed(c) if name == "delimiter" => Ok(V::str(c.to_string())),
            Native::CsvRows(..) | Native::Sniffed(_) => Err(no_attr(v, name)),
            Native::RespObj(r) => super::resp::attr(r, name),
            Native::Upload(u) => super::files::upload_attr(u, name),
            Native::Path(p) => super::pathio::path_attr(p, name),
            Native::Uuid(u) => super::pathio::uuid_attr(*u, name),
            _ => unreachable!(),
        },
        V::Native(n) => match &**n {
            Native::Request(r) => Ok(match name {
                "headers" => V::native(Native::Headers(r.clone())),
                "state" => V::native(Native::State(r.clone())),
                "url" => V::native(Native::Url(r.clone())),
                "method" => V::str(&r.method),
                "query_params" => V::dict_from(r.query.iter().map(|(k, x)| (V::str(k), V::str(x))).collect())?,
                "cookies" => cookies(r)?,
                "client" => r.client.as_ref().map(|(h, p)| V::native(Native::Address(h.clone(), *p))).unwrap_or(V::None),
                "path_params" => V::dict_from(r.path_params.lock().iter().map(|(k, x)| (V::str(k), V::str(x))).collect())?,
                "app" => super::routing::app(),
                "scope" => super::routing::scope(cx)?,
                _ => return Err(no_attr(v, name)),
            }),
            Native::Totp(t) => super::auth::totp_attr(t, name),
            Native::UrlParts(u) => super::stdlib::url_parts_attr(u, name),
            Native::IniConfig(c) => super::ini::attr(c, name),
            Native::PydUrl(_, u) => pyd::url_attr(u, name),
            Native::Row(names, vals) => match name {
                "_mapping" => V::dict_from(names.iter().zip(vals.iter()).map(|(k, x)| (V::str(k), x.clone())).collect()),
                "_fields" => Ok(V::tuple(names.iter().map(V::str).collect())),
                _ => names.iter().position(|k| &**k == name).map(|i| vals[i].clone()).ok_or_else(|| no_attr(v, name)),
            },
            Native::Record(_, f) => f.iter().find(|(k, _)| *k == name).map(|(_, x)| x.clone()).ok_or_else(|| no_attr(v, name)),
            Native::HttpResp(r) => super::http::resp_attr(r, name),
            Native::HttpClient(c) => super::http::client_attr(c, name),
            Native::HttpUrl(u) => super::http::url_attr(u, name),
            // `re.Pattern` -> Pattern (type names carry their module where CPython's repr does)
            Native::Type(t) if name == "__name__" || name == "__qualname__" => Ok(V::str(t.rsplit('.').next().unwrap_or(t))),
            Native::TThread(t) => super::thread::thread_attr(t, name),
            Native::YarlUrl(u) => super::http::yarl_attr(u, name),
            Native::AmqpConn(c) => super::rmq::conn_attr(c, name),
            Native::AmqpChan(c) => super::rmq::chan_attr(c, name),
            Native::AmqpIncoming(m) => super::rmq::incoming_attr(m, name),
            Native::AmqpQueue(_, q) if name == "name" => Ok(V::str(q)),
            Native::ExtType(t) if name == "__name__" || name == "__qualname__" => Ok(V::str(t.rsplit('.').next().unwrap_or(t))),
            Native::ExtType(t) if name == "__module__" => Ok(V::str(t.rsplit_once('.').map(|x| x.0).unwrap_or("builtins"))),
            Native::PyFn(f) => f.attrs.lock().iter().find(|(k, _)| k == name).map(|(_, x)| x.clone()).ok_or_else(|| no_attr(v, name)),
            Native::Address(h, p) => Ok(match name {
                "host" => V::str(h),
                "port" => V::Int(*p as i64),
                _ => return Err(no_attr(v, name)),
            }),
            Native::Url(r) => Ok(match name {
                "path" => V::str(&r.path),
                "query" => V::str(&r.raw_query),
                "hostname" => r.header("host").map(|h| V::str(h.split(':').next().unwrap_or(""))).unwrap_or(V::None),
                _ => return Err(no_attr(v, name)),
            }),
            Native::State(r) => r
                .state
                .lock()
                .get(name)
                .cloned()
                .ok_or_else(|| Exc::attr_error(format!("'State' object has no attribute '{name}'"))),
            Native::Response(r) => match name {
                "status_code" => Ok(r.status.lock().map(|s| V::Int(s as i64)).unwrap_or(V::None)),
                "headers" => Ok(V::native(Native::CellHeaders(r.clone()))),
                _ => Err(no_attr(v, name)),
            },
            Native::Logger(n) => match name {
                "name" => Ok(V::str(&**n)),
                _ => Err(no_attr(v, name)),
            },
            // a method of a lock, event, loop... read as a value (`loop.call_soon(ready.set)`)
            Native::TLock(_) | Native::TEvent(_) | Native::ELoop(_) | Native::Sem(_) => match NATIVE_METHODS.iter().find(|m| **m == name) {
                Some(m) => Ok(V::native(Native::MethodOf(v.clone(), m))),
                None => Err(no_attr(v, name)),
            },
            _ => Err(no_attr(v, name)),
        },
        V::Session(_) if name == "bind" => Ok(V::native(Native::Engine)),
        V::List(_) | V::Dict(_) | V::Set(_) | V::Str(_) => match BUILTIN_METHODS.iter().find(|(t, m)| *t == v.type_name() && *m == name) {
            // `items.append` read as a value: called later with its receiver
            Some((_, m)) => Ok(V::native(Native::MethodOf(v.clone(), m))),
            None => Err(no_attr(v, name)),
        },
        _ => Err(no_attr(v, name)),
    }
}

const BUILTIN_METHODS: &[(&str, &str)] = &[
    ("list", "append"), ("list", "extend"), ("list", "pop"), ("list", "remove"), ("list", "insert"), ("list", "clear"),
    ("list", "index"), ("list", "count"),
    ("dict", "get"), ("dict", "pop"), ("dict", "setdefault"), ("dict", "update"), ("dict", "keys"), ("dict", "values"),
    ("dict", "items"), ("dict", "clear"),
    ("set", "add"), ("set", "discard"), ("set", "remove"), ("set", "pop"), ("set", "clear"), ("set", "update"),
    ("str", "upper"), ("str", "lower"), ("str", "strip"), ("str", "format"), ("str", "join"), ("str", "split"),
];

/// `request.cookies`: Starlette's `cookie_parser` (lenient, `http.cookies._unquote` on values)
fn cookies(r: &web::ReqCell) -> R {
    let mut out: Vec<(V, V)> = Vec::new();
    for raw in r.headers.iter().filter(|(k, _)| k == "cookie").map(|(_, v)| v) {
        for chunk in raw.split(';') {
            let (k, v) = match chunk.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => ("", chunk.trim()),
            };
            if !k.is_empty() || !v.is_empty() {
                let val = V::str(cookie_unquote(v));
                match out.iter_mut().find(|(x, _)| matches!(x, V::Str(s) if &**s == k)) {
                    Some(e) => e.1 = val,
                    None => out.push((V::str(k), val)),
                }
            }
        }
        // Starlette reads the first Cookie header only
        break;
    }
    V::dict_from(out)
}

/// `http.cookies._unquote`
fn cookie_unquote(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b'"' || b[b.len() - 1] != b'"' {
        return s.to_string();
    }
    let inner: Vec<char> = s[1..s.len() - 1].chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == '\\' && i + 1 < inner.len() {
            let oct: String = inner[i + 1..(i + 4).min(inner.len())].iter().collect();
            let o: Vec<char> = oct.chars().collect();
            if o.len() == 3 && ('0'..='3').contains(&o[0]) && ('0'..='7').contains(&o[1]) && ('0'..='7').contains(&o[2]) {
                out.push(char::from(u8::from_str_radix(&oct, 8).unwrap_or(0)));
                i += 4;
            } else if inner[i + 1] != '\n' {
                out.push(inner[i + 1]);
                i += 2;
            } else {
                out.push(inner[i]);
                i += 1;
            }
        } else {
            out.push(inner[i]);
            i += 1;
        }
    }
    out
}

fn schema_fields(s: &'static pyd::SchemaDesc) -> R {
    V::dict_from(s.fields.iter().map(|f| (V::str(f.name), V::native(Native::FieldInfo))).collect())
}

fn set_of(items: Vec<V>) -> R {
    let mut m = IndexMap::new();
    for v in items {
        m.insert(Key::of(&v)?, v);
    }
    Ok(V::Set(Arc::new(Mutex::new(m))))
}

pub fn setattr(v: &V, name: &str, val: V) -> R<()> {
    match v {
        V::Obj(o) => o.set_attr(name, val),
        V::Inst(i) => i.set_field(name, val),
        // any exception instance takes attributes (`self.code = ...` in a project exception's __init__)
        V::Exc(e) => {
            e.0.attrs.lock().insert(name.to_string(), val);
            Ok(())
        }
        V::Native(n) => match &**n {
            Native::State(r) => {
                r.state.lock().insert(name.to_string(), val);
                Ok(())
            }
            Native::RespObj(r) if name == "status_code" => super::resp::set_status(r, &val),
            Native::PyFn(f) => {
                let mut a = f.attrs.lock();
                match a.iter_mut().find(|(k, _)| k == name) {
                    Some(slot) => slot.1 = val,
                    None => a.push((name.to_string(), val)),
                }
                Ok(())
            }
            Native::Response(r) if name == "status_code" => {
                *r.status.lock() = Some(match val {
                    V::Int(i) => i as u16,
                    _ => return Err(Exc::type_error("status_code must be an int")),
                });
                Ok(())
            }
            _ => Err(Exc::attr_error(format!("'{}' object attribute '{}' is read-only", v.type_name(), name))),
        },
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{}'", v.type_name(), name))),
    }
}

pub async fn hasattr(cx: &Cx, v: &V, name: &str) -> R {
    match getattr(cx, v, name).await {
        Ok(_) => Ok(V::Bool(true)),
        Err(e) if e.isinstance(&ATTRIBUTE_ERROR) => Ok(V::Bool(false)),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------- method calls

pub async fn call_method(cx: &Cx, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    match recv {
        V::Native(n) if matches!(&**n, Native::Module(_)) => {
            let f = getattr(cx, recv, name).await?;
            call_value(cx, &f, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::Tenacity(_)) => {
            let Native::Tenacity(t) = &**n else { unreachable!() };
            if !kwargs.is_empty() {
                return Err(Exc::type_error(format!("{name}() takes no keyword arguments")));
            }
            super::tenacity::method(t, name, &args)
        }
        V::Native(n) if matches!(&**n, Native::Routing(_)) => {
            let Native::Routing(o) = &**n else { unreachable!() };
            super::routing::method(cx, o, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::Prom(_)) => {
            let Native::Prom(p) = &**n else { unreachable!() };
            super::prom::method(cx, p, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::Adapter(..)) => {
            let Native::Adapter(td, _) = &**n else { unreachable!() };
            pyd::adapter_method(cx, td, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::AmqpConn(_) | Native::AmqpChan(_) | Native::AmqpQueue(..) | Native::AmqpExchange(..) | Native::AmqpIncoming(_)) => match &**n {
            Native::AmqpConn(c) => super::rmq::conn_method(c, name, &args).await,
            Native::AmqpChan(c) => super::rmq::chan_method(c, name, &args, &kwargs).await,
            Native::AmqpQueue(c, q) if name == "get" => super::rmq::queue_get(c, q, &kwargs).await,
            Native::AmqpExchange(c, x) if name == "publish" => super::rmq::publish(c, x, &args, &kwargs).await,
            Native::AmqpIncoming(m) => super::rmq::incoming_method(m, name).await,
            _ => Err(no_attr(recv, name)),
        },
        V::Native(n) if matches!(&**n, Native::Redis(_)) => {
            let Native::Redis(c) = &**n else { unreachable!() };
            super::rds::method(c, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::Sem(_)) => {
            let Native::Sem(s) = &**n else { unreachable!() };
            super::aio::sem_method(s, name).await
        }
        V::Native(n) if matches!(&**n, Native::TLock(_) | Native::TEvent(_) | Native::TThread(_) | Native::ELoop(_)) => match &**n {
            Native::TLock(l) => super::thread::lock_method(l, name, &args, &kwargs),
            Native::TEvent(e) => super::thread::event_method(e, name, &args, &kwargs),
            Native::TThread(t) => super::thread::thread_method(cx, t, name, &args),
            Native::ELoop(l) => Box::pin(super::thread::loop_method(cx, recv, l, name, args)).await,
            _ => unreachable!(),
        },
        // a method of a project exception class
        V::Exc(e) if matches!(e.0.class.exc_lookup(name), Some(ExcMember::Method(false, _))) => {
            let Some(ExcMember::Method(_, f)) = e.0.class.exc_lookup(name) else { unreachable!() };
            f(cx, recv.clone(), super::pack(args, kwargs)).await
        }
        // pydantic_core.ValidationError
        V::Exc(e) if e.0.errors.is_some() && matches!(name, "errors" | "error_count") => {
            let errs = e.0.errors.as_ref().unwrap();
            if name == "error_count" {
                return Ok(V::Int(errs.len() as i64));
            }
            if !args.is_empty() {
                return Err(Exc::type_error("ValidationError.errors() takes keyword arguments only"));
            }
            let flag = |k: &str| -> R<bool> {
                kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| ops::truthy(v)).unwrap_or(Ok(true))
            };
            if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "include_url" | "include_context" | "include_input")) {
                return Err(Exc::type_error(format!("errors() got an unexpected keyword argument '{k}'")));
            }
            let (url, ctx, input) = (flag("include_url")?, flag("include_context")?, flag("include_input")?);
            let mut out = Vec::new();
            for d in errs {
                let mut items = vec![
                    (V::str("type"), V::str(d.kind)),
                    (V::str("loc"), V::tuple(d.loc.clone())),
                    (V::str("msg"), V::str(&d.msg)),
                ];
                if input {
                    items.push((V::str("input"), d.input.clone()));
                }
                if let (true, Some(c)) = (ctx, &d.ctx) {
                    items.push((V::str("ctx"), V::dict_from(c.iter().map(|(k, v)| (V::str(*k), v.clone())).collect())?));
                }
                if url {
                    items.push((V::str("url"), V::str(format!("https://errors.pydantic.dev/{}/v/{}", super::pydantic(), d.kind))));
                }
                out.push(V::dict_from(items)?);
            }
            Ok(V::list(out))
        }
        // a function object's attribute holding a callable (`f.__wrapped__(...)`)
        V::Native(n) if matches!(&**n, Native::PyFn(_)) => {
            let f = getattr(cx, recv, name).await?;
            Box::pin(call_value(cx, &f, args, kwargs)).await
        }
        V::Enum(e, _) if find_method(e.methods, name).is_some() => {
            let (prop, f) = find_method(e.methods, name).unwrap();
            if prop {
                let v = f(cx, recv.clone(), vec![]).await?;
                return Box::pin(call_value(cx, &v, args, kwargs)).await;
            }
            f(cx, recv.clone(), super::pack(args, kwargs)).await
        }
        V::Class(c) if matches!(c.kind, ClassKind::Enum(e) if find_method(e.methods, name).is_some()) => {
            let ClassKind::Enum(e) = c.kind else { unreachable!() };
            let (_, f) = find_method(e.methods, name).unwrap();
            f(cx, recv.clone(), super::pack(args, kwargs)).await
        }
        V::Enum(e, i) if e.kind != EnumKind::Plain => Box::pin(call_method(cx, &e.value(*i), name, args, kwargs)).await,
        V::Decimal(d) => super::decimal::method(d, name, &args, &kwargs),
        V::Str(s) => str_method(s, name, &args, &kwargs),
        V::Bytes(b) if name == "decode" => {
            let enc = args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "encoding").map(|(_, v)| v));
            if let Some(e) = enc {
                let e = ops::str_(e)?.to_ascii_lowercase().replace('_', "-");
                if !matches!(e.as_str(), "utf-8" | "utf8") || args.len() > 1 || kwargs.iter().any(|(k, _)| k != "encoding") {
                    return Err(Exc::type_error("py2axum: bytes.decode() supports UTF-8 with strict errors only"));
                }
            }
            match std::str::from_utf8(b) {
                Ok(s) => Ok(V::str(s)),
                Err(e) => Err(Exc::msg(
                    &UNICODE_DECODE_ERROR,
                    format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte", b[e.valid_up_to()], e.valid_up_to()),
                )),
            }
        }
        V::List(l) => list_method(cx, l, name, args, kwargs).await,
        V::Dict(d) => dict_method(d, name, &args, &kwargs),
        V::Set(s) => set_method(s, name, &args),
        V::Tuple(t) => match name {
            "index" => t.iter().position(|x| ops::eq_bool(x, &args[0])).map(|i| V::Int(i as i64)).ok_or_else(|| Exc::value_error("tuple.index(x): x not in tuple")),
            "count" => Ok(V::Int(t.iter().filter(|x| ops::eq_bool(x, &args[0])).count() as i64)),
            _ => Err(no_attr(recv, name)),
        },
        V::DateTime(d) => datetime_method(d, name, &args, &kwargs),
        V::Date(d) => date_method(d, name, &args, &kwargs),
        V::Delta(d) => match name {
            "total_seconds" => Ok(V::Float(dt::micros(d) as f64 / 1e6)),
            _ => Err(no_attr(recv, name)),
        },
        V::Obj(o) => match find_method(o.desc.methods, name) {
            Some((false, f)) => f(cx, recv.clone(), super::pack(args, kwargs)).await,
            _ => {
                let f = getattr(cx, recv, name).await?;
                call_value(cx, &f, args, kwargs).await
            }
        },
        V::Inst(i) => match name {
            "model_dump" | "model_dump_json" => {
                for (k, _) in &kwargs {
                    if !matches!(k.as_str(), "mode" | "exclude_none" | "exclude_unset" | "by_alias" | "exclude" | "include") || (name == "model_dump_json" && k == "mode") {
                        return Err(Exc::type_error(format!("{name}({k}=) is not supported by py2axum")));
                    }
                }
                let o = pyd::DumpOpts {
                    json: name == "model_dump_json" || kw(&kwargs, "mode").and_then(|m| m.as_str().map(|s| s == "json")).unwrap_or(false),
                    exclude_none: kw(&kwargs, "exclude_none").map(ops::truthy).transpose()?.unwrap_or(false),
                    exclude_unset: kw(&kwargs, "exclude_unset").map(ops::truthy).transpose()?.unwrap_or(false),
                    by_alias: kw(&kwargs, "by_alias").map(ops::truthy).transpose()?.unwrap_or(false),
                };
                let by_alias = o.by_alias;
                let mut d = pyd::dump(recv, o)?;
                // exclude= / include=: top-level field names (a set, list or tuple of names)
                for (opt, keep) in [("exclude", false), ("include", true)] {
                    let Some(sel) = kw(&kwargs, opt).filter(|v| !v.is_none()) else { continue };
                    if matches!(sel, V::Dict(_)) {
                        return Err(Exc::type_error(format!("py2axum: {name}({opt}=dict) is not supported (a set of field names only)")));
                    }
                    let names: Vec<String> = ops::iter(sel)?.iter().map(ops::str_).collect::<R<_>>()?;
                    let keys: Vec<&str> = i.desc.fields.iter().filter(|f| names.iter().any(|n| n == f.name))
                        .map(|f| if by_alias { f.alias.unwrap_or(f.name) } else { f.name })
                        .chain(i.desc.computed.iter().map(|(n, _)| *n).filter(|c| names.iter().any(|n| n == c)))
                        .collect();
                    if let V::Dict(m) = &d {
                        let kept: Vec<(V, V)> = m.lock().values().filter(|(k, _)| matches!(k, V::Str(s) if keys.contains(&&**s) == keep)).cloned().collect();
                        d = V::dict_from(kept)?;
                    }
                }
                if name == "model_dump_json" {
                    Ok(V::str(pyd::to_json(&d, &pyd::RESPONSE, false)?))
                } else {
                    Ok(d)
                }
            }
            "model_copy" => {
                if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "update" && k != "deep") {
                    return Err(Exc::type_error(format!("model_copy({k}=) is not supported by py2axum")));
                }
                let deep = kw(&kwargs, "deep").map(ops::truthy).transpose()?.unwrap_or(false);
                let vals = i.vals.lock().clone();
                let vals = if deep { vals.iter().map(deepcopy).collect::<R<Vec<_>>>()? } else { vals };
                let inst = pyd::Inst {
                    desc: i.desc,
                    vals: Mutex::new(vals),
                    set: Mutex::new(i.set.lock().clone()),
                    extra: Mutex::new(i.extra.lock().clone()),
                };
                // update= is applied as is: Pydantic does not validate it
                if let Some(V::Dict(upd)) = kw(&kwargs, "update") {
                    for (k, x) in upd.lock().values() {
                        let k = ops::str_(k)?;
                        let fi = i.desc.field_index(&k).ok_or_else(|| Exc::type_error(format!("py2axum: model_copy(update=) with a non-field key {k:?}")))?;
                        inst.vals.lock()[fi] = x.clone();
                        inst.set.lock()[fi] = true;
                    }
                }
                Ok(V::Inst(Arc::new(inst)))
            }
            _ => match find_method(i.desc.methods, name) {
                Some((false, f)) => f(cx, recv.clone(), super::pack(args, kwargs)).await,
                _ => {
                    let f = getattr(cx, recv, name).await?;
                    call_value(cx, &f, args, kwargs).await
                }
            },
        },
        V::Class(c) => match (&c.kind, name) {
            (ClassKind::Schema(s), "model_validate") => {
                if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "from_attributes") {
                    return Err(Exc::type_error(format!("py2axum: model_validate({k}=) is not supported")));
                }
                let obj = args.first().cloned().unwrap_or(V::None);
                let from_attrs = match kwargs.iter().find(|(k, _)| k == "from_attributes") {
                    Some((_, v)) if !v.is_none() => ops::truthy(v)?,
                    _ => s.from_attributes,
                };
                // an object is read attribute by attribute only with from_attributes (else model_type)
                let foreign = match &obj {
                    V::Obj(_) => true,
                    V::Inst(i) => !i.desc.class.is_subclass(s.class),
                    _ => false,
                };
                if foreign && !from_attrs {
                    return Err(Exc::validation(
                        &VALIDATION_ERROR,
                        vec![pyd::ErrDetail {
                            kind: "model_type",
                            loc: vec![],
                            msg: format!("Input should be a valid dictionary or instance of {}", s.name),
                            input: obj,
                            ctx: Some(vec![("class_name", V::str(s.name))]),
                        }],
                    ));
                }
                pyd::construct(cx, s, obj).await
            }
            (ClassKind::Schema(s), "model_validate_json") => {
                let obj = pyd::loads(&ops::str_(args.first().unwrap_or(&V::None))?)?;
                pyd::construct(cx, s, obj).await
            }
            (ClassKind::Model(m), _) if m.class_methods.contains(&name) => match find_method(m.methods, name) {
                Some((_, f)) => f(cx, recv.clone(), super::pack(args, kwargs)).await,
                None => unreachable!("class method {name} not in the method table"),
            },
            _ => Err(Exc::attr_error(format!("type object '{}' has no attribute '{}'", c.name, name))),
        },
        V::Session(s) => {
            // positional/keyword arguments beyond the supported ones are refused, never ignored
            let (max_pos, kws): (usize, &[&str]) = match name {
                "get" => (2, &["options"]),
                "refresh" => (2, &["attribute_names"]),
                "execute" => (2, &["params"]),
                "add" | "add_all" | "scalar" | "scalars" | "delete" => (1, &[]),
                "query" => (usize::MAX, &[]),
                _ => (0, &[]),
            };
            if args.len() > max_pos.max(if matches!(name, "commit" | "flush" | "rollback" | "close") { 0 } else { 1 }) {
                return Err(Exc::type_error(format!("py2axum: session.{name}() with {} positional arguments is not supported", args.len())));
            }
            if let Some((k, _)) = kwargs.iter().find(|(k, _)| !kws.contains(&k.as_str())) {
                return Err(Exc::type_error(format!("py2axum: session.{name}({k}=) is not supported")));
            }
            let kw = |k: &str| kwargs.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
            match name {
            "add" => {
                s.add(&args[0])?;
                Ok(V::None)
            }
            "add_all" => {
                for x in ops::iter(&args[0])? {
                    s.add(&x)?;
                }
                Ok(V::None)
            }
            "get" => match kw("options") {
                Some(o) => s.get_with(&args[0], &args[1], orm::load_chains(&o)?).await,
                None => s.get(&args[0], &args[1]).await,
            },
            "execute" => s.execute_params(&args[0], args.get(1).or_else(|| kwargs.first().map(|(_, v)| v))).await,
            "scalar" => s.scalar(&args[0]).await,
            "scalars" => s.scalars(&args[0]).await,
            "commit" => s.commit().await.map(|_| V::None),
            "flush" => s.flush().await.map(|_| V::None),
            "rollback" => s.rollback().await.map(|_| V::None),
            "refresh" => match args.get(1).cloned().or_else(|| kw("attribute_names")) {
                Some(names) if !names.is_none() => {
                    let names = ops::iter(&names)?.iter().map(ops::str_).collect::<R<Vec<_>>>()?;
                    s.refresh_attrs(&args[0], names).await.map(|_| V::None)
                }
                _ => s.refresh(&args[0]).await.map(|_| V::None),
            },
            "delete" => s.delete(&args[0]).await.map(|_| V::None),
            "begin_nested" => s.begin_nested().await,
            "query" => orm::query(s, args),
            "close" | "aclose" => Ok(V::None),
            // the engine the session is bound to (sync side for get_bind(), but the same process pool)
            "get_bind" => Ok(V::native(Native::Engine)),
            _ => Err(Exc::attr_error(format!("'AsyncSession' object has no attribute '{name}' (not supported by py2axum)"))),
            }
        }
        V::Result(r) => orm::result_method(r, name),
        V::Col(..) | V::Sql(_) => orm::sql_method(recv, name, args, kwargs),
        V::Native(n) if matches!(&**n, Native::Query(..)) => match &**n {
            Native::Query(sess, sel) => orm::query_method(sess, sel, name, args, kwargs).await,
            _ => unreachable!(),
        },
        V::Native(n) => match &**n {
            Native::Headers(r) => match name {
                "get" => {
                    let key = ops::str_(&args[0])?;
                    Ok(r.header(&key).map(V::str).unwrap_or_else(|| arg(&args, &kwargs, 1, "default").cloned().unwrap_or(V::None)))
                }
                "keys" => Ok(V::list(r.headers.iter().map(|(k, _)| V::str(k)).collect())),
                "items" => Ok(V::list(r.headers.iter().map(|(k, x)| V::tuple(vec![V::str(k), V::str(x)])).collect())),
                _ => Err(no_attr(recv, name)),
            },
            Native::Request(r) => match name {
                "is_disconnected" => Ok(V::Bool(r.disconnected.load(std::sync::atomic::Ordering::Relaxed))),
                "body" => Ok(V::Bytes(Arc::from(&r.body[..]))),
                "json" => pyd::loads(&String::from_utf8_lossy(&r.body)),
                _ => Err(no_attr(recv, name)),
            },
            Native::Logger(n) => web::log(n, name, &args),
            Native::Queue(q) => match name {
                "put_nowait" => q.put_nowait(args[0].clone()).map(|_| V::None),
                "put" => q.put(args[0].clone()).await.map(|_| V::None),
                "get" => q.get().await,
                "get_nowait" => q.get_nowait(),
                "qsize" => Ok(V::Int(q.items.lock().len() as i64)),
                "empty" => Ok(V::Bool(q.items.lock().is_empty())),
                "full" => Ok(V::Bool(q.maxsize > 0 && q.items.lock().len() >= q.maxsize)),
                _ => Err(no_attr(recv, name)),
            },
            Native::Hash(h) => libs::hash_method(h, name, &args),
            Native::Serializer(s) => super::itsd::method(s, name, &args, &kwargs),
            Native::Fernet(f) => super::fernet::method(f, name, &args, &kwargs),
            Native::RespObj(r) => super::resp::method(r, name, &args, &kwargs),
            Native::RespHeaders(r) => super::resp::headers_method(&r.headers, name, &args, &kwargs),
            Native::CellHeaders(c) => super::resp::headers_method(&c.headers, name, &args, &kwargs),
            Native::Totp(t) => super::auth::totp_method(t, name, &args, &kwargs),
            Native::Row(names, vals) if name == "_asdict" => V::dict_from(names.iter().zip(vals.iter()).map(|(k, x)| (V::str(k), x.clone())).collect()),
            Native::Row(_, vals) if name == "_tuple" => Ok(V::tuple(vals.to_vec())),
            Native::UrlParts(u) if name == "geturl" => Ok(V::str(super::stdlib::url_parts_geturl(u))),
            Native::Task(t) => web::task_method(cx, t, recv, name, &args).await,
            Native::Gen(g) => super::agen::method(g, recv, name, &args).await,
            Native::CtxVar(var) => super::agen::ctxvar_method(cx, var, name, &args),
            Native::McpServer(s) => super::mcp::server_method(s, name),
            Native::McpManager(s) => super::mcp::manager_method(cx, s, name, &args).await,
            Native::IniConfig(c) => super::ini::method(c, name, &args, &kwargs),
            Native::PydUrl(_, u) if name == "unicode_string" => Ok(V::str(u.as_str())),
            Native::Engine => match name {
                // a connection: its own transaction, rolled back when the `async with` ends
                "connect" => Ok(V::Session(orm::Session::new(cx.app.pool.clone(), false, true, false, Arc::downgrade(cx)))),
                "dispose" => Ok(V::None),
                _ => Err(no_attr(recv, name)),
            },
            Native::HttpClient(c) => super::http::client_method(cx, recv, c, name, args, kwargs).await,
            Native::HttpResp(r) => super::http::resp_method(r, recv, name, &args, &kwargs),
            Native::Savepoint(sess, sp) => match name {
                "commit" | "rollback" => sess.end_savepoint(sp, name == "commit").await.map(|_| V::None),
                _ => Err(no_attr(recv, name)),
            },
            Native::Pattern(p) => super::stdlib::pattern_method(cx, p, name, &args, &kwargs).await,
            Native::Match(m) => super::stdlib::match_method(m, name, &args),
            Native::StringIO(s) => super::stdlib::stringio_method(s, name, &args),
            Native::CsvWriter(w) => super::stdlib::writer_method(w, name, &args),
            Native::SnifferObj if name == "sniff" => super::stdlib::sniff(&args, &kwargs),
            Native::Hmac(h) => super::stdlib::hmac_method(h, name, &args),
            Native::Jinja(j) => super::mail::jinja_method(j, name, &args),
            Native::JinjaTpl(t) => super::mail::tpl_method(t, name, &args, &kwargs),
            Native::Mime(m) => super::mail::mime_method(m, name, &args, &kwargs),
            Native::Tasks(t) => super::resp::tasks_method(t, name, &args, &kwargs),
            Native::Response(r) if name == "set_cookie" || name == "delete_cookie" => {
                super::resp::set_cookie(&r.headers, &args, &kwargs, name == "delete_cookie")
            }
            Native::BytesIO(b) => super::files::bytesio_method(b, name, &args),
            Native::Type("dict") if name == "fromkeys" => libs::dict_fromkeys(&args),
            Native::Type("str") if name == "maketrans" => libs::maketrans(&args),
            Native::Path(p) => super::pathio::path_method(p, name, &args, &kwargs),
            Native::File(f) => super::pathio::file_method(f, name, &args),
            Native::Upload(u) => super::files::upload_method(u, name, &args),
            Native::Func(_) => Err(no_attr(recv, name)),
            _ => Err(no_attr(recv, name)),
        },
        _ => Err(no_attr(recv, name)),
    }
}

/// Calling a first-class function value (lambda, bound method, function passed as an argument).
/// `call_value` behind a declared `Send` future: for runtime code that a `call_value` can itself reach
/// (ASGI send/receive, MCP tools), whose auto traits would otherwise be computed in a cycle
pub fn call_value_boxed<'a>(cx: &'a Cx, f: &'a V, args: Vec<V>) -> super::BoxFut<'a> {
    Box::pin(call_value(cx, f, args, vec![]))
}

pub fn call_value_kw_boxed<'a>(cx: &'a Cx, f: &'a V, kwargs: Vec<(String, V)>) -> super::BoxFut<'a> {
    Box::pin(call_value(cx, f, vec![], kwargs))
}

pub async fn call_value(cx: &Cx, f: &V, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if let V::Native(n) = f {
        if let Native::Bound(m, recv) = &**n {
            return m(cx, recv.clone(), super::pack(args, kwargs)).await;
        }
        if let Native::CallNext(next) = &**n {
            return super::asgi::call_next(cx, next).await;
        }
        if let Native::PyFn(pf) = &**n {
            return (pf.call)(cx, args, kwargs).await;
        }
        if let Native::Prom(p) = &**n {
            if !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: keyword arguments to a prometheus_client decorator are not supported"));
            }
            return super::prom::decorate(p, &args);
        }
        if let Native::Tenacity(t) = &**n {
            if !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: keyword arguments to a tenacity decorator are not supported"));
            }
            return super::tenacity::decorate(t, &args);
        }
        // `async_sessionmaker(...)()`: a new session on the process pool
        if let Native::Maker(expire, autoflush, sync) = &**n {
            if !args.is_empty() || !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: calling a sessionmaker with arguments is not supported"));
            }
            return Ok(V::Session(orm::Session::new(cx.app.pool.clone(), *expire, *autoflush, *sync, Arc::downgrade(cx))));
        }
        // a builtin type used as a function value (`map(str, xs)`, `defaultdict(list)`)
        if let Native::Type(t) = &**n {
            if *t != "dict" && !kwargs.is_empty() {
                return Err(Exc::type_error(format!("{t}() takes no keyword arguments")));
            }
            return match *t {
                "str" => b_str(&args),
                "int" => b_int(&args),
                "float" => b_float(&args),
                "bool" => b_bool(&args),
                "list" => b_list(&args),
                "tuple" => b_tuple(&args),
                "set" | "frozenset" => b_set(&args),
                "dict" => b_dict(&args, &kwargs),
                other => Err(Exc::type_error(format!("py2axum: calling the type {other} as a value is not supported"))),
            };
        }
        if let Native::MethodOf(recv, name) = &**n {
            return Box::pin(call_method(cx, recv, name, args, kwargs)).await;
        }
        if let Native::McpToolDeco(s, spec) = &**n {
            return super::mcp::decorate(s, spec, &args);
        }
        // raw ASGI: `await receive()`, `await send(message)`, `await response(scope, receive, send)`
        if let Native::AsgiReceive(c) = &**n {
            let c = c.clone();
            return Ok(super::aio::coro(Box::pin(async move { super::rawasgi::receive(&c).await })));
        }
        if let Native::AsgiSend(c) = &**n {
            let (c, msg) = (c.clone(), args.into_iter().next().unwrap_or(V::None));
            return Ok(super::aio::coro(Box::pin(async move { super::rawasgi::send(&c, &msg).await })));
        }
        if let Native::RespObj(_) = &**n {
            let (cx2, resp) = (cx.clone(), f.clone());
            return Ok(super::aio::coro(Box::pin(async move { super::rawasgi::send_response(&cx2, &resp, &args).await })));
        }
    }
    // an instance of a project class with `__call__`
    let call_dunder = match f {
        V::Inst(i) => find_method(i.desc.methods, "__call__"),
        V::Obj(o) => find_method(o.desc.methods, "__call__"),
        _ => None,
    };
    if let Some((_, m)) = call_dunder {
        return m(cx, f.clone(), super::pack(args, kwargs)).await;
    }
    // a builtin exception class held as a value: `(ValueError if c else TypeError)(msg)`
    if let V::Class(c) = f {
        if matches!(c.kind, ClassKind::Exception) {
            if !kwargs.is_empty() {
                return Err(Exc::type_error(format!("{}() takes no keyword arguments", c.name)));
            }
            return Ok(V::Exc(Exc::new(c, args)));
        }
        // a mapped class held as a value (`cls(...)` in a classmethod): SQLAlchemy's keyword constructor
        if let ClassKind::Model(m) = c.kind {
            if !args.is_empty() {
                return Err(Exc::type_error(format!("_declarative_constructor() takes 1 positional argument but {} were given", args.len() + 1)));
            }
            return orm::construct(m, kwargs);
        }
        // a project class held as a value (`cls(**data)`, `type(m)(a=1)`): what calling it by name does
        if let ClassKind::Schema(s) = c.kind {
            return Box::pin(schema_new(cx, s, args, kwargs)).await;
        }
        if let ClassKind::Enum(e) = c.kind {
            if args.len() != 1 || !kwargs.is_empty() {
                return Err(Exc::type_error(format!("py2axum: {}() held as a value takes exactly one value", c.name)));
            }
            return enum_call(cx, e, &args[0]).await;
        }
    }
    if !kwargs.is_empty() {
        return Err(Exc::type_error("keyword arguments to a function value are not supported by py2axum"));
    }
    match f {
        V::Native(n) => match &**n {
            Native::Func(fv) => fv(cx, args).await,
            _ => Err(Exc::type_error(format!("'{}' object is not callable", f.type_name()))),
        },
        _ => Err(Exc::type_error(format!("'{}' object is not callable", f.type_name()))),
    }
}

/// Calling a Pydantic model, settings, dataclass or plain class known only at run time: the same
/// paths as a call by name (`dyn.py` `construct`), with the errors CPython raises.
async fn schema_new(cx: &Cx, s: &'static pyd::SchemaDesc, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if std::ptr::eq(s, &super::libs::NAMESPACE) {
        return super::libs::namespace(&args, &kwargs);
    }
    if s.dataclass {
        return pyd::dataclass_new(cx, s, args, kwargs).await;
    }
    if s.open {
        let o = pyd::object_new(s);
        return match s.init {
            Some(init) => init(cx, o.clone(), super::pack(args, kwargs)).await.map(|_| o),
            None if args.is_empty() && kwargs.is_empty() => Ok(o),
            None => Err(Exc::type_error(format!("{}() takes no arguments", s.name))),
        };
    }
    if let Some(st) = s.settings {
        // positional arguments and `_env_prefix=`-style keywords are BaseSettings init options
        if !args.is_empty() || kwargs.iter().any(|(k, _)| k.starts_with('_')) {
            return Err(Exc::type_error(format!(
                "py2axum: {}(): pydantic-settings init options are not supported (set them in model_config)", s.name)));
        }
        return super::settings(cx, s, st.prefix, st.case_sensitive, st.none_str, kwargs).await;
    }
    if !args.is_empty() {
        return Err(Exc::type_error(format!("BaseModel.__init__() takes 1 positional argument but {} were given", args.len() + 1)));
    }
    if let Some(init) = s.init {
        // the model's own `__init__(self, **data)`
        let o = pyd::object_new_blank(s);
        return init(cx, o.clone(), super::pack(vec![], kwargs)).await.map(|_| o);
    }
    pyd::construct(cx, s, super::kwargs_dict(kwargs)?).await
}

// ---------------------------------------------------------------- str

fn strs(v: &V) -> R<String> {
    match v {
        V::Str(s) => Ok(s.to_string()),
        other => Err(Exc::type_error(format!("must be str, not {}", other.type_name()))),
    }
}

fn py_split_ws(s: &str, maxsplit: i64) -> Vec<V> {
    let mut out = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        if maxsplit >= 0 && out.len() as i64 == maxsplit {
            out.push(V::str(rest));
            return out;
        }
        match rest.find(char::is_whitespace) {
            Some(i) => {
                out.push(V::str(&rest[..i]));
                rest = rest[i..].trim_start();
            }
            None => {
                out.push(V::str(rest));
                break;
            }
        }
    }
    out
}

fn str_method(s: &Arc<str>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let a0 = || arg(args, kwargs, 0, "");
    Ok(match name {
        "translate" => {
            // table: {ord: str | ord | None} (str.maketrans)
            let Some(V::Dict(t)) = a0() else { return Err(Exc::type_error("py2axum: str.translate() takes a dict table")) };
            let t = t.lock();
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match t.get(&Key::Int(c as i64)) {
                    None => out.push(c),
                    Some((_, V::None)) => {}
                    Some((_, V::Str(r))) => out.push_str(r),
                    Some((_, V::Int(o))) => out.push(char::from_u32(*o as u32).ok_or_else(|| Exc::value_error("character mapping must be in range(0x110000)"))?),
                    Some(_) => return Err(Exc::type_error("character mapping must return integer, None or str")),
                }
            }
            V::str(out)
        }
        "lower" => V::str(s.to_lowercase()),
        "upper" => V::str(s.to_uppercase()),
        "strip" | "lstrip" | "rstrip" => {
            let chars: Option<Vec<char>> = match a0() {
                Some(V::Str(c)) => Some(c.chars().collect()),
                _ => None,
            };
            let pred = |c: char| match &chars {
                Some(cs) => cs.contains(&c),
                None => c.is_whitespace(),
            };
            V::str(match name {
                "strip" => s.trim_matches(pred),
                "lstrip" => s.trim_start_matches(pred),
                _ => s.trim_end_matches(pred),
            })
        }
        "split" | "rsplit" => {
            let sep = arg(args, kwargs, 0, "sep").cloned().unwrap_or(V::None);
            let maxsplit = match arg(args, kwargs, 1, "maxsplit") {
                Some(V::Int(i)) => *i,
                _ => -1,
            };
            match sep {
                V::None => {
                    if name == "rsplit" && maxsplit >= 0 {
                        let mut parts = py_split_ws(&s.chars().rev().collect::<String>(), maxsplit);
                        parts.reverse();
                        V::list(parts.into_iter().map(|p| V::str(p.as_str().unwrap().chars().rev().collect::<String>())).collect())
                    } else {
                        V::list(py_split_ws(s, maxsplit))
                    }
                }
                V::Str(sep) => {
                    if sep.is_empty() {
                        return Err(Exc::value_error("empty separator"));
                    }
                    let parts: Vec<V> = if maxsplit < 0 {
                        s.split(&*sep).map(V::str).collect()
                    } else if name == "split" {
                        s.splitn(maxsplit as usize + 1, &*sep).map(V::str).collect()
                    } else {
                        let mut p: Vec<V> = s.rsplitn(maxsplit as usize + 1, &*sep).map(V::str).collect();
                        p.reverse();
                        p
                    };
                    V::list(parts)
                }
                _ => return Err(Exc::type_error("must be str or None")),
            }
        }
        "splitlines" => V::list(s.lines().map(V::str).collect()),
        "join" => {
            let items = ops::iter(a0().ok_or_else(|| Exc::type_error("join() takes one argument"))?)?;
            let parts = items
                .iter()
                .enumerate()
                .map(|(i, x)| match x {
                    V::Str(t) => Ok(t.to_string()),
                    other => Err(Exc::type_error(format!("sequence item {i}: expected str instance, {} found", other.type_name()))),
                })
                .collect::<R<Vec<_>>>()?;
            V::str(parts.join(s))
        }
        "replace" => {
            let (old, new) = (strs(&args[0])?, strs(&args[1])?);
            match args.get(2) {
                Some(V::Int(n)) if *n >= 0 => V::str(s.replacen(&old, &new, *n as usize)),
                _ => V::str(s.replace(&old, &new)),
            }
        }
        "startswith" | "endswith" => {
            let check = |p: &str| if name == "startswith" { s.starts_with(p) } else { s.ends_with(p) };
            match &args[0] {
                V::Str(p) => V::Bool(check(p)),
                V::Tuple(t) => V::Bool(t.iter().any(|p| p.as_str().map(check).unwrap_or(false))),
                _ => return Err(Exc::type_error("startswith first arg must be str or a tuple of str")),
            }
        }
        "find" | "index" | "rfind" | "rindex" => {
            let sub = strs(&args[0])?;
            let pos = if name.starts_with('r') { s.rfind(&sub) } else { s.find(&sub) };
            match pos {
                Some(b) => V::Int(s[..b].chars().count() as i64),
                None if name.ends_with("find") => V::Int(-1),
                None => return Err(Exc::value_error("substring not found")),
            }
        }
        "count" => V::Int(s.matches(&*strs(&args[0])?).count() as i64),
        "encode" => V::Bytes(Arc::from(s.as_bytes())),
        "title" => {
            let mut out = String::new();
            let mut prev_alpha = false;
            for c in s.chars() {
                if c.is_alphabetic() {
                    if prev_alpha { out.extend(c.to_lowercase()) } else { out.extend(c.to_uppercase()) }
                    prev_alpha = true;
                } else {
                    out.push(c);
                    prev_alpha = false;
                }
            }
            V::str(out)
        }
        "capitalize" => {
            let mut cs = s.chars();
            V::str(match cs.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &cs.as_str().to_lowercase(),
                None => String::new(),
            })
        }
        "isdigit" | "isnumeric" | "isdecimal" => V::Bool(!s.is_empty() && s.chars().all(|c| c.is_numeric())),
        "isalpha" => V::Bool(!s.is_empty() && s.chars().all(char::is_alphabetic)),
        "isalnum" => V::Bool(!s.is_empty() && s.chars().all(char::is_alphanumeric)),
        "isspace" => V::Bool(!s.is_empty() && s.chars().all(char::is_whitespace)),
        "islower" => V::Bool(s.chars().any(char::is_alphabetic) && !s.chars().any(char::is_uppercase)),
        "isupper" => V::Bool(s.chars().any(char::is_alphabetic) && !s.chars().any(char::is_lowercase)),
        "zfill" => {
            let w = match &args[0] {
                V::Int(i) => *i as usize,
                _ => 0,
            };
            let n = s.chars().count();
            if n >= w {
                V::Str(s.clone())
            } else if let Some(rest) = s.strip_prefix(['-', '+']) {
                V::str(format!("{}{}{}", &s[..1], "0".repeat(w - n), rest))
            } else {
                V::str(format!("{}{}", "0".repeat(w - n), s))
            }
        }
        "removeprefix" => V::str(s.strip_prefix(&*strs(&args[0])?).unwrap_or(s)),
        "removesuffix" => V::str(s.strip_suffix(&*strs(&args[0])?).unwrap_or(s)),
        "partition" | "rpartition" => {
            let sep = strs(&args[0])?;
            let pos = if name == "partition" { s.find(&sep) } else { s.rfind(&sep) };
            match pos {
                Some(p) => V::tuple(vec![V::str(&s[..p]), V::str(&sep), V::str(&s[p + sep.len()..])]),
                None if name == "partition" => V::tuple(vec![V::Str(s.clone()), V::str(""), V::str("")]),
                None => V::tuple(vec![V::str(""), V::str(""), V::Str(s.clone())]),
            }
        }
        "ljust" | "rjust" | "center" => {
            let w = match &args[0] {
                V::Int(i) => *i as usize,
                _ => 0,
            };
            let fill = args.get(1).and_then(|f| f.as_str()).and_then(|f| f.chars().next()).unwrap_or(' ');
            let n = s.chars().count();
            if n >= w {
                V::Str(s.clone())
            } else {
                let pad = w - n;
                let rep = |k: usize| std::iter::repeat_n(fill, k).collect::<String>();
                V::str(match name {
                    "ljust" => format!("{s}{}", rep(pad)),
                    "rjust" => format!("{}{s}", rep(pad)),
                    _ => format!("{}{s}{}", rep(pad / 2 + (pad % 2 & n % 2)), rep(pad - (pad / 2 + (pad % 2 & n % 2)))),
                })
            }
        }
        "format" => {
            let pos = |i: usize| args.get(i).cloned().ok_or_else(|| Exc::msg(&INDEX_ERROR, format!("Replacement index {i} out of range for positional args tuple")));
            V::str(format_with(s, &pos, &|n: &str| kw(kwargs, n).cloned().ok_or_else(|| Exc::new(&KEY_ERROR, vec![V::str(n)])), false)?)
        }
        _ => return Err(Exc::attr_error(format!("'str' object has no attribute '{name}'"))),
    })
}

// ---------------------------------------------------------------- list / dict / set

async fn sort_key(cx: &Cx, items: Vec<V>, key: Option<&V>, reverse: bool) -> R<Vec<V>> {
    let keys = match key {
        None | Some(V::None) => items.clone(),
        Some(f) => {
            let mut ks = Vec::with_capacity(items.len());
            for it in &items {
                ks.push(call_value(cx, f, vec![it.clone()], vec![]).await?);
            }
            ks
        }
    };
    let mut idx: Vec<usize> = (0..items.len()).collect();
    let mut err = None;
    idx.sort_by(|a, b| match ops::cmp(&keys[*a], &keys[*b]) {
        Ok(o) => {
            if reverse { o.reverse() } else { o }
        }
        Err(e) => {
            err = Some(e);
            std::cmp::Ordering::Equal
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    Ok(idx.into_iter().map(|i| items[i].clone()).collect())
}

async fn list_method(cx: &Cx, l: &Arc<Mutex<Vec<V>>>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    match name {
        "append" => {
            l.lock().push(args[0].clone());
            Ok(V::None)
        }
        "extend" => {
            let items = ops::iter(&args[0])?;
            l.lock().extend(items);
            Ok(V::None)
        }
        "insert" => {
            let i = match &args[0] {
                V::Int(i) => *i,
                _ => 0,
            };
            let mut g = l.lock();
            let len = g.len() as i64;
            let j = if i < 0 { (i + len).max(0) } else { i.min(len) } as usize;
            g.insert(j, args[1].clone());
            Ok(V::None)
        }
        "pop" => {
            let mut g = l.lock();
            if g.is_empty() {
                return Err(Exc::msg(&INDEX_ERROR, "pop from empty list"));
            }
            let len = g.len() as i64;
            let i = match args.first() {
                Some(V::Int(i)) => if *i < 0 { i + len } else { *i },
                _ => len - 1,
            };
            if i < 0 || i >= len {
                return Err(Exc::msg(&INDEX_ERROR, "pop index out of range"));
            }
            Ok(g.remove(i as usize))
        }
        "remove" => {
            // compared outside the lock: a project __eq__ may read the list
            let snapshot = l.lock().clone();
            match snapshot.iter().position(|x| ops::eq_bool(x, &args[0])) {
                Some(i) => {
                    l.lock().remove(i);
                    Ok(V::None)
                }
                None => Err(Exc::value_error("list.remove(x): x not in list")),
            }
        }
        "index" => match l.lock().clone().iter().position(|x| ops::eq_bool(x, &args[0])) {
            Some(i) => Ok(V::Int(i as i64)),
            // CPython 3.14 stopped printing the value
            None if super::python() >= (3, 14) => Err(Exc::value_error("list.index(x): x not in list")),
            None => Err(Exc::value_error(format!("{} is not in list", ops::repr(&args[0])?))),
        },
        "count" => Ok(V::Int(l.lock().clone().iter().filter(|x| ops::eq_bool(x, &args[0])).count() as i64)),
        "reverse" => {
            l.lock().reverse();
            Ok(V::None)
        }
        "copy" => Ok(V::list(l.lock().clone())),
        "clear" => {
            l.lock().clear();
            Ok(V::None)
        }
        "sort" => {
            let items = l.lock().clone();
            let reverse = kw(&kwargs, "reverse").map(ops::truthy).transpose()?.unwrap_or(false);
            let sorted = sort_key(cx, items, kw(&kwargs, "key"), reverse).await?;
            *l.lock() = sorted;
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'list' object has no attribute '{name}'"))),
    }
}

fn dict_method(d: &Arc<Mutex<IndexMap<Key, (V, V)>>>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "get" => {
            let k = Key::of(&args[0])?;
            Ok(d.lock().get(&k).map(|(_, v)| v.clone()).unwrap_or_else(|| args.get(1).cloned().unwrap_or(V::None)))
        }
        "items" => Ok(V::list(d.lock().values().map(|(k, v)| V::tuple(vec![k.clone(), v.clone()])).collect())),
        "keys" => Ok(V::list(d.lock().values().map(|(k, _)| k.clone()).collect())),
        "values" => Ok(V::list(d.lock().values().map(|(_, v)| v.clone()).collect())),
        "pop" => {
            let k = Key::of(&args[0])?;
            match d.lock().shift_remove(&k) {
                Some((_, v)) => Ok(v),
                None => args.get(1).cloned().ok_or_else(|| Exc::new(&KEY_ERROR, vec![args[0].clone()])),
            }
        }
        "setdefault" => {
            let k = Key::of(&args[0])?;
            let mut g = d.lock();
            Ok(g.entry(k).or_insert_with(|| (args[0].clone(), args.get(1).cloned().unwrap_or(V::None))).1.clone())
        }
        "update" => {
            let mut items: Vec<(V, V)> = Vec::new();
            if let Some(src) = args.first() {
                match src {
                    V::Dict(o) => items.extend(o.lock().values().cloned()),
                    other => {
                        for pair in ops::iter(other)? {
                            let kv = ops::unpack(&pair, 2)?;
                            items.push((kv[0].clone(), kv[1].clone()));
                        }
                    }
                }
            }
            for (k, v) in kwargs {
                items.push((V::str(k), v.clone()));
            }
            let mut g = d.lock();
            for (k, v) in items {
                g.insert(Key::of(&k)?, (k, v));
            }
            Ok(V::None)
        }
        "copy" => Ok(V::Dict(Arc::new(Mutex::new(d.lock().clone())))),
        "clear" => {
            d.lock().clear();
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'dict' object has no attribute '{name}'"))),
    }
}

fn set_method(s: &Arc<Mutex<IndexMap<Key, V>>>, name: &str, args: &[V]) -> R {
    match name {
        "add" => {
            s.lock().insert(Key::of(&args[0])?, args[0].clone());
            Ok(V::None)
        }
        "discard" => {
            s.lock().shift_remove(&Key::of(&args[0])?);
            Ok(V::None)
        }
        "remove" => match s.lock().shift_remove(&Key::of(&args[0])?) {
            Some(_) => Ok(V::None),
            None => Err(Exc::new(&KEY_ERROR, vec![args[0].clone()])),
        },
        "update" => {
            for x in ops::iter(&args[0])? {
                s.lock().insert(Key::of(&x)?, x);
            }
            Ok(V::None)
        }
        "copy" => Ok(V::Set(Arc::new(Mutex::new(s.lock().clone())))),
        "clear" => {
            s.lock().clear();
            Ok(V::None)
        }
        // the methods take any iterables (the operators only sets)
        "union" | "intersection" | "difference" | "symmetric_difference" => {
            let mut acc = V::Set(Arc::new(Mutex::new(s.lock().clone())));
            for other in args {
                let o = set_of(ops::iter(other)?)?;
                acc = match name {
                    "union" => ops::bitor(&acc, &o)?,
                    "intersection" => ops::bitand(&acc, &o)?,
                    "difference" => ops::sub(&acc, &o)?,
                    _ => {
                        let (V::Set(x), V::Set(y)) = (&acc, &o) else { unreachable!() };
                        let (x, y) = (x.lock().clone(), y.lock().clone());
                        let mut m = x.clone();
                        m.retain(|k, _| !y.contains_key(k));
                        for (k, v) in y.iter() {
                            if !x.contains_key(k) {
                                m.insert(k.clone(), v.clone());
                            }
                        }
                        V::Set(Arc::new(Mutex::new(m)))
                    }
                };
            }
            Ok(acc)
        }
        "difference_update" | "intersection_update" => {
            let mut keep = s.lock().clone();
            for other in args {
                let o = ops::iter(other)?.iter().map(Key::of).collect::<R<std::collections::HashSet<Key>>>()?;
                keep.retain(|k, _| o.contains(k) == (name == "intersection_update"));
            }
            *s.lock() = keep;
            Ok(V::None)
        }
        "issubset" | "issuperset" | "isdisjoint" => {
            let o = ops::iter(&args[0])?.iter().map(Key::of).collect::<R<std::collections::HashSet<Key>>>()?;
            let mine: Vec<Key> = s.lock().keys().cloned().collect();
            Ok(V::Bool(match name {
                "issubset" => mine.iter().all(|k| o.contains(k)),
                "issuperset" => o.iter().all(|k| mine.contains(k)),
                _ => !mine.iter().any(|k| o.contains(k)),
            }))
        }
        _ => Err(Exc::attr_error(format!("'set' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- datetime methods

fn datetime_method(d: &DateTime, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    Ok(match name {
        "isoformat" => {
            let sep = arg(args, kwargs, 0, "sep").and_then(|v| v.as_str().and_then(|s| s.chars().next())).unwrap_or('T');
            let ts = arg(args, kwargs, 1, "timespec").and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_else(|| "auto".into());
            V::str(d.isoformat(sep, &ts))
        }
        "date" => V::Date(d.wall.date()),
        "time" => V::Time(d.wall.time()),
        "timestamp" => {
            let utc = match d.tz {
                Some(_) => d.utc(),
                None => {
                    let local = chrono::Local.from_local_datetime(&d.wall).single().map(|x| x.naive_utc()).unwrap_or(d.wall);
                    local
                }
            };
            V::Float(utc.and_utc().timestamp_micros() as f64 / 1e6)
        }
        "astimezone" => {
            let tz = match arg(args, kwargs, 0, "tz") {
                Some(v) => libs::tz_of(v)?.unwrap_or(Tz::Utc),
                None => Tz::Utc,
            };
            V::DateTime(d.astimezone(tz))
        }
        "replace" => {
            let mut w = d.wall;
            let mut tz = d.tz;
            for (k, v) in kwargs {
                let i = || -> R<u32> { match v { V::Int(i) => Ok(*i as u32), _ => Err(Exc::type_error("an integer is required")) } };
                w = match k.as_str() {
                    "year" => w.with_year(i()? as i32),
                    "month" => w.with_month(i()?),
                    "day" => w.with_day(i()?),
                    "hour" => w.with_hour(i()?),
                    "minute" => w.with_minute(i()?),
                    "second" => w.with_second(i()?),
                    "microsecond" => w.with_nanosecond(i()? * 1000),
                    "tzinfo" => {
                        tz = libs::tz_of(v)?;
                        Some(w)
                    }
                    _ => return Err(Exc::type_error(format!("'{k}' is an invalid keyword argument for replace()"))),
                }
                .ok_or_else(|| Exc::value_error("replace(): value out of range"))?;
            }
            V::DateTime(DateTime { wall: w, tz, fold: 0 })
        }
        "utcoffset" => d.offset().map(|o| V::Delta(Duration::seconds(o as i64))).unwrap_or(V::None),
        "weekday" => V::Int(d.wall.weekday().num_days_from_monday() as i64),
        "isoweekday" => V::Int(d.wall.weekday().number_from_monday() as i64),
        "toordinal" => V::Int(d.wall.date().num_days_from_ce() as i64),
        "strftime" => V::str(libs::strftime(&d.wall, d.offset(), d.tz.map(|t| t.name()), &strs(&args[0])?)),
        _ => return Err(Exc::attr_error(format!("'datetime.datetime' object has no attribute '{name}'"))),
    })
}

use chrono::TimeZone;

fn date_method(d: &chrono::NaiveDate, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    Ok(match name {
        "isoformat" => V::str(dt::date_iso(d)),
        "weekday" => V::Int(d.weekday().num_days_from_monday() as i64),
        "isoweekday" => V::Int(d.weekday().number_from_monday() as i64),
        "toordinal" => V::Int(d.num_days_from_ce() as i64),
        "strftime" => V::str(libs::strftime(&d.and_hms_opt(0, 0, 0).unwrap(), None, None, &strs(&args[0])?)),
        "replace" => {
            let mut x = *d;
            for (k, v) in kwargs {
                let i = match v {
                    V::Int(i) => *i,
                    _ => return Err(Exc::type_error("an integer is required")),
                };
                x = match k.as_str() {
                    "year" => x.with_year(i as i32),
                    "month" => x.with_month(i as u32),
                    "day" => x.with_day(i as u32),
                    _ => None,
                }
                .ok_or_else(|| Exc::value_error("replace(): value out of range"))?;
            }
            V::Date(x)
        }
        _ => return Err(Exc::attr_error(format!("'datetime.date' object has no attribute '{name}'"))),
    })
}

// ---------------------------------------------------------------- builtins

pub fn b_len(v: &V) -> R {
    Ok(V::Int(ops::len(v)? as i64))
}

pub fn b_str(args: &[V]) -> R {
    match args.first() {
        None => Ok(V::str("")),
        Some(V::Bytes(b)) if args.len() > 1 => Ok(V::str(String::from_utf8_lossy(b))),
        Some(v) => Ok(V::str(ops::str_(v)?)),
    }
}

/// `callable(x)`: functions, bound methods, classes, builtin types, instances with `__call__`.
pub fn b_callable(args: &[V]) -> R {
    let [v] = args else { return Err(Exc::type_error(format!("callable() takes exactly one argument ({} given)", args.len()))) };
    Ok(V::Bool(match v {
        V::Class(_) => true,
        V::Native(n) => matches!(&**n, Native::Bound(..) | Native::PyFn(_) | Native::Type(_) | Native::Maker(..) | Native::CallNext(_)),
        V::Inst(i) => find_method(i.desc.methods, "__call__").is_some(),
        V::Obj(o) => find_method(o.desc.methods, "__call__").is_some(),
        _ => false,
    }))
}

/// `str.format` / `string.Formatter().vformat`: `{}`/`{0}`/`{name}`, `!r`/`!s` conversions, format specs;
/// `positional`/`named` look a field up. `formatter`: `string.Formatter`'s messages (it is written in Python).
/// Attribute/index fields (`{a.b}`, `{a[0]}`) and nested specs are refused.
pub fn format_with(s: &str, positional: &dyn Fn(usize) -> R, named: &dyn Fn(&str) -> R, formatter: bool) -> R<String> {
    let mut out = String::new();
    let (mut auto, mut manual) = (0usize, false);
    let mut auto_used = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '}' {
            if it.peek() == Some(&'}') {
                it.next();
                out.push('}');
                continue;
            }
            return Err(Exc::value_error("Single '}' encountered in format string"));
        }
        if c != '{' {
            out.push(c);
            continue;
        }
        if it.peek() == Some(&'{') {
            it.next();
            out.push('{');
            continue;
        }
        let mut field = String::new();
        let mut closed = false;
        for d in it.by_ref() {
            if d == '}' {
                closed = true;
                break;
            }
            if d == '{' {
                return Err(Exc::type_error("py2axum: nested replacement fields in a format spec are not supported"));
            }
            field.push(d);
        }
        if !closed {
            return Err(Exc::value_error("expected '}' before end of string"));
        }
        let (head, spec) = field.split_once(':').map(|(a, b)| (a, b)).unwrap_or((field.as_str(), ""));
        let (fname, conv) = match head.split_once('!') {
            Some((f, c)) => (f, Some(c)),
            None => (head, None),
        };
        if fname.contains('.') || fname.contains('[') {
            return Err(Exc::type_error(format!("py2axum: format field '{fname}' (attribute or index access) is not supported")));
        }
        let v = if fname.is_empty() {
            if manual {
                return Err(Exc::value_error("cannot switch from manual field specification to automatic field numbering"));
            }
            auto_used = true;
            auto += 1;
            positional(auto - 1)?
        } else if let Ok(i) = fname.parse::<usize>() {
            if auto_used {
                return Err(Exc::value_error(if formatter {
                    "cannot switch from manual field specification to automatic field numbering"
                } else {
                    "cannot switch from automatic field numbering to manual field specification"
                }));
            }
            manual = true;
            positional(i)?
        } else {
            named(fname)?
        };
        let v = match conv {
            None => v,
            Some("r") => V::str(ops::repr(&v)?),
            Some("s") => V::str(ops::str_(&v)?),
            Some(c) if c.chars().count() == 1 && c != "a" => return Err(Exc::value_error(format!("Unknown conversion specifier {c}"))),
            Some("a") => return Err(Exc::type_error("py2axum: the !a conversion is not supported")),
            Some(_) => return Err(Exc::value_error("expected ':' after conversion specifier")),
        };
        out += &ops::format_spec(&v, spec)?;
    }
    Ok(out)
}

/// `string.Formatter().vformat(format_string, args, kwargs)`: named fields read with `kwargs[name]`
/// (a defaultdict fills them).
pub fn vformat(fmt: &V, args: &V, mapping: &V) -> R {
    let V::Str(s) = fmt else { return Err(Exc::type_error(format!("expected str, got {}", fmt.type_name()))) };
    Ok(V::str(format_with(s, &|i: usize| ops::getitem(args, &V::Int(i as i64)), &|n: &str| ops::getitem(mapping, &V::str(n)), true)?))
}

pub fn b_repr(v: &V) -> R {
    Ok(V::str(ops::repr(v)?))
}

pub fn b_int(args: &[V]) -> R {
    match args.first() {
        None => Ok(V::Int(0)),
        Some(V::Decimal(d)) => num_traits::ToPrimitive::to_i64(&d.to_int()).map(V::Int).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer beyond 64 bits")),
        Some(V::Int(i)) => Ok(V::Int(*i)),
        Some(V::Bool(b)) => Ok(V::Int(*b as i64)),
        Some(V::Float(f)) => {
            if !f.is_finite() {
                return Err(Exc::value_error("cannot convert float NaN or infinity to integer"));
            }
            Ok(V::Int(f.trunc() as i64))
        }
        Some(V::Str(s)) => {
            let base = match args.get(1) {
                Some(V::Int(b)) => *b as u32,
                _ => 10,
            };
            let t = s.trim().replace('_', "");
            i64::from_str_radix(&t, base).map(V::Int).map_err(|_| Exc::value_error(format!("invalid literal for int() with base {base}: {}", ops::str_repr(s))))
        }
        Some(other) => Err(Exc::type_error(format!("int() argument must be a string, a bytes-like object or a real number, not '{}'", other.type_name()))),
    }
}

pub fn b_float(args: &[V]) -> R {
    match args.first() {
        None => Ok(V::Float(0.0)),
        Some(V::Int(i)) => Ok(V::Float(*i as f64)),
        Some(V::Bool(b)) => Ok(V::Float(*b as i64 as f64)),
        Some(V::Float(f)) => Ok(V::Float(*f)),
        Some(V::Decimal(d)) => Ok(V::Float(d.to_f64())),
        Some(V::Str(s)) => s.trim().parse::<f64>().map(V::Float).map_err(|_| Exc::value_error(format!("could not convert string to float: {}", ops::str_repr(s)))),
        Some(other) => Err(Exc::type_error(format!("float() argument must be a string or a real number, not '{}'", other.type_name()))),
    }
}

pub fn b_bool(args: &[V]) -> R {
    Ok(V::Bool(match args.first() {
        None => false,
        Some(v) => ops::truthy(v)?,
    }))
}

pub fn b_list(args: &[V]) -> R {
    Ok(V::list(match args.first() {
        None => vec![],
        Some(v) => ops::iter(v)?,
    }))
}

pub fn b_tuple(args: &[V]) -> R {
    Ok(V::tuple(match args.first() {
        None => vec![],
        Some(v) => ops::iter(v)?,
    }))
}

pub fn b_set(args: &[V]) -> R {
    set_of(match args.first() {
        None => vec![],
        Some(v) => ops::iter(v)?,
    })
}

pub fn b_dict(args: &[V], kwargs: &[(String, V)]) -> R {
    let mut items = Vec::new();
    if let Some(src) = args.first() {
        match src {
            V::Dict(d) => items.extend(d.lock().values().cloned()),
            other => {
                for pair in ops::iter(other)? {
                    let kv = ops::unpack(&pair, 2)?;
                    items.push((kv[0].clone(), kv[1].clone()));
                }
            }
        }
    }
    for (k, v) in kwargs {
        items.push((V::str(k), v.clone()));
    }
    V::dict_from(items)
}

/// `id(x)`: the address of a heap object (stable for its lifetime), a value-derived number otherwise
pub fn b_id(args: &[V]) -> R {
    let [v] = args else {
        return Err(Exc::type_error(format!("id() takes exactly one argument ({} given)", args.len())));
    };
    let p = match v {
        V::List(x) => Arc::as_ptr(x) as *const () as usize,
        V::Dict(x) => Arc::as_ptr(x) as *const () as usize,
        V::Set(x) => Arc::as_ptr(x) as *const () as usize,
        V::Obj(x) => Arc::as_ptr(x) as *const () as usize,
        V::Inst(x) => Arc::as_ptr(x) as *const () as usize,
        V::Native(x) => Arc::as_ptr(x) as *const () as usize,
        V::Exc(x) => Arc::as_ptr(&x.0) as *const () as usize,
        V::Class(c) => *c as *const _ as usize,
        other => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            Key::of(other).map(|k| k.hash(&mut h)).unwrap_or(());
            (h.finish() >> 4) as usize
        }
    };
    Ok(V::Int((p & (i64::MAX as usize)) as i64))
}

/// `filter(f, iterable)` (f None: truthiness), materialised
pub async fn b_filter(cx: &Cx, args: &[V]) -> R {
    let [f, it] = args else {
        return Err(Exc::type_error(format!("filter expected 2 arguments, got {}", args.len())));
    };
    let mut out = Vec::new();
    for x in ops::iter(it)? {
        let keep = if f.is_none() { ops::truthy(&x)? } else { ops::truthy(&Box::pin(call_value(cx, f, vec![x.clone()], vec![])).await?)? };
        if keep {
            out.push(x);
        }
    }
    Ok(V::list(out))
}

/// `map(f, *iterables)`, materialised (shortest iterable)
pub async fn b_map(cx: &Cx, args: &[V]) -> R {
    let Some((f, its)) = args.split_first().filter(|(_, r)| !r.is_empty()) else {
        return Err(Exc::type_error("map() must have at least two arguments."));
    };
    let lists = its.iter().map(ops::iter).collect::<R<Vec<_>>>()?;
    let n = lists.iter().map(|l| l.len()).min().unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(Box::pin(call_value(cx, f, lists.iter().map(|l| l[i].clone()).collect(), vec![])).await?);
    }
    Ok(V::list(out))
}

pub async fn b_sorted(cx: &Cx, v: &V, kwargs: &[(String, V)]) -> R {
    let reverse = kw(kwargs, "reverse").map(ops::truthy).transpose()?.unwrap_or(false);
    Ok(V::list(sort_key(cx, ops::iter(v)?, kw(kwargs, "key"), reverse).await?))
}

pub fn b_reversed(v: &V) -> R {
    let mut items = ops::iter(v)?;
    items.reverse();
    Ok(V::list(items))
}

pub async fn b_minmax(cx: &Cx, is_max: bool, args: &[V], kwargs: &[(String, V)]) -> R {
    let items = if args.len() == 1 { ops::iter(&args[0])? } else { args.to_vec() };
    if items.is_empty() {
        return match kw(kwargs, "default") {
            Some(d) => Ok(d.clone()),
            None => Err(Exc::value_error(format!("{}() iterable argument is empty", if is_max { "max" } else { "min" }))),
        };
    }
    let key = kw(kwargs, "key");
    let mut best = items[0].clone();
    let mut best_k = match key {
        Some(f) if !f.is_none() => call_value(cx, f, vec![best.clone()], vec![]).await?,
        _ => best.clone(),
    };
    for it in items.into_iter().skip(1) {
        let k = match key {
            Some(f) if !f.is_none() => call_value(cx, f, vec![it.clone()], vec![]).await?,
            _ => it.clone(),
        };
        let better = if is_max { ops::cmp(&k, &best_k)? == std::cmp::Ordering::Greater } else { ops::cmp(&k, &best_k)? == std::cmp::Ordering::Less };
        if better {
            best = it;
            best_k = k;
        }
    }
    Ok(best)
}

pub fn b_sum(args: &[V]) -> R {
    let mut acc = args.get(1).cloned().unwrap_or(V::Int(0));
    for x in ops::iter(&args[0])? {
        acc = ops::add(&acc, &x)?;
    }
    Ok(acc)
}

pub fn b_any(v: &V) -> R {
    for x in ops::iter(v)? {
        if ops::truthy(&x)? {
            return Ok(V::Bool(true));
        }
    }
    Ok(V::Bool(false))
}

pub fn b_all(v: &V) -> R {
    for x in ops::iter(v)? {
        if !ops::truthy(&x)? {
            return Ok(V::Bool(false));
        }
    }
    Ok(V::Bool(true))
}

/// `next(it[, default])` on a stored value: only iterators keep a position (a list is a TypeError)
pub fn b_next_iter(args: &[V]) -> R {
    match &args[0] {
        V::Native(n) => match &**n {
            Native::Iter(q) => match q.lock().pop_front() {
                Some(v) => Ok(v),
                None => args.get(1).cloned().ok_or_else(|| Exc::new(&STOP_ITERATION, vec![])),
            },
            _ => Err(Exc::type_error(format!("'{}' object is not an iterator", args[0].type_name()))),
        },
        other => Err(Exc::type_error(format!("'{}' object is not an iterator", other.type_name()))),
    }
}

pub fn b_next(args: &[V]) -> R {
    match ops::iter(&args[0])?.into_iter().next() {
        Some(v) => Ok(v),
        None => args.get(1).cloned().ok_or_else(|| Exc::new(&STOP_ITERATION, vec![])),
    }
}

pub fn b_round(args: &[V]) -> R {
    let x = &args[0];
    let nd = match args.get(1) {
        None | Some(V::None) => None,
        Some(V::Int(n)) => Some(*n),
        _ => return Err(Exc::type_error("round() ndigits must be an integer")),
    };
    match (x, nd) {
        // round(Decimal) -> int, round(Decimal, n) -> Decimal quantized (half even)
        (V::Decimal(d), None) => {
            let q = d.quantize(0, super::decimal::Rounding::HalfEven)?;
            num_traits::ToPrimitive::to_i64(&q.to_int()).map(V::Int).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer beyond 64 bits"))
        }
        (V::Decimal(d), Some(n)) => Ok(super::decimal::v(d.quantize(-n, super::decimal::Rounding::HalfEven)?)),
        (V::Int(i), _) => Ok(V::Int(*i)),
        (V::Bool(b), _) => Ok(V::Int(*b as i64)),
        (V::Float(f), None) => {
            let r = f.round();
            let r = if (f - f.trunc()).abs() == 0.5 { 2.0 * (f / 2.0).round() } else { r };
            Ok(V::Int(r as i64))
        }
        (V::Float(f), Some(n)) => {
            if n >= 0 {
                let s = format!("{:.*}", n as usize, f);
                Ok(V::Float(s.parse().unwrap_or(*f)))
            } else {
                let p = 10f64.powi((-n) as i32);
                Ok(V::Float((f / p).round() * p))
            }
        }
        _ => Err(Exc::type_error(format!("type {} doesn't define __round__ method", x.type_name()))),
    }
}

pub fn b_abs(v: &V) -> R {
    Ok(match v {
        V::Int(i) => V::Int(i.abs()),
        V::Bool(b) => V::Int(*b as i64),
        V::Float(f) => V::Float(f.abs()),
        V::Decimal(d) => super::decimal::v(super::decimal::Dec { neg: false, ..(**d).clone() }.fix()),
        V::Delta(d) => V::Delta(d.abs()),
        _ => return Err(Exc::type_error(format!("bad operand type for abs(): '{}'", v.type_name()))),
    })
}

pub fn b_enumerate(args: &[V]) -> R {
    let start = match args.get(1) {
        Some(V::Int(i)) => *i,
        _ => 0,
    };
    Ok(V::list(ops::iter(&args[0])?.into_iter().enumerate().map(|(i, x)| V::tuple(vec![V::Int(start + i as i64), x])).collect()))
}

pub fn b_zip(args: &[V]) -> R {
    let lists = args.iter().map(ops::iter).collect::<R<Vec<_>>>()?;
    let n = lists.iter().map(|l| l.len()).min().unwrap_or(0);
    Ok(V::list((0..n).map(|i| V::tuple(lists.iter().map(|l| l[i].clone()).collect())).collect()))
}

pub fn b_range(args: &[V]) -> R {
    let ints = args.iter().map(|a| match a { V::Int(i) => Ok(*i), _ => Err(Exc::type_error("range() integer argument expected")) }).collect::<R<Vec<_>>>()?;
    let (start, stop, step) = match ints.len() {
        1 => (0, ints[0], 1),
        2 => (ints[0], ints[1], 1),
        _ => (ints[0], ints[1], ints[2]),
    };
    if step == 0 {
        return Err(Exc::value_error("range() arg 3 must not be zero"));
    }
    let mut out = Vec::new();
    let mut i = start;
    while (step > 0 && i < stop) || (step < 0 && i > stop) {
        out.push(V::Int(i));
        i += step;
    }
    Ok(V::list(out))
}

pub fn b_print(args: &[V]) -> R {
    let parts = args.iter().map(ops::str_).collect::<R<Vec<_>>>()?;
    println!("{}", parts.join(" "));
    Ok(V::None)
}

/// `isinstance(x, <builtin type name>)`
/// `type(x)` (one argument): the class of a project/library instance, else the builtin type by name
/// builtins that are functions, not types (`type(len)` is builtin_function_or_method)
const BUILTIN_FUNCTIONS: &[&str] = &["len", "repr", "sorted", "min", "max", "sum", "any", "all", "next", "round", "abs",
    "print", "isinstance", "getattr", "setattr", "hasattr", "iter", "open", "chr", "ord", "divmod", "callable", "hash", "id",
    "issubclass", "vars"];

pub fn b_type(args: &[V]) -> R {
    if args.len() != 1 {
        return Err(Exc::type_error("py2axum: type() takes exactly one argument here"));
    }
    Ok(match &args[0] {
        V::Exc(e) => V::Class(e.0.class),
        V::Inst(i) => V::Class(i.desc.class),
        V::Obj(o) => V::Class(o.desc.class),
        V::Enum(e, _) => V::Class(e.class),
        V::Decimal(_) => V::native(Native::Type("Decimal")),
        V::Native(n) if matches!(&**n, Native::TypeExpr(..)) => V::native(Native::ExtType("types.GenericAlias")),
        // builtins held as values are `Native::Type(name)`: the functions among them are not types
        V::Native(n) if matches!(&**n, Native::Type(t) if BUILTIN_FUNCTIONS.contains(t)) => V::native(Native::Type("builtin_function_or_method")),
        V::Native(n) if matches!(&**n, Native::ExtType(_) | Native::Type(_)) => V::native(Native::Type("type")),
        // the metaclass (Python ≥ 3.11 names Enum's `EnumType`)
        V::Class(c) => V::native(Native::Type(match c.kind {
            ClassKind::Schema(s) if !s.dataclass => "ModelMetaclass",
            ClassKind::Enum(_) => "EnumType",
            _ => "type",
        })),
        v => V::native(Native::Type(v.type_name())),
    })
}

/// `divmod(a, b)`: (a // b, a % b) with Python's floor semantics
/// `hash(x)`: CPython's value for integers (modulo 2**61 - 1, -1 → -2); for other hashable values a
/// stable hash (CPython randomises str/bytes hashes per process anyway). Unhashable values raise.
pub fn b_hash(args: &[V]) -> R {
    let [v] = args else {
        return Err(Exc::type_error(format!("hash() takes exactly one argument ({} given)", args.len())));
    };
    const M: i128 = (1 << 61) - 1;
    let int_hash = |i: i128| -> i64 {
        let h = if i >= 0 { i % M } else { -((-i) % M) };
        if h == -1 { -2 } else { h as i64 }
    };
    match v {
        V::Int(i) => Ok(V::Int(int_hash(*i as i128))),
        V::Bool(b) => Ok(V::Int(*b as i64)),
        _ => {
            use std::hash::{Hash, Hasher};
            let k = Key::of(v)?;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            k.hash(&mut h);
            Ok(V::Int(int_hash(h.finish() as i64 as i128)))
        }
    }
}

pub fn b_divmod(args: &[V]) -> R {
    match args {
        [a, b] => Ok(V::tuple(vec![ops::floordiv(a, b)?, ops::modulo(a, b)?])),
        _ => Err(Exc::type_error(format!("divmod expected 2 arguments, got {}", args.len()))),
    }
}

pub fn b_chr(args: &[V]) -> R {
    match args {
        [V::Int(i)] => match u32::try_from(*i).ok().filter(|&c| c <= 0x10FFFF) {
            Some(c) => Ok(V::str(char::from_u32(c).map(String::from).ok_or_else(|| Exc::value_error("py2axum: chr() of a lone surrogate"))?)),
            None => Err(Exc::value_error("chr() arg not in range(0x110000)")),
        },
        [o] => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
        _ => Err(Exc::type_error(format!("chr() takes exactly one argument ({} given)", args.len()))),
    }
}

/// `bytes(...)`: empty, a copy, n zero bytes, an iterable of ints, or a str with its encoding
pub fn b_bytes(args: &[V], kwargs: &[(String, V)]) -> R {
    let mut enc = None;
    for (k, v) in kwargs {
        match k.as_str() {
            "encoding" => enc = Some(v.clone()),
            "errors" => {}
            _ => return Err(Exc::type_error(format!("'{k}' is an invalid keyword argument for bytes()"))),
        }
    }
    let enc = enc.or_else(|| args.get(1).cloned());
    let out: Vec<u8> = match (args.first(), enc) {
        (None, None) => Vec::new(),
        (Some(V::Str(s)), Some(V::Str(e))) => match e.to_ascii_lowercase().replace('_', "-").as_str() {
            "utf-8" | "utf8" => s.as_bytes().to_vec(),
            _ => return Err(Exc::runtime(format!("py2axum: bytes(str, {e:?}): only utf-8 is supported"))),
        },
        (Some(V::Str(_)), None) => return Err(Exc::type_error("string argument without an encoding")),
        (_, Some(_)) => return Err(Exc::type_error("encoding without a string argument")),
        (Some(V::Bytes(b)), None) => b.to_vec(),
        (Some(V::Bool(b)), None) => vec![0; *b as usize],
        (Some(V::Int(n)), None) => {
            if *n < 0 {
                return Err(Exc::value_error("negative count"));
            }
            vec![0; *n as usize]
        }
        (Some(V::None), None) => return Err(Exc::type_error("cannot convert 'NoneType' object to bytes")),
        (Some(it @ (V::List(_) | V::Tuple(_) | V::Set(_) | V::Dict(_))), None) => {
            let mut out = Vec::new();
            for x in ops::iter(it)? {
                match x {
                    V::Int(i) if (0..256).contains(&i) => out.push(i as u8),
                    V::Bool(b) => out.push(b as u8),
                    V::Int(_) => return Err(Exc::value_error("bytes must be in range(0, 256)")),
                    o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
                }
            }
            out
        }
        (Some(o), None) => return Err(Exc::type_error(format!("cannot convert '{}' object to bytes", o.type_name()))),
    };
    Ok(V::Bytes(Arc::from(out)))
}

pub fn b_ord(args: &[V]) -> R {
    match args {
        [V::Str(s)] => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Ok(V::Int(c as i64)),
                _ => Err(Exc::type_error(format!("ord() expected a character, but string of length {} found", s.chars().count()))),
            }
        }
        [V::Bytes(b)] if b.len() == 1 => Ok(V::Int(b[0] as i64)),
        [V::Bytes(b)] => Err(Exc::type_error(format!("ord() expected a character, but string of length {} found", b.len()))),
        [o] => Err(Exc::type_error(format!("ord() expected string of length 1, but {} found", o.type_name()))),
        _ => Err(Exc::type_error(format!("ord() takes exactly one argument ({} given)", args.len()))),
    }
}

pub fn isinstance_builtin(v: &V, ty: &str) -> bool {
    match ty {
        "object" => true,
        "str" => matches!(v, V::Str(_)) || matches!(v, V::Enum(e, _) if matches!(e.kind, EnumKind::Str | EnumKind::StrEnum)),
        "int" => matches!(v, V::Int(_) | V::Bool(_)) || matches!(v, V::Enum(e, _) if matches!(e.kind, EnumKind::Int | EnumKind::IntEnum)),
        "float" => matches!(v, V::Float(_)),
        "bool" => matches!(v, V::Bool(_)),
        "dict" => matches!(v, V::Dict(_)),
        "list" => matches!(v, V::List(_)),
        "tuple" => matches!(v, V::Tuple(_)),
        "set" | "frozenset" => matches!(v, V::Set(_)),
        "bytes" => matches!(v, V::Bytes(_)),
        "datetime" => matches!(v, V::DateTime(_)),
        "date" => matches!(v, V::Date(_) | V::DateTime(_)),
        "timedelta" => matches!(v, V::Delta(_)),
        "time" => matches!(v, V::Time(_)),
        "Enum" => matches!(v, V::Enum(..)),
        "UUID" => matches!(v, V::Native(n) if matches!(&**n, Native::Uuid(_))),
        "Path" => matches!(v, V::Native(n) if matches!(&**n, Native::Path(_))),
        "Decimal" => matches!(v, V::Decimal(_)),
        "NoneType" => v.is_none(),
        _ => false,
    }
}

pub fn isinstance_class(v: &V, c: &'static Class) -> bool {
    match v {
        V::Enum(e, _) => e.class.is_subclass(c),
        V::Obj(o) => o.desc.class.is_subclass(c),
        V::Inst(i) => i.desc.class.is_subclass(c),
        V::Exc(e) => e.isinstance(c),
        _ => false,
    }
}

pub fn now(tz: &V) -> R {
    Ok(V::DateTime(DateTime::now(libs::tz_of(tz)?)))
}

pub fn today() -> R {
    Ok(V::Date(chrono::Local::now().date_naive()))
}

/// `EnumClass(value)`
pub async fn enum_call(cx: &Cx, desc: &'static EnumDesc, v: &V) -> R {
    if let V::Enum(d, _) = v {
        if std::ptr::eq(*d, desc) {
            return Ok(v.clone());
        }
    }
    if let Some(m) = desc.by_value(v) {
        return Ok(m);
    }
    let invalid = || Exc::value_error(format!("{} is not a valid {}", ops::repr(v).unwrap_or_default(), desc.name));
    match desc.missing {
        None => Err(invalid()),
        Some(f) => match f(cx, V::Class(desc.class), vec![v.clone()]).await? {
            V::None => Err(invalid()),
            V::Enum(d, i) if std::ptr::eq(d, desc) => Ok(V::Enum(d, i)),
            other => Err(Exc::type_error(format!(
                "error in {}._missing_: returned {} instead of None or a valid member",
                desc.name,
                ops::repr(&other).unwrap_or_default()
            ))),
        },
    }
}
