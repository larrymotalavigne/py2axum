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
    super::stack_guard()?;
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
            Native::Jinja(j) if name == "filters" => return Ok(V::native(Native::JinjaFilters(j.clone()))),
            Native::Tenacity(t) => return super::tenacity::attr(t, name),
            Native::RelDelta(r) => return super::reldelta::attr(r, name).ok_or_else(|| no_attr(v, name)),
            Native::Prom(p) => return super::prom::attr(p, name),
            Native::Routing(o) => return super::routing::attr(o, name),
            Native::TraceFrame(f) => return super::trace::frame_attr(f, name),
            Native::TraceCode(f) => return super::trace::code_attr(f, name),
            Native::Traceback(c, i) => return super::trace::tb_attr(c, *i, name),
            Native::FrameSummary(f, l) => return super::trace::summary_attr(f, *l, name),
            Native::Rsa(k) => {
                if let Some(r) = super::crypto::attr(k, name) {
                    return r;
                }
            }
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
                // __getattr__: only once the normal lookup failed
                _ => match find_method(i.desc.methods, "__getattr__") {
                    Some((false, f)) => f(cx, v.clone(), vec![V::str(name)]).await,
                    _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{}'", i.desc.name, name))),
                },
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
            (ClassKind::Model(m), _) if matches!(find_method(m.methods, name), Some((true, _))) => {
                Ok(V::native(Native::Property(m.methods.iter().find(|(n, _, _)| *n == name).unwrap().0)))
            }
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
            // the frames a `sys.settrace` function saw, else none (the binary has no Python frames)
            "__traceback__" => Ok({
                let chain: Vec<_> = e.0.tb.lock().iter().rev().cloned().collect();
                if chain.is_empty() { V::None } else { V::native(Native::Traceback(Arc::new(chain), 0)) }
            }),
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
            // a method read as a value (`hasattr(x, "isoformat")`, `f = d.strftime`)
            _ => match DATETIME_METHODS.iter().find(|m| **m == name) {
                Some(m) => V::native(Native::MethodOf(v.clone(), m)),
                None => return Err(no_attr(v, name)),
            },
        }),
        V::Date(d) => Ok(match name {
            "year" => V::Int(d.year() as i64),
            "month" => V::Int(d.month() as i64),
            "day" => V::Int(d.day() as i64),
            _ => match DATE_METHODS.iter().find(|m| **m == name) {
                Some(m) => V::native(Native::MethodOf(v.clone(), m)),
                None => return Err(no_attr(v, name)),
            },
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
            "key" => Ok(V::str(m.cols[*i].name)),
            // the Column's SQL name (`Column("metadata", ...)` mapped as `extra_data`)
            "name" => Ok(V::str(m.cols[*i].sql)),
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
            Native::WebSocket(s) => super::ws::attr(cx, s, name),
            Native::Request(r) => Ok(match name {
                "headers" => V::native(Native::Headers(r.clone())),
                "state" => V::native(Native::State(r.clone())),
                "url" => V::native(Native::Url(r.clone())),
                // root_path is always empty here: the origin and "/"
                "base_url" => V::native(Native::HttpUrl(format!("{}/", ops::request_origin(r)))),
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
                // Starlette's URL is built from scope["path"], which uvicorn percent-decodes
                // ... then urllib's urlsplit drops every tab and newline (WHATWG)
                "path" => V::str(web::unquote(&r.path).replace(['\t', '\r', '\n'], "")),
                "query" => V::str(r.raw_query.replace(['\t', '\r', '\n'], "")),
                "hostname" => r.header("host").map(|h| V::str(h.split(':').next().unwrap_or(""))).unwrap_or(V::None),
                "scheme" => V::str("http"),
                "netloc" => V::str(r.header("host").unwrap_or_default()),
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

const DATETIME_METHODS: &[&str] = &[
    "isoformat", "strftime", "date", "time", "timetz", "replace", "astimezone", "timestamp", "weekday", "isoweekday",
    "isocalendar", "utcoffset", "tzname", "dst", "timetuple", "utctimetuple", "toordinal", "ctime",
];
const DATE_METHODS: &[&str] = &[
    "isoformat", "strftime", "replace", "weekday", "isoweekday", "isocalendar", "toordinal", "timetuple", "ctime",
];

const BUILTIN_METHODS: &[(&str, &str)] = &[
    ("list", "append"), ("list", "extend"), ("list", "pop"), ("list", "remove"), ("list", "insert"), ("list", "clear"),
    ("list", "index"), ("list", "count"),
    ("dict", "get"), ("dict", "pop"), ("dict", "setdefault"), ("dict", "update"), ("dict", "keys"), ("dict", "values"),
    ("dict", "items"), ("dict", "clear"),
    ("set", "add"), ("set", "discard"), ("set", "remove"), ("set", "pop"), ("set", "clear"), ("set", "update"),
    ("str", "upper"), ("str", "lower"), ("str", "strip"), ("str", "format"), ("str", "join"), ("str", "split"),
];

/// `request.cookies`: Starlette's `cookie_parser` (lenient, `http.cookies._unquote` on values)
pub fn cookies(r: &web::ReqCell) -> R {
    // insertion order, last value wins (a dict, as Starlette): indexed, a header of many cookies stays linear
    let mut out: indexmap::IndexMap<&str, V> = indexmap::IndexMap::new();
    for raw in r.headers.iter().filter(|(k, _)| k == "cookie").map(|(_, v)| v) {
        for chunk in raw.split(';') {
            let (k, v) = match chunk.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => ("", chunk.trim()),
            };
            if !k.is_empty() || !v.is_empty() {
                out.insert(k, V::str(cookie_unquote(v)));
            }
        }
        // Starlette reads the first Cookie header only
        break;
    }
    V::dict_from(out.into_iter().map(|(k, v)| (V::str(k), v)).collect())
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

/// UTF-8 decoding with CPython's error messages (`offset`: bytes already consumed, a BOM)
pub(super) fn utf8_decode(b: &[u8], offset: usize) -> R {
    match std::str::from_utf8(b) {
        Ok(s) => Ok(V::str(s)),
        Err(e) => {
            let p = e.valid_up_to();
            let msg = match e.error_len() {
                None if b.len() - p > 1 => format!("can't decode bytes in position {}-{}: unexpected end of data", p + offset, b.len() - 1 + offset),
                None => format!("can't decode byte 0x{:02x} in position {}: unexpected end of data", b[p], p + offset),
                Some(_) => {
                    let start = matches!(b[p], 0xc2..=0xf4);
                    format!("can't decode byte 0x{:02x} in position {}: invalid {} byte", b[p], p + offset, if start { "continuation" } else { "start" })
                }
            };
            Err(Exc::msg(&UNICODE_DECODE_ERROR, format!("'utf-8' codec {msg}")))
        }
    }
}

/// The `bytes` methods that return text or bytes: strip family, split, startswith/endswith, lower/upper
fn bytes_method(b: &Arc<[u8]>, name: &str, args: &[V]) -> R {
    let chars = |i: usize| -> R<Option<Vec<u8>>> {
        match args.get(i) {
            None | Some(V::None) => Ok(None),
            Some(V::Bytes(c)) => Ok(Some(c.to_vec())),
            Some(o) => Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
        }
    };
    let ws = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c');
    let bytes = |v: &[u8]| V::Bytes(Arc::from(v));
    match name {
        "strip" | "lstrip" | "rstrip" => {
            let set = chars(0)?;
            let keep = |c: &u8| match &set {
                Some(s) => !s.contains(c),
                None => !ws(c),
            };
            let start = if name == "rstrip" { 0 } else { b.iter().position(keep).unwrap_or(b.len()) };
            let end = if name == "lstrip" { b.len() } else { b.iter().rposition(keep).map(|i| i + 1).unwrap_or(start) };
            Ok(bytes(&b[start..end.max(start)]))
        }
        "join" => {
            let [it] = &args[..] else {
                return Err(Exc::type_error(format!("bytes.join() takes exactly one argument ({} given)", args.len())));
            };
            let mut out = Vec::new();
            for (i, x) in super::ops::iter(it)?.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b);
                }
                match x {
                    V::Bytes(p) => out.extend_from_slice(p),
                    o => return Err(Exc::type_error(format!("sequence item {i}: expected a bytes-like object, {} found", o.type_name()))),
                }
            }
            Ok(bytes(&out))
        }
        "startswith" | "endswith" => {
            let check = |p: &[u8]| if name == "startswith" { b.starts_with(p) } else { b.ends_with(p) };
            match args.first() {
                Some(V::Bytes(p)) => Ok(V::Bool(check(p))),
                Some(V::Tuple(t)) => {
                    for p in t.iter() {
                        match p {
                            V::Bytes(p) if check(p) => return Ok(V::Bool(true)),
                            V::Bytes(_) => {}
                            o => return Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
                        }
                    }
                    Ok(V::Bool(false))
                }
                o => Err(Exc::type_error(format!(
                    "{name} first arg must be bytes or a tuple of bytes, not {}",
                    o.map(|o| o.type_name()).unwrap_or("NoneType")
                ))),
            }
        }
        // bytes.splitlines(keepends=False): \n, \r and \r\n only (not str's Unicode line boundaries)
        "splitlines" => {
            let keep = match args.first() {
                Some(v) => ops::truthy(v)?,
                None => false,
            };
            let mut out = Vec::new();
            let mut start = 0;
            let mut i = 0;
            while i < b.len() {
                let eol = match b[i] {
                    b'\r' if b.get(i + 1) == Some(&b'\n') => 2,
                    b'\r' | b'\n' => 1,
                    _ => 0,
                };
                if eol > 0 {
                    out.push(bytes(&b[start..if keep { i + eol } else { i }]));
                    i += eol;
                    start = i;
                } else {
                    i += 1;
                }
            }
            if start < b.len() {
                out.push(bytes(&b[start..]));
            }
            Ok(V::list(out))
        }
        "lower" => Ok(bytes(&b.to_ascii_lowercase())),
        "upper" => Ok(bytes(&b.to_ascii_uppercase())),
        "hex" if args.is_empty() => Ok(V::str(hex::encode(b))),
        _ => Err(Exc::attr_error(format!("'bytes' object has no attribute '{name}'"))),
    }
}

fn set_of(items: Vec<V>) -> R {
    let mut m = IndexMap::new();
    for v in items {
        m.insert(Key::set_elem(&v)?, v);
    }
    Ok(V::Set(Arc::new(Mutex::new(m))))
}

/// `object.__setattr__(obj, name, value)`: past the class's `__setattr__`
pub fn setattr_raw(v: &V, name: &str, val: V) -> R<()> {
    match v {
        V::Inst(i) => i.set_field(name, val),
        _ => setattr(v, name, val),
    }
}

/// `obj.name = val` in project code: a model with `validate_assignment=True` runs its validators (async)
pub async fn setattr_cx(cx: &Cx, v: &V, name: &str, val: V) -> R<()> {
    if let V::Inst(i) = v {
        if i.desc.validate_assignment && ops::dunder(v, "__setattr__").is_none() {
            return pyd::assign_validated(cx, i, name, val).await;
        }
    }
    setattr(v, name, val)
}

pub fn setattr(v: &V, name: &str, val: V) -> R<()> {
    match v {
        V::Obj(o) => o.set_attr(name, val),
        V::Inst(i) => match ops::dunder(v, "__setattr__") {
            Some(f) => ops::call_dunder(f, v, vec![V::str(name), val], "__setattr__").map(|_| ()),
            None => i.set_field(name, val),
        },
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
    // a builtin value's methods are not values here (`getattr(d, "date")` fails): CPython's dir() answers
    if builtin_dir(v).is_some_and(|d| d.contains(&name)) {
        return Ok(V::Bool(true));
    }
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
        V::Native(n) if matches!(&**n, Native::Sentry(_)) => {
            let Native::Sentry(o) = &**n else { unreachable!() };
            super::sentry::method(cx, o, name, args, kwargs, None).await
        }
        V::Native(n) if matches!(&**n, Native::Logger(_)) => {
            let Native::Logger(l) = &**n else { unreachable!() };
            web::log(cx, l, name, &args, &kwargs).await
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
        V::Native(n) if matches!(&**n, Native::ZipW(_)) => {
            let Native::ZipW(z) = &**n else { unreachable!() };
            super::zipw::method(z, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::Socket(_)) => {
            let Native::Socket(s) = &**n else { unreachable!() };
            super::net::sock_method(s, name)
        }
        V::Native(n) if matches!(&**n, Native::Rsa(_)) => {
            let Native::Rsa(k) = &**n else { unreachable!() };
            super::crypto::method(k, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::Stream(..)) => {
            let Native::Stream(s, w) = &**n else { unreachable!() };
            if let Some((k, _)) = kwargs.first() {
                return Err(Exc::type_error(format!("py2axum: stream.{name}({k}=) is not supported (positional arguments only)")));
            }
            super::net::stream_method(s, *w, name, &args).await
        }
        V::Native(n) if matches!(&**n, Native::TLock(_) | Native::TEvent(_) | Native::TThread(_) | Native::ELoop(_)) => match &**n {
            Native::TLock(l) => super::thread::lock_method(l, name, &args, &kwargs),
            Native::TEvent(e) => super::thread::event_method(e, name, &args, &kwargs),
            Native::TThread(t) => {
                // join(timeout=None): the keyword as the positional; any other keyword refused, never dropped
                let mut args = args;
                for (k, v) in kwargs {
                    if k != "timeout" || name != "join" || !args.is_empty() {
                        return Err(Exc::type_error(format!("py2axum: Thread.{name}({k}=) is not supported")));
                    }
                    args.push(v);
                }
                super::thread::thread_method(cx, t, name, &args)
            }
            Native::ELoop(_) if !kwargs.is_empty() => Err(Exc::type_error(format!("py2axum: loop.{name}({}=) is not supported", kwargs[0].0))),
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
            // FastAPI's ValidationException.errors(): the stored list, without `url`, no options
            let fastapi = e.isinstance(&VALIDATION_EXCEPTION);
            if fastapi && (!args.is_empty() || !kwargs.is_empty()) {
                return Err(Exc::type_error(format!("ValidationException.errors() takes 1 positional argument but {} were given", args.len() + kwargs.len() + 1)));
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
            let (url, ctx, input) = (flag("include_url")? && !fastapi, flag("include_context")?, flag("include_input")?);
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
        V::Bytes(b) if name == "decode" => bytes_decode(b, &args, &kwargs, "decode"),
        V::Bytes(b) => bytes_method(b, name, &args),
        V::List(l) => list_method(cx, l, name, args, kwargs).await,
        V::Dict(d) => match ops::is_counter(d).then(|| ops::counter_method(d, name, &args, &kwargs)).flatten() {
            Some(r) => r,
            None => dict_method(d, name, &args, &kwargs),
        },
        V::Set(s) => set_method(s, name, &args),
        V::Tuple(t) => match name {
            "count" if args.len() != 1 || !kwargs.is_empty() => Err(Exc::type_error(format!("count() takes exactly one argument ({} given)", args.len()))),
            "index" if args.len() != 1 || !kwargs.is_empty() => seq_index(t, &args, kwargs.is_empty())?
                .map(|i| V::Int(i as i64))
                .ok_or_else(|| Exc::value_error("tuple.index(x): x not in tuple")),
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
            // pydantic v1's `.dict()`, still on v2 models (deprecated): `model_dump` in Python mode
            "dict" if !i.desc.dataclass && !i.desc.open && i.desc.settings.is_none() && find_method(i.desc.methods, "dict").is_none() => {
                if !args.is_empty() {
                    return Err(Exc::type_error(format!("BaseModel.dict() takes 1 positional argument but {} were given", args.len() + 1)));
                }
                if let Some((k, _)) = kwargs.iter().find(|(k, _)| k == "mode") {
                    return Err(Exc::type_error(format!("BaseModel.dict() got an unexpected keyword argument '{k}'")));
                }
                Box::pin(call_method(cx, recv, "model_dump", args, kwargs)).await
            }
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
            (ClassKind::Schema(s), "model_json_schema") => match s.json_schema {
                Some(j) => pyd::loads(j),
                None => Err(Exc::type_error(format!("py2axum: {}.model_json_schema() was not computed", s.name))),
            },
            (ClassKind::Schema(s), "model_validate_json") => {
                if let Some((k, _)) = kwargs.first() {
                    return Err(Exc::type_error(format!("py2axum: model_validate_json({k}=) is not supported")));
                }
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
            "close" | "aclose" => s.close().await.map(|_| V::None),
            "connection" => s.connection().await,
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
            Native::WebSocket(s) => super::ws::method(cx, s, name, args, kwargs).await,
            Native::WsIter(s, kind) if name == "__anext__" => match super::ws::iter_next(s, kind).await? {
                Some(v) => Ok(v),
                None => Err(Exc::new(&STOP_ASYNC_ITERATION, vec![])),
            },
            Native::Request(r) => match name {
                "is_disconnected" => Ok(V::Bool(r.disconnected.load(std::sync::atomic::Ordering::Relaxed))),
                "body" => Ok(V::Bytes(Arc::from(&r.body[..]))),
                "json" => pyd::loads(&String::from_utf8_lossy(&r.body)),
                _ => Err(no_attr(recv, name)),
            },
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
            Native::SessConn(s) => match name {
                "close" => s.close_connection().await.map(|_| V::None),
                _ => Err(no_attr(recv, name)),
            },
            Native::Engine => match name {
                // a connection: its own transaction, rolled back when the `async with` ends
                "connect" => Ok(V::Session(orm::Session::new(cx.app.pool.clone(), false, true, false, Arc::downgrade(cx)))),
                // a connection in a transaction: committed when the `async with` ends without an exception
                "begin" => Ok(V::Session(orm::Session::new(cx.app.pool.clone(), false, true, false, Arc::downgrade(cx)).begin_block())),
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
            Native::Match(m) => {
                // groups(default=None), groupdict(default=None): the keyword as the positional; any other refused
                let mut args = args;
                for (k, v) in kwargs {
                    if k != "default" || !args.is_empty() || !matches!(name, "groups" | "groupdict") {
                        return Err(Exc::type_error(format!("py2axum: Match.{name}({k}=) is not supported")));
                    }
                    args.push(v);
                }
                super::stdlib::match_method(m, name, &args)
            }
            Native::StringIO(_) | Native::CsvWriter(_) if !kwargs.is_empty() => {
                Err(Exc::type_error(format!("py2axum: {name}({}=) is not supported (positional arguments only)", kwargs[0].0)))
            }
            Native::StringIO(s) => super::stdlib::stringio_method(s, name, &args),
            Native::CsvWriter(w) => super::stdlib::writer_method(w, name, &args),
            Native::SnifferObj if name == "sniff" => super::stdlib::sniff(&args, &kwargs),
            Native::Hmac(h) => super::stdlib::hmac_method(h, name, &args),
            Native::Jinja(j) => super::mail::jinja_method(j, name, &args, &kwargs),
            Native::JinjaTpl(t) => super::mail::tpl_method(t, name, &args, &kwargs),
            Native::Mime(m) => super::mail::mime_method(m, name, &args, &kwargs),
            Native::Deque(d) => super::deque::method(d, name, &args),
            Native::Template(t) => super::stdlib::template_method(t, name, &args, &kwargs),
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
        // `await self.app(scope, receive, send)` in a raw middleware: the rest of the stack
        if let Native::AsgiApp(slot) = &**n {
            let (cx2, slot) = (cx.clone(), slot.clone());
            return Ok(super::aio::coro(Box::pin(async move { super::asgi::call_app(&cx2, &slot, args).await })));
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

/// `str.isspace()` of one character: CPython's whitespace (with U+001C..U+001F, which Rust's lacks)
pub fn py_space(c: char) -> bool {
    str_class(c, |t| t.space, char::is_whitespace)
}

fn py_split_ws(s: &str, maxsplit: i64) -> Vec<V> {
    let mut out = Vec::new();
    let mut rest = s.trim_start_matches(py_space);
    while !rest.is_empty() {
        if maxsplit >= 0 && out.len() as i64 == maxsplit {
            out.push(V::str(rest));
            return out;
        }
        match rest.find(py_space) {
            Some(i) => {
                out.push(V::str(&rest[..i]));
                rest = rest[i..].trim_start_matches(py_space);
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
    // `sub, start, end`: the method on the slice s[start:end] (a position found is counted from the whole string)
    if matches!(name, "find" | "rfind" | "index" | "rindex" | "count" | "startswith" | "endswith") && args.len() > 1 {
        if args.len() > 3 || !kwargs.is_empty() {
            return Err(Exc::type_error(format!("{name}() takes at most 3 arguments ({} given)", args.len())));
        }
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len() as i64;
        let bound = |v: Option<&V>, d: i64| -> R<i64> {
            match v {
                None | Some(V::None) => Ok(d),
                Some(V::Int(i)) => Ok(if *i < 0 { (*i + n).max(0) } else { *i }),
                Some(V::Bool(b)) => Ok(*b as i64),
                Some(o) => Err(Exc::type_error(format!("slice indices must be integers or None or have an __index__ method (got {})", o.type_name()))),
            }
        };
        let (start, end) = (bound(args.get(1), 0)?, bound(args.get(2), n)?.min(n));
        if start > n || start > end {
            // CPython: nothing found, even the empty string, past the end or in an empty range
            return Ok(match name {
                "find" | "rfind" => V::Int(-1),
                "index" | "rindex" => return Err(Exc::value_error("substring not found")),
                "count" => V::Int(0),
                _ => V::Bool(false),
            });
        }
        let slice: Arc<str> = Arc::from(chars[start.min(n) as usize..end.max(start).min(n) as usize].iter().collect::<String>().as_str());
        let r = str_method(&slice, name, &args[..1], &[])?;
        return Ok(match (name, r) {
            ("find" | "rfind" | "index" | "rindex", V::Int(i)) if i >= 0 => V::Int(i + start),
            (_, r) => r,
        });
    }
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
                None => py_space(c),
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
        // str.splitlines: CPython's line boundaries (\r alone, \v, \f, \x1c-\x1e, \x85, U+2028/2029 too)
        "splitlines" => {
            let keep = match a0() {
                Some(v) => ops::truthy(v)?,
                None => false,
            };
            let cs: Vec<char> = s.chars().collect();
            let (mut out, mut start, mut i) = (Vec::new(), 0, 0);
            while i < cs.len() {
                let eol = match cs[i] {
                    '\r' if cs.get(i + 1) == Some(&'\n') => 2,
                    '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => 1,
                    _ => 0,
                };
                if eol > 0 {
                    out.push(V::str(cs[start..if keep { i + eol } else { i }].iter().collect::<String>()));
                    i += eol;
                    start = i;
                } else {
                    i += 1;
                }
            }
            if start < cs.len() {
                out.push(V::str(cs[start..].iter().collect::<String>()));
            }
            V::list(out)
        }
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
        "encode" => str_encode(s, args, kwargs)?,
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
        "isdigit" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.digit, char::is_numeric))),
        "isdecimal" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.decimal, |c| c.is_ascii_digit()))),
        "isnumeric" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.numeric, char::is_numeric))),
        "isalpha" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.alpha, char::is_alphabetic))),
        // Python: isalpha or isdecimal or isdigit or isnumeric (numeric includes the other two)
        "isalnum" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.alpha, char::is_alphabetic) || str_class(c, |t| t.numeric, char::is_numeric))),
        "isspace" => V::Bool(!s.is_empty() && s.chars().all(py_space)),
        // CPython's unicode_islower_impl / unicode_isupper_impl: no cased character of the other kind (nor a
        // titlecase one), at least one of this kind
        "islower" | "isupper" => {
            let (this, other): (fn(&StrClasses) -> &'static [(u32, u32)], fn(&StrClasses) -> &'static [(u32, u32)]) =
                if name == "islower" { (|t| t.lower, |t| t.upper) } else { (|t| t.upper, |t| t.lower) };
            let (fthis, fother): (fn(char) -> bool, fn(char) -> bool) =
                if name == "islower" { (char::is_lowercase, char::is_uppercase) } else { (char::is_uppercase, char::is_lowercase) };
            let mut cased = false;
            let mut ok = true;
            for c in s.chars() {
                if str_class(c, other, fother) || str_class(c, |t| t.title, |_| false) {
                    ok = false;
                    break;
                }
                cased |= str_class(c, this, fthis);
            }
            V::Bool(ok && cased)
        }
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
        "count" if args.len() != 1 => Err(Exc::type_error(format!("list.count() takes exactly one argument ({} given)", args.len()))),
        "index" => match if args.len() == 1 { l.lock().clone().iter().position(|x| ops::eq_bool(x, &args[0])) } else { seq_index(&l.lock().clone(), &args, kwargs.is_empty())? } {
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
            let k = Key::dict_key(&args[0])?;
            Ok(d.lock().get(&k).map(|(_, v)| v.clone()).unwrap_or_else(|| args.get(1).cloned().unwrap_or(V::None)))
        }
        "items" => Ok(V::list(d.lock().values().map(|(k, v)| V::tuple(vec![k.clone(), v.clone()])).collect())),
        "keys" => Ok(V::list(d.lock().values().map(|(k, _)| k.clone()).collect())),
        "values" => Ok(V::list(d.lock().values().map(|(_, v)| v.clone()).collect())),
        "pop" => {
            let k = Key::dict_key(&args[0])?;
            match d.lock().shift_remove(&k) {
                Some((_, v)) => Ok(v),
                None => args.get(1).cloned().ok_or_else(|| Exc::new(&KEY_ERROR, vec![args[0].clone()])),
            }
        }
        "setdefault" => {
            let k = Key::dict_key(&args[0])?;
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
                g.insert(Key::dict_key(&k)?, (k, v));
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
            s.lock().insert(Key::set_elem(&args[0])?, args[0].clone());
            Ok(V::None)
        }
        "discard" => {
            s.lock().shift_remove(&Key::set_elem(&args[0])?);
            Ok(V::None)
        }
        "remove" => match s.lock().shift_remove(&Key::set_elem(&args[0])?) {
            Some(_) => Ok(V::None),
            None => Err(Exc::new(&KEY_ERROR, vec![args[0].clone()])),
        },
        // update(*others)
        "update" => {
            for other in args {
                for x in ops::iter(other)? {
                    s.lock().insert(Key::set_elem(&x)?, x);
                }
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
            // CPython 3.14 names the role in difference_update, not in intersection_update
            let key = if name == "intersection_update" { Key::of } else { Key::set_elem };
            for other in args {
                let o = ops::iter(other)?.iter().map(key).collect::<R<std::collections::HashSet<Key>>>()?;
                keep.retain(|k, _| o.contains(k) == (name == "intersection_update"));
            }
            *s.lock() = keep;
            Ok(V::None)
        }
        "issubset" | "issuperset" | "isdisjoint" => {
            let key = if name == "issubset" { Key::of } else { Key::set_elem };
            let o = ops::iter(&args[0])?.iter().map(key).collect::<R<std::collections::HashSet<Key>>>()?;
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

/// a date's or datetime's method, synchronously (from a template)
pub fn value_method_sync(v: &V, name: &str, args: &[V]) -> R {
    match v {
        V::DateTime(d) => datetime_method(d, name, args, &[]),
        V::Date(d) => date_method(d, name, args, &[]),
        _ => Err(no_attr(v, name)),
    }
}

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
            // no zone (or None): the process's local zone, as a fixed offset at that instant (UTC when it is UTC)
            let local = || {
                let off = chrono::TimeZone::offset_from_utc_datetime(&chrono::Local, &d.utc()).local_minus_utc();
                if off == 0 { Tz::Utc } else { Tz::Fixed(off) }
            };
            let tz = match arg(args, kwargs, 0, "tz") {
                Some(v) => libs::tz_of(v)?.unwrap_or_else(local),
                None => local(),
            };
            V::DateTime(d.astimezone(tz))
        }
        "replace" => {
            let w = d.wall;
            let mut tz = d.tz;
            let mut f = [w.year() as i64, w.month() as i64, w.day() as i64, w.hour() as i64, w.minute() as i64, w.second() as i64, (w.nanosecond() / 1000) as i64];
            for (k, v) in kwargs {
                let slot = match k.as_str() {
                    "year" => 0, "month" => 1, "day" => 2, "hour" => 3, "minute" => 4, "second" => 5, "microsecond" => 6,
                    "tzinfo" => {
                        tz = libs::tz_of(v)?;
                        continue;
                    }
                    _ => return Err(Exc::type_error(format!("'{k}' is an invalid keyword argument for replace()"))),
                };
                f[slot] = match v { V::Int(i) => *i, V::Bool(b) => *b as i64, o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))) };
            }
            let date = ymd(f[0], f[1], f[2])?;
            let (h, mi, sec, us) = (f[3], f[4], f[5], f[6]);
            for (v, hi, what) in [(h, 23, "hour"), (mi, 59, "minute"), (sec, 59, "second"), (us, 999_999, "microsecond")] {
                time_field(v, hi, what)?;
            }
            let wall = date.and_hms_micro_opt(h as u32, mi as u32, sec as u32, us as u32).ok_or_else(|| Exc::value_error("replace(): value out of range"))?;
            V::DateTime(DateTime { wall, tz, fold: 0 })
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

