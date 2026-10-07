//! `alembic.config.Config(file)`: the ini file read like configparser (`BasicInterpolation`, `%(here)s`),
//! `get_main_option` / `set_main_option` and their section variants. Nothing runs alembic itself.
use indexmap::IndexMap;
use parking_lot::Mutex;

use super::ops;
use super::v::*;

pub struct IniConfig {
    file: Option<String>,
    section: String,
    here: String,
    sections: Mutex<IndexMap<String, IndexMap<String, String>>>,
}

fn parse(text: &str) -> IndexMap<String, IndexMap<String, String>> {
    let mut out: IndexMap<String, IndexMap<String, String>> = IndexMap::new();
    let mut cur: Option<String> = None;
    let mut last: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            last = None;
            continue;
        }
        if t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        // a continuation line (indented) extends the previous value
        if line.starts_with(|c: char| c.is_whitespace()) {
            if let (Some(sec), Some(key)) = (&cur, &last) {
                if let Some(v) = out.get_mut(sec).and_then(|s| s.get_mut(key)) {
                    v.push('\n');
                    v.push_str(t);
                }
            }
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            cur = Some(name.to_string());
            out.entry(name.to_string()).or_default();
            last = None;
            continue;
        }
        if let (Some(sec), Some(i)) = (&cur, t.find(['=', ':'])) {
            let key = t[..i].trim().to_lowercase();
            out.entry(sec.clone()).or_default().insert(key.clone(), t[i + 1..].trim().to_string());
            last = Some(key);
        }
    }
    out
}

/// `alembic.config.Config(file_=None, ini_section="alembic")`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    let arg = |i: usize, n: &str| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v)).filter(|v| !v.is_none());
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "file_" | "ini_section")) {
        return Err(Exc::type_error(format!("py2axum: Config({k}=) is not supported")));
    }
    let file = arg(0, "file_").map(super::pathio::fspath).transpose()?;
    let section = arg(1, "ini_section").map(ops::str_).transpose()?.unwrap_or_else(|| "alembic".into());
    let (sections, here) = match &file {
        Some(f) => {
            let text = std::fs::read_to_string(f).unwrap_or_default();
            let abs = std::fs::canonicalize(f).ok().and_then(|p| p.parent().map(|d| d.display().to_string())).unwrap_or_else(|| ".".into());
            (parse(&text), abs)
        }
        None => (IndexMap::new(), ".".into()),
    };
    Ok(V::native(Native::IniConfig(std::sync::Arc::new(IniConfig { file, section, here, sections: Mutex::new(sections) }))))
}

impl IniConfig {
    /// BasicInterpolation: `%%` -> `%`, `%(name)s` -> the option (section, then DEFAULT/here)
    fn interpolate(&self, section: &str, value: &str, depth: usize) -> R<String> {
        if depth > 10 {
            return Err(Exc::value_error("InterpolationDepthError"));
        }
        let mut out = String::new();
        let mut rest = value;
        while let Some(i) = rest.find('%') {
            out.push_str(&rest[..i]);
            let tail = &rest[i..];
            if let Some(r) = tail.strip_prefix("%%") {
                out.push('%');
                rest = r;
            } else if let Some(r) = tail.strip_prefix("%(") {
                let end = r.find(")s").ok_or_else(|| Exc::value_error(format!("bad interpolation variable reference {tail:?}")))?;
                let name = r[..end].to_lowercase();
                let v = match self.raw(section, &name) {
                    Some(v) => self.interpolate(section, &v, depth + 1)?,
                    None if name == "here" => self.here.clone(),
                    None => return Err(Exc::value_error(format!("Bad value substitution: option {:?} in section {:?} contains an interpolation key {:?} which is not a valid option name.", name, section, name))),
                };
                out.push_str(&v);
                rest = &r[end + 2..];
            } else {
                return Err(Exc::value_error(format!("'%' must be followed by '%' or '(', found: {:?}", tail)));
            }
        }
        out.push_str(rest);
        Ok(out)
    }

    fn raw(&self, section: &str, name: &str) -> Option<String> {
        let s = self.sections.lock();
        s.get(section).and_then(|m| m.get(name)).or_else(|| s.get("DEFAULT").and_then(|m| m.get(name))).cloned()
    }
}

pub fn method(c: &IniConfig, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let arg = |i: usize, n: &str| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v));
    let (section, off) = match name {
        "get_main_option" | "set_main_option" => (c.section.clone(), 0),
        "get_section_option" | "set_section_option" => (ops::str_(arg(0, "section").ok_or_else(|| Exc::type_error("missing 'section'"))?)?, 1),
        _ => return Err(Exc::attr_error(format!("'Config' object has no attribute '{name}'"))),
    };
    let key = ops::str_(arg(off, "name").ok_or_else(|| Exc::type_error("missing 'name'"))?)?.to_lowercase();
    if name.starts_with("get") {
        return match c.raw(&section, &key) {
            Some(v) => Ok(V::str(c.interpolate(&section, &v, 0)?)),
            None => Ok(arg(off + 1, "default").cloned().unwrap_or(V::None)),
        };
    }
    let value = ops::str_(arg(off + 1, "value").ok_or_else(|| Exc::type_error("missing 'value'"))?)?;
    // configparser validates the interpolation syntax on set
    let probe = value.replace("%%", "");
    if let Some(pos) = probe.find('%').filter(|&i| !probe[i..].starts_with("%(")) {
        return Err(Exc::value_error(format!("invalid interpolation syntax in {} at position {}", ops::repr(&V::str(&value))?, pos)));
    }
    c.sections.lock().entry(section).or_default().insert(key, value);
    Ok(V::None)
}

pub fn attr(c: &IniConfig, name: &str) -> R {
    match name {
        "config_file_name" => Ok(c.file.as_deref().map(V::str).unwrap_or(V::None)),
        "config_ini_section" => Ok(V::str(&c.section)),
        _ => Err(Exc::attr_error(format!("'Config' object has no attribute '{name}'"))),
    }
}
