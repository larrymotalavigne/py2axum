//! `zipfile.ZipFile(io.BytesIO(), "w", compression)`: `writestr`, `close` (or leaving `with`), `namelist`,
//! with the bytes CPython's zipfile writes into a seekable file (local header with the sizes, central
//! directory, `0o600` permissions, the local time of the call, zlib's raw deflate at level 6, memLevel 8).
use std::sync::Arc;

use parking_lot::Mutex;

use super::v::*;

struct Entry {
    name: Vec<u8>,
    flags: u16,
    method: u16,
    time: u16,
    date: u16,
    crc: u32,
    csize: u32,
    usize: u32,
    offset: u32,
    external: u32,
}

pub struct ZipW {
    target: V,
    method: u16,
    level: i32,
    entries: Mutex<Vec<Entry>>,
    closed: Mutex<bool>,
}

fn bio_write(target: &V, data: &[u8]) -> R<()> {
    let V::Native(n) = target else { unreachable!() };
    let Native::BytesIO(b) = &**n else { unreachable!() };
    super::files::bytesio_method(b, "write", &[V::Bytes(Arc::from(data))])?;
    Ok(())
}

fn bio_tell(target: &V) -> R<u64> {
    let V::Native(n) = target else { unreachable!() };
    let Native::BytesIO(b) = &**n else { unreachable!() };
    match super::files::bytesio_method(b, "tell", &[])? {
        V::Int(i) => Ok(i as u64),
        _ => Ok(0),
    }
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn method_of(v: &V) -> R<u16> {
    match v {
        V::Int(0) => Ok(0),
        V::Int(8) => Ok(8),
        V::Int(12) | V::Int(14) => Err(Exc::type_error("py2axum: ZIP_BZIP2 and ZIP_LZMA are not supported")),
        _ => Err(Exc::msg(&NOT_IMPLEMENTED_ERROR, "That compression method is not supported")),
    }
}

fn level_of(v: Option<&V>) -> R<i32> {
    match v {
        None | Some(V::None) => Ok(-1),
        Some(V::Int(n)) if (0..=9).contains(n) => Ok(*n as i32),
        Some(V::Int(n)) => Err(Exc::value_error(format!("py2axum: compresslevel={n} is not supported (0 to 9)"))),
        Some(o) => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    }
}

/// `zipfile.ZipFile(file, mode="r", compression=ZIP_STORED, allowZip64=True, compresslevel=None)`
pub fn open(args: &[V], kwargs: &[(String, V)]) -> R {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "file" | "mode" | "compression" | "allowZip64" | "compresslevel")) {
        return Err(Exc::type_error(format!("py2axum: ZipFile({k}=) is not supported")));
    }
    let arg = |i: usize, name: &str| args.get(i).or_else(|| kw(kwargs, name));
    let target = arg(0, "file").cloned().unwrap_or(V::None);
    let is_bio = matches!(&target, V::Native(n) if matches!(&**n, Native::BytesIO(_)));
    let mode = match arg(1, "mode") {
        None => "r".to_string(),
        Some(V::Str(s)) => s.to_string(),
        Some(_) => return Err(Exc::type_error("py2axum: ZipFile(mode=) must be a str")),
    };
    if mode != "w" || !is_bio {
        return Err(Exc::type_error("py2axum: only ZipFile(io.BytesIO(...), \"w\") is supported (writing into memory)"));
    }
    let method = method_of(arg(2, "compression").unwrap_or(&V::Int(0)))?;
    if let Some(v) = arg(3, "allowZip64") {
        if !super::ops::truthy(v)? {
            return Err(Exc::type_error("py2axum: ZipFile(allowZip64=False) is not supported"));
        }
    }
    let level = level_of(arg(4, "compresslevel"))?;
    if bio_tell(&target)? != 0 {
        return Err(Exc::type_error("py2axum: ZipFile on an io.BytesIO not at offset 0 is not supported"));
    }
    Ok(V::native(Native::ZipW(Arc::new(ZipW { target, method, level, entries: Mutex::new(Vec::new()), closed: Mutex::new(false) }))))
}

fn deflate(data: &[u8], level: i32) -> R<Vec<u8>> {
    use std::io::Write;
    // zlib.compressobj(level, DEFLATED, -15): raw deflate, 32 KiB window, memLevel 8 (flate2's defaults)
    let lvl = if level < 0 { flate2::Compression::default() } else { flate2::Compression::new(level as u32) };
    let mut e = flate2::write::DeflateEncoder::new(Vec::with_capacity(data.len() / 2 + 64), lvl);
    e.write_all(data).map_err(|x| Exc::runtime(x.to_string()))?;
    e.finish().map_err(|x| Exc::runtime(x.to_string()))
}

