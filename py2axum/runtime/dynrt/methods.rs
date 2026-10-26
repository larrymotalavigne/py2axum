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

/// A NamedTuple class or instance: its descriptor
fn ntuple_desc(v: &V) -> Option<&'static pyd::SchemaDesc> {
    match v {
        V::Inst(i) if i.desc.x.ntuple => Some(i.desc),
        V::Class(c) => match c.kind {
            ClassKind::Schema(s) if s.x.ntuple => Some(s),
            _ => None,
        },
        _ => None,
    }
}

/// The attributes of a NamedTuple (class or instance): fields (on the instance), `_fields`, `_field_defaults`,
/// its methods; None for the rest (`__class__`, ...), an AttributeError for a pydantic or dataclass name.
async fn ntuple_attr(cx: &Cx, v: &V, d: &'static pyd::SchemaDesc, name: &str) -> Option<R> {
    let inst = matches!(v, V::Inst(_));
    if let V::Inst(i) = v {
        if let Some(x) = i.desc.field_index(name).map(|k| i.vals.read()[k].clone()) {
            return Some(Ok(x));
        }
    }
    Some(match name {
        "_fields" => Ok(V::tuple(d.fields.iter().map(|f| V::str(f.name)).collect())),
        "_field_defaults" => {
            let mut items = Vec::new();
            for f in d.fields {
                if let pyd::Dflt::Value(g) = &f.default {
                    items.push((V::str(f.name), g()));
                }
            }
            V::dict_from(items)
        }
        _ if name.starts_with("__") || find_method(d.methods, name).is_some() => return None,
        "_asdict" | "_replace" | "count" | "index" if inst => return None,
        "_make" => return None,
        _ => {
            let _ = cx;
            Err(if inst { no_attr(v, name) } else { Exc::attr_error(format!("type object '{}' has no attribute '{name}'", d.name)) })
        }
    })
}

/// The methods of a NamedTuple instance (its own ones aside): `_asdict()`, `_replace(**kw)`, `count`, `index`.
async fn ntuple_method(cx: &Cx, recv: &V, i: &pyd::Inst, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> Option<R> {
    let d = i.desc;
    Some(match name {
        "_asdict" if args.is_empty() && kwargs.is_empty() => {
            V::dict_from(d.fields.iter().zip(i.vals.read().iter()).map(|(f, x)| (V::str(f.name), x.clone())).collect())
        }
        "_asdict" => Err(Exc::type_error(format!("{}._asdict() takes 1 positional argument but {} were given", d.name, args.len() + 1))),
        "_replace" => {
            if !args.is_empty() {
                return Some(Err(Exc::type_error(format!("{}._replace() takes 1 positional argument but {} were given", d.name, args.len() + 1))));
            }
            let mut vals = i.vals.read().clone();
            let mut unknown = Vec::new();
            for (k, x) in kwargs {
                match d.field_index(&k) {
                    Some(n) => vals[n] = x,
                    None => unknown.push(V::str(k)),
                }
            }
            if !unknown.is_empty() {
                let msg = format!("Got unexpected field names: {}", ops::repr(&V::list(unknown)).unwrap_or_default());
                // ValueError until 3.12, TypeError from 3.13
                return Some(Err(if super::python() >= (3, 13) { Exc::type_error(msg) } else { Exc::value_error(msg) }));
            }
            Box::pin(pyd::dataclass_new(cx, d, vals, vec![])).await
        }
        "count" | "index" => {
            let t = ops::seq_tuple(recv).unwrap();
            Box::pin(call_method(cx, &t, name, args, kwargs)).await
        }
        _ => return None,
    })
}

pub fn find_method(table: &'static [(&'static str, bool, pyd::MethodFn)], name: &str) -> Option<(bool, pyd::MethodFn)> {
    table.iter().find(|(n, _, _)| *n == name).map(|(_, p, f)| (*p, *f))
}

fn bound(f: pyd::MethodFn, recv: V) -> V {
    V::native(Native::Bound(f, recv))
}

/// pydantic 2's deprecated v1 class methods (pydantic/main.py): `parse_obj`/`validate`/`from_orm` are
/// `model_validate`, `construct` is `model_construct(_fields_set=...)`, `schema()` `model_json_schema()`,
/// `parse_raw` decodes JSON first (its errors become a ValidationError at `__root__`)
async fn pyd_v1_class_method(cx: &Cx, recv: &V, s: &'static pyd::SchemaDesc, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let one = |pname: &str| -> R<V> {
        let given = args.len() + kwargs.len();
        if given != 1 || (args.is_empty() && kwargs[0].0 != pname) {
            return Err(if given == 0 {
                Exc::type_error(format!("BaseModel.{name}() missing 1 required positional argument: '{pname}'"))
            } else if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != pname) {
                Exc::type_error(format!("BaseModel.{name}() got an unexpected keyword argument '{k}'"))
            } else {
                Exc::type_error(format!("BaseModel.{name}() takes 2 positional arguments but {} were given", given + 1))
            });
        }
        Ok(args.first().cloned().unwrap_or_else(|| kwargs[0].1.clone()))
    };
    match name {
        "parse_obj" | "validate" | "from_orm" => {
            let obj = one(match name { "parse_obj" => "obj", "validate" => "value", _ => "obj" })?;
            if name == "from_orm" && !s.from_attributes {
                let class = if super::pydantic_before(2, 13) { &PYDANTIC_USER_ERROR_212 } else { &PYDANTIC_USER_ERROR };
                return Err(Exc::msg(class, "You must set the config attribute `from_attributes=True` to use from_orm"));
            }
            call_method(cx, recv, "model_validate", vec![obj], vec![]).await
        }
        "construct" => {
            if args.len() > 1 {
                return Err(Exc::type_error(format!("BaseModel.construct() takes from 1 to 2 positional arguments but {} were given", args.len() + 1)));
            }
            let mut kw = kwargs;
            if let Some(f) = args.into_iter().next() {
                kw.insert(0, ("_fields_set".into(), f));
            }
            pyd::model_construct(cx, s, vec![], kw).await
        }
        "schema" => {
            if !args.is_empty() || !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: BaseModel.schema() is supported without arguments only"));
            }
            call_method(cx, recv, "model_json_schema", vec![], vec![]).await
        }
        "update_forward_refs" => {
            if !args.is_empty() {
                return Err(Exc::type_error(format!("BaseModel.update_forward_refs() takes 1 positional argument but {} were given", args.len() + 1)));
            }
            if !kwargs.is_empty() {
                return Err(Exc::type_error("`localns` arguments are not longer accepted."));
            }
            // model_rebuild(force=True): the schema was built at translation time
            Ok(V::None)
        }
        _ => {
            // parse_raw(b, *, content_type=None, encoding='utf8', proto=None, allow_pickle=False): JSON only
            if let Some((k, _)) = kwargs.iter().find(|(k, v)| k != "b" && !(k == "content_type" && v.is_none()) && !(k == "encoding" && matches!(v, V::Str(e) if &**e == "utf8" || &**e == "utf-8"))) {
                return Err(Exc::type_error(format!("py2axum: BaseModel.parse_raw({k}=) is not supported")));
            }
            let b = match (args.as_slice(), kwargs.iter().find(|(k, _)| k == "b")) {
                ([b], None) => b.clone(),
                ([], Some((_, b))) => b.clone(),
                ([], None) => return Err(Exc::type_error("BaseModel.parse_raw() missing 1 required positional argument: 'b'")),
                _ => return Err(Exc::type_error(format!("BaseModel.parse_raw() takes 2 positional arguments but {} were given", args.len() + 1))),
            };
            let loaded = async {
                let text = match &b {
                    V::Bytes(_) => call_method(cx, &b, "decode", vec![V::str("utf8")], vec![]).await?,
                    _ => b.clone(),
                };
                super::libs::json_loads(&text)
            }
            .await;
            match loaded {
                Ok(obj) => call_method(cx, recv, "model_validate", vec![obj], vec![]).await,
                Err(e) if e.isinstance(&VALUE_ERROR) || e.isinstance(&TYPE_ERROR) => {
                    let kind = if e.isinstance(&UNICODE_DECODE_ERROR) {
                        "value_error.unicodedecode"
                    } else if e.isinstance(&JSON_DECODE_ERROR) {
                        "value_error.jsondecode"
                    } else if e.isinstance(&VALUE_ERROR) {
                        "value_error"
                    } else {
                        "type_error"
                    };
                    let msg = ops::str_(&V::Exc(e))?;
                    Err(Exc::validation_titled(&VALIDATION_ERROR, vec![pyd::ErrDetail { kind, loc: vec![V::str("__root__")], msg, input: b,
                                                                                         ctx: None, custom: true }], s.name))
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// pydantic 2's deprecated `.json(...)` (`model_dump_json` with by_alias=False) and `.copy(update=, deep=)`
async fn pyd_v1_method(cx: &Cx, recv: &V, i: &Arc<pyd::Inst>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error(format!("BaseModel.{name}() takes 1 positional argument but {} were given", args.len() + 1)));
    }
    if name == "json" {
        for (k, _) in &kwargs {
            match k.as_str() {
                "encoder" => return Err(Exc::type_error("The `encoder` argument is no longer supported; use field serializers instead.")),
                "models_as_dict" => return Err(Exc::type_error("The `models_as_dict` argument is no longer supported; use a model serializer instead.")),
                "include" | "exclude" | "by_alias" | "exclude_unset" | "exclude_defaults" | "exclude_none" => {}
                _ => return Err(Exc::type_error("`dumps_kwargs` keyword arguments are no longer supported.")),
            }
        }
        let mut kw = kwargs;
        if !kw.iter().any(|(k, _)| k == "by_alias") {
            kw.push(("by_alias".into(), V::Bool(false)));
        }
        return call_method(cx, recv, "model_dump_json", vec![], kw).await;
    }
    // copy(*, include=None, exclude=None, update=None, deep=False): the copied values (update included) deep-copied
    // with deep=True
    let mut kw = Vec::new();
    let mut deep = false;
    for (k, v) in kwargs {
        match k.as_str() {
            "deep" => {
                deep = ops::truthy(&v)?;
                kw.push((k, v));
            }
            "update" if v.is_none() => {}
            "update" => kw.push((k, v)),
            "include" | "exclude" if v.is_none() => {}
            "include" | "exclude" => return Err(Exc::type_error(format!("py2axum: BaseModel.copy({k}=) is not supported (update= and deep= only)"))),
            _ => return Err(Exc::type_error(format!("BaseModel.copy() got an unexpected keyword argument '{k}'"))),
        }
    }
    if deep {
        for (k, v) in kw.iter_mut() {
            if k == "update" {
                *v = super::copymod::deepcopy(v)?;
            }
        }
    }
    let _ = i;
    call_method(cx, recv, "model_copy", vec![], kw).await
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
        V::Inst(i) => V::Inst(Arc::new(pyd::Inst { raw: false,
            desc: i.desc,
            vals: super::pyd::Slots::new(i.vals.lock().iter().map(deepcopy).collect::<R<Vec<_>>>()?),
            set: Mutex::new(i.set.lock().clone()),
            extra: super::pyd::Slots::new(i.extra.lock().clone()),
        })),
        V::Obj(_) => return Err(Exc::type_error("py2axum: deepcopy of a mapped object is not supported")),
        V::Set(_) => return Err(Exc::type_error("py2axum: deepcopy of a set is not supported")),
        V::Frozen(_) => return Err(Exc::type_error("py2axum: deepcopy of a frozenset is not supported")),
        other => other.clone(),
    })
}

/// pydantic's `smart_deepcopy` of a field default evaluated once (`x: deque[int] = deque()`): each instance
/// gets its own copy of a mutable value (a Counter or defaultdict stays one).
pub fn default_copy(v: &V) -> R {
    super::stack_guard()?;
    Ok(match v {
        V::List(l) => V::list(l.lock().iter().map(default_copy).collect::<R<Vec<_>>>()?),
        V::Dict(d) => {
            let mut out = Vec::new();
            for (_, (k, x)) in d.lock().iter() {
                out.push((k.clone(), default_copy(x)?));
            }
            let copy = V::dict_from(out)?;
            ops::defaultdict_like(d, copy)?
        }
        V::Set(s) => V::Set(Arc::new(Mutex::new(s.lock().clone()))),
        // copy.copy(frozenset) is the same object
        V::Frozen(_) => v.clone(),
        V::Native(n) => match &**n {
            Native::Deque(d) => V::native(Native::Deque(Arc::new(super::deque::Deque {
                items: Mutex::new(d.items.lock().iter().map(default_copy).collect::<R<_>>()?),
                maxlen: d.maxlen,
            }))),
            _ => v.clone(),
        },
        V::Inst(_) => deepcopy(v)?,
        other => other.clone(),
    })
}

// ---------------------------------------------------------------- getattr / setattr

/// methods of the runtime's lock / event / loop / semaphore objects that may be read as values
const NATIVE_METHODS: &[&str] = &["acquire", "release", "locked", "set", "clear", "is_set", "wait", "is_running", "is_closed",
    "call_soon", "call_soon_threadsafe", "run_forever", "stop", "close", "create_task", "run_in_executor"];

tokio::task_local! {
    /// set by `object.__getattribute__`: the lookup does not fall back to the class's `__getattr__`
    static NO_GETATTR_FALLBACK: bool;
}

/// `object.__getattribute__(obj, name)`: `getattr` without `__getattr__` (a proxy's forwarding cannot answer)
pub async fn getattribute(cx: &Cx, v: &V, name: &str) -> R {
    NO_GETATTR_FALLBACK.scope(true, getattr(cx, v, name)).await
}