/// `date(year, month, day)` with CPython's checks and messages (reworded in 3.14: the value and the bounds)
pub fn ymd(y: i64, m: i64, d: i64) -> R<chrono::NaiveDate> {
    let new = super::python() >= (3, 14);
    if !(1..=9999).contains(&y) {
        return Err(Exc::value_error(if new { format!("year must be in 1..9999, not {y}") } else { format!("year {y} is out of range") }));
    }
    if !(1..=12).contains(&m) {
        return Err(Exc::value_error(if new { format!("month must be in 1..12, not {m}") } else { "month must be in 1..12".into() }));
    }
    chrono::NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32).filter(|_| d >= 1).ok_or_else(|| {
        Exc::value_error(if new {
            format!("day {d} must be in range 1..{} for month {m} in year {y}", days_in(y, m))
        } else {
            "day is out of range for month".into()
        })
    })
}

/// `hour/minute/second/microsecond must be in 0..N` (3.14 adds `, not V`)
pub fn time_field(v: i64, hi: i64, what: &str) -> R<()> {
    if (0..=hi).contains(&v) {
        return Ok(());
    }
    Err(Exc::value_error(if super::python() >= (3, 14) { format!("{what} must be in 0..{hi}, not {v}") } else { format!("{what} must be in 0..{hi}") }))
}

