//! `pathlib.Path` (POSIX semantics), `open()` file objects, `uuid.uuid4()`, and the `with` protocol
//! of these runtime values.
use std::io::{Read, Write};
use std::sync::Arc;

use parking_lot::Mutex;

use super::ops;
use super::v::*;

// ---------------------------------------------------------------- Path

/// PurePosixPath normalisation: no empty or "." parts, no trailing slash, "." for nothing.
pub fn norm(p: &str) -> String {
    let abs = p.starts_with('/');
    let parts: Vec<&str> = p.split('/').filter(|x| !x.is_empty() && *x != ".").collect();
    match (abs, parts.is_empty()) {
        (true, _) => format!("/{}", parts.join("/")),
        (false, true) => ".".into(),
        (false, false) => parts.join("/"),
    }
}

pub fn path(p: &str) -> V {
    V::native(Native::Path(norm(p)))
}

pub fn fspath(v: &V) -> R<String> {
    match v {
        V::Str(s) => Ok(s.to_string()),
        V::Native(n) => match &**n {
            Native::Path(p) => Ok(p.clone()),
            _ => Err(Exc::type_error(format!("expected str, bytes or os.PathLike object, not {}", v.type_name()))),
        },
        _ => Err(Exc::type_error(format!("expected str, bytes or os.PathLike object, not {}", v.type_name()))),
    }
}

pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        norm(b)
    } else if a == "." {
        norm(b)
    } else {
        norm(&format!("{a}/{b}"))
    }
}

/// `Path(*segments)`
pub fn path_new(args: &[V]) -> R {
    let mut p = ".".to_string();
    for a in args {
        p = join(&p, &fspath(a)?);
    }
    Ok(path(&p))
}

/// `a / b` with a Path on either side
pub fn truediv(a: &V, b: &V) -> Option<R> {
    let is_path = |v: &V| matches!(v, V::Native(n) if matches!(&**n, Native::Path(_)));
    if !is_path(a) && !is_path(b) {
        return None;
    }
    Some((|| Ok(path(&join(&fspath(a)?, &fspath(b)?))))())
}

fn name_of(p: &str) -> &str {
    if p == "." || p == "/" {
        return "";
    }
    p.rsplit('/').next().unwrap_or("")
}

fn suffix_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

fn parent_of(p: &str) -> String {
    match p.rfind('/') {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(i) => p[..i].to_string(),
    }
}

fn parts_of(p: &str) -> Vec<V> {
    let mut out = Vec::new();
    if p.starts_with('/') {
        out.push(V::str("/"));
    }
    out.extend(p.split('/').filter(|x| !x.is_empty() && *x != ".").map(V::str));
    out
}

fn os_err(e: std::io::Error, p: &str) -> Exc {
    use std::io::ErrorKind::*;
    let (class, errno, text): (&'static Class, i32, &str) = match e.kind() {
        NotFound => (&FILE_NOT_FOUND_ERROR, 2, "No such file or directory"),
        AlreadyExists => (&FILE_EXISTS_ERROR, 17, "File exists"),
        PermissionDenied => (&PERMISSION_ERROR, 13, "Permission denied"),
        IsADirectory => (&IS_A_DIRECTORY_ERROR, 21, "Is a directory"),
        _ => (&OS_ERROR, e.raw_os_error().unwrap_or(0), "OS error"),
    };
    let text = if class.is_subclass(&OS_ERROR) && std::ptr::eq(class, &OS_ERROR) { e.to_string() } else { text.to_string() };
    Exc::msg(class, format!("[Errno {errno}] {text}: {}", ops::str_repr(p)))
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

pub fn path_attr(p: &str, name: &str) -> R {
    Ok(match name {
        "name" => V::str(name_of(p)),
        "suffix" => V::str(suffix_of(name_of(p))),
        "suffixes" => {
            let n = name_of(p);
            let n = n.strip_prefix('.').unwrap_or(n);
            V::list(n.split('.').skip(1).filter(|s| !s.is_empty()).map(|s| V::str(format!(".{s}"))).collect())
        }
        "stem" => {
            let n = name_of(p);
            V::str(&n[..n.len() - suffix_of(n).len()])
        }
        "parent" => path(&parent_of(p)),
        "parts" => V::tuple(parts_of(p)),
        _ => return Err(Exc::attr_error(format!("'PosixPath' object has no attribute '{name}'"))),
    })
}

fn absolute(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        let cwd = std::env::current_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_else(|_| "/".into());
        join(&cwd, p)
    }
}

