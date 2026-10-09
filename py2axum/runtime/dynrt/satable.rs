//! `Model.__table__` of a mapped class, read for introspection: the table's name, its column collection
//! (`columns` / `c`: `in`, `[key]`, iteration, `keys()`, `get()`), each column's name, key, nullable,
//! primary_key and type. A type is known by the real SQLAlchemy class computed at translation time (its MRO
//! for `isinstance`, its `str()` and `repr()`); nothing else of SQLAlchemy's schema objects is available.

use super::v::*;

pub struct TableInfo {
    pub name: &'static str,
    pub cols: &'static [ColInfo],
}

pub struct ColInfo {
    pub table: &'static str,
    pub key: &'static str,
    pub name: &'static str,
    pub nullable: bool,
    pub primary_key: bool,
    /// `str(col.type)`: the type compiled by SQLAlchemy's default dialect
    pub type_str: &'static str,
    pub type_repr: &'static str,
    /// `module.qualname` of the type's class and its bases (`isinstance(col.type, JSON)`)
    pub mro: &'static [&'static str],
}

fn unsupported(what: &str, name: &str) -> Exc {
    Exc::type_error(format!("py2axum: {what}.{name} is not supported (Model.__table__ is read for name, columns, \
                             their name, key, nullable, primary_key and type only)"))
}

pub fn type_name(n: &Native) -> &'static str {
    match n {
        Native::SaTable(_) => "Table",
        Native::SaColumns(_) => "ReadOnlyColumnCollection",
        Native::SaColumn(_) => "Column",
        Native::SaType(c) => c.mro[0].rsplit('.').next().unwrap_or(c.mro[0]),
        _ => unreachable!(),
    }
}

pub fn attr(n: &Native, name: &str) -> R {
    Ok(match (n, name) {
        (Native::SaTable(t), "name") => V::str(t.name),
        (Native::SaTable(t), "columns" | "c") => V::native(Native::SaColumns(t)),
        (Native::SaTable(_), _) => return Err(unsupported("Table", name)),
        (Native::SaColumn(c), "name") => V::str(c.name),
        (Native::SaColumn(c), "key") => V::str(c.key),
        (Native::SaColumn(c), "nullable") => V::Bool(c.nullable),
        (Native::SaColumn(c), "primary_key") => V::Bool(c.primary_key),
        (Native::SaColumn(c), "type") => V::native(Native::SaType(c)),
        (Native::SaColumn(_), _) => return Err(unsupported("Column", name)),
        (Native::SaColumns(_), _) => return Err(unsupported("ColumnCollection", name)),
        (Native::SaType(c), _) => return Err(unsupported(type_name(&Native::SaType(c)), name)),
        _ => unreachable!(),
    })
}

fn find(t: &'static TableInfo, key: &str) -> Option<&'static ColInfo> {
    t.cols.iter().find(|c| c.key == key)
}

/// `key in table.columns`: a column key (SQLAlchemy raises ArgumentError for anything but a string)
pub fn contains(t: &'static TableInfo, item: &V) -> R<bool> {
    match item {
        V::Str(s) => Ok(find(t, s).is_some()),
        V::Native(n) if matches!(&**n, Native::SaColumn(_)) => {
            let Native::SaColumn(c) = &**n else { unreachable!() };
            Ok(t.cols.iter().any(|x| std::ptr::eq(x, *c)))
        }
        _ => Err(Exc::msg(&ARGUMENT_ERROR, "__contains__ requires a string argument")),
    }
}

pub fn getitem(t: &'static TableInfo, k: &V) -> R {
    match k {
        V::Str(s) => find(t, s).map(|c| V::native(Native::SaColumn(c))).ok_or_else(|| Exc::new(&KEY_ERROR, vec![k.clone()])),
        V::Int(i) => {
            let n = t.cols.len() as i64;
            let j = if *i < 0 { i + n } else { *i };
            if j < 0 || j >= n {
                return Err(Exc::msg(&INDEX_ERROR, "list index out of range"));
            }
            Ok(V::native(Native::SaColumn(&t.cols[j as usize])))
        }
        _ => Err(Exc::type_error(format!("py2axum: ColumnCollection[{}] is not supported (a key or an index)", k.type_name()))),
    }
}

pub fn iter(t: &'static TableInfo) -> Vec<V> {
    t.cols.iter().map(|c| V::native(Native::SaColumn(c))).collect()
}

pub fn method(t: &'static TableInfo, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    if !kwargs.is_empty() {
        return Err(Exc::type_error(format!("{name}() takes no keyword arguments")));
    }
    match (name, args) {
        ("keys", []) => Ok(V::list(t.cols.iter().map(|c| V::str(c.key)).collect())),
        ("values", []) => Ok(V::list(iter(t))),
        ("get", [k]) | ("get", [k, _]) => match k {
            V::Str(s) => Ok(find(t, s).map(|c| V::native(Native::SaColumn(c))).unwrap_or_else(|| args.get(1).cloned().unwrap_or(V::None))),
            _ => Err(Exc::type_error("py2axum: ColumnCollection.get() of a non-string key is not supported")),
        },
        _ => Err(unsupported("ColumnCollection", name)),
    }
}

/// `str()` / `repr()` of a type; None for the other schema objects (refused)
pub fn text(n: &Native, repr: bool) -> R<String> {
    match n {
        Native::SaType(c) => Ok(if repr { c.type_repr } else { c.type_str }.to_string()),
        Native::SaTable(t) if !repr => Ok(t.name.to_string()),
        Native::SaColumn(c) if !repr => Ok(format!("{}.{}", c.table, c.name)),
        _ => Err(Exc::type_error("py2axum: the repr of Model.__table__ objects is not supported")),
    }
}

/// `isinstance(v, <SQLAlchemy type class>)`: `qual` is the class's `module.qualname`
pub fn isinstance_type(v: &V, qual: &str) -> bool {
    matches!(v, V::Native(n) if matches!(&**n, Native::SaType(c) if c.mro.contains(&qual)))
}