fn days_in(y: i64, m: i64) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    chrono::NaiveDate::from_ymd_opt(ny as i32, nm as u32, 1)
        .and_then(|n| n.pred_opt())
        .map(|p| p.day())
        .unwrap_or(31)
}

fn date_method(d: &chrono::NaiveDate, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    Ok(match name {
        "isoformat" => V::str(dt::date_iso(d)),
        "weekday" => V::Int(d.weekday().num_days_from_monday() as i64),
        "isoweekday" => V::Int(d.weekday().number_from_monday() as i64),
        "toordinal" => V::Int(d.num_days_from_ce() as i64),
        "strftime" => V::str(libs::strftime(&d.and_hms_opt(0, 0, 0).unwrap(), None, None, &strs(&args[0])?)),
        "replace" => {
            let mut f = [d.year() as i64, d.month() as i64, d.day() as i64];
            for (k, v) in kwargs {
                let slot = match k.as_str() {
                    "year" => 0, "month" => 1, "day" => 2,
                    _ => return Err(Exc::type_error(format!("'{k}' is an invalid keyword argument for replace()"))),
                };
                f[slot] = match v { V::Int(i) => *i, V::Bool(b) => *b as i64, o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))) };
            }
            V::Date(ymd(f[0], f[1], f[2])?)
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
        // str(b, encoding[, errors]): bytes.decode
        Some(V::Bytes(b)) if args.len() > 1 => bytes_decode(b, &args[1..], &[], "str"),
        Some(_) if args.len() > 1 => Err(Exc::type_error(format!("decoding to str: need a bytes-like object, {} found", args[0].type_name()))),
        Some(v) => Ok(V::str(ops::str_(v)?)),
    }
}