pub fn path_method(p: &str, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let flag = |k: &str, i: usize| -> R<bool> { kw(kwargs, k).or(args.get(i)).map(ops::truthy).transpose().map(|b| b.unwrap_or(false)) };
    if p.contains('\0') {
        // CPython: the predicates answer False, a system call refuses the path (ValueError)
        match name {
            "exists" | "is_file" | "is_dir" => return Ok(V::Bool(false)),
            "resolve" => return Err(Exc::value_error("lstat: embedded null character in path")),
            "mkdir" => return Err(Exc::value_error("mkdir: embedded null character in path")),
            "unlink" => return Err(Exc::value_error("unlink: embedded null character in path")),
            "read_bytes" | "read_text" | "write_bytes" | "write_text" => return Err(Exc::value_error("embedded null byte")),
            _ => {}
        }
    }
    match name {
        "exists" => Ok(V::Bool(std::fs::metadata(p).is_ok())),
        "is_file" => Ok(V::Bool(std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false))),
        "is_dir" => Ok(V::Bool(std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false))),
        "mkdir" => {
            let (parents, exist_ok) = (flag("parents", 1)?, flag("exist_ok", 2)?);
            if std::path::Path::new(p).exists() {
                return if exist_ok && std::path::Path::new(p).is_dir() {
                    Ok(V::None)
                } else {
                    Err(os_err(std::io::Error::from(std::io::ErrorKind::AlreadyExists), p))
                };
            }
            let r = if parents { std::fs::create_dir_all(p) } else { std::fs::create_dir(p) };
            match r {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && exist_ok && std::path::Path::new(p).is_dir() => Ok(V::None),
                Err(e) => Err(os_err(e, p)),
                // create_dir_all succeeds on an existing directory: Python raises unless exist_ok
                Ok(()) => Ok(V::None),
            }
        }
        "unlink" => match std::fs::remove_file(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && flag("missing_ok", 0)? => Ok(V::None),
            Err(e) => Err(os_err(e, p)),
            Ok(()) => Ok(V::None),
        },
        // `resolve()` = os.path.realpath (symlinks followed even below a missing tail, like CPython)
        "resolve" => Ok(path(&super::stdlib::realpath(&absolute(p)))),
        "absolute" => {
            let a = absolute(p);
            let parts: Vec<&str> = a.split('/').filter(|x| !x.is_empty() && *x != ".").collect();
            Ok(path(&format!("/{}", parts.join("/"))))
        }
        "relative_to" => {
            let other = norm(&fspath(args.first().ok_or_else(|| Exc::type_error("relative_to() missing argument"))?)?);
            let (mine, base) = (parts_of(p), parts_of(&other));
            let ok = base.len() <= mine.len() && mine.iter().zip(&base).all(|(a, b)| ops::eq_bool(a, b));
            if !ok || (other == "." && p.starts_with('/')) {
                return Err(Exc::value_error(format!("{} is not in the subpath of {}", ops::str_repr(p), ops::str_repr(&other))));
            }
            let rest: Vec<String> = mine[base.len()..].iter().map(|x| ops::str_(x).unwrap_or_default()).collect();
            Ok(path(&rest.join("/")))
        }
        "with_suffix" => {
            let s = match args.first() {
                Some(V::Str(s)) => s.to_string(),
                _ => return Err(Exc::type_error("with_suffix() takes a str")),
            };
            if !s.is_empty() && (!s.starts_with('.') || s == "." || s.contains('/')) {
                return Err(Exc::value_error(format!("Invalid suffix {}", ops::str_repr(&s))));
            }
            let n = name_of(p);
            if n.is_empty() {
                return Err(Exc::value_error(format!("PosixPath({}) has an empty name", ops::str_repr(p))));
            }
            let stem = &n[..n.len() - suffix_of(n).len()];
            Ok(path(&join(&parent_of(p), &format!("{stem}{s}"))))
        }
        "joinpath" => {
            let mut q = p.to_string();
            for a in args {
                q = join(&q, &fspath(a)?);
            }
            Ok(path(&q))
        }
        "read_bytes" => std::fs::read(p).map(|b| V::Bytes(Arc::from(b))).map_err(|e| os_err(e, p)),
        // read_text(encoding=None, errors=None): bytes.decode (the locale's encoding is utf-8 here)
        "read_text" => {
            if args.len() > 2 || kwargs.iter().any(|(k, _)| k != "encoding" && k != "errors") {
                return Err(Exc::type_error("py2axum: Path.read_text(encoding=, errors=) only"));
            }
            let b = std::fs::read(p).map_err(|e| os_err(e, p))?;
            let ea: Vec<V> = (0..2).map(|i| args.get(i).or_else(|| kw(kwargs, ["encoding", "errors"][i])).cloned().unwrap_or(V::None)).collect();
            let ea: Vec<V> = if ea[1].is_none() { ea[..1].iter().filter(|v| !v.is_none()).cloned().collect() } else { vec![if ea[0].is_none() { V::str("utf-8") } else { ea[0].clone() }, ea[1].clone()] };
            super::methods::bytes_decode(&b, &ea, &[], "read_text")
        }
        "write_bytes" => {
            if args.len() != 1 || !kwargs.is_empty() {
                return Err(Exc::type_error("write_bytes() takes exactly one argument"));
            }
            let V::Bytes(b) = &args[0] else { return Err(Exc::type_error(format!("memoryview: a bytes-like object is required, not '{}'", args[0].type_name()))) };
            std::fs::write(p, &b[..]).map_err(|e| os_err(e, p))?;
            Ok(V::Int(b.len() as i64))
        }
        // write_text(data, encoding=None, errors=None, newline=None): str.encode; newline translation refused
        "write_text" => {
            if args.len() > 4 || kwargs.iter().any(|(k, _)| !matches!(k.as_str(), "data" | "encoding" | "errors" | "newline")) {
                return Err(Exc::type_error("py2axum: Path.write_text(data, encoding=, errors=, newline=) only"));
            }
            let at = |i: usize, n: &str| args.get(i).or_else(|| kw(kwargs, n)).cloned().unwrap_or(V::None);
            let data = match at(0, "data") {
                V::Str(s) => s,
                V::None => return Err(Exc::type_error("write_text() missing 1 required positional argument: 'data'")),
                o => return Err(Exc::type_error(format!("data must be str, not {}", o.type_name()))),
            };
            match at(3, "newline") {
                V::None => {}
                V::Str(n) if &*n == "\n" || n.is_empty() => {}
                _ => return Err(Exc::type_error("py2axum: Path.write_text(newline=) other than None, '' or '\\n' is not supported")),
            }
            let (enc, err) = (at(1, "encoding"), at(2, "errors"));
            let ea: Vec<V> = vec![if enc.is_none() { V::str("utf-8") } else { enc }, if err.is_none() { V::str("strict") } else { err }];
            let V::Bytes(b) = super::methods::str_encode(&data, &ea, &[])? else { unreachable!() };
            std::fs::write(p, &b[..]).map_err(|e| os_err(e, p))?;
            Ok(V::Int(data.chars().count() as i64))
        }
        "as_posix" | "__fspath__" => Ok(V::str(p)),
        "is_absolute" => Ok(V::Bool(p.starts_with('/'))),
        _ => Err(Exc::attr_error(format!("'PosixPath' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- open()

pub struct File {
    path: String,
    binary: bool,
    f: Mutex<Option<std::fs::File>>,
}

/// `open(file, mode="r", encoding=None)` (utf-8 text, no newline translation)
pub fn open(args: &[V], kwargs: &[(String, V)]) -> R {
    let p = fspath(args.first().or_else(|| kw(kwargs, "file")).ok_or_else(|| Exc::type_error("open() missing required argument 'file' (pos 1)"))?)?;
    if p.contains('\0') {
        return Err(Exc::value_error("embedded null byte"));
    }
    let mode = match args.get(1).or_else(|| kw(kwargs, "mode")) {
        None => "r".to_string(),
        Some(m) => ops::str_(m)?,
    };
    if let Some(enc) = args.get(3).or_else(|| kw(kwargs, "encoding")) {
        if !matches!(enc, V::None) && !matches!(ops::str_(enc)?.to_ascii_lowercase().replace('_', "-").as_str(), "utf-8" | "utf8") {
            return Err(Exc::type_error("py2axum: open() supports the utf-8 encoding only"));
        }
    }
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "file" | "mode" | "encoding")) {
        return Err(Exc::type_error(format!("py2axum: open({k}=) is not supported")));
    }
    let binary = mode.contains('b');
    let base: String = mode.chars().filter(|c| *c != 'b' && *c != 't').collect();
    let mut o = std::fs::OpenOptions::new();
    match base.as_str() {
        "r" => o.read(true),
        "w" => o.write(true).create(true).truncate(true),
        "a" => o.append(true).create(true),
        "x" => o.write(true).create_new(true),
        "r+" => o.read(true).write(true),
        "w+" => o.read(true).write(true).create(true).truncate(true),
        _ => return Err(Exc::value_error(format!("invalid mode: {}", ops::str_repr(&mode)))),
    };
    let f = o.open(&p).map_err(|e| os_err(e, &p))?;
    Ok(V::native(Native::File(File { path: p, binary, f: Mutex::new(Some(f)) })))
}

pub fn file_method(f: &File, name: &str, args: &[V]) -> R {
    let closed = || Exc::value_error("I/O operation on closed file.");
    match name {
        "read" => {
            let mut g = f.f.lock();
            let file = g.as_mut().ok_or_else(closed)?;
            let mut buf = Vec::new();
            match args.first() {
                Some(V::Int(n)) if *n >= 0 => {
                    buf.resize(*n as usize, 0);
                    let k = file.read(&mut buf).map_err(|e| os_err(e, &f.path))?;
                    buf.truncate(k);
                }
                _ => {
                    file.read_to_end(&mut buf).map_err(|e| os_err(e, &f.path))?;
                }
            }
            Ok(if f.binary { V::Bytes(Arc::from(buf)) } else { V::str(String::from_utf8_lossy(&buf)) })
        }
        "write" => {
            let mut g = f.f.lock();
            let file = g.as_mut().ok_or_else(closed)?;
            let (data, n): (Vec<u8>, usize) = match (f.binary, args.first()) {
                (true, Some(V::Bytes(b))) => (b.to_vec(), b.len()),
                (false, Some(V::Str(s))) => (s.as_bytes().to_vec(), s.chars().count()),
                (true, Some(o)) => return Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
                (false, Some(o)) => return Err(Exc::type_error(format!("write() argument must be str, not {}", o.type_name()))),
                (_, None) => return Err(Exc::type_error("write() takes exactly one argument (0 given)")),
            };
            file.write_all(&data).map_err(|e| os_err(e, &f.path))?;
            Ok(V::Int(n as i64))
        }
        "close" => {
            f.f.lock().take();
            Ok(V::None)
        }
        "flush" => Ok(V::None),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if f.binary { "BufferedReader" } else { "TextIOWrapper" }))),
    }
}