pub async fn getattr(cx: &Cx, v: &V, name: &str) -> R {
    if let Some(r) = super::stripe::getattr(v, name) {
        return r;
    }
    if let V::Native(n) = v {
        match &**n {
            Native::Module(m) => return (m.attr)(cx, name).await,
            Native::Jinja(j) if name == "filters" => return Ok(V::native(Native::JinjaFilters(j.clone()))),
            Native::Tenacity(t) => return super::tenacity::attr(t, name),
            Native::SaTable(_) | Native::SaColumns(_) | Native::SaColumn(_) | Native::SaType(_) => return super::satable::attr(n, name),
            Native::RelDelta(r) => return super::reldelta::attr(r, name).ok_or_else(|| no_attr(v, name)),
            Native::Cron(c) => return super::croniter::attr(c, name),
            Native::Xl(x) => return super::xlsx::attr(x, name),
            Native::Rl(x) => return super::rlbind::attr(x, name),
            Native::File(f) if name == "name" => return Ok(super::pathio::file_name(f)),
            Native::TempDir(t) => return super::pathio::tempdir_attr(v, t, name),
            // `deque.maxlen`: read-only, None when unbounded
            Native::Deque(d) if name == "maxlen" => return Ok(d.maxlen.map(|n| V::Int(n as i64)).unwrap_or(V::None)),
            Native::CtCache(c) => {
                if let Some(r) = super::ctools::attr(c, name) {
                    return r;
                }
                const M: [&str; 13] = ["get", "pop", "setdefault", "popitem", "clear", "expire", "getsizeof", "keys", "values", "items",
                                       "update", "__getitem__", "__contains__"];
                return match M.iter().find(|m| **m == name && (**m != "expire" || c.kind == super::ctools::Kind::Ttl)) {
                    Some(m) => Ok(V::native(Native::MethodOf(v.clone(), m))),
                    None => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", c.name))),
                };
            }
            Native::IsoParser(_) => {
                const M: [&str; 4] = ["isoparse", "parse_isodate", "parse_isotime", "parse_tzstr"];
                return match M.iter().find(|m| **m == name) {
                    Some(m) => Ok(V::native(Native::MethodOf(v.clone(), m))),
                    None => Err(Exc::attr_error(format!("'isoparser' object has no attribute '{name}'"))),
                };
            }
            Native::DnsResolver(r) => return super::dnsres::resolver_attr(r, name),
            Native::DnsAnswer(a) => return super::dnsres::answer_attr(a, name),
            Native::DnsRdata(d) => return super::dnsres::rdata_attr(d, name),
            Native::Prom(p) => return super::prom::attr(p, name),
            Native::Routing(o) => return super::routing::attr(o, name),
            Native::TraceFrame(f) => return super::trace::frame_attr(f, name),
            Native::YamlMark(m) => return super::pyyaml::mark_attr(m, name),
            Native::TraceCode(f) => return super::trace::code_attr(f, name),
            Native::Traceback(c, i) => return super::trace::tb_attr(c, *i, name),
            Native::FrameSummary(f, l) => return super::trace::summary_attr(f, *l, name),
            Native::Rsa(k) => {
                if let Some(r) = super::crypto::attr(k, name) {
                    return r;
                }
            }
            Native::Pgp(o) => {
                if let super::pgpy::Obj::Fp(s) = &**o {
                    if !matches!(name, "keyid" | "shortid") {
                        return Box::pin(getattr(cx, &V::Str(s.clone()), name)).await;
                    }
                }
                return super::pgpy::attr(v, o, name);
            }
            Native::Qr(o) => return super::qr::attr(o, name),
            _ => {}
        }
    }
    if let Some(h) = orm::alias_hybrid(v, name) {
        // a hybrid read on an aliased() class: its expression given the alias (`cls.word` is the alias's column)
        let f = h.expr.unwrap_or(h.get);
        return if h.method { Ok(bound(f, v.clone())) } else { f(cx, v.clone(), vec![]).await };
    }
    if let Some(r) = orm::sql_attr(v, name) {
        return r;
    }
    if let Some(d) = ntuple_desc(v) {
        if let Some(r) = ntuple_attr(cx, v, d, name).await {
            return r;
        }
    }
    match v {
        V::Obj(o) => {
            if name == "__class__" {
                return Ok(V::Class(o.desc.class));
            }
            if o.desc.col_index(name).is_some() || o.desc.rel_index(name).is_some() || o.desc.composite(name).is_some() {
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
                    let mut names: Vec<V> = i.desc.fields.iter().zip(set).filter(|(_, s)| *s).map(|(f, _)| V::str(f.name)).collect();
                    if i.desc.extra == pyd::Extra::Allow && !i.raw {
                        // `extra='allow'`: the extra inputs are set fields too (not those given to model_construct())
                        names.extend(i.extra.lock().keys().filter(|k| !k.starts_with('_') && !i.desc.is_private(k)).map(|k| V::str(k)));
                    }
                    set_of(names)
                }
                "model_fields" => schema_fields(i.desc),
                // `__pydantic_extra__`: the extra inputs with `extra='allow'`, else None
                "model_extra" if !i.desc.open && !i.desc.dataclass => match i.desc.extra {
                    pyd::Extra::Allow => V::dict_from(
                        i.extra.lock().iter().filter(|(k, _)| !k.starts_with('_') && !i.desc.is_private(k)).map(|(k, x)| (V::str(k), x.clone())).collect(),
                    ),
                    _ => Ok(V::None),
                },
                // a snapshot of the instance dict (fields, then the free attributes)
                "__dict__" if i.desc.slots.is_empty() => {
                    let vals = i.vals.lock().clone();
                    let mut items: Vec<(V, V)> = i.desc.fields.iter().zip(vals).map(|(f, x)| (V::str(f.name), x)).collect();
                    items.extend(i.extra.lock().iter().map(|(k, x)| (V::str(k), x.clone())));
                    V::dict_from(items)
                }
                // a project subclass of Request: the attributes of the request it wraps
                _ if i.extra.lock().contains_key(super::web::INNER_REQUEST) => {
                    let inner = i.extra.lock().get(super::web::INNER_REQUEST).cloned().unwrap_or(V::None);
                    Box::pin(getattr(cx, &inner, name)).await
                }
                // __getattr__: only once the normal lookup failed
                _ => match find_method(i.desc.methods, "__getattr__") {
                    Some((false, f)) if !NO_GETATTR_FALLBACK.try_with(|b| *b).unwrap_or(false) => f(cx, v.clone(), vec![V::str(name)]).await,
                    _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{}'", i.desc.name, name))),
                },
            }
        }
        V::Class(c) if matches!(c.kind, ClassKind::UserException(_)) && c.exc_lookup(name).is_some() => match c.exc_lookup(name).unwrap() {
            ExcMember::Attr(f) => f(cx).await,
            ExcMember::Method(..) => Err(Exc::type_error(format!("py2axum: {}.{name} read on the class is not supported", c.name))),
        },
        V::Class(c) => match (&c.kind, name) {
            // a Core table: `t.c` / `t.columns`, `t.name`; no column attributes
            (ClassKind::Model(m), "c" | "columns") if m.core => Ok(orm::sql(orm::Sql::TableCols(m))),
            (ClassKind::Model(m), "name" | "fullname") if m.core => Ok(V::str(m.table)),
            (ClassKind::Model(m), _) if m.core => Err(Exc::attr_error(format!("'Table' object has no attribute '{name}'"))),
            (ClassKind::Model(m), _) if m.hybrid(name).is_some() => {
                // a hybrid read on the class: its SQL expression
                let h = m.hybrid(name).unwrap();
                let f = h.expr.unwrap_or(h.get);
                if h.method {
                    Ok(bound(f, v.clone()))
                } else {
                    f(cx, v.clone(), vec![]).await
                }
            }
            (ClassKind::Model(m), _) if m.composite(name).is_some() => Ok(orm::sql(orm::Sql::Composite(m, m.composite(name).unwrap()))),
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
            // a method read on the class (`Cls.method`, `alias.method` where `alias = Cls`): bound to the class for a
            // `@classmethod`, else a function (taking the receiver first unless a `@staticmethod`), as in CPython
            (ClassKind::Schema(s), _) if matches!(find_method(s.methods, name), Some((false, _))) => {
                let (_, f) = find_method(s.methods, name).unwrap();
                if s.async_methods.contains(&name) {
                    // not awaited, it would run at once instead of making a coroutine
                    return Err(Exc::type_error(format!("py2axum: the async method {}.{name} read on its class is not supported", c.name)));
                }
                Ok(if s.x.classmethods.contains(&name) { bound(f, v.clone()) } else { V::native(Native::Unbound(f, s.x.statics.contains(&name))) })
            }
            // a class attribute of a plain class read on the class held as a value (`cls.X` in `__new__` or a classmethod)
            (ClassKind::Schema(s), _) if s.open && matches!(find_method(s.methods, name), Some((true, _))) => {
                let (_, f) = find_method(s.methods, name).unwrap();
                f(cx, v.clone(), vec![]).await
            }
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
            // the chain (set when raised `from`, or while another was handled: read above), else none
            "__cause__" | "__context__" => Ok(V::None),
            "__suppress_context__" => Ok(V::Bool(false)),
            // set by add_note (an instance attribute, read above): absent before the first note
            "__notes__" => Err(no_attr(v, name)),
            // SQLAlchemy's StatementError/DBAPIError: the exception met (read above), absent elsewhere
            "orig" => Err(no_attr(v, name)),
            // DBAPIError: false unless the connection failed (set when raised: v.rs)
            "connection_invalidated" if e.isinstance(&DBAPI_ERROR) => Ok(V::Bool(false)),
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
            // the attributes PyYAML's exceptions set in their `__init__`
            _ if super::pyyaml::exc_attr(e, name).is_some() => Ok(super::pyyaml::exc_attr(e, name).unwrap()),
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
        V::Time(t) => Ok(match name {
            "hour" => V::Int(t.hour() as i64),
            "minute" => V::Int(t.minute() as i64),
            "second" => V::Int(t.second() as i64),
            "microsecond" => V::Int((t.nanosecond() / 1000) as i64),
            "tzinfo" => V::None,
            "fold" => V::Int(0),
            _ => match TIME_METHODS.iter().find(|m| **m == name) {
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
        V::Tz(t) => {
            if let Some(r) = tz_attr(t, name) {
                return r;
            }
            match name {
                "key" if matches!(t, Tz::Zone(_)) => Ok(V::str(t.name())),
                _ => match TZ_METHODS.iter().find(|m| **m == name) {
                    Some(m) if tz_has_method(t, m) => Ok(V::native(Native::MethodOf(v.clone(), m))),
                    _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", tz_cpython_type(t)))),
                },
            }
        }
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
            Native::Request(r, o) => Ok(match name {
                "_body" => match r.cached_body(*o) {
                    Some(b) => V::Bytes(Arc::from(&b[..])),
                    None => return Err(no_attr(v, name)),
                },
                "headers" => V::native(Native::Headers(r.clone())),
                "state" => V::native(Native::State(r.clone())),
                "url" => V::native(Native::Url(r.clone())),
                // the origin, the root_path, "/"
                "base_url" => V::native(Native::HttpUrl(format!("{}{}/", ops::request_origin(r), web::root_path().trim_end_matches('/')))),
                "method" => V::str(&r.method),
                "query_params" => V::dict_from(r.query.iter().map(|(k, x)| (V::str(k), V::str(x))).collect())?,
                "cookies" => cookies(r)?,
                "client" => r.client.as_ref().map(|(h, p)| V::native(Native::Address(h.clone(), *p))).unwrap_or(V::None),
                "path_params" => V::dict_from(r.path_params.lock().iter().map(|(k, x)| (V::str(k), V::str(x))).collect())?,
                "app" => super::routing::app(),
                "scope" => super::routing::scope(cx)?,
                // a bound method in Starlette, only handed to `Request(scope, receive)` here
                "receive" => V::native(Native::RequestReceive),
                _ => return Err(no_attr(v, name)),
            }),
            Native::Totp(t) => super::auth::totp_attr(t, name),
            Native::Array(a) => super::arr::attr(a, name).ok_or_else(|| no_attr(v, name)),
            Native::IpAddr(a) => super::net::addr_attr(a, name).ok_or_else(|| no_attr(v, name)),
            Native::UrlParts(u) => super::stdlib::url_parts_attr(u, name),
            Native::IniConfig(c) => super::ini::attr(c, name),
            Native::PydUrl(kind, u) => pyd::url_attr(kind, u, name),
            Native::Row(names, vals) => match name {
                "_mapping" => V::dict_from(names.iter().zip(vals.iter()).map(|(k, x)| (V::str(k), x.clone())).collect()),
                "_fields" => Ok(V::tuple(names.iter().map(V::str).collect())),
                _ => names.iter().position(|k| &**k == name).map(|i| vals[i].clone()).ok_or_else(|| no_attr(v, name)),
            },
            Native::Record(_, f) => f.iter().find(|(k, _)| *k == name).map(|(_, x)| x.clone()).ok_or_else(|| no_attr(v, name)),
            Native::HttpResp(r) => super::http::resp_attr(r, name),
            Native::HttpClient(c) => super::http::client_attr(c, name),
            Native::Jwk(j) => super::jwks::jwk_attr(j, name),
            Native::JwkSet(s) => super::jwks::set_attr(s, name),
            Native::JwkClient(c) => super::jwks::client_attr(c, name),
            Native::RsaPub(k) => super::jwks::rsa_attr(k, name),
            Native::HttpUrl(u) => super::http::url_attr(u, name),
            Native::HttpReq(r) => super::http::req_attr(r, name),
            // a CryptContext's method read as a value (`v = pwd_context.verify`)
            Native::PassCtx(_) => match ["hash", "verify", "verify_and_update", "needs_update", "identify", "dummy_verify"].iter().find(|m| **m == name) {
                Some(m) => Ok(V::native(Native::MethodOf(v.clone(), m))),
                None => Err(no_attr(v, name)),
            },
            // `re.Pattern` -> Pattern (type names carry their module where CPython's repr does)
            Native::Type(t) if name == "__name__" || name == "__qualname__" => Ok(V::str(t.rsplit('.').next().unwrap_or(t))),
            Native::TThread(t) => super::thread::thread_attr(t, name),
            Native::YarlUrl(u) => super::http::yarl_attr(u, name),
            Native::RPubSub(p) if name == "subscribed" => Ok(V::Bool(p.subscribed())),
            Native::AmqpConn(c) => super::rmq::conn_attr(c, name),
            Native::AmqpChan(c) => super::rmq::chan_attr(c, name),
            Native::AmqpIncoming(m) => super::rmq::incoming_attr(m, name),
            Native::AmqpQueue(q) => super::rmq::queue_attr(q, name),
            Native::AmqpExchange(x) => super::rmq::exchange_attr(x, name),
            Native::AmqpFrame(_, fields) => fields.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()).ok_or_else(|| no_attr(v, name)),
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
                // urlsplit of the scheme://Host header+path URL; uvicorn passes no fragment, no userinfo
                "fragment" => V::str(""),
                "username" | "password" => V::None,
                "port" => r.header("host").and_then(|h| h.rsplit_once(':').and_then(|(_, p)| p.parse::<i64>().ok())).map(V::Int).unwrap_or(V::None),
                "scheme" => V::str(&r.scheme),
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
            Native::Log(o) => super::logging::attr(o, v, name),
            // `sp.is_active` of `session.begin_nested()` / `session.begin()` ("": the session's transaction)
            Native::Savepoint(sess, sp) if name == "is_active" => {
                Ok(V::Bool(if sp.is_empty() { sess.in_transaction().await } else { sess.savepoint_active(sp).await }))
            }
            Native::Proc(p) => super::subproc::attr(p, name),
            // a method of a lock, event, loop... read as a value (`loop.call_soon(ready.set)`)
            Native::TLock(_) | Native::TEvent(_) | Native::ELoop(_) | Native::Sem(_) => match NATIVE_METHODS.iter().find(|m| **m == name) {
                Some(m) => Ok(V::native(Native::MethodOf(v.clone(), m))),
                None => Err(no_attr(v, name)),
            },
            _ => Err(no_attr(v, name)),
        },
        V::Session(s) if name == "bind" => Ok(V::native(Native::Engine(s.is_sync()))),
        V::List(_) | V::Dict(_) | V::Set(_) | V::Frozen(_) | V::Str(_) | V::Tuple(_) | V::Bytes(_) => match BUILTIN_METHODS.iter().find(|(t, m)| *t == v.type_name() && *m == name) {
            // `items.append` read as a value: called later with its receiver
            Some((_, m)) => Ok(V::native(Native::MethodOf(v.clone(), m))),
            // `frozenset(words).__contains__` (keyword.iskeyword): the operator's method
            None => match CONTAINER_DUNDERS.iter().find(|m| **m == name) {
                Some(m) => Ok(V::native(Native::MethodOf(v.clone(), m))),
                None => Err(no_attr(v, name)),
            },
        },
        _ => Err(no_attr(v, name)),
    }
}

const DATETIME_METHODS: &[&str] = &[
    "isoformat", "strftime", "date", "time", "timetz", "replace", "astimezone", "timestamp", "weekday", "isoweekday",
    "isocalendar", "utcoffset", "tzname", "dst", "timetuple", "utctimetuple", "toordinal", "ctime",
];
const TIME_METHODS: &[&str] = &["isoformat", "strftime"];
const DATE_METHODS: &[&str] = &[
    "isoformat", "strftime", "replace", "weekday", "isoweekday", "isocalendar", "toordinal", "timetuple", "ctime",
];

/// the operator methods of the builtin containers read as values (`s.__contains__`), called through `call_method`
const CONTAINER_DUNDERS: &[&str] = &["__contains__", "__getitem__", "__len__"];

const BUILTIN_METHODS: &[(&str, &str)] = &[
    ("list", "append"), ("list", "extend"), ("list", "pop"), ("list", "remove"), ("list", "insert"), ("list", "clear"),
    ("list", "index"), ("list", "count"),
    ("dict", "get"), ("dict", "pop"), ("dict", "setdefault"), ("dict", "update"), ("dict", "keys"), ("dict", "values"),
    ("dict", "items"), ("dict", "clear"),
    ("frozenset", "union"), ("frozenset", "intersection"), ("frozenset", "difference"), ("frozenset", "symmetric_difference"),
    ("frozenset", "copy"), ("frozenset", "issubset"), ("frozenset", "issuperset"), ("frozenset", "isdisjoint"),
    ("set", "add"), ("set", "discard"), ("set", "remove"), ("set", "pop"), ("set", "clear"), ("set", "update"),
    ("str", "upper"), ("str", "lower"), ("str", "strip"), ("str", "format"), ("str", "join"), ("str", "split"),
];

/// `request.cookies`: Starlette's `cookie_parser` (lenient, `http.cookies._unquote` on values)
pub fn cookies(r: &web::ReqCell) -> R {
    V::dict_from(cookie_map(r).into_iter().map(|(k, v)| (V::str(k), V::str(v))).collect())
}

/// Starlette's `cookie_parser` as a map (a `Cookie()` parameter reads it too)
pub fn cookie_map(r: &web::ReqCell) -> indexmap::IndexMap<&str, String> {
    // insertion order, last value wins (a dict, as Starlette): indexed, a header of many cookies stays linear
    let mut out: indexmap::IndexMap<&str, String> = indexmap::IndexMap::new();
    for raw in r.headers.iter().filter(|(k, _)| k == "cookie").map(|(_, v)| v) {
        for chunk in raw.split(';') {
            let (k, v) = match chunk.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => ("", chunk.trim()),
            };
            if !k.is_empty() || !v.is_empty() {
                out.insert(k, cookie_unquote(v));
            }
        }
        // Starlette reads the first Cookie header only
        break;
    }
    out
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
/// `takes at least N argument(s)` of str/bytes methods, as each Python words it (3.13: argument clinic)
fn at_least(name: &str, n: usize, got: usize) -> Exc {
    let s = if n == 1 { "" } else { "s" };
    Exc::type_error(if super::python() >= (3, 13) {
        format!("{name} expected at least {n} argument{s}, got {got}")
    } else {
        format!("{name}() takes at least {n} argument{s} ({got} given)")
    })
}

/// `str.replace(old, new, count=-1)` / `bytes.replace`: their arity messages, and `count=` as a keyword (str
/// from 3.13, bytes from 3.15)
fn replace_count(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R<Option<i64>> {
    let since = if kind == "str" { (3, 13) } else { (3, 15) };
    if args.len() < 2 {
        return Err(Exc::type_error(if super::python() >= since {
            format!("replace() takes at least 2 positional arguments ({} given)", args.len())
        } else {
            format!("replace expected at least 2 arguments, got {}", args.len())
        }));
    }
    if !kwargs.is_empty() && super::python() < since {
        return Err(Exc::type_error(format!("{kind}.replace() takes no keyword arguments")));
    }
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "count") {
        return Err(Exc::type_error(format!("replace() got an unexpected keyword argument '{k}'")));
    }
    if args.len() > 3 || (args.len() == 3 && !kwargs.is_empty()) {
        return Err(Exc::type_error(format!("replace() takes at most 3 arguments ({} given)", args.len() + kwargs.len())));
    }
    match args.get(2).or(kwargs.first().map(|(_, v)| v)) {
        None => Ok(None),
        Some(V::Int(n)) => Ok((*n >= 0).then_some(*n)),
        Some(V::Bool(b)) => Ok(Some(*b as i64)),
        Some(o) => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    }
}

fn bytes_replace(b: &Arc<[u8]>, args: &[V], kwargs: &[(String, V)]) -> R {
    let count = replace_count("bytes", args, kwargs)?;
    let part = |v: &V| match v {
        V::Bytes(x) => Ok(x.to_vec()),
        o => Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
    };
    let (old, new) = (part(&args[0])?, part(&args[1])?);
    let mut out = Vec::with_capacity(b.len());
    let (mut i, mut done) = (0usize, 0i64);
    while i <= b.len() {
        if count.is_some_and(|c| done >= c) {
            out.extend_from_slice(&b[i..]);
            break;
        }
        if old.is_empty() {
            // an empty pattern matches before every byte and at the end
            out.extend_from_slice(&new);
            done += 1;
            if i < b.len() {
                out.push(b[i]);
            }
            i += 1;
            continue;
        }
        if b[i..].starts_with(&old) {
            out.extend_from_slice(&new);
            i += old.len();
            done += 1;
        } else if i < b.len() {
            out.push(b[i]);
            i += 1;
        } else {
            break;
        }
    }
    Ok(V::Bytes(Arc::from(out)))
}

fn bytes_method(b: &Arc<[u8]>, name: &str, args: &[V]) -> R {
    if matches!(name, "count" | "find" | "index" | "rfind" | "rindex" | "startswith" | "endswith") && args.is_empty() {
        return Err(at_least(name, 1, 0));
    }
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

fn frozen_of(items: Vec<V>) -> R {
    match set_of(items)? {
        V::Set(m) => Ok(V::Frozen(m)),
        other => Ok(other),
    }
}

/// a frozenset's methods: the read-only ones of a set, the results frozen
fn frozen_method(s: &Arc<Mutex<IndexMap<Key, V>>>, name: &str, args: &[V]) -> R {
    match name {
        "copy" => Ok(V::Frozen(s.clone())),
        "union" | "intersection" | "difference" | "symmetric_difference" | "issubset" | "issuperset" | "isdisjoint" => {
            match set_method(s, name, args)? {
                V::Set(m) => Ok(V::Frozen(m)),
                other => Ok(other),
            }
        }
        _ => Err(Exc::attr_error(format!("'frozenset' object has no attribute '{name}'"))),
    }
}

pub fn set_of(items: Vec<V>) -> R {
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
    if let V::Obj(o) = v {
        if let Some(h) = o.desc.hybrid(name) {
            // a hybrid's setter (without one: AttributeError, like a property without setter)
            return match h.set {
                Some(f) => f(cx, v.clone(), vec![val]).await.map(|_| ()),
                None => Err(Exc::attr_error("can't set attribute")),
            };
        }
    }
    if let V::Inst(i) = v {
        if i.desc.validate_assignment && ops::dunder(v, "__setattr__").is_none() {
            return pyd::assign_validated(cx, i, name, val).await;
        }
        if i.desc.open && !name.starts_with("__") && ops::dunder(v, "__setattr__").is_none() {
            // a property is a data descriptor: the class's `@name.setter` runs, without one the assignment fails
            // (the table holds `set:<name>` beside the getter `<name>`, `ro:<name>` for a property without setter)
            if let Some((_, f)) = find_method(i.desc.methods, &format!("set:{name}")) {
                return f(cx, v.clone(), vec![val]).await.map(|_| ());
            }
            if find_method(i.desc.methods, &format!("ro:{name}")).is_some() {
                return Err(Exc::attr_error(if super::python() >= (3, 11) {
                    format!("property '{name}' of '{}' object has no setter", i.desc.name)
                } else {
                    "can't set attribute".to_string()
                }));
            }
        }
    }
    if let V::Class(c) = v {
        if let ClassKind::Schema(s) = c.kind {
            if s.open {
                // `cls.X = v` (a plain class's own class attribute, `=X` in its method table)
                return match find_method(s.methods, &format!("={name}")) {
                    Some((_, f)) => f(cx, v.clone(), vec![val]).await.map(|_| ()),
                    None => Err(Exc::type_error(format!(
                        "py2axum: setting {name} on class {} at run time is not supported (only its own class attributes)", c.name))),
                };
            }
        }
    }
    setattr(v, name, val)
}

pub fn setattr(v: &V, name: &str, val: V) -> R<()> {
    if let Some(r) = super::stripe::setattr(v, name, &val) {
        return r;
    }
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
            // redis-py's `client.auto_close_connection_pool = True` (aiocache): the pool closes with the client here
            Native::Redis(_) if name == "auto_close_connection_pool" => Ok(()),
            Native::State(r) => {
                // shared by every request (`app.state.counter += 1` in a middleware): the shortest hold
                let mut st = r.state.lock();
                let old = match st.get_mut(name) {
                    Some(slot) => Some(std::mem::replace(slot, val)),
                    None => {
                        drop(st);
                        let key = name.to_string();
                        r.state.lock().insert(key, val);
                        return Ok(());
                    }
                };
                drop(st);
                drop(old);
                Ok(())
            }
            Native::RespObj(r) if name == "status_code" => super::resp::set_status(r, &val),
            // a BaseHTTPMiddleware's `request._body = data`: what its `call_next` hands the inner app
            Native::Request(r, o) if name == "_body" => match val {
                V::Bytes(b) => {
                    r.set_body(*o, axum::body::Bytes::copy_from_slice(&b));
                    Ok(())
                }
                other => Err(Exc::type_error(format!("py2axum: request._body = <{}> is not supported (only bytes)", other.type_name()))),
            },
            // `request._receive = receive` next to `_body`: the inner app reads `_body` first (wrapped_receive), the
            // channel is never asked for the body again; without `_body` the new channel would be read: refused
            Native::Request(r, o) if name == "_receive" => match r.cached_body(*o) {
                Some(_) => Ok(()),
                None => Err(Exc::type_error("py2axum: request._receive = ... without request._body set first is not supported")),
            },
            Native::DnsResolver(r) => super::dnsres::resolver_setattr(r, name, val),
            Native::Xl(x) => super::xlsx::setattr(x, name, val),
            Native::Rl(x) => super::rlbind::setattr(x, name, val),
            Native::Log(o) => super::logging::setattr(o, name, val),
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
    if let Some(f) = super::stripe::call_method(cx, recv, name, &args, &kwargs) {
        return f.await;
    }
    if matches!(recv, V::List(_) | V::Dict(_) | V::Set(_) | V::Frozen(_) | V::Str(_) | V::Tuple(_) | V::Bytes(_)) && CONTAINER_DUNDERS.contains(&name) {
        let t = recv.type_name();
        if !kwargs.is_empty() {
            return Err(Exc::type_error(format!("{t}.{name}() takes no keyword arguments")));
        }
        let want = if name == "__len__" { 0 } else { 1 };
        if args.len() != want {
            return Err(Exc::type_error(if want == 0 {
                format!("{t}.__len__() takes no arguments ({} given)", args.len())
            } else {
                format!("{t}.{name}() takes exactly one argument ({} given)", args.len())
            }));
        }
        return match name {
            "__contains__" => super::ops::contains(recv, &args[0]).map(V::Bool),
            "__getitem__" => super::ops::getitem(recv, &args[0]),
            _ => super::ops::len(recv).map(|n| V::Int(n as i64)),
        };
    }
    match recv {
        // BaseException.add_note(note): appended to `__notes__` (an instance attribute, as in CPython)
        V::Exc(e) if name == "add_note" && !e.0.attrs.lock().contains_key("add_note") => match (&args[..], kwargs.is_empty()) {
            ([V::Str(s)], true) => {
                e.add_note(s);
                Ok(V::None)
            }
            ([other], true) => Err(Exc::type_error(format!("note must be a str, not '{}'", other.type_name()))),
            _ => Err(Exc::type_error(format!("BaseException.add_note() takes exactly one argument ({} given)", args.len() + kwargs.len()))),
        },
        // float.is_integer(), int.is_integer() (3.12+)
        V::Float(_) | V::Int(_) | V::Bool(_) if name == "is_integer" => {
            let t = if matches!(recv, V::Float(_)) { "float" } else { "int" };
            if !kwargs.is_empty() {
                return Err(Exc::type_error(format!("{t}.is_integer() takes no keyword arguments")));
            }
            if !args.is_empty() {
                return Err(Exc::type_error(format!("{t}.is_integer() takes no arguments ({} given)", args.len())));
            }
            Ok(V::Bool(match recv {
                V::Float(f) => f.is_finite() && f.fract() == 0.0,
                _ => true,
            }))
        }
        V::Native(n) if matches!(&**n, Native::Module(_)) => {
            let f = getattr(cx, recv, name).await?;
            call_value(cx, &f, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::CtCache(_)) => {
            let Native::CtCache(c) = &**n else { unreachable!() };
            match name {
                "__getitem__" if args.len() == 1 => super::ctools::getitem(c, &args[0]),
                "__contains__" if args.len() == 1 => super::ctools::contains(c, &args[0]).map(V::Bool),
                _ => super::ctools::method(c, name, &args, &kwargs),
            }
        }
        V::Native(n) if matches!(&**n, Native::IsoParser(_)) => {
            let Native::IsoParser(sep) = &**n else { unreachable!() };
            super::duparse::isoparser_method(*sep, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::YamlMark(_)) => {
            let Native::YamlMark(m) = &**n else { unreachable!() };
            super::pyyaml::mark_method(m, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::Cron(_)) => {
            let Native::Cron(c) = &**n else { unreachable!() };
            super::croniter::method(c, name, args, kwargs)
        }
        V::Native(n) if matches!(&**n, Native::DnsResolver(_)) => {
            let Native::DnsResolver(r) = &**n else { unreachable!() };
            super::dnsres::resolver_method(r, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::DnsRdata(_)) => {
            let Native::DnsRdata(d) = &**n else { unreachable!() };
            super::dnsres::rdata_method(d, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::DnsName(_)) => {
            let Native::DnsName(d) = &**n else { unreachable!() };
            super::dnsres::name_method(d, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::ICal(_)) => {
            let Native::ICal(c) = &**n else { unreachable!() };
            super::ical::method(c, name, args, kwargs)
        }
        V::Native(n) if matches!(&**n, Native::SaColumns(_)) => {
            let Native::SaColumns(t) = &**n else { unreachable!() };
            super::satable::method(t, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::Secret(_)) => {
            let Native::Secret(s) = &**n else { unreachable!() };
            match name {
                "get_secret_value" if args.is_empty() && kwargs.is_empty() => Ok(V::str(&**s)),
                _ => Err(Exc::attr_error(format!("'SecretStr' object has no attribute '{name}'"))),
            }
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
        V::Native(n) if matches!(&**n, Native::Log(_)) => {
            let Native::Log(o) = &**n else { unreachable!() };
            Box::pin(super::logging::method(cx, o, recv, name, args, kwargs, &super::logging::NO_LOC)).await
        }
        V::Native(n) if matches!(&**n, Native::Adapter(..)) => {
            let Native::Adapter(td, _) = &**n else { unreachable!() };
            pyd::adapter_method(cx, td, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::AmqpConn(_) | Native::AmqpChan(_) | Native::AmqpQueue(..) | Native::AmqpExchange(..)
                                       | Native::AmqpIncoming(_) | Native::AmqpIter(_)) => match &**n {
            Native::AmqpConn(c) => super::rmq::conn_method(c, name, &args).await,
            Native::AmqpChan(c) => super::rmq::chan_method(c, name, &args, &kwargs).await,
            Native::AmqpQueue(q) => super::rmq::queue_method(cx, q, name, &args, &kwargs).await,
            Native::AmqpExchange(x) => super::rmq::exchange_method(x, name, &args, &kwargs).await,
            Native::AmqpIncoming(m) => super::rmq::incoming_method(m, name, &args, &kwargs).await,
            Native::AmqpIter(it) if name == "close" && args.is_empty() => super::rmq::iter_close(it).await.map(|_| V::None),
            _ => Err(no_attr(recv, name)),
        },
        V::Native(n) if matches!(&**n, Native::AFuture(_) | Native::AEvent(_)) => match &**n {
            Native::AFuture(f) => super::aio::future_method(recv, f, name, &args),
            Native::AEvent(e) => super::aio::event_method(e, name, &args).await,
            _ => unreachable!(),
        },
        V::Native(n) if matches!(&**n, Native::Redis(_)) => {
            let Native::Redis(c) = &**n else { unreachable!() };
            super::rds::method(c, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::RPipe(_)) => {
            let Native::RPipe(p) = &**n else { unreachable!() };
            super::rds::pipe_method(p, recv, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::RPubSub(_)) => {
            let Native::RPubSub(p) = &**n else { unreachable!() };
            super::rps::method(p, name, args, kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::Sem(_)) => {
            let Native::Sem(s) = &**n else { unreachable!() };
            super::aio::sem_method(s, name).await
        }
        V::Native(n) if matches!(&**n, Native::ZipW(_)) => {
            let Native::ZipW(z) = &**n else { unreachable!() };
            super::zipw::method(z, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::TempDir(_)) => {
            let Native::TempDir(t) = &**n else { unreachable!() };
            if let Some((k, _)) = kwargs.first() {
                return Err(Exc::type_error(format!("TemporaryDirectory.{name}() got an unexpected keyword argument '{k}'")));
            }
            super::pathio::tempdir_method(t, name, &args)
        }
        V::Native(n) if matches!(&**n, Native::Socket(_)) => {
            let Native::Socket(s) = &**n else { unreachable!() };
            super::net::sock_method(s, name)
        }
        V::Native(n) if matches!(&**n, Native::Proc(_)) => {
            let Native::Proc(p) = &**n else { unreachable!() };
            super::subproc::method(p, name, &args, &kwargs).await
        }
        V::Native(n) if matches!(&**n, Native::Rsa(_)) => {
            let Native::Rsa(k) = &**n else { unreachable!() };
            super::crypto::method(k, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::Pgp(_)) => {
            let Native::Pgp(o) = &**n else { unreachable!() };
            if let super::pgpy::Obj::Fp(s) = &**o {
                // Fingerprint.__eq__/__ne__/__hash__ are its own; the rest is str's (and returns a str)
                return match (name, &args[..]) {
                    ("__eq__", [x]) => Ok(V::Bool(super::ops::eq_bool(recv, x))),
                    ("__ne__", [x]) => Ok(V::Bool(!super::ops::eq_bool(recv, x))),
                    _ => Box::pin(call_method(cx, &V::Str(s.clone()), name, args, kwargs)).await,
                };
            }
            super::pgpy::method(recv, o, name, &args, &kwargs)
        }
        V::Native(n) if matches!(&**n, Native::Qr(_)) => {
            let Native::Qr(o) = &**n else { unreachable!() };
            super::qr::method(o, name, &args, &kwargs)
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
        V::Bytes(b) if name == "replace" => bytes_replace(b, &args, &kwargs),
        V::Bytes(b) => bytes_method(b, name, &args),
        V::List(l) => list_method(cx, l, name, args, kwargs).await,
        V::Dict(d) => match ops::is_counter(d).then(|| ops::counter_method(d, name, &args, &kwargs)).flatten() {
            Some(r) => r,
            None => dict_method(d, name, &args, &kwargs),
        },
        V::Set(s) => set_method(s, name, &args),
        V::Frozen(s) => frozen_method(s, name, &args),
        V::Tuple(t) => match name {
            "count" if args.len() != 1 || !kwargs.is_empty() => Err(Exc::type_error(format!("count() takes exactly one argument ({} given)", args.len()))),
            "index" if args.len() != 1 || !kwargs.is_empty() => seq_index(t, &args, kwargs.is_empty())?
                .map(|i| V::Int(i as i64))
                .ok_or_else(|| Exc::value_error("tuple.index(x): x not in tuple")),
            "index" => t.iter().position(|x| ops::eq_bool(x, &args[0])).map(|i| V::Int(i as i64)).ok_or_else(|| Exc::value_error("tuple.index(x): x not in tuple")),
            "count" if args.len() != 1 => Err(Exc::type_error(format!("tuple.count() takes exactly one argument ({} given)", args.len()))),
            "count" => Ok(V::Int(t.iter().filter(|x| ops::eq_bool(x, &args[0])).count() as i64)),
            _ => Err(no_attr(recv, name)),
        },
        V::DateTime(d) => datetime_method(d, name, &args, &kwargs),
        V::Tz(t) => tz_method(t, name, &args, &kwargs),
        V::Date(d) => date_method(d, name, &args, &kwargs),
        V::Time(t) => time_method(t, name, &args, &kwargs),
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
        V::Inst(i) if i.desc.x.ntuple && find_method(i.desc.methods, name).is_none() => match ntuple_method(cx, recv, i, name, args.clone(), kwargs.clone()).await {
            Some(r) => r,
            None => {
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
            // pydantic v1's `.json()` / `.copy()`, still on v2 models (deprecated)
            "json" | "copy" if !i.desc.dataclass && !i.desc.open && find_method(i.desc.methods, name).is_none() => {
                Box::pin(pyd_v1_method(cx, recv, i, name, args, kwargs)).await
            }
            // a class method read on an instance
            "model_construct" if !i.desc.open && !i.desc.dataclass && find_method(i.desc.methods, "model_construct").is_none() => pyd::model_construct(cx, i.desc, args, kwargs).await,
            "model_dump" | "model_dump_json" => {
                for (k, _) in &kwargs {
                    if !matches!(k.as_str(), "mode" | "exclude_none" | "exclude_unset" | "exclude_defaults" | "by_alias" | "exclude" | "include" | "context") || (name == "model_dump_json" && k == "mode") {
                        return Err(Exc::type_error(format!("{name}({k}=) is not supported by py2axum")));
                    }
                }
                let o = pyd::DumpOpts {
                    json: name == "model_dump_json" || kw(&kwargs, "mode").and_then(|m| m.as_str().map(|s| s == "json")).unwrap_or(false),
                    exclude_none: kw(&kwargs, "exclude_none").map(ops::truthy).transpose()?.unwrap_or(false),
                    exclude_unset: kw(&kwargs, "exclude_unset").map(ops::truthy).transpose()?.unwrap_or(false),
                    by_alias: kw(&kwargs, "by_alias").map(ops::truthy).transpose()?.unwrap_or(false),
                    alias_cfg: kw(&kwargs, "by_alias").is_none_or(|v| v.is_none()),
                    exclude_defaults: kw(&kwargs, "exclude_defaults").map(ops::truthy).transpose()?.unwrap_or(false),
                    ..Default::default()
                };
                let by_alias = o.by_alias || (o.alias_cfg && i.desc.x.ser_by_alias);
                // `context=`: what serializers read as `info.context`
                let mut d = pyd::with_context(kw(&kwargs, "context").cloned(), || pyd::dump(recv, o))
                    .map_err(|x| if name == "model_dump_json" { pyd::ser_json_error(x) } else { x })?;
                // exclude= / include=: top-level field names (a set, list or tuple of names)
                for (opt, keep) in [("exclude", false), ("include", true)] {
                    let Some(sel) = kw(&kwargs, opt).filter(|v| !v.is_none()) else { continue };
                    if matches!(sel, V::Dict(_)) {
                        return Err(Exc::type_error(format!("py2axum: {name}({opt}=dict) is not supported (a set of field names only)")));
                    }
                    let names: Vec<String> = ops::iter(sel)?.iter().map(ops::str_).collect::<R<_>>()?;
                    let keys: Vec<&str> = i.desc.fields.iter().filter(|f| names.iter().any(|n| n == f.name))
                        .map(|f| if by_alias { f.ser_key() } else { f.name })
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
                let inst = pyd::Inst { raw: i.raw,
                    desc: i.desc,
                    vals: super::pyd::Slots::new(vals),
                    set: Mutex::new(i.set.lock().clone()),
                    extra: super::pyd::Slots::new(i.extra.lock().clone()),
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
        // `P._make(iterable)`: the class built from exactly as many items as fields
        V::Class(c) if matches!(c.kind, ClassKind::Schema(s) if s.x.ntuple) && name == "_make" => {
            let ClassKind::Schema(d) = c.kind else { unreachable!() };
            let [it] = args.as_slice() else {
                return Err(Exc::type_error(format!("{}._make() takes 2 positional arguments but {} were given", d.name, args.len() + 1)));
            };
            let items = ops::iter(it)?;
            if items.len() != d.fields.len() {
                return Err(Exc::type_error(format!("Expected {} arguments, got {}", d.fields.len(), items.len())));
            }
            Box::pin(pyd::dataclass_new(cx, d, items, vec![])).await
        }
        V::Class(c) => match (&c.kind, name) {
            // a Core table's statement methods
            (ClassKind::Model(m), "join" | "outerjoin") if m.core => orm::table_join(recv, name, args, kwargs),
            (ClassKind::Model(m), "alias" | "select" | "insert" | "update" | "delete") if m.core => {
                if !args.is_empty() || !kwargs.is_empty() {
                    return Err(Exc::type_error(format!("py2axum: Table.{name}() takes no arguments here")));
                }
                match name {
                    "alias" => orm::aliased(recv),
                    "select" => orm::select(vec![recv.clone()]),
                    "insert" => orm::insert(recv, false),
                    "update" => orm::update(recv),
                    _ => orm::delete(recv),
                }
            }
            (ClassKind::Model(m), _) if m.hybrid(name).is_some_and(|h| h.method) => {
                // `Model.hybrid_method(x)`: its SQL expression
                let h = m.hybrid(name).unwrap();
                (h.expr.unwrap_or(h.get))(cx, recv.clone(), super::pack(args.clone(), kwargs.clone())).await
            }
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
                    return Err(Exc::validation_titled(
                        &VALIDATION_ERROR,
                        vec![pyd::ErrDetail {
                            kind: "model_type",
                            loc: vec![],
                            msg: format!("Input should be a valid dictionary or instance of {}", s.name),
                            input: obj,
                            ctx: Some(vec![("class_name", V::str(s.name))]), custom: false }],
                        s.name,
                    ));
                }
                pyd::construct_mode(cx, s, obj, !from_attrs).await
            }
            (ClassKind::Schema(s), "model_construct") => pyd::model_construct(cx, s, args, kwargs).await,
            // pydantic v1's class methods, still on v2 models (deprecated: their warning is filtered by default)
            (ClassKind::Schema(s), "parse_obj" | "validate" | "from_orm" | "construct" | "schema" | "parse_raw" | "update_forward_refs")
                if !s.dataclass && !s.open && find_method(s.methods, name).is_none() =>
            {
                Box::pin(pyd_v1_class_method(cx, recv, s, name, args, kwargs)).await
            }
            (ClassKind::Schema(s), "model_json_schema") => match s.json_schema {
                Some(j) => pyd::loads(j),
                None => Err(Exc::type_error(format!("py2axum: {}.model_json_schema() was not computed", s.name))),
            },
            (ClassKind::Schema(s), "model_validate_json") => {
                if let Some((k, _)) = kwargs.first() {
                    return Err(Exc::type_error(format!("py2axum: model_validate_json({k}=) is not supported")));
                }
                // str, bytes (`await request.body()`) or anything else (json_type), parsed by jiter
                let [data] = &args[..] else {
                    // more than one is refused by the transpiler
                    return Err(Exc::type_error("BaseModel.model_validate_json() missing 1 required positional argument: 'json_data'"));
                };
                pyd::construct_json(cx, s, data).await
            }
            (ClassKind::Model(m), _) if m.class_methods.contains(&name) => match find_method(m.methods, name) {
                Some((_, f)) => f(cx, recv.clone(), super::pack(args, kwargs)).await,
                None => unreachable!("class method {name} not in the method table"),
            },
            // anything else is read then called, as CPython does (`alias.NT()` where `class G: NT = _NT`): getattr
            // knows the class attributes and raises the same AttributeError otherwise
            _ => {
                let f = getattr(cx, recv, name).await?;
                Box::pin(call_value(cx, &f, args, kwargs)).await
            }
        },
        V::Session(s) => {
            // positional/keyword arguments beyond the supported ones are refused, never ignored
            let (max_pos, kws): (usize, &[&str]) = match name {
                "get" => (2, &["options"]),
                "refresh" => (2, &["attribute_names"]),
                "expire" => (2, &["attribute_names"]),
                "execute" | "scalar" | "scalars" => (2, &["params", "execution_options"]),
                "add" | "add_all" | "delete" | "expunge" => (1, &[]),
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
            "execute" | "scalar" | "scalars" => {
                // execution_options={...}: as the statement's own .execution_options(...)
                let stmt = match kw("execution_options") {
                    Some(V::Dict(d)) => {
                        let opts = d.lock().iter().map(|(_, (k, v))| Ok((ops::str_(k)?.to_string(), v.clone()))).collect::<R<Vec<_>>>()?;
                        orm::sql_method(&args[0], "execution_options", vec![], opts)?
                    }
                    Some(other) if !other.is_none() => return Err(Exc::type_error("py2axum: execution_options= must be a dict")),
                    _ => args[0].clone(),
                };
                let params = args.get(1).cloned().or_else(|| kw("params"));
                match name {
                    "execute" => s.execute_params(&stmt, params.as_ref()).await,
                    "scalar" => s.scalar_params(&stmt, params.as_ref()).await,
                    _ => s.scalars_params(&stmt, params.as_ref()).await,
                }
            }
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
            "in_transaction" => Ok(V::Bool(s.in_transaction().await)),
            "in_nested_transaction" => Ok(V::Bool(s.in_nested_transaction().await)),
            // `async with session.begin():` (its transaction, committed or rolled back at the block's end); the
            // name "" marks the session's own transaction, not a savepoint
            "begin" => {
                if s.in_transaction().await {
                    return Err(Exc::msg(&INVALID_REQUEST_ERROR, "A transaction is already begun on this Session."));
                }
                Ok(V::native(Native::Savepoint(s.clone(), String::new())))
            }
            "query" => orm::query(s, args),
            "close" | "aclose" => s.close().await.map(|_| V::None),
            "expunge_all" => s.expunge(None).await.map(|_| V::None),
            "expire_all" => s.expire_all().await.map(|_| V::None),
            "expire" => s.expire(&args[0], args.get(1).cloned().or_else(|| kw("attribute_names")).as_ref()).await.map(|_| V::None),
            "expunge" => s.expunge(Some(&args[0])).await.map(|_| V::None),
            "connection" => s.connection().await,
            // the engine the session is bound to (sync side for get_bind(), but the same process pool)
            "get_bind" => Ok(V::native(Native::Engine(true))),
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
                "values" => Ok(V::list(r.headers.iter().map(|(_, x)| V::str(x)).collect())),
                "items" => Ok(V::list(r.headers.iter().map(|(k, x)| V::tuple(vec![V::str(k), V::str(x)])).collect())),
                // Headers.getlist(key): every value of the (case-insensitive) name, in order
                "getlist" if args.len() == 1 && kwargs.is_empty() => {
                    let key = ops::str_(&args[0])?.to_lowercase();
                    Ok(V::list(r.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(&key)).map(|(_, x)| V::str(x)).collect()))
                }
                _ => Err(no_attr(recv, name)),
            },
            Native::WebSocket(s) => super::ws::method(cx, s, name, args, kwargs).await,
            Native::WsIter(s, kind) if name == "__anext__" => match super::ws::iter_next(s, kind).await? {
                Some(v) => Ok(v),
                None => Err(Exc::new(&STOP_ASYNC_ITERATION, vec![])),
            },
            Native::Request(r, o) => match name {
                "is_disconnected" => Ok(V::Bool(r.disconnected.load(std::sync::atomic::Ordering::Relaxed))),
                "body" => Ok(V::Bytes(Arc::from(&r.read_body(*o)?[..]))),
                // `request.stream()`: the body in one chunk then b"" (the binary has it whole)
                "stream" if args.is_empty() && kwargs.is_empty() => {
                    Ok(V::native(Native::AsyncItems(parking_lot::Mutex::new(Some(r.stream_items(*o)?)))))
                }
                // Starlette: `json.loads(await self.body())` (bytes: json.detect_encoding, UnicodeDecodeError)
                "json" => super::libs::json_loads(&V::Bytes(Arc::from(&r.read_body(*o)?[..]))),
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
            Native::PwdHash(p) => super::auth::pwd_method(p, name, &args, &kwargs),
            Native::PassCtx(c) => super::auth::passlib_method(cx, c, name, &args, &kwargs).await,
            Native::Row(names, vals) if name == "_asdict" => V::dict_from(names.iter().zip(vals.iter()).map(|(k, x)| (V::str(k), x.clone())).collect()),
            Native::Row(_, vals) if name == "_tuple" => Ok(V::tuple(vals.to_vec())),
            Native::UrlParts(u) if name == "geturl" => Ok(V::str(super::stdlib::url_parts_geturl(u))),
            Native::Timer(t) => super::thread::timer_method(t, name),
            Native::UrlParts(u) if name == "_replace" => super::stdlib::url_parts_replace(u, &args, &kwargs),
            Native::Task(t) => web::task_method(cx, t, recv, name, &args).await,
            Native::Gen(g) => super::agen::method(g, recv, name, &args).await,
            Native::CtxVar(var) => super::agen::ctxvar_method(cx, var, name, &args),
            Native::McpServer(s) => super::mcp::server_method(s, name),
            Native::McpManager(s) => super::mcp::manager_method(cx, s, name, &args).await,
            Native::IniConfig(c) => super::ini::method(c, name, &args, &kwargs),
            Native::PydUrl(kind, u) if pyd::is_multi_host(kind) => match name {
                "hosts" => pyd::url_hosts(u),
                "unicode_string" => Ok(V::str(u.as_str())),
                _ => Err(Exc::attr_error(format!("'{kind}' object has no attribute '{name}'"))),
            },
            Native::PydUrl(_, u) if name == "unicode_string" => Ok(V::str(u.as_str())),

            Native::SessConn(s) => match name {
                "close" => s.close_connection().await.map(|_| V::None),
                // Core execution in the session's transaction (no ORM bulk mode, no synchronization)
                "execute" if !args.is_empty() && args.len() <= 2 && kwargs.is_empty() => s.execute_conn(&args[0], args.get(1)).await,
                _ => Err(no_attr(recv, name)),
            },
            Native::Engine(sync) => match name {
                // a connection: its own transaction, rolled back when the `(async) with` ends
                "connect" => Ok(V::Session(orm::Session::new(cx.app.pool.clone(), false, true, *sync, Arc::downgrade(cx)))),
                // a connection in a transaction: committed when the `(async) with` ends without an exception
                "begin" => Ok(V::Session(orm::Session::new(cx.app.pool.clone(), false, true, *sync, Arc::downgrade(cx)).begin_block())),
                "dispose" => Ok(V::None),
                _ => Err(no_attr(recv, name)),
            },
            Native::HttpClient(c) => super::http::client_method(cx, recv, c, name, args, kwargs).await,
            Native::JwkClient(c) => super::jwks::client_method(c, name, &args, &kwargs).await,
            Native::HttpResp(r) => super::http::resp_method(r, recv, name, &args, &kwargs),
            Native::WriteOnly(o, ri) => orm::write_only_method(o, *ri, name, args, kwargs),
            Native::Savepoint(sess, sp) => match name {
                "commit" if sp.is_empty() => sess.commit().await.map(|_| V::None),
                "rollback" if sp.is_empty() => sess.rollback().await.map(|_| V::None),
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
            Native::EmailMsg(m) => super::emailmsg::method(m, name, &args, &kwargs),
            Native::Xl(x) => super::xlsx::method(x, name, &args, &kwargs),
            Native::Rl(x) => super::rlbind::method(cx, recv, x, name, &args, &kwargs).await,
            Native::Deque(d) => super::deque::method(d, name, &args),
            Native::Array(a) => super::arr::method(a, name, &args),
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
            Native::Func(_) | Native::Builtin(..) => Err(no_attr(recv, name)),
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

/// A method whose decorators left a function object (`@wraps` wrappers, `@contextmanager`...): read on an instance
/// it is bound, the instance goes first. Another object (a descriptor, a partial) binds by its own rules: refused.
pub async fn call_decorated_method(cx: &Cx, d: &V, recv: V, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if !matches!(d, V::Native(n) if matches!(&**n, Native::PyFn(_) | Native::Unbound(_, false))) {
        return Err(Exc::type_error(format!(
            "py2axum: a method decorated into a '{}' object is not supported (only a function is bound to its instance)", d.type_name())));
    }
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(recv);
    all.extend(args);
    call_value(cx, d, all, kwargs).await
}

pub async fn call_value(cx: &Cx, f: &V, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if let V::Native(n) = f {
        if let Native::Pgp(o) = &**n {
            return super::pgpy::call(o);
        }
        if let Native::Bound(m, recv) = &**n {
            return m(cx, recv.clone(), super::pack(args, kwargs)).await;
        }
        if let Native::Unbound(m, true) = &**n {
            return m(cx, V::None, super::pack(args, kwargs)).await;
        }
        if let Native::Unbound(m, false) = &**n {
            let mut args = args.into_iter();
            let Some(recv) = args.next() else {
                return Err(Exc::type_error("py2axum: a method read on its class takes the instance as first argument"));
            };
            return m(cx, recv, super::pack(args.collect(), kwargs)).await;
        }
        if let Native::CallNext(next) = &**n {
            return super::asgi::call_next(cx, next).await;
        }
        if let Native::RouteOrig(run) = &**n {
            if !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: a route handler takes the request only"));
            }
            return super::web::call_original(cx, *run, &args).await;
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
        if let Native::CtDecorator(c) = &**n {
            if !kwargs.is_empty() {
                return Err(Exc::type_error("decorator() got an unexpected keyword argument"));
            }
            return super::ctools::decorate(c, &args);
        }
        // `async_sessionmaker(...)()`: a new session on the process pool
        if let Native::Maker(expire, autoflush, sync, params) = &**n {
            if !args.is_empty() || !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: calling a sessionmaker with arguments is not supported"));
            }
            let s = orm::Session::new(cx.app.pool.clone(), *expire, *autoflush, *sync, Arc::downgrade(cx));
            s.set_params(params.clone());
            return Ok(V::Session(s));
        }
        // a builtin type used as a function value (`map(str, xs)`, `defaultdict(list)`)
        if let Native::Type(t) = &**n {
            if *t != "dict" && *t != "datetime" && !kwargs.is_empty() {
                return Err(Exc::type_error(format!("{t}() takes no keyword arguments")));
            }
            return match *t {
                "str" => b_str(&args),
                "int" => b_int(&args),
                "float" => b_float(&args),
                "bool" => b_bool(&args),
                "list" => b_list(&args),
                "tuple" => b_tuple(&args),
                "set" => b_set(&args),
                "frozenset" => b_frozenset(&args),
                "dict" => b_dict(&args, &kwargs),
                "datetime" => super::libs::datetime_new(&args, &kwargs),
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
            if m.methods.iter().any(|(n, _, _)| *n == "__init__") {
                return orm::construct_init(cx, m, args, kwargs).await;
            }
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
            Native::Func(fv) | Native::Builtin(_, fv) => fv(cx, args).await,
            _ => Err(Exc::type_error(format!("'{}' object is not callable", f.type_name()))),
        },
        _ => Err(Exc::type_error(format!("'{}' object is not callable", f.type_name()))),
    }
}

/// Calling a Pydantic model, settings, dataclass or plain class known only at run time: the same
/// paths as a call by name (`dyn.py` `construct`), with the errors CPython raises.
pub async fn schema_new(cx: &Cx, s: &'static pyd::SchemaDesc, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    if std::ptr::eq(s, &super::libs::NAMESPACE) {
        return super::libs::namespace(&args, &kwargs);
    }
    if s.dataclass || s.x.ntuple {
        return pyd::dataclass_new(cx, s, args, kwargs).await;
    }
    if s.open && s.x.new_.is_some() {
        return Box::pin(plain_new(cx, s, args, kwargs)).await;
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
        return super::settings(cx, s, st.prefix, st.case_sensitive, st.none_str, st.ignore_empty, kwargs).await;
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

/// `Cls(...)` of a plain class with a `__new__`: `__new__(cls, ...)`, then `__init__` when it returned an instance
/// of the class (CPython's `type.__call__`)
pub async fn plain_new(cx: &Cx, s: &'static pyd::SchemaDesc, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let Some(new_) = s.x.new_ else { return Box::pin(schema_new(cx, s, args, kwargs)).await };
    let r = new_(cx, V::Class(s.class), super::pack(args.clone(), kwargs.clone())).await?;
    if let (V::Inst(i), Some(init)) = (&r, s.init) {
        if i.desc.class.is_subclass(s.class) {
            init(cx, r.clone(), super::pack(args, kwargs)).await?;
        }
    }
    Ok(r)
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
    if matches!(name, "find" | "rfind" | "index" | "rindex" | "count" | "startswith" | "endswith") && args.is_empty() {
        return Err(at_least(name, 1, 0));
    }
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
            let count = replace_count("str", args, kwargs)?;
            let (old, new) = (strs(&args[0])?, strs(&args[1])?);
            match count {
                Some(n) => V::str(s.replacen(&old, &new, n as usize)),
                None => V::str(s.replace(&old, &new)),
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
        "isascii" => V::Bool(s.is_ascii()),
        "isdigit" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.digit, char::is_numeric))),
        "isdecimal" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.decimal, |c| c.is_ascii_digit()))),
        "isnumeric" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.numeric, char::is_numeric))),
        "isalpha" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.alpha, char::is_alphabetic))),
        // Python: isalpha or isdecimal or isdigit or isnumeric (numeric includes the other two)
        "isalnum" => V::Bool(!s.is_empty() && s.chars().all(|c| str_class(c, |t| t.alpha, char::is_alphabetic) || str_class(c, |t| t.numeric, char::is_numeric))),
        "isspace" => V::Bool(!s.is_empty() && s.chars().all(py_space)),
        "isprintable" => V::Bool(s.chars().all(printable)),
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
            // a negative width pads nothing (it was 2^64 here: capacity overflow), a huge one is MemoryError
            let w = match &args[0] {
                V::Int(i) => (*i).max(0) as usize,
                _ => 0,
            };
            let n = s.chars().count();
            if n < w {
                super::ops::repeat_room(1, w - n, "padded string is too long")?;
            }
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
                V::Int(i) => (*i).max(0) as usize,
                _ => 0,
            };
            let fill = args.get(1).and_then(|f| f.as_str()).and_then(|f| f.chars().next()).unwrap_or(' ');
            let n = s.chars().count();
            if n < w {
                super::ops::repeat_room(fill.len_utf8(), w - n, "padded string is too long")?;
            }
            if n >= w {
                V::Str(s.clone())
            } else {
                let pad = w - n;
                let rep = |k: usize| std::iter::repeat_n(fill, k).collect::<String>();
                V::str(match name {
                    "ljust" => format!("{s}{}", rep(pad)),
                    "rjust" => format!("{}{s}", rep(pad)),
                    // CPython's `pad()`: the odd margin goes left when the width is odd
                    _ => format!("{}{s}{}", rep(pad / 2 + (pad & w & 1)), rep(pad - (pad / 2 + (pad & w & 1)))),
                })
            }
        }
        "format" => {
            let pos = |i: usize| args.get(i).cloned().ok_or_else(|| Exc::msg(&INDEX_ERROR, format!("Replacement index {i} out of range for positional args tuple")));
            V::str(format_with(s, &pos, &|n: &str| kw(kwargs, n).cloned().ok_or_else(|| Exc::new(&KEY_ERROR, vec![V::str(n)])), false)?)
        }
        // CPython reads each field through `mapping[key]` (a defaultdict inserts its default, a missing key raises
        // the mapping's KeyError); a positional field is refused before any lookup of its own
        "format_map" => {
            if args.len() != 1 || !kwargs.is_empty() {
                return Err(Exc::type_error(if !kwargs.is_empty() {
                    "str.format_map() takes no keyword arguments".to_string()
                } else {
                    format!("format_map() takes exactly one argument ({} given)", args.len())
                }));
            }
            let m = &args[0];
            let pos = |_: usize| Err(Exc::value_error("Format string contains positional fields"));
            V::str(format_with(s, &pos, &|n: &str| super::ops::getitem(m, &V::str(n)), false)?)
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
        "count" if args.len() != 1 => Err(Exc::type_error(format!("list.count() takes exactly one argument ({} given)", args.len()))),
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
                        items.extend(update_pairs(other)?);
                    }
                }
            }
            for (k, v) in kwargs {
                items.push((V::str(k), v.clone()));
            }
            let mut g = d.lock();
            for (k, v) in items {
                dict_put(&mut g, Key::dict_key(&k)?, k, v);
            }
            Ok(V::None)
        }
        // a defaultdict's copy keeps its factory (defaultdict.copy is __copy__)
        "copy" => ops::defaultdict_like(d, V::Dict(Arc::new(Mutex::new(d.lock().clone())))),
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
            V::DateTime(super::dutz::astimezone(d, tz)?)
        }
        "tzname" => match d.tz {
            Some(t) => t.tzname(&d.wall, d.fold)?.map(V::str).unwrap_or(V::None),
            None => V::None,
        },
        "dst" => match dt::dst_of(d)? {
            Some(s) => V::Delta(Duration::seconds(s)),
            None => V::None,
        },
        "replace" => {
            let w = d.wall;
            let mut tz = d.tz;
            let mut fold = d.fold;
            let mut f = [w.year() as i64, w.month() as i64, w.day() as i64, w.hour() as i64, w.minute() as i64, w.second() as i64, (w.nanosecond() / 1000) as i64];
            for (k, v) in kwargs {
                let slot = match k.as_str() {
                    "year" => 0, "month" => 1, "day" => 2, "hour" => 3, "minute" => 4, "second" => 5, "microsecond" => 6,
                    "tzinfo" => {
                        tz = libs::tz_of(v)?;
                        continue;
                    }
                    "fold" => {
                        fold = fold_arg(v)?;
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
            V::DateTime(DateTime { wall, tz, fold })
        }
        "utcoffset" => d.offset().map(|o| V::Delta(Duration::seconds(o as i64))).unwrap_or(V::None),
        "weekday" => V::Int(d.wall.weekday().num_days_from_monday() as i64),
        "isoweekday" => V::Int(d.wall.weekday().number_from_monday() as i64),
        "toordinal" => V::Int(d.wall.date().num_days_from_ce() as i64),
        "strftime" => V::str(libs::strftime(&d.wall, d.offset(), d.tz.and_then(|t| t.tzname(&d.wall, d.fold).ok().flatten()), &strs(&args[0])?)?),
        _ => return Err(Exc::attr_error(format!("'datetime.datetime' object has no attribute '{name}'"))),
    })
}

use chrono::TimeZone;

/// a `fold=` argument (datetime(), replace()): 0 or 1, CPython's messages (3.14 adds the value)
pub fn fold_arg(v: &V) -> R<u8> {
    Ok(match v {
        V::Int(i @ (0 | 1)) => *i as u8,
        V::Bool(b) => *b as u8,
        V::Int(i) if super::python() >= (3, 14) => return Err(Exc::value_error(format!("fold must be either 0 or 1, not {i}"))),
        V::Int(_) => return Err(Exc::value_error("fold must be either 0 or 1")),
        o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    })
}

/// the methods a tzinfo object has (pytz's localize/normalize, dateutil's is_ambiguous)
const TZ_METHODS: [&str; 7] = ["utcoffset", "dst", "tzname", "fromutc", "localize", "normalize", "is_ambiguous"];

fn tz_has_method(t: &Tz, m: &str) -> bool {
    match m {
        "localize" | "normalize" => matches!(t, Tz::PyUtc | Tz::Py(..) | Tz::PyFixed(_)),
        "is_ambiguous" => matches!(t, Tz::DuUtc | Tz::DuOffset(..) | Tz::DuLocal(_) | Tz::Du(_)),
        _ => true,
    }
}

/// CPython's name of a tzinfo's type in messages (`'datetime.timezone' object ...`)
pub fn tz_cpython_type(t: &Tz) -> &'static str {
    match t {
        Tz::Utc | Tz::Fixed(_) => "datetime.timezone",
        Tz::Zone(_) => "zoneinfo.ZoneInfo",
        _ => t.type_name(),
    }
}

/// a library tzinfo's own attributes (pytz's `zone`...)
fn tz_attr(t: &Tz, name: &str) -> Option<R> {
    match t {
        Tz::PyUtc | Tz::Py(..) | Tz::PyFixed(_) => super::pytzrt::attr(t, name),
        Tz::DuUtc | Tz::DuOffset(..) | Tz::DuLocal(_) | Tz::Du(_) => super::dutz::attr(t, name),
        _ => None,
    }
}

/// a method of a tzinfo: pytz's and dateutil's (their modules), the standard library's `utcoffset(dt)`, `dst(dt)`,
/// `tzname(dt)`, `fromutc(dt)`
fn tz_method(t: &Tz, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let lib = match t {
        Tz::PyUtc | Tz::Py(..) | Tz::PyFixed(_) => super::pytzrt::method(t, name, args, kwargs),
        Tz::DuUtc | Tz::DuOffset(..) | Tz::DuLocal(_) | Tz::Du(_) => super::dutz::method(t, name, args, kwargs),
        _ => None,
    };
    if let Some(r) = lib {
        return r;
    }
    let cls = tz_cpython_type(t);
    let short = cls.rsplit('.').next().unwrap_or(cls);
    let one = || -> R<V> {
        match (args, kwargs) {
            ([d], []) => Ok(d.clone()),
            _ => Err(Exc::type_error(format!("{short}.{name}() takes exactly one argument ({} given)", args.len() + kwargs.len()))),
        }
    };
    match name {
        "utcoffset" | "dst" | "tzname" => {
            let d = one()?;
            let (wall, fold) = match &d {
                V::DateTime(x) => (x.wall, x.fold),
                V::None if matches!(t, Tz::Zone(_)) => return Ok(V::None),
                V::None => (chrono::NaiveDateTime::default(), 0),
                o => return Err(Exc::type_error(format!("{name}() argument must be a datetime instance or None, not {}", o.type_name()))),
            };
            Ok(match name {
                "utcoffset" => V::Delta(Duration::seconds(t.offset_for_wall(&wall, fold) as i64)),
                "dst" => t.dst(&wall, fold)?.map(|s| V::Delta(Duration::seconds(s))).unwrap_or(V::None),
                _ => t.tzname(&wall, fold)?.map(V::str).unwrap_or(V::None),
            })
        }
        "fromutc" => match one()? {
            V::DateTime(x) if x.tz == Some(*t) => Ok(V::DateTime(dt::from_utc_checked(x.wall, *t)?)),
            V::DateTime(_) => Err(Exc::value_error("fromutc: dt.tzinfo is not self")),
            o => Err(Exc::type_error(format!("fromutc: argument must be a datetime, not {}", o.type_name()))),
        },
        _ => Err(Exc::attr_error(format!("'{cls}' object has no attribute '{name}'"))),
    }
}

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

/// `datetime.time` (naive): isoformat() / strftime()
fn time_method(t: &chrono::NaiveTime, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "isoformat" if args.is_empty() && kwargs.is_empty() => Ok(V::str(dt::time_iso(t))),
        "isoformat" => Err(Exc::type_error("py2axum: time.isoformat() with arguments (timespec=) is not supported")),
        // CPython formats a time as on 1900-01-01
        "strftime" if args.len() == 1 => Ok(V::str(libs::strftime(&chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap().and_time(*t),
                                                                   None, None, &strs(&args[0])?)?)),
        _ => Err(Exc::attr_error(format!("'datetime.time' object has no attribute '{name}'"))),
    }
}

fn date_method(d: &chrono::NaiveDate, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    Ok(match name {
        "isoformat" => V::str(dt::date_iso(d)),
        "weekday" => V::Int(d.weekday().num_days_from_monday() as i64),
        "isoweekday" => V::Int(d.weekday().number_from_monday() as i64),
        "toordinal" => V::Int(d.num_days_from_ce() as i64),
        "strftime" => V::str(libs::strftime(&d.and_hms_opt(0, 0, 0).unwrap(), None, None, &strs(&args[0])?)?),
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
    let errors = match arg_or_kw(args, kwargs, 1, "errors") {
        None => "strict".to_string(),
        Some(e) => ops::str_(e)?,
    };
    let enc = arg_or_kw(args, kwargs, 0, "encoding");
    let lossy = |b: &[u8], sig: bool| -> R {
        if errors == "strict" {
            return if sig { utf8_decode(b, 3) } else { utf8_decode(b, 0) };
        }
        decode_handled(b, &errors, false, what)
    };
    match codec(enc)? {
        Some("utf-8") => lossy(b, false),
        Some("utf-8-sig") => match b.strip_prefix(b"\xef\xbb\xbf") {
            Some(rest) => lossy(rest, true),
            None => lossy(b, false),
        },
        Some("latin-1") => Ok(V::str(b.iter().map(|&c| c as char).collect::<String>())),
        Some(_) => match b.iter().position(|c| *c >= 0x80) {
            None => Ok(V::str(b.iter().map(|&c| c as char).collect::<String>())),
            Some(_) if errors != "strict" => decode_handled(b, &errors, true, what),
            Some(p) => Err(Exc::msg(
                &UNICODE_DECODE_ERROR,
                format!("'ascii' codec can't decode byte 0x{:02x} in position {p}: ordinal not in range(128)", b[p]),
            )),
        },
        None => Err(Exc::type_error(format!("py2axum: {what}({}) is not supported (utf-8, utf-8-sig, latin-1, ascii)", ops::repr(enc.unwrap())?))),
    }
}

/// decoding with `errors=` ignore / replace / backslashreplace, as CPython's utf-8 and ascii decoders (each
/// invalid sequence is the maximal subpart, one U+FFFD for `replace`). The other handlers: surrogateescape
/// would need lone surrogates (not representable), xmlcharrefreplace / namereplace raise TypeError in CPython
/// for a decode error, a custom one is looked up lazily (LookupError): refused (a literal one at generation).
fn decode_handled(b: &[u8], errors: &str, ascii: bool, what: &str) -> R {
    let valid = if ascii { b.is_ascii() } else { std::str::from_utf8(b).is_ok() };
    if valid {
        return Ok(V::str(String::from_utf8_lossy(b).into_owned()));
    }
    if !matches!(errors, "ignore" | "replace" | "backslashreplace") {
        return Err(Exc::type_error(format!("py2axum: {what}(errors={errors:?}) is not supported (strict, ignore, replace, backslashreplace)")));
    }
    let mut out = String::with_capacity(b.len());
    let bad = |out: &mut String, seq: &[u8]| match errors {
        "replace" => out.push('\u{fffd}'),
        "backslashreplace" => seq.iter().for_each(|c| out.push_str(&format!("\\x{c:02x}"))),
        _ => {}
    };
    let mut rest = b;
    while !rest.is_empty() {
        if ascii {
            let n = rest.iter().position(|c| *c >= 0x80).unwrap_or(rest.len());
            out.extend(rest[..n].iter().map(|&c| c as char));
            if n < rest.len() {
                bad(&mut out, &rest[n..n + 1]);
            }
            rest = &rest[(n + 1).min(rest.len())..];
            continue;
        }
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let p = e.valid_up_to();
                out.push_str(std::str::from_utf8(&rest[..p]).unwrap());
                let n = e.error_len().unwrap_or(rest.len() - p);
                bad(&mut out, &rest[p..p + n]);
                rest = &rest[p + n..];
            }
        }
    }
    Ok(V::str(out))
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
            "backslashreplace" => {
                let n = c as u32;
                out.extend(if n < 0x100 { format!("\\x{n:02x}") } else if n < 0x10000 { format!("\\u{n:04x}") } else { format!("\\U{n:08x}") }.bytes());
            }
            "xmlcharrefreplace" => out.extend(format!("&#{};", c as u32).bytes()),
            // no lone surrogate exists in a Rust string: these two raise as "strict" does for any other character
            "strict" | "surrogateescape" | "surrogatepass" => {
                // CPython reports the whole run of unencodable characters
                let end = (i..chars.len()).find(|j| (chars[*j] as u32) < limit).unwrap_or(chars.len());
                let msg = if end - i == 1 {
                    format!("'{name}' codec can't encode character {} in position {i}: ordinal not in range({limit})", char_escape(c))
                } else {
                    format!("'{name}' codec can't encode characters in position {i}-{}: ordinal not in range({limit})", end - 1)
                };
                return Err(Exc::msg(&UNICODE_ENCODE_ERROR, msg));
            }
            other => return Err(Exc::type_error(format!("py2axum: str.encode(errors={other:?}) is not supported (strict, ignore, replace, backslashreplace, xmlcharrefreplace, surrogateescape, surrogatepass)"))),
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
        V::Native(n) => matches!(&**n, Native::RequestReceive | Native::Bound(..) | Native::Unbound(..) | Native::PyFn(_) | Native::Builtin(..) | Native::Type(_) | Native::Maker(..) | Native::CallNext(_) | Native::AsgiApp(_)),
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
            // CPython's MarkupIterator: a '{' that ends the string is a lone brace, a field left open is not
            return Err(Exc::value_error(if field.is_empty() { "Single '{' encountered in format string" } else { "expected '}' before end of string" }));
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

/// `itertools.chain(*its)` / `chain.from_iterable(its)` consumed whole: the items of each iterable, in order.
pub fn chain(its: &V) -> R {
    let mut out = Vec::new();
    for it in ops::iter(its)? {
        out.extend(ops::iter(&it)?);
    }
    Ok(V::list(out))
}

/// `string.Formatter().parse(s)` consumed whole (`list(...)`): CPython's MarkupIterator (Objects/stringlib/
/// unicode_format.h), one `(literal, field_name, format_spec, conversion)` tuple per item, same errors.
pub fn formatter_parse(s: &V) -> R {
    let V::Str(st) = s else { return Err(Exc::type_error(format!("expected str, got {}", s.type_name()))) };
    let c: Vec<char> = st.chars().collect();
    let text = |a: usize, b: usize| V::str(c[a..b].iter().collect::<String>());
    let (end, mut pos) = (c.len(), 0usize);
    let mut out = Vec::new();
    while pos < end {
        let start = pos;
        let (mut ch, mut markup) = ('\0', false);
        while pos < end {
            ch = c[pos];
            pos += 1;
            if ch == '{' || ch == '}' {
                markup = true;
                break;
            }
        }
        let at_end = pos >= end;
        let mut len = pos - start;
        if ch == '}' && (at_end || c[pos] != ch) {
            return Err(Exc::value_error("Single '}' encountered in format string"));
        }
        if at_end && ch == '{' {
            return Err(Exc::value_error("Single '{' encountered in format string"));
        }
        if !at_end {
            if c[pos] == ch {
                // an escaped brace: in the literal, no field follows
                pos += 1;
                markup = false;
            } else {
                len -= 1;
            }
        }
        let literal = text(start, start + len);
        if !markup {
            out.push(V::tuple(vec![literal, V::None, V::None, V::None]));
            continue;
        }
        // the field: name up to '}', ':' or '!' ('[...]' skipped), then a conversion and/or a format spec
        let name_start = pos;
        let mut ch = '\0';
        while pos < end {
            ch = c[pos];
            pos += 1;
            match ch {
                '{' => return Err(Exc::value_error("unexpected '{' in field name")),
                '[' => {
                    while pos < end && c[pos] != ']' {
                        pos += 1;
                    }
                    continue;
                }
                '}' | ':' | '!' => break,
                _ => continue,
            }
        }
        let name = text(name_start, pos.saturating_sub(1).max(name_start));
        let (mut spec, mut conv) = (V::str(""), V::None);
        if ch == '!' || ch == ':' {
            let mut done = false;
            if ch == '!' {
                if pos >= end {
                    return Err(Exc::value_error("end of string while looking for conversion specifier"));
                }
                conv = V::str(c[pos].to_string());
                pos += 1;
                if pos < end {
                    let d = c[pos];
                    pos += 1;
                    if d == '}' {
                        done = true;
                    } else if d != ':' {
                        return Err(Exc::value_error("expected ':' after conversion specifier"));
                    }
                }
            }
            if !done {
                let spec_start = pos;
                let mut count = 1;
                loop {
                    if pos >= end {
                        return Err(Exc::value_error("unmatched '{' in format spec"));
                    }
                    let d = c[pos];
                    pos += 1;
                    if d == '{' {
                        count += 1;
                    } else if d == '}' {
                        count -= 1;
                        if count == 0 {
                            spec = text(spec_start, pos - 1);
                            break;
                        }
                    }
                }
            }
        } else if ch != '}' {
            return Err(Exc::value_error("expected '}' before end of string"));
        }
        out.push(V::tuple(vec![literal, name, spec, conv]));
    }
    Ok(V::list(out))
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
        Some(V::Decimal(d)) => d.to_i64().map(V::Int).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer beyond 64 bits")),
        Some(V::Int(i)) => Ok(V::Int(*i)),
        Some(V::Bool(b)) => Ok(V::Int(*b as i64)),
        Some(V::Float(f)) => {
            ops::float_to_int(f.trunc())
        }
        Some(V::Str(s)) => {
            let base = match args.get(1) {
                Some(V::Int(b)) => *b as u32,
                _ => 10,
            };
            let t = s.trim().replace('_', "");
            i64::from_str_radix(&t, base).map(V::Int).map_err(|_| Exc::value_error(format!("invalid literal for int() with base {base}: {}", ops::str_repr(s))))
        }
        // bytes: parsed as their ASCII text, the error showing the bytes' repr
        Some(V::Bytes(b)) => {
            let base = match args.get(1) {
                Some(V::Int(b)) => *b as u32,
                _ => 10,
            };
            let bad = || Exc::value_error(format!("invalid literal for int() with base {base}: {}", ops::repr(&V::Bytes(b.clone())).unwrap_or_default()));
            let t = std::str::from_utf8(b).ok().filter(|t| t.is_ascii()).ok_or_else(bad)?;
            match b_int(&[V::str(t), V::Int(base as i64)]) {
                Ok(v) => Ok(v),
                Err(_) => Err(bad()),
            }
        }
        Some(other) => Err(Exc::type_error(format!("int() argument must be a string, a bytes-like object or a real number, not '{}'", other.type_name()))),
    }
}

/// The text of a number as CPython's `float()` and `int()` read a str: surrounding whitespace stripped, Unicode
/// decimal digits as their ASCII digit, an underscore only between two digits (removed); None otherwise.
fn number_text(s: &str, hex: bool) -> Option<String> {
    let t = s.trim_matches(py_space);
    let is_dec = |c: char| str_class(c, |t| t.decimal, |c| c.is_ascii_digit());
    let digit = |c: char| -> Option<char> {
        if c.is_ascii_digit() {
            return Some(c);
        }
        if !is_dec(c) {
            return None;
        }
        // Unicode decimal digits come in runs of ten, 0 to 9: the value is the distance to the run's start
        let mut k = 0u32;
        let mut cp = c as u32;
        while cp > 0 && char::from_u32(cp - 1).is_some_and(is_dec) {
            cp -= 1;
            k += 1;
        }
        char::from_digit(k % 10, 10)
    };
    let chars: Vec<char> = t.chars().collect();
    let mut out = String::with_capacity(t.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            let around = |j: Option<usize>| j.and_then(|j| chars.get(j)).is_some_and(|&x| (hex && x.is_ascii_hexdigit()) || is_dec(x));
            if !(around(i.checked_sub(1)) && around(Some(i + 1))) {
                return None;
            }
            continue;
        }
        match digit(c) {
            Some(d) => out.push(d),
            None if c.is_ascii() => out.push(c),
            None => return None,
        }
    }
    Some(out)
}

/// `float(str)`: CPython's grammar (sign, digits with a point and an exponent, `inf`/`infinity`/`nan` in any case)
fn float_text(s: &str) -> Option<f64> {
    let t = number_text(s, false)?;
    let body = t.strip_prefix(['+', '-']).unwrap_or(&t);
    let neg = t.starts_with('-');
    match body.to_ascii_lowercase().as_str() {
        "inf" | "infinity" => return Some(if neg { f64::NEG_INFINITY } else { f64::INFINITY }),
        "nan" => return Some(if neg { -f64::NAN } else { f64::NAN }),
        _ => {}
    }
    let b = body.as_bytes();
    let (mut i, mut digits) = (0, 0);
    while i < b.len() && b[i].is_ascii_digit() { i += 1; digits += 1; }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() { i += 1; digits += 1; }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') { i += 1; }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() { i += 1; }
        if i == start {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    t.parse::<f64>().ok()
}

pub fn b_float(args: &[V]) -> R {
    match args.first() {
        None => Ok(V::Float(0.0)),
        Some(V::Int(i)) => Ok(V::Float(*i as f64)),
        Some(V::Bool(b)) => Ok(V::Float(*b as i64 as f64)),
        Some(V::Float(f)) => Ok(V::Float(*f)),
        Some(V::Decimal(d)) => Ok(V::Float(d.to_f64())),
        Some(V::Str(s)) => float_text(s).map(V::Float).ok_or_else(|| Exc::value_error(format!("could not convert string to float: {}", ops::str_repr(s)))),
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

pub fn b_frozenset(args: &[V]) -> R {
    // frozenset(fs) is fs itself
    if let [fs @ V::Frozen(_)] = args {
        return Ok(fs.clone());
    }
    frozen_of(match args.first() {
        None => vec![],
        Some(v) => ops::iter(v)?,
    })
}

pub fn b_set(args: &[V]) -> R {
    set_of(match args.first() {
        None => vec![],
        Some(v) => ops::iter(v)?,
    })
}

/// The pairs of `dict(seq)` / `d.update(seq)`, with CPython's messages for an element that is not a pair
pub fn update_pairs(seq: &V) -> R<Vec<(V, V)>> {
    // a Mapping (request.headers): keys() then __getitem__ (the first value of a repeated name)
    if let V::Native(n) = seq {
        if let Native::Headers(r) = &**n {
            let mut out: Vec<(V, V)> = Vec::new();
            for (k, _) in r.headers.iter() {
                let k = k.to_lowercase();
                out.push((V::str(&k), r.header(&k).map(V::str).unwrap_or(V::None)));
            }
            return Ok(out);
        }
    }
    let mut items = Vec::new();
    for (i, pair) in ops::iter(seq)?.into_iter().enumerate() {
        let kv = match ops::iter(&pair) {
            Ok(kv) => kv,
            // 3.14: the element's own TypeError, noted
            Err(_) if super::python() >= (3, 14) => {
                let e = Exc::type_error("object is not iterable");
                e.add_note(&format!("Cannot convert dictionary update sequence element #{i} to a sequence"));
                return Err(e);
            }
            Err(_) => return Err(Exc::type_error(format!("cannot convert dictionary update sequence element #{i} to a sequence"))),
        };
        if kv.len() != 2 {
            return Err(Exc::value_error(format!("dictionary update sequence element #{i} has length {}; 2 is required", kv.len())));
        }
        let mut kv = kv.into_iter();
        items.push((kv.next().unwrap(), kv.next().unwrap()));
    }
    Ok(items)
}

pub fn b_dict(args: &[V], kwargs: &[(String, V)]) -> R {
    let mut items = Vec::new();
    if let Some(src) = args.first() {
        match src {
            V::Dict(d) => items.extend(d.lock().values().cloned()),
            other => {
                items.extend(update_pairs(other)?);
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
        V::Set(x) | V::Frozen(x) => Arc::as_ptr(x) as *const () as usize,
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
/// `it = iter(x)` kept in a variable: an iterator stays itself; anything else iterable becomes an iterator over its
/// items as they are now (a list changed later is not seen, unlike CPython's list iterator)
pub fn b_iter_stored(x: &V) -> R {
    if let V::Native(n) = x {
        if matches!(&**n, Native::Iter(_) | Native::ValIter(_) | Native::CronGen(_)) {
            return Ok(x.clone());
        }
    }
    Ok(V::native(Native::Iter(Mutex::new(ops::iter(x)?.into()))))
}

/// `super().__init__(*args, **kwargs)` reaching `object.__init__`: arguments are a TypeError (CPython's message when
/// the class does not override `__new__`)
pub fn object_init_args(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() || !kwargs.is_empty() {
        return Err(Exc::type_error("object.__init__() takes exactly one argument (the instance to initialize)"));
    }
    Ok(V::None)
}

pub fn b_next_iter(args: &[V]) -> R {
    if let Some(r) = super::proto::next(&args[0], args.get(1)) {
        return r;
    }
    match &args[0] {
        V::Native(n) => match &**n {
            Native::Iter(q) => match q.lock().pop_front() {
                Some(v) => Ok(v),
                None => args.get(1).cloned().ok_or_else(|| Exc::new(&STOP_ITERATION, vec![])),
            },
            Native::ValIter(it) => match it.next()? {
                Some(v) => Ok(v),
                None => args.get(1).cloned().ok_or_else(|| Exc::new(&STOP_ITERATION, vec![])),
            },
            Native::Cron(c) => super::croniter::next(c),
            Native::CronGen(g) => match super::croniter::gen_next(g)? {
                Some(v) => Ok(v),
                None => args.get(1).cloned().ok_or_else(|| Exc::new(&STOP_ITERATION, vec![])),
            },
            _ => Err(Exc::type_error(format!("'{}' object is not an iterator", args[0].type_name()))),
        },
        other => Err(Exc::type_error(format!("'{}' object is not an iterator", other.type_name()))),
    }
}

pub fn b_next(args: &[V]) -> R {
    if let Some(first) = super::proto::first(&args[0]) {
        return match first? {
            Some(v) => Ok(v),
            None => args.get(1).cloned().ok_or_else(|| Exc::new(&STOP_ITERATION, vec![])),
        };
    }
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
            q.to_i64().map(V::Int).ok_or_else(|| Exc::msg(&OVERFLOW_ERROR, "py2axum: integer beyond 64 bits"))
        }
        (V::Decimal(d), Some(n)) => Ok(super::decimal::v(d.quantize(-n, super::decimal::Rounding::HalfEven)?)),
        (V::Int(i), _) => Ok(V::Int(*i)),
        (V::Bool(b), _) => Ok(V::Int(*b as i64)),
        (V::Float(f), None) => {
            let r = f.round();
            let r = if (f - f.trunc()).abs() == 0.5 { 2.0 * (f / 2.0).round() } else { r };
            ops::float_to_int(r)
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
    if let Some(r) = super::proto::unary(v, "__abs__") {
        return r;
    }
    Ok(match v {
        V::Int(i) => V::Int(i.abs()),
        V::Bool(b) => V::Int(*b as i64),
        V::Float(f) => V::Float(f.abs()),
        V::Decimal(d) => super::decimal::v(super::decimal::Dec { neg: false, ..(**d).clone() }.fix()?),
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
    // the range is a list here (not lazy): its length computed first, without overflow (`i += step` wrapped
    // past i64::MAX and never ended), and reserved up front, MemoryError rather than an aborted process
    let (lo, hi, st) = (start as i128, stop as i128, step as i128);
    let len = if st > 0 && lo < hi { (hi - lo - 1) / st + 1 } else if st < 0 && lo > hi { (lo - hi - 1) / -st + 1 } else { 0 };
    let len = usize::try_from(len).map_err(|_| Exc::msg(&MEMORY_ERROR, ""))?;
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|_| Exc::msg(&MEMORY_ERROR, ""))?;
    out.extend((0..len as i128).map(|k| V::Int((lo + k * st) as i64)));
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
    "issubclass", "vars", "format"];

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
        // a defaultdict or Counter is a dict registered with its factory
        V::Dict(d) => V::native(Native::Type(match ops::default_factory(d) {
            Some("Counter") => "collections.Counter",
            Some(_) => "collections.defaultdict",
            None => "dict",
        })),
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

/// `format(value, format_spec="")`
pub fn b_format(args: &[V]) -> R {
    match args {
        [v] => Ok(V::str(ops::format_spec(v, "")?)),
        [v, V::Str(spec)] => Ok(V::str(ops::format_spec(v, spec)?)),
        [_, o] => Err(Exc::type_error(format!("format() argument 2 must be str, not {}", o.type_name()))),
        _ => Err(Exc::type_error(format!("format expected at least 1 argument, got {}", args.len()))),
    }
}

pub fn b_divmod(args: &[V]) -> R {
    match args {
        [a, b] => {
            if matches!(b, V::Int(0) | V::Bool(false)) || matches!(b, V::Float(f) if *f == 0.0) {
                if ops::num(a).is_some() {
                    return Err(ops::zero_div_of("divmod", matches!(a, V::Float(_)) || matches!(b, V::Float(_))));
                }
            }
            Ok(V::tuple(vec![ops::floordiv(a, b)?, ops::modulo(a, b)?]))
        }
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
        // `bytes(msg)`: EmailMessage.__bytes__ = as_bytes()
        (Some(V::Native(n)), None) if matches!(&**n, Native::EmailMsg(_)) => match &**n {
            Native::EmailMsg(m) => return m.as_bytes(None),
            _ => unreachable!(),
        },
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
        "str" => matches!(v, V::Str(_)) || matches!(v, V::Enum(e, _) if matches!(e.kind, EnumKind::Str | EnumKind::StrEnum))
            || super::pgpy::as_str(v).is_some(),
        "int" => matches!(v, V::Int(_) | V::Bool(_)) || matches!(v, V::Enum(e, _) if matches!(e.kind, EnumKind::Int | EnumKind::IntEnum)),
        "float" => matches!(v, V::Float(_)),
        "bool" => matches!(v, V::Bool(_)),
        "dict" => matches!(v, V::Dict(_)),
        "list" => matches!(v, V::List(_)),
        "tuple" => matches!(v, V::Tuple(_)) || matches!(v, V::Inst(i) if i.desc.x.ntuple),
        "set" => matches!(v, V::Set(_)),
        "frozenset" => matches!(v, V::Frozen(_)),
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
        "complex" => matches!(v, V::Native(n) if matches!(&**n, Native::Complex(..))),
        // bytearray, memoryview: the runtime never makes one (dyn.TYPE_ONLY)
        "tzinfo" => matches!(v, V::Tz(_)),
        "timezone" => matches!(v, V::Tz(Tz::Utc | Tz::Fixed(_))),
        "ZoneInfo" => matches!(v, V::Tz(Tz::Zone(_))),
        "pytz.BaseTzInfo" => matches!(v, V::Tz(Tz::PyUtc | Tz::Py(..) | Tz::PyFixed(_))),
        "pytz.DstTzInfo" | "pytz.StaticTzInfo" => match v {
            V::Tz(t @ Tz::Py(..)) => (super::pytzrt::tz_repr(t).starts_with("<Dst")) == (ty == "pytz.DstTzInfo"),
            _ => false,
        },
        "tzutc" | "tzoffset" | "tzlocal" | "tzfile" | "tzstr" => matches!(v, V::Tz(t) if t.type_name() == ty),
        "tzrange" => matches!(v, V::Tz(t) if t.type_name() == "tzstr"),
        "cachetools.Cache" => matches!(v, V::Native(n) if matches!(&**n, Native::CtCache(_))),
        "cachetools.FIFOCache" | "cachetools.LRUCache" | "cachetools.TTLCache" => match v {
            V::Native(n) => matches!(&**n, Native::CtCache(c) if format!("cachetools.{}", c.name) == ty || (ty == "cachetools.TTLCache" && c.name == "_UnboundTTLCache")),
            _ => false,
        },
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
        Some(f) => match f(cx, V::Class(desc.class), vec![v.clone()]).await {
            Ok(V::None) => Err(invalid()),
            Ok(V::Enum(d, i)) if std::ptr::eq(d, desc) => Ok(V::Enum(d, i)),
            Ok(other) => Err(missing_context(
                Exc::type_error(format!(
                    "error in {}._missing_: returned {} instead of None or a valid member",
                    desc.name,
                    ops::repr(&other).unwrap_or_default()
                )),
                invalid(),
            )),
            Err(e) => Err(missing_context(e, invalid())),
        },
    }
}

/// `Enum.__new__`: an exception of `_missing_` other than a ValueError (or the TypeError of a value that is no
/// member) is raised with the never-raised `ValueError('x is not a valid E')` as its `__context__`
pub fn missing_context(e: Exc, invalid: Exc) -> Exc {
    if !e.isinstance(&VALUE_ERROR) {
        invalid.0.attrs.lock().insert("__unraised__".into(), V::Bool(true));
        e.0.attrs.lock().insert("__context__".into(), V::Exc(invalid));
    }
    e
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
        V::Frozen(_) => &["__and__", "__class__", "__class_getitem__", "__contains__", "__delattr__", "__dir__", "__doc__", "__eq__", "__format__", "__ge__", "__getattribute__", "__getstate__", "__gt__", "__hash__", "__init__", "__init_subclass__", "__iter__", "__le__", "__len__", "__lt__", "__ne__", "__new__", "__or__", "__rand__", "__reduce__", "__reduce_ex__", "__repr__", "__ror__", "__rsub__", "__rxor__", "__setattr__", "__sizeof__", "__str__", "__sub__", "__subclasshook__", "__xor__", "copy", "difference", "intersection", "isdisjoint", "issubset", "issuperset", "symmetric_difference", "union"],
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
    /// `str.isprintable()`: what `repr()` leaves unescaped
    pub printable: &'static [(u32, u32)],
}

/// a character `repr()` writes as is (CPython's `Py_UNICODE_ISPRINTABLE`)
pub fn printable(c: char) -> bool {
    str_class(c, |t| t.printable, |c| !c.is_control())
}

static STR_CLASSES: std::sync::OnceLock<&'static StrClasses> = std::sync::OnceLock::new();

pub fn set_str_classes(c: &'static StrClasses) {
    let _ = STR_CLASSES.set(c);
}

pub fn str_class(c: char, table: fn(&StrClasses) -> &'static [(u32, u32)], fallback: fn(char) -> bool) -> bool {
    match STR_CLASSES.get() {
        Some(t) => {
            let ranges = table(t);
            let cp = c as u32;
            ranges.binary_search_by(|&(lo, hi)| if hi < cp { std::cmp::Ordering::Less } else if lo > cp { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }).is_ok()
        }
        None => fallback(c),
    }
}