/// `list.index(x, start=0, stop=sys.maxsize)` / `tuple.index`: the first equal item within [start, stop)
/// (bounds clamped like a slice)
fn seq_index(items: &[V], args: &[V], no_kwargs: bool) -> R<Option<usize>> {
    if args.is_empty() || args.len() > 3 || !no_kwargs {
        return Err(Exc::type_error(format!("index expected at least 1 argument, got {}", args.len())));
    }
    let n = items.len() as i64;
    let bound = |v: Option<&V>, d: i64| -> R<i64> {
        match v {
            None => Ok(d),
            Some(V::Int(i)) => Ok(if *i < 0 { (*i + n).max(0) } else { (*i).min(n) }),
            Some(V::Bool(b)) => Ok(*b as i64),
            Some(o) => Err(Exc::type_error(format!("slice indices must be integers or have an __index__ method (got {})", o.type_name()))),
        }
    };
    let (start, stop) = (bound(args.get(1), 0)?, bound(args.get(2), n)?);
    Ok((start..stop.max(start)).map(|i| i as usize).find(|i| ops::eq_bool(&items[*i], &args[0])))
}

/// a codec name as CPython normalizes it (`encodings.normalize_encoding` and its aliases), for the three codecs
/// the runtime implements; None for any other
fn codec(v: Option<&V>) -> R<Option<&'static str>> {
    let e = match v {
        Some(e) => ops::str_(e)?.to_ascii_lowercase().replace(['_', ' '], "-"),
        None => return Ok(Some("utf-8")),
    };
    Ok(match e.as_str() {
        "utf-8" | "utf8" | "u8" | "utf" | "cp65001" => Some("utf-8"),
        "utf-8-sig" | "utf8-sig" => Some("utf-8-sig"),
        "latin-1" | "latin1" | "iso-8859-1" | "iso8859-1" | "8859" | "l1" | "latin" | "cp819" | "iso-ir-100" | "csisolatin1" => Some("latin-1"),
        "ascii" | "us-ascii" | "646" | "us" | "ansi-x3.4-1968" | "cp367" | "csascii" | "ibm367" | "iso646-us" => Some("ascii"),
        _ => None,
    })
}