// ---------------------------------------------------------------- uuid

/// `uuid.UUID(hex)` (the other constructors' arguments are refused)
pub fn uuid_new(args: &[V], kwargs: &[(String, V)]) -> R {
    // UUID(int=n)
    if let ([], [(k, n)]) = (args, kwargs) {
        if k == "int" {
            let n: u128 = match n {
                V::Int(i) if *i >= 0 => *i as u128,
                V::Decimal(d) if d.is_integral() => d.to_int().to_string().parse().map_err(|_| Exc::value_error("int is out of range (need a 128-bit value)"))?,
                V::Int(_) | V::Decimal(_) => return Err(Exc::value_error("int is out of range (need a 128-bit value)")),
                o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
            };
            return Ok(V::native(Native::Uuid(n)));
        }
    }
    if kwargs.iter().any(|(k, _)| k != "hex") || args.len() > 1 {
        return Err(Exc::type_error("py2axum: only uuid.UUID(hex) is supported"));
    }
    let hex = args.first().or_else(|| kwargs.first().map(|(_, v)| v)).ok_or_else(|| Exc::type_error("one of the hex, bytes, bytes_le, fields, or int arguments must be given"))?;
    let s = match hex {
        V::Str(s) => s.to_string(),
        o => return Err(Exc::attr_error(format!("'{}' object has no attribute 'replace'", o.type_name()))),
    };
    let mut h = s.replace("urn:", "").replace("uuid:", "");
    h = h.trim_matches(|c| c == '{' || c == '}').replace('-', "");
    if h.len() != 32 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Exc::value_error("badly formed hexadecimal UUID string"));
    }
    Ok(V::native(Native::Uuid(u128::from_str_radix(&h, 16).map_err(|_| Exc::value_error("badly formed hexadecimal UUID string"))?)))
}

pub fn uuid4() -> V {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    V::native(Native::Uuid(u128::from_be_bytes(b)))
}

pub fn uuid_hex(u: u128) -> String {
    format!("{u:032x}")
}

pub fn uuid_str(u: u128) -> String {
    let h = uuid_hex(u);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub fn uuid_attr(u: u128, name: &str) -> R {
    Ok(match name {
        "hex" => V::str(uuid_hex(u)),
        "version" => V::Int(((u >> 76) & 0xf) as i64),
        _ => return Err(Exc::attr_error(format!("'UUID' object has no attribute '{name}'"))),
    })
}

// ---------------------------------------------------------------- with

/// `with x as y`: the value bound to `y` (these runtime values return themselves)
pub fn ctx_enter(v: &V) -> R {
    match v {
        V::Native(n) if matches!(&**n, Native::File(_) | Native::BytesIO(_)) => Ok(v.clone()),
        other => Err(Exc::type_error(format!("'{}' object does not support the context manager protocol", other.type_name()))),
    }
}

/// End of the `with` block (also on an exception): files are closed.
pub fn ctx_exit(v: &V) -> R {
    if let V::Native(n) = v {
        if let Native::File(f) = &**n {
            f.f.lock().take();
        }
    }
    Ok(V::None)
}
