//! Types used as values: builtin types (`Native::Type`), project classes (`V::Class`), library classes
//! (`Native::ExtType`, e.g. `pydantic.BaseModel`) and generic aliases / unions written in the source
//! (`Native::TypeExpr`, e.g. `list[Schema]`, backed by their validator). `isinstance`, `issubclass`,
//! `inspect.isclass`, `typing.get_args/get_origin/get_type_hints`, `TypeAdapter` of a run-time type.
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use super::pyd::{NumC, StrC, TD};
use super::v::*;

fn is_schema(c: &'static Class) -> bool {
    matches!(c.kind, ClassKind::Schema(s) if !s.open && !s.dataclass)
}

/// `isinstance(v, t)` with `t` a run-time value (a type, or a tuple of them)
pub fn isinstance(v: &V, t: &V) -> R<bool> {
    match t {
        V::Tuple(ts) => {
            for x in ts.iter() {
                if isinstance(v, x)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        V::Class(c) => Ok(super::methods::isinstance_class(v, c)),
        V::Native(n) => match &**n {
            Native::Type(name) => Ok(super::methods::isinstance_builtin(v, name)),
            Native::ExtType(name) => Ok(match *name {
                "pydantic.BaseModel" => matches!(v, V::Inst(i) if is_schema(i.desc.class)),
                "sqlalchemy.ext.asyncio.AsyncSession" => matches!(v, V::Session(_)),
                "sqlalchemy.orm.Session" | "sqlalchemy.ext.asyncio.AsyncConnection" => false,
                "sqlalchemy.ext.asyncio.AsyncEngine" => matches!(v, V::Native(m) if matches!(&**m, Native::Engine(false))),
                "types.GenericAlias" => matches!(v, V::Native(m) if matches!(&**m, Native::TypeExpr(..))),
                p if p.starts_with("prometheus_client.") => super::prom::isinstance(v, &p["prometheus_client.".len()..]),
                p if p.starts_with("starlette.") || p.starts_with("fastapi.") => super::routing::isinstance(v, p),
                other => return Err(Exc::type_error(format!("py2axum: isinstance(x, {other}) is not supported"))),
            }),
            Native::TypeExpr(..) => Err(Exc::type_error("isinstance() argument 2 cannot be a parameterized generic")),
            _ => Err(Exc::type_error("isinstance() arg 2 must be a type, a tuple of types, or a union")),
        },
        V::None => Err(Exc::type_error("isinstance() arg 2 must be a type, a tuple of types, or a union")),
        _ => Err(Exc::type_error("isinstance() arg 2 must be a type, a tuple of types, or a union")),
    }
}

fn is_class(v: &V) -> bool {
    matches!(v, V::Class(_)) || matches!(v, V::Native(n) if matches!(&**n, Native::Type(_) | Native::ExtType(_)))
}

/// `inspect.isclass(v)`
pub fn isclass(args: &[V]) -> R {
    let [v] = args else { return Err(Exc::type_error("isclass() takes 1 positional argument")) };
    Ok(V::Bool(is_class(v)))
}

/// `issubclass(a, b)`
pub fn issubclass(args: &[V]) -> R {
    let [a, b] = args else { return Err(Exc::type_error(format!("issubclass expected 2 arguments, got {}", args.len()))) };
    if let V::Tuple(bs) = b {
        for x in bs.iter() {
            if issubclass(&[a.clone(), x.clone()])?.is_true() {
                return Ok(V::Bool(true));
            }
        }
        return Ok(V::Bool(false));
    }
    if !is_class(a) {
        return Err(Exc::type_error("issubclass() arg 1 must be a class"));
    }
    Ok(V::Bool(match (a, b) {
        (V::Class(x), V::Class(y)) => x.is_subclass(y),
        (V::Class(x), V::Native(n)) => match &**n {
            Native::ExtType("pydantic.BaseModel") => is_schema(x),
            Native::ExtType(_) | Native::Type(_) => false,
            _ => return Err(Exc::type_error("issubclass() arg 2 must be a class, a tuple of classes, or a union")),
        },
        (V::Native(x), V::Native(y)) => match (&**x, &**y) {
            (Native::Type(p), Native::Type(q)) => p == q || (*p == "bool" && *q == "int") || *q == "object",
            (Native::ExtType(p), Native::ExtType(q)) => p == q,
            (_, Native::Type("object")) => true,
            (_, Native::Type(_) | Native::ExtType(_)) => false,
            _ => return Err(Exc::type_error("issubclass() arg 2 must be a class, a tuple of classes, or a union")),
        },
        (V::Native(_), V::Class(_)) => false,
        _ => return Err(Exc::type_error("issubclass() arg 2 must be a class, a tuple of classes, or a union")),
    }))
}

trait Truth {
    fn is_true(&self) -> bool;
}

impl Truth for V {
    fn is_true(&self) -> bool {
        matches!(self, V::Bool(true))
    }
}

static NO_NUM: NumC = NumC { ge: None, gt: None, le: None, lt: None };
static NO_STR: StrC = StrC { min: None, max: None, pattern: None, strip: false, lower: false, upper: false };
static TD_INT: TD = TD::Int(NumC { ge: None, gt: None, le: None, lt: None });
static TD_FLOAT: TD = TD::Float(NumC { ge: None, gt: None, le: None, lt: None });
static TD_STR: TD = TD::Str(StrC { min: None, max: None, pattern: None, strip: false, lower: false, upper: false });
static TD_BOOL: TD = TD::Bool;
static TD_ANY: TD = TD::Any;
static TD_NONE: TD = TD::NoneT;
static TD_LIST: TD = TD::List(None);
static TD_DICT: TD = TD::Dict(None);

/// the validator of a schema class, built once per class (a `&'static TD` made per call would leak)
pub fn schema_td(s: &'static super::pyd::SchemaDesc) -> &'static TD {
    static CACHE: std::sync::OnceLock<StdMutex<HashMap<usize, &'static TD>>> = std::sync::OnceLock::new();
    let mut m = CACHE.get_or_init(Default::default).lock().unwrap();
    m.entry(s as *const _ as usize).or_insert_with(|| Box::leak(Box::new(TD::Schema(s))))
}

/// a string made `&'static` once per distinct value (operators, separators: few distinct values, used per call)
pub fn intern(s: &str) -> &'static str {
    static CACHE: std::sync::OnceLock<StdMutex<std::collections::HashSet<&'static str>>> = std::sync::OnceLock::new();
    let mut m = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(v) = m.get(s) {
        return v;
    }
    let v: &'static str = Box::leak(s.to_string().into_boxed_str());
    m.insert(v);
    v
}

/// a type value as a validator (`TypeAdapter(t)` with `t` known at run time); a class's is built once
pub fn td_of(v: &V) -> R<&'static TD> {
    let _ = (&NO_NUM, &NO_STR);
    match v {
        V::Native(n) => match &**n {
            Native::TypeExpr(td, _) => Ok(td),
            Native::Type(t) => Ok(match *t {
                "int" => &TD_INT,
                "float" => &TD_FLOAT,
                "str" => &TD_STR,
                "bool" => &TD_BOOL,
                "list" => &TD_LIST,
                "dict" => &TD_DICT,
                "object" => &TD_ANY,
                other => return Err(Exc::type_error(format!("py2axum: TypeAdapter({other}) is not supported"))),
            }),
            _ => Err(Exc::type_error(format!("py2axum: TypeAdapter of a {} is not supported", v.type_name()))),
        },
        V::None => Ok(&TD_NONE),
        V::Class(c) => match c.kind {
            ClassKind::Schema(s) => Ok(schema_td(s)),
            ClassKind::Enum(e) => {
                static CACHE: std::sync::OnceLock<StdMutex<HashMap<usize, &'static TD>>> = std::sync::OnceLock::new();
                let mut m = CACHE.get_or_init(Default::default).lock().unwrap();
                Ok(*m.entry(e as *const _ as usize).or_insert_with(|| Box::leak(Box::new(TD::Enum(e, false)))))
            }
            _ => Err(Exc::type_error(format!("py2axum: TypeAdapter({}) is not supported", c.name))),
        },
        _ => Err(Exc::type_error(format!("py2axum: TypeAdapter of a {} is not supported", v.type_name()))),
    }
}

/// the value of a validator's type (what `get_args` returns for its members)
pub fn td_value(td: &'static TD) -> V {
    match td {
        TD::Len(t, _, _) => td_value(t),
        TD::Any => V::native(Native::ExtType("typing.Any")),
        TD::NoneT => V::native(Native::Type("NoneType")),
        TD::Bool => V::native(Native::Type("bool")),
        TD::Int(_) => V::native(Native::Type("int")),
        TD::Float(_) => V::native(Native::Type("float")),
        TD::Str(_) | TD::Email(_) => V::native(Native::Type("str")),
        TD::DateTime => V::native(Native::Type("datetime")),
        TD::Date => V::native(Native::Type("date")),
        TD::Time => V::native(Native::Type("time")),
        TD::Delta => V::native(Native::Type("timedelta")),
        TD::Schema(s) => V::Class(s.class),
        TD::Enum(e, _) => V::Class(e.class),
        TD::List(None) => V::native(Native::Type("list")),
        TD::Dict(None) => V::native(Native::Type("dict")),
        TD::Set(None) => V::native(Native::Type("set")),
        TD::Tuple(None) => V::native(Native::Type("tuple")),
        _ => V::native(Native::TypeExpr(td, "")),
    }
}

/// `typing.get_args(t)`
pub fn get_args(args: &[V]) -> R {
    let [t] = args else { return Err(Exc::type_error("get_args() takes 1 positional argument")) };
    let V::Native(n) = t else { return Ok(V::tuple(vec![])) };
    let Native::TypeExpr(td, _) = &**n else { return Ok(V::tuple(vec![])) };
    Ok(V::tuple(match td.bare() {
        TD::List(Some(x)) | TD::Set(Some(x)) => vec![td_value(x)],
        TD::Tuple(Some(x)) => vec![td_value(x), V::native(Native::Type("Ellipsis"))],
        TD::Dict(Some((k, x))) => vec![td_value(k), td_value(x)],
        TD::Optional(x) => vec![td_value(x), V::native(Native::Type("NoneType"))],
        TD::Union(xs) => xs.iter().map(|x| td_value(x)).collect(),
        TD::Literal(_) => return Err(Exc::type_error("py2axum: get_args(Literal[...]) is not supported")),
        _ => vec![],
    }))
}

/// `typing.get_origin(t)`
pub fn get_origin(args: &[V]) -> R {
    let [t] = args else { return Err(Exc::type_error("get_origin() takes 1 positional argument")) };
    let V::Native(n) = t else { return Ok(V::None) };
    let Native::TypeExpr(td, _) = &**n else { return Ok(V::None) };
    Ok(match td.bare() {
        TD::List(Some(_)) => V::native(Native::Type("list")),
        TD::Set(Some(_)) => V::native(Native::Type("set")),
        TD::Tuple(Some(_)) => V::native(Native::Type("tuple")),
        TD::Dict(Some(_)) => V::native(Native::Type("dict")),
        TD::Optional(_) | TD::Union(_) => V::native(Native::ExtType("typing.Union")),
        _ => V::None,
    })
}

/// `typing.get_type_hints(f)`: the annotations kept on a project function (resolved at compile time)
pub async fn get_type_hints(cx: &super::Cx, args: &[V], kwargs: &[(String, V)]) -> R {
    if !kwargs.is_empty() {
        return Err(Exc::type_error("py2axum: get_type_hints() options are not supported"));
    }
    let [f] = args else { return Err(Exc::type_error("get_type_hints() takes 1 positional argument")) };
    match super::methods::getattr(cx, f, "__annotations__").await {
        Ok(V::Dict(d)) => Ok(V::Dict(std::sync::Arc::new(parking_lot::Mutex::new(d.lock().clone())))),
        Ok(_) | Err(_) => Err(Exc::type_error(format!("py2axum: get_type_hints() of a {} without annotations kept", f.type_name()))),
    }
}