fn arg_or_kw<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v))
}

/// `bytes.decode(encoding="utf-8", errors="strict")` (and `str(b, encoding, errors)`)
pub fn bytes_decode(b: &[u8], args: &[V], kwargs: &[(String, V)], what: &str) -> R {
    if args.len() > 2 || kwargs.iter().any(|(k, _)| k != "encoding" && k != "errors") {
        return Err(Exc::type_error(format!("{what}() takes at most 2 arguments")));
    }
    if let Some(e) = arg_or_kw(args, kwargs, 1, "errors") {
        if ops::str_(e)? != "strict" {
            return Err(Exc::type_error(format!("py2axum: {what}(errors=) supports 'strict' only")));
        }
    }
    let enc = arg_or_kw(args, kwargs, 0, "encoding");
    match codec(enc)? {
        Some("utf-8") => utf8_decode(b, 0),
        Some("utf-8-sig") => match b.strip_prefix(b"\xef\xbb\xbf") {
            Some(rest) => utf8_decode(rest, 3),
            None => utf8_decode(b, 0),
        },
        Some("latin-1") => Ok(V::str(b.iter().map(|&c| c as char).collect::<String>())),
        Some(_) => match b.iter().position(|c| *c >= 0x80) {
            None => Ok(V::str(b.iter().map(|&c| c as char).collect::<String>())),
            Some(p) => Err(Exc::msg(
                &UNICODE_DECODE_ERROR,
                format!("'ascii' codec can't decode byte 0x{:02x} in position {p}: ordinal not in range(128)", b[p]),
            )),
        },
        None => Err(Exc::type_error(format!("py2axum: {what}({}) is not supported (utf-8, utf-8-sig, latin-1, ascii)", ops::repr(enc.unwrap())?))),
    }
}