fn writestr(z: &ZipW, args: &[V], kwargs: &[(String, V)]) -> R {
    if *z.closed.lock() {
        return Err(Exc::value_error("Attempt to write to ZIP archive that was already closed"));
    }
    let name = match args.first().or_else(|| kw(kwargs, "zinfo_or_arcname")) {
        Some(V::Str(s)) => s.to_string(),
        Some(_) => return Err(Exc::type_error("py2axum: writestr() takes a str name (ZipInfo is not supported)")),
        None => return Err(Exc::type_error("ZipFile.writestr() missing 1 required positional argument: 'zinfo_or_arcname'")),
    };
    let data: Vec<u8> = match args.get(1).or_else(|| kw(kwargs, "data")) {
        Some(V::Bytes(b)) => b.to_vec(),
        Some(V::Str(s)) => s.as_bytes().to_vec(),
        Some(o) => return Err(Exc::type_error(format!("object of type '{}' has no len()", o.type_name()))),
        None => return Err(Exc::type_error("ZipFile.writestr() missing 1 required positional argument: 'data'")),
    };
    let method = match args.get(2).or_else(|| kw(kwargs, "compress_type")) {
        None | Some(V::None) => z.method,
        Some(v) => method_of(v)?,
    };
    let level = match args.get(3).or_else(|| kw(kwargs, "compresslevel")) {
        None | Some(V::None) => z.level,
        v => level_of(v)?,
    };
    // ZipInfo: the name stops at a NUL; a name ending in "/" is a directory
    let name = name.split('\0').next().unwrap_or("").to_string();
    let external: u32 = if name.ends_with('/') { (0o40775 << 16) | 0x10 } else { 0o600 << 16 };
    if data.len() as f64 * 1.05 > ((1u64 << 31) - 1) as f64 {
        return Err(Exc::type_error("py2axum: a ZIP64 member (over 2 GiB) is not supported"));
    }
    use chrono::{Datelike, Timelike};
    let now = chrono::Local::now();
    if now.year() < 1980 {
        return Err(Exc::value_error("ZIP does not support timestamps before 1980"));
    }
    let date = (((now.year() - 1980) as u16) << 9) | ((now.month() as u16) << 5) | now.day() as u16;
    let time = ((now.hour() as u16) << 11) | ((now.minute() as u16) << 5) | (now.second() as u16 / 2);
    let crc = {
        let mut h = flate2::Crc::new();
        h.update(&data);
        h.sum()
    };
    let body = if method == 8 { deflate(&data, level)? } else { data.clone() };
    let flags: u16 = if name.is_ascii() { 0 } else { 0x800 };
    let offset = bio_tell(&z.target)?;
    if offset > u32::MAX as u64 {
        return Err(Exc::type_error("py2axum: a ZIP64 archive (over 4 GiB) is not supported"));
    }
    let nb = name.as_bytes().to_vec();
    let mut h = Vec::with_capacity(30 + nb.len());
    h.extend_from_slice(b"PK\x03\x04");
    h.extend_from_slice(&20u16.to_le_bytes()); // extract_version, reserved
    h.extend_from_slice(&flags.to_le_bytes());
    h.extend_from_slice(&method.to_le_bytes());
    h.extend_from_slice(&time.to_le_bytes());
    h.extend_from_slice(&date.to_le_bytes());
    h.extend_from_slice(&crc.to_le_bytes());
    h.extend_from_slice(&(body.len() as u32).to_le_bytes());
    h.extend_from_slice(&(data.len() as u32).to_le_bytes());
    h.extend_from_slice(&(nb.len() as u16).to_le_bytes());
    h.extend_from_slice(&0u16.to_le_bytes());
    h.extend_from_slice(&nb);
    bio_write(&z.target, &h)?;
    bio_write(&z.target, &body)?;
    z.entries.lock().push(Entry {
        name: nb,
        flags,
        method,
        time,
        date,
        crc,
        csize: body.len() as u32,
        usize: data.len() as u32,
        offset: offset as u32,
        external,
    });
    Ok(V::None)
}

fn close(z: &ZipW) -> R {
    {
        let mut c = z.closed.lock();
        if *c {
            return Ok(V::None);
        }
        *c = true;
    }
    let entries = std::mem::take(&mut *z.entries.lock());
    if entries.len() > 0xFFFF {
        return Err(Exc::type_error("py2axum: a ZIP64 archive (over 65535 members) is not supported"));
    }
    let start = bio_tell(&z.target)?;
    let mut cd = Vec::new();
    for e in &entries {
        cd.extend_from_slice(b"PK\x01\x02");
        cd.extend_from_slice(&[20, 3, 20, 0]); // create_version, create_system (unix), extract_version, reserved
        cd.extend_from_slice(&e.flags.to_le_bytes());
        cd.extend_from_slice(&e.method.to_le_bytes());
        cd.extend_from_slice(&e.time.to_le_bytes());
        cd.extend_from_slice(&e.date.to_le_bytes());
        cd.extend_from_slice(&e.crc.to_le_bytes());
        cd.extend_from_slice(&e.csize.to_le_bytes());
        cd.extend_from_slice(&e.usize.to_le_bytes());
        cd.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // extra, comment, disk, internal attributes
        cd.extend_from_slice(&e.external.to_le_bytes());
        cd.extend_from_slice(&e.offset.to_le_bytes());
        cd.extend_from_slice(&e.name);
    }
    let n = entries.len() as u16;
    cd.extend_from_slice(b"PK\x05\x06");
    cd.extend_from_slice(&[0, 0, 0, 0]);
    cd.extend_from_slice(&n.to_le_bytes());
    cd.extend_from_slice(&n.to_le_bytes());
    let size = (cd.len() - 4 - 4 - 4) as u32;
    cd.extend_from_slice(&size.to_le_bytes());
    cd.extend_from_slice(&(start as u32).to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    bio_write(&z.target, &cd)?;
    *z.entries.lock() = entries;
    Ok(V::None)
}

pub fn method(z: &Arc<ZipW>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "writestr" => writestr(z, args, kwargs),
        "close" => close(z),
        "namelist" => Ok(V::list(z.entries.lock().iter().map(|e| V::str(String::from_utf8_lossy(&e.name))).collect())),
        _ => Err(Exc::attr_error(format!("'ZipFile' object has no attribute '{name}'"))),
    }
}
