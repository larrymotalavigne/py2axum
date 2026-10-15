//! `unidecode` 1.3–1.4: `unidecode` / `unidecode_expect_ascii` / `unidecode_expect_nonascii(string, errors,
//! replace_str)` over Unidecode's own tables (unidectab.rs, tools/gen_unidecode.py), `UnidecodeError`.

use std::sync::OnceLock;

use super::ops;
use super::v::*;

/// the tables by section, an entry `None` where Unidecode has no replacement
fn table() -> &'static std::collections::HashMap<u32, Vec<Option<&'static str>>> {
    static T: OnceLock<std::collections::HashMap<u32, Vec<Option<&'static str>>>> = OnceLock::new();
    T.get_or_init(|| {
        super::unidectab::SECTIONS.iter()
            .map(|(sec, data)| (*sec, data.split('\x01').map(|e| if e == "\x02" { None } else { Some(e) }).collect()))
            .collect()
    })
}

/// `_get_repl_str`
fn repl(c: char) -> Option<&'static str> {
    let cp = c as u32;
    if cp > 0xeffff {
        return None;
    }
    table().get(&(cp >> 8)).and_then(|t| t.get((cp & 0xff) as usize).copied().flatten())
}

fn unidecode_error(msg: String, index: Option<usize>) -> Exc {
    let e = Exc::msg(&UNIDECODE_ERROR, msg);
    e.0.attrs.lock().insert("index".into(), index.map(|i| V::Int(i as i64)).unwrap_or(V::None));
    e
}

/// `unidecode(string, errors='ignore', replace_str='?')`; `expect_ascii`: the ASCII string itself, unchanged
pub fn unidecode(args: &[V], kwargs: &[(String, V)], expect_ascii: bool) -> R {
    let get = |i: usize, name: &str| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v));
    let Some(string) = get(0, "string") else {
        return Err(Exc::type_error(format!("{}() missing 1 required positional argument: 'string'",
                                           if expect_ascii { "unidecode_expect_ascii" } else { "unidecode_expect_nonascii" })));
    };
    let errors = get(1, "errors").cloned().unwrap_or_else(|| V::str("ignore"));
    let replace = get(2, "replace_str").cloned().unwrap_or_else(|| V::str("?"));
    let s = match string {
        V::Str(s) => s.clone(),
        o if expect_ascii => return Err(Exc::attr_error(format!("'{}' object has no attribute 'encode'", o.type_name()))),
        V::Bytes(_) => return Err(Exc::type_error("ord() expected string of length 1, but int found")),
        o => return Err(Exc::type_error(format!("'{}' object is not iterable", o.type_name()))),
    };
    if expect_ascii && s.is_ascii() {
        return Ok(string.clone());
    }
    let mut out = String::with_capacity(s.len());
    for (index, c) in s.chars().enumerate() {
        if c.is_ascii() {
            out.push(c);
            continue;
        }
        if let Some(r) = repl(c) {
            out.push_str(r);
            continue;
        }
        match errors.as_str() {
            Some("ignore") => {}
            Some("strict") => {
                return Err(unidecode_error(format!("no replacement found for character {} in position {index}",
                                                   ops::repr(&V::str(c.to_string()))?), Some(index)));
            }
            Some("replace") => match &replace {
                V::Str(r) => out.push_str(r),
                o => return Err(Exc::type_error(format!("sequence item {index}: expected str instance, {} found", o.type_name()))),
            },
            Some("preserve") => out.push(c),
            _ => return Err(unidecode_error(format!("invalid value for errors parameter {}", ops::repr(&errors)?), None)),
        }
    }
    Ok(V::str(out))
}