/// `str.encode(encoding="utf-8", errors="strict")`: utf-8, ascii and latin-1, errors strict / ignore / replace
pub fn str_encode(s: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    if args.len() > 2 || kwargs.iter().any(|(k, _)| k != "encoding" && k != "errors") {
        return Err(Exc::type_error("encode() takes at most 2 arguments"));
    }
    let enc = arg_or_kw(args, kwargs, 0, "encoding");
    let errors = match arg_or_kw(args, kwargs, 1, "errors") {
        None => "strict".to_string(),
        Some(e) => ops::str_(e)?,
    };
    let limit = match codec(enc)? {
        Some("utf-8") => return Ok(V::Bytes(Arc::from(s.as_bytes()))),
        Some("utf-8-sig") => return Ok(V::Bytes(Arc::from([b"\xef\xbb\xbf".as_slice(), s.as_bytes()].concat()))),
        Some("latin-1") => 0x100,
        Some(_) => 0x80,
        None => return Err(Exc::type_error(format!("py2axum: str.encode({}) is not supported (utf-8, utf-8-sig, latin-1, ascii)", ops::repr(enc.unwrap())?))),
    };
    let name = if limit == 0x80 { "ascii" } else { "latin-1" };
    let mut out = Vec::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c as u32) < limit {
            out.push(c as u32 as u8);
            i += 1;
            continue;
        }
        match errors.as_str() {
            "ignore" => {}
            "replace" => out.push(b'?'),
            "strict" => {
                // CPython reports the whole run of unencodable characters
                let end = (i..chars.len()).find(|j| (chars[*j] as u32) < limit).unwrap_or(chars.len());
                let msg = if end - i == 1 {
                    format!("'{name}' codec can't encode character {} in position {i}: ordinal not in range({limit})", char_escape(c))
                } else {
                    format!("'{name}' codec can't encode characters in position {i}-{}: ordinal not in range({limit})", end - 1)
                };
                return Err(Exc::msg(&UNICODE_ENCODE_ERROR, msg));
            }
            other => return Err(Exc::type_error(format!("py2axum: str.encode(errors={other:?}) is not supported (strict, ignore, replace)"))),
        }
        i += 1;
    }
    Ok(V::Bytes(Arc::from(out)))
}

/// a character as `repr()` writes it inside the codec error messages ('\xe9', '\u20ac', '\U0001f600')
fn char_escape(c: char) -> String {
    let n = c as u32;
    if n < 0x100 { format!("'\\x{n:02x}'") } else if n < 0x10000 { format!("'\\u{n:04x}'") } else { format!("'\\U{n:08x}'") }
}

