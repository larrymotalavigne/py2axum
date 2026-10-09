//! In-memory files: `io.BytesIO` and FastAPI's `UploadFile` (whose `.file` is a BytesIO over the part).
use std::sync::Arc;

use parking_lot::Mutex;

use super::ops;
use super::v::*;

pub struct BytesIO {
    buf: Mutex<Vec<u8>>,
    pos: Mutex<usize>,
}

pub struct Upload {
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub headers: Vec<(String, String)>,
    /// the content, as `upload.file`
    pub file: V,
}

pub fn bytesio(data: Vec<u8>) -> V {
    V::native(Native::BytesIO(BytesIO { buf: Mutex::new(data), pos: Mutex::new(0) }))
}

/// `io.BytesIO(initial_bytes=b"")`
pub fn bytesio_new(init: Option<&V>) -> R {
    match init {
        None | Some(V::None) => Ok(bytesio(Vec::new())),
        Some(V::Bytes(b)) => Ok(bytesio(b.to_vec())),
        Some(other) => Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", other.type_name()))),
    }
}

pub fn upload(filename: Option<String>, content_type: Option<String>, headers: Vec<(String, String)>, data: Vec<u8>) -> V {
    V::native(Native::Upload(Arc::new(Upload { filename, content_type, headers, file: bytesio(data) })))
}

/// `await upload.read()`: the rest of an UploadFile's content (from its position, which moves to the end)
pub fn upload_read_all(v: &V) -> Option<Vec<u8>> {
    let V::Native(n) = v else { return None };
    let Native::Upload(u) = &**n else { return None };
    let b = as_bio(&u.file);
    let buf = b.buf.lock();
    let mut pos = b.pos.lock();
    let start = (*pos).min(buf.len());
    *pos = buf.len();
    Some(buf[start..].to_vec())
}

fn as_bio(v: &V) -> &BytesIO {
    match v {
        V::Native(n) => match &**n {
            Native::BytesIO(b) => b,
            _ => unreachable!("upload.file is a BytesIO"),
        },
        _ => unreachable!("upload.file is a BytesIO"),
    }
}

fn size_arg(args: &[V]) -> R<Option<usize>> {
    match args.first() {
        None | Some(V::None) => Ok(None),
        Some(V::Int(n)) if *n < 0 => Ok(None),
        Some(V::Int(n)) => Ok(Some(*n as usize)),
        Some(other) => Err(Exc::type_error(format!("argument should be integer or None, not '{}'", other.type_name()))),
    }
}

/// iterating a BytesIO: its remaining lines (each ending with b"\n" but the last), the position at the end
pub fn bytesio_lines(b: &BytesIO) -> Vec<V> {
    let buf = b.buf.lock();
    let mut pos = b.pos.lock();
    let rest = &buf[(*pos).min(buf.len())..];
    let out = rest.split_inclusive(|c| *c == b'\n').map(|l| V::Bytes(Arc::from(l))).collect();
    *pos = (*pos).max(buf.len());
    out
}

pub fn bytesio_method(b: &BytesIO, name: &str, args: &[V]) -> R {
    match name {
        "read" => {
            let buf = b.buf.lock();
            let mut pos = b.pos.lock();
            let start = (*pos).min(buf.len());
            let end = match size_arg(args)? {
                None => buf.len(),
                Some(n) => (start + n).min(buf.len()),
            };
            *pos = end;
            Ok(V::Bytes(Arc::from(&buf[start..end])))
        }
        "write" => {
            let data: Vec<u8> = match args.first() {
                Some(V::Bytes(d)) => d.to_vec(),
                Some(other) => return Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", other.type_name()))),
                None => return Err(Exc::type_error("write() takes exactly one argument (0 given)")),
            };
            let mut buf = b.buf.lock();
            let mut pos = b.pos.lock();
            if *pos > buf.len() {
                buf.resize(*pos, 0);
            }
            let end = *pos + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[*pos..end].copy_from_slice(&data);
            *pos = end;
            Ok(V::Int(data.len() as i64))
        }
        "seek" => {
            let off = match args.first() {
                Some(V::Int(i)) => *i,
                _ => return Err(Exc::type_error("seek() takes an integer offset")),
            };
            let whence = match args.get(1) {
                None => 0,
                Some(V::Int(w)) => *w,
                Some(_) => return Err(Exc::type_error("an integer is required")),
            };
            let len = b.buf.lock().len() as i64;
            let mut pos = b.pos.lock();
            let new = match whence {
                0 if off < 0 => return Err(Exc::value_error(format!("negative seek value {off}"))),
                0 => off,
                1 => (*pos as i64 + off).max(0),
                2 => (len + off).max(0),
                w => return Err(Exc::value_error(format!("invalid whence ({w}, should be 0, 1 or 2)"))),
            };
            *pos = new as usize;
            Ok(V::Int(new))
        }
        "tell" => Ok(V::Int(*b.pos.lock() as i64)),
        "getvalue" => Ok(V::Bytes(Arc::from(&b.buf.lock()[..]))),
        "close" | "flush" => Ok(V::None),
        "truncate" => {
            let size = size_arg(args)?.unwrap_or(*b.pos.lock());
            b.buf.lock().truncate(size);
            Ok(V::Int(size as i64))
        }
        _ => Err(Exc::attr_error(format!("'_io.BytesIO' object has no attribute '{name}'"))),
    }
}

/// UploadFile's (async) methods: they delegate to its file.
pub fn upload_method(u: &Upload, name: &str, args: &[V]) -> R {
    match name {
        "read" | "seek" | "write" | "close" => bytesio_method(as_bio(&u.file), name, args),
        _ => Err(Exc::attr_error(format!("'UploadFile' object has no attribute '{name}'"))),
    }
}

pub fn upload_attr(u: &Upload, name: &str) -> R {
    Ok(match name {
        "filename" => u.filename.as_deref().map(V::str).unwrap_or(V::None),
        "content_type" => u.content_type.as_deref().map(V::str).unwrap_or(V::None),
        "size" => V::Int(as_bio(&u.file).buf.lock().len() as i64),
        "file" => u.file.clone(),
        "headers" => V::dict_from(u.headers.iter().map(|(k, v)| (V::str(k), V::str(v))).collect())?,
        _ => return Err(Exc::attr_error(format!("'UploadFile' object has no attribute '{name}'"))),
    })
}

pub fn repr_upload(u: &Upload) -> R<String> {
    Ok(format!(
        "UploadFile(filename={}, size={}, headers=Headers({}))",
        ops::repr(&u.filename.as_deref().map(V::str).unwrap_or(V::None))?,
        as_bio(&u.file).buf.lock().len(),
        ops::repr(&V::dict_from(u.headers.iter().map(|(k, v)| (V::str(k), V::str(v))).collect())?)?
    ))
}