/// `callable(x)`: functions, bound methods, classes, builtin types, instances with `__call__`.
pub fn b_callable(args: &[V]) -> R {
    let [v] = args else { return Err(Exc::type_error(format!("callable() takes exactly one argument ({} given)", args.len()))) };
    Ok(V::Bool(match v {
        V::Class(_) => true,
        V::Native(n) => matches!(&**n, Native::Bound(..) | Native::PyFn(_) | Native::Type(_) | Native::Maker(..) | Native::CallNext(_) | Native::AsgiApp(_)),
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

pub fn b_zip(args: &[V], kwargs: &[(String, V)]) -> R {
    let lists = args.iter().map(ops::iter).collect::<R<Vec<_>>>()?;
    let n = lists.iter().map(|l| l.len()).min().unwrap_or(0);
    let strict = match kwargs {
        [] => false,
        [(k, v)] if k == "strict" => ops::truthy(v)?,
        [(k, _), ..] => return Err(Exc::type_error(format!("zip() got an unexpected keyword argument '{k}'"))),
    };
    if strict {
        // raised at the call, not after the common prefix was consumed (docs/supported.md)
        let args_word = |j: usize| if j == 1 { "argument 1".to_string() } else { format!("arguments 1-{j}") };
        if let Some(i) = lists.iter().position(|l| l.len() == n) {
            if i > 0 {
                return Err(Exc::value_error(format!("zip() argument {} is shorter than {}", i + 1, args_word(i))));
            }
            if let Some(j) = lists.iter().position(|l| l.len() > n) {
                return Err(Exc::value_error(format!("zip() argument {} is longer than {}", j + 1, args_word(j))));
            }
        }
    }
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
/// `dir()` of CPython 3.14's builtin values: what `hasattr` answers for them (methods included)
pub fn builtin_dir(v: &V) -> Option<&'static [&'static str]> {
    Some(match v {
        V::None => &["__bool__", "__class__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__le__", "__lt__", "__ne__", "__new__", "__reduce__", "__reduce_ex__", "__repr__", "__setattr__", "__sizeof__", "__str__", "__subclasshook__"],
        V::Bool(_) => &["__abs__", "__add__", "__and__", "__bool__", "__ceil__", "__class__", "__delattr__", "__dir__", "__divmod__", "__doc__", "__eq__", "__float__", "__floor__", "__floordiv__", "__format__", "__ge__", "__getattribute__", "__getnewargs__", "__getstate__", "__gt__", "__hash__", "__index__", "__init__", "__init_subclass__", "__int__", "__invert__", "__le__", "__lshift__", "__lt__", "__mod__", "__mul__", "__ne__", "__neg__", "__new__", "__or__", "__pos__", "__pow__", "__radd__", "__rand__", "__rdivmod__", "__reduce__", "__reduce_ex__", "__repr__", "__rfloordiv__", "__rlshift__", "__rmod__", "__rmul__", "__ror__", "__round__", "__rpow__", "__rrshift__", "__rshift__", "__rsub__", "__rtruediv__", "__rxor__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "__truediv__", "__trunc__", "__xor__", "as_integer_ratio", "bit_count", "bit_length", "conjugate", "denominator", "from_bytes", "imag", "is_integer", "numerator", "real", "to_bytes"],
        V::Int(_) => &["__abs__", "__add__", "__and__", "__bool__", "__ceil__", "__class__", "__delattr__", "__dir__", "__divmod__", "__doc__", "__eq__", "__float__", "__floor__", "__floordiv__", "__format__", "__ge__", "__getattribute__", "__getnewargs__", "__getstate__", "__gt__", "__hash__", "__index__", "__init__", "__init_subclass__", "__int__", "__invert__", "__le__", "__lshift__", "__lt__", "__mod__", "__mul__", "__ne__", "__neg__", "__new__", "__or__", "__pos__", "__pow__", "__radd__", "__rand__", "__rdivmod__", "__reduce__", "__reduce_ex__", "__repr__", "__rfloordiv__", "__rlshift__", "__rmod__", "__rmul__", "__ror__", "__round__", "__rpow__", "__rrshift__", "__rshift__", "__rsub__", "__rtruediv__", "__rxor__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "__truediv__", "__trunc__", "__xor__", "as_integer_ratio", "bit_count", "bit_length", "conjugate", "denominator", "from_bytes", "imag", "is_integer", "numerator", "real", "to_bytes"],
        V::Float(_) => &["__abs__", "__add__", "__bool__", "__ceil__", "__class__", "__delattr__", "__dir__", "__divmod__", "__doc__", "__eq__", "__float__", "__floor__", "__floordiv__", "__format__", "__ge__", "__getattribute__", "__getformat__", "__getnewargs__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__int__", "__le__", "__lt__", "__mod__", "__mul__", "__ne__", "__neg__", "__new__", "__pos__", "__pow__", "__radd__", "__rdivmod__", "__reduce__", "__reduce_ex__", "__repr__", "__rfloordiv__", "__rmod__", "__rmul__", "__round__", "__rpow__", "__rsub__", "__rtruediv__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "__truediv__", "__trunc__", "as_integer_ratio", "conjugate", "from_number", "fromhex", "hex", "imag", "is_integer", "real"],
        V::Str(_) => &["__add__", "__class__", "__contains__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getitem__", "__getnewargs__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__iter__", "__le__", "__len__", "__lt__", "__mod__", "__mul__", "__ne__", "__new__", "__reduce__", "__reduce_ex__", "__repr__", "__rmod__", "__rmul__", "__setattr__", "__sizeof__", "__str__", "__subclasshook__", "capitalize", "casefold", "center", "count", "encode", "endswith", "expandtabs", "find", "format", "format_map", "index", "isalnum", "isalpha", "isascii", "isdecimal", "isdigit", "isidentifier", "islower", "isnumeric", "isprintable", "isspace", "istitle", "isupper", "join", "ljust", "lower", "lstrip", "maketrans", "partition", "removeprefix", "removesuffix", "replace", "rfind", "rindex", "rjust", "rpartition", "rsplit", "rstrip", "split", "splitlines", "startswith", "strip", "swapcase", "title", "translate", "upper", "zfill"],
        V::Bytes(_) => &["__add__", "__buffer__", "__bytes__", "__class__", "__contains__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getitem__", "__getnewargs__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__iter__", "__le__", "__len__", "__lt__", "__mod__", "__mul__", "__ne__", "__new__", "__reduce__", "__reduce_ex__", "__repr__", "__rmod__", "__rmul__", "__setattr__", "__sizeof__", "__str__", "__subclasshook__", "capitalize", "center", "count", "decode", "endswith", "expandtabs", "find", "fromhex", "hex", "index", "isalnum", "isalpha", "isascii", "isdigit", "islower", "isspace", "istitle", "isupper", "join", "ljust", "lower", "lstrip", "maketrans", "partition", "removeprefix", "removesuffix", "replace", "rfind", "rindex", "rjust", "rpartition", "rsplit", "rstrip", "split", "splitlines", "startswith", "strip", "swapcase", "title", "translate", "upper", "zfill"],
        V::List(_) => &["__add__", "__class__", "__class_getitem__", "__contains__", "__delattr__", "__delitem__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getitem__", "__getstate__", "__gt__", "__hash__", "__iadd__", "__imul__", "__init__", "__init_subclass__", "__iter__", "__le__", "__len__", "__lt__", "__mul__", "__ne__", "__new__", "__reduce__", "__reduce_ex__", "__repr__", "__reversed__", "__rmul__", "__setattr__", "__setitem__", "__sizeof__", "__str__", "__subclasshook__", "append", "clear", "copy", "count", "extend", "index", "insert", "pop", "remove", "reverse", "sort"],
        V::Tuple(_) => &["__add__", "__class__", "__class_getitem__", "__contains__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getitem__", "__getnewargs__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__iter__", "__le__", "__len__", "__lt__", "__mul__", "__ne__", "__new__", "__reduce__", "__reduce_ex__", "__repr__", "__rmul__", "__setattr__", "__sizeof__", "__str__", "__subclasshook__", "count", "index"],
        V::Dict(_) => &["__class__", "__class_getitem__", "__contains__", "__delattr__", "__delitem__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getitem__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__ior__", "__iter__", "__le__", "__len__", "__lt__", "__ne__", "__new__", "__or__", "__reduce__", "__reduce_ex__", "__repr__", "__reversed__", "__ror__", "__setattr__", "__setitem__", "__sizeof__", "__str__", "__subclasshook__", "clear", "copy", "fromkeys", "get", "items", "keys", "pop", "popitem", "setdefault", "update", "values"],
        V::Set(_) => &["__and__", "__class__", "__class_getitem__", "__contains__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__iand__", "__init__", "__init_subclass__", "__ior__", "__isub__", "__iter__", "__ixor__", "__le__", "__len__", "__lt__", "__ne__", "__new__", "__or__", "__rand__", "__reduce__", "__reduce_ex__", "__repr__", "__ror__", "__rsub__", "__rxor__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "__xor__", "add", "clear", "copy", "difference", "difference_update", "discard", "intersection", "intersection_update", "isdisjoint", "issubset", "issuperset", "pop", "remove", "symmetric_difference", "symmetric_difference_update", "union", "update"],
        V::DateTime(_) => &["__add__", "__class__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__le__", "__lt__", "__ne__", "__new__", "__radd__", "__reduce__", "__reduce_ex__", "__replace__", "__repr__", "__rsub__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "astimezone", "combine", "ctime", "date", "day", "dst", "fold", "fromisocalendar", "fromisoformat", "fromordinal", "fromtimestamp", "hour", "isocalendar", "isoformat", "isoweekday", "max", "microsecond", "min", "minute", "month", "now", "replace", "resolution", "second", "strftime", "strptime", "time", "timestamp", "timetuple", "timetz", "today", "toordinal", "tzinfo", "tzname", "utcfromtimestamp", "utcnow", "utcoffset", "utctimetuple", "weekday", "year"],
        V::Date(_) => &["__add__", "__class__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__le__", "__lt__", "__ne__", "__new__", "__radd__", "__reduce__", "__reduce_ex__", "__replace__", "__repr__", "__rsub__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "ctime", "day", "fromisocalendar", "fromisoformat", "fromordinal", "fromtimestamp", "isocalendar", "isoformat", "isoweekday", "max", "min", "month", "replace", "resolution", "strftime", "strptime", "timetuple", "today", "toordinal", "weekday", "year"],
        V::Time(_) => &["__class__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__le__", "__lt__", "__ne__", "__new__", "__reduce__", "__reduce_ex__", "__replace__", "__repr__", "__setattr__", "__sizeof__", "__str__", "__subclasshook__", "dst", "fold", "fromisoformat", "hour", "isoformat", "max", "microsecond", "min", "minute", "replace", "resolution", "second", "strftime", "strptime", "tzinfo", "tzname", "utcoffset"],
        V::Delta(_) => &["__abs__", "__add__", "__bool__", "__class__", "__delattr__", "__dir__", "__divmod__", "__doc__", "__eq__", "__floordiv__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__le__", "__lt__", "__mod__", "__mul__", "__ne__", "__neg__", "__new__", "__pos__", "__radd__", "__rdivmod__", "__reduce__", "__reduce_ex__", "__repr__", "__rfloordiv__", "__rmod__", "__rmul__", "__rsub__", "__rtruediv__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "__truediv__", "days", "max", "microseconds", "min", "resolution", "seconds", "total_seconds"],
        V::Decimal(_) => &["__abs__", "__add__", "__bool__", "__ceil__", "__class__", "__complex__", "__copy__", "__deepcopy__", "__delattr__", "__dir__", "__divmod__", "__doc__", "__eq__", "__float__", "__floor__", "__floordiv__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__int__", "__le__", "__lt__", "__mod__", "__module__", "__mul__", "__ne__", "__neg__", "__new__", "__pos__", "__pow__", "__radd__", "__rdivmod__", "__reduce__", "__reduce_ex__", "__repr__", "__rfloordiv__", "__rmod__", "__rmul__", "__round__", "__rpow__", "__rsub__", "__rtruediv__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "__truediv__", "__trunc__", "adjusted", "as_integer_ratio", "as_tuple", "canonical", "compare", "compare_signal", "compare_total", "compare_total_mag", "conjugate", "copy_abs", "copy_negate", "copy_sign", "exp", "fma", "from_float", "from_number", "imag", "is_canonical", "is_finite", "is_infinite", "is_nan", "is_normal", "is_qnan", "is_signed", "is_snan", "is_subnormal", "is_zero", "ln", "log10", "logb", "logical_and", "logical_invert", "logical_or", "logical_xor", "max", "max_mag", "min", "min_mag", "next_minus", "next_plus", "next_toward", "normalize", "number_class", "quantize", "radix", "real", "remainder_near", "rotate", "same_quantum", "scaleb", "shift", "sqrt", "to_eng_string", "to_integral", "to_integral_exact", "to_integral_value"],
        _ => return None,
    })
}


/// The `str.is*()` classes of the translating Python's Unicode database (gen.rs `STR_CLASSES`, code point ranges):
/// Rust's predicates follow other properties ('¾'.is_numeric() but '¾'.isdigit() is False in Python).
pub struct StrClasses {
    pub digit: &'static [(u32, u32)],
    pub decimal: &'static [(u32, u32)],
    pub numeric: &'static [(u32, u32)],
    pub alpha: &'static [(u32, u32)],
    pub space: &'static [(u32, u32)],
    pub lower: &'static [(u32, u32)],
    pub upper: &'static [(u32, u32)],
    pub title: &'static [(u32, u32)],
}

static STR_CLASSES: std::sync::OnceLock<&'static StrClasses> = std::sync::OnceLock::new();

pub fn set_str_classes(c: &'static StrClasses) {
    let _ = STR_CLASSES.set(c);
}

fn str_class(c: char, table: fn(&StrClasses) -> &'static [(u32, u32)], fallback: fn(char) -> bool) -> bool {
    match STR_CLASSES.get() {
        Some(t) => {
            let ranges = table(t);
            let cp = c as u32;
            ranges.binary_search_by(|&(lo, hi)| if hi < cp { std::cmp::Ordering::Less } else if lo > cp { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }).is_ok()
        }
        None => fallback(c),
    }
}
