//! pgpy 0.6.0 as Python values: the module, `PGPKey.from_blob()` and what it returns (PGPKey, PGPUID,
//! Fingerprint); the parsing itself is `pgpycore`. Its warnings are written on stderr as CPython's default
//! filters write them: once per process for a (text, category, line), at the path pgpy had when the project was
//! translated. Every other name of pgpy raises a py2axum AttributeError (docs/supported.md, "pgpy").
use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, LazyLock, OnceLock};

use parking_lot::Mutex;

use super::pgpycore as core;
use super::v::*;

pub enum Obj {
    Module,
    /// the class `pgpy.PGPKey`
    KeyClass,
    Key(Arc<KeyO>),
    Uid(Arc<UidO>),
    /// `pgpy.types.Fingerprint`, a str subclass
    Fp(Arc<str>),
}

/// a PGPKey; its user ids and subkeys are the Python objects themselves (`key.userids[0] is key.userids[0]`)
pub struct KeyO {
    key: Option<core::KeyPkt>,
    uids: Vec<(bool, V)>,
    children: Vec<(String, V)>,
}

pub struct UidO {
    b: core::UidB,
}

/// where pgpy was installed when the project was translated (the warnings name its files)
static DIR: OnceLock<String> = OnceLock::new();
static MODULE: LazyLock<V> = LazyLock::new(|| V::native(Native::Pgp(Arc::new(Obj::Module))));
static KEY_CLASS: LazyLock<V> = LazyLock::new(|| V::native(Native::Pgp(Arc::new(Obj::KeyClass))));

/// the module `pgpy` (`import pgpy`, `return pgpy`); `dir`: its site-packages directory at translation time
pub fn module(dir: &str) -> V {
    DIR.get_or_init(|| dir.to_string());
    MODULE.clone()
}

/// `pgpy.PGPKey`
pub fn key_class(dir: &str) -> V {
    DIR.get_or_init(|| dir.to_string());
    KEY_CLASS.clone()
}

/// the module's names (`dir(pgpy)` of 0.6.0) and the class's: one that is not ported raises the py2axum error
const MODULE_NAMES: &[&str] = &["PGPKeyring", "PGPMessage", "PGPSignature", "PGPUID", "_curves", "constants", "decorators",
    "errors", "packet", "pgp", "symenc", "types", "__version__", "__all__", "__author__", "__copyright__", "__license__"];
const KEY_NAMES: &[&str] = &["_do_self_signatures_verification", "_get_key_flags", "_parent", "_sign", "add_subkey", "add_uid",
    "ascii_unarmor", "bind", "bytes_to_int", "bytes_to_text", "certify", "charset", "check_management", "check_primitives",
    "check_soundness", "crc24", "created", "decrypt", "del_uid", "encrypt", "expires_at", "from_file", "get_uid", "hashdata",
    "int_byte_len", "int_to_bytes", "is_armor", "is_ascii", "is_considered_insecure", "is_expired", "is_protected",
    "is_unlocked", "key_algorithm", "key_size", "magic", "new", "parent", "parse", "protect", "pubkey", "revocation_keys",
    "revocation_signatures", "revoke", "revoker", "self_signatures", "self_verified", "self_verify", "sign", "signers",
    "text_to_bytes", "unlock", "verify", "ascii_headers"];
const UID_NAMES: &[&str] = &["_parent", "_splitstring", "attested_third_party_certifications", "attested_to", "hashdata", "image",
    "new", "parent", "selfsig", "signers", "third_party_certifications"];

fn unported(what: &str) -> Exc {
    Exc::attr_error(format!("py2axum: {what} is not supported (pgpy: only PGPKey.from_blob() and the PGPKey, PGPUID and \
        Fingerprint attributes listed in docs/supported.md are reproduced)"))
}

pub fn type_name(o: &Obj) -> &'static str {
    match o {
        Obj::Module => "module",
        Obj::KeyClass => "ABCMeta",
        Obj::Key(_) => "PGPKey",
        Obj::Uid(_) => "PGPUID",
        Obj::Fp(_) => "Fingerprint",
    }
}

fn exc_class(c: core::ECls) -> &'static Class {
    use core::ECls::*;
    match c {
        Index => &INDEX_ERROR,
        Value => &VALUE_ERROR,
        Pgp => &PGP_ERROR,
        Type => &TYPE_ERROR,
        Attribute => &ATTRIBUTE_ERROR,
        Stop => &STOP_ITERATION,
        UEncode => &UNICODE_ENCODE_ERROR,
        UDecode => &UNICODE_DECODE_ERROR,
        NotImpl => &NOT_IMPLEMENTED_ERROR,
        Assertion => &ASSERTION_ERROR,
        Struct => &STRUCT_ERROR,
        Zlib => &ZLIB_ERROR,
        OS => &OS_ERROR,
        Asn1 => &PYASN1_ERROR,
        Underrun => &PYASN1_UNDERRUN,
        Eos => &PYASN1_END_OF_STREAM,
        EcFmt => &PGP_EC_FORMAT_ERROR,
        Binascii => &BINASCII_ERROR,
        Unsupported => &NOT_IMPLEMENTED_ERROR,
    }
}

fn to_exc(e: core::PErr) -> Exc {
    // StopIteration() and AssertionError() have no args: str() is ''
    let x = if e.msg.is_empty() && matches!(e.cls, core::ECls::Stop | core::ECls::Assertion | core::ECls::Eos) {
        Exc::new(exc_class(e.cls), vec![])
    } else {
        Exc::msg(exc_class(e.cls), &e.msg)
    };
    if let Some(c) = e.cause {
        x.0.attrs.lock().insert("__cause__".into(), V::Exc(to_exc(*c)));
    }
    x
}

/// CPython's default warning filters: shown once per (text, category, line) of the module it names
fn show(w: &core::Warn) {
    static SEEN: LazyLock<Mutex<HashSet<(String, &'static str, &'static str, u32)>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
    if !SEEN.lock().insert((w.text.clone(), w.category, w.file, w.line)) {
        return;
    }
    let dir = DIR.get().map(|d| d.trim_end_matches('/')).unwrap_or("");
    let path = if dir.is_empty() { w.file.to_string() } else { format!("{dir}/{}", w.file) };
    let mut o = std::io::stderr().lock();
    let _ = write!(o, "{path}:{}: {}: {}\n  {}\n", w.line, w.category, w.text, w.src);
    let _ = o.flush();
}

fn b64(s: &[u8]) -> Result<Vec<u8>, String> {
    super::stdlib::b64dec(s, false).map_err(|e| e.message())
}

/// `pgpy.PGPKey.from_blob(blob)`: `(PGPKey, OrderedDict {(keyid, is_public): PGPKey})`
pub fn from_blob(args: &[V], kwargs: &[(String, V)]) -> R {
    let blob = match (args, kwargs) {
        ([b], []) => b,
        ([], [(k, b)]) if k == "blob" => b,
        _ => {
            return Err(Exc::type_error(format!(
                "PGPKey.from_blob() takes 2 positional arguments but {} were given",
                args.len() + kwargs.len() + 1
            )))
        }
    };
    let mut ctx = core::Ctx::new(b64);
    let r = match blob {
        V::Str(s) => core::from_text(s, &mut ctx),
        V::Bytes(b) => core::from_bytes(b.to_vec(), &mut ctx),
        _ => {
            return Err(Exc::type_error("encoding without a string argument"));
        }
    };
    for w in &ctx.warnings {
        show(w);
    }
    let p = r.map_err(to_exc)?;
    let mut memo: Vec<Option<V>> = (0..p.arena.len()).map(|_| None).collect();
    let mut arena: Vec<Option<core::KeyB>> = p.arena.into_iter().map(Some).collect();
    fn build(i: usize, arena: &mut [Option<core::KeyB>], memo: &mut [Option<V>]) -> V {
        if let Some(k) = &memo[i] {
            return k.clone();
        }
        let k = arena[i].take().expect("a key object is built once");
        let children = k.children.iter().map(|(id, &c)| (id.clone(), build(c, arena, memo))).collect();
        let uids = k.uids.into_iter().map(|b| (b.is_uid(), V::native(Native::Pgp(Arc::new(Obj::Uid(Arc::new(UidO { b }))))))).collect();
        let o = V::native(Native::Pgp(Arc::new(Obj::Key(Arc::new(KeyO { key: k.key, uids, children })))));
        memo[i] = Some(o.clone());
        o
    }
    let first = build(0, &mut arena, &mut memo);
    let mut items = vec![];
    for ((keyid, public), i) in p.keys {
        items.push((V::tuple(vec![V::str(keyid), V::Bool(public)]), build(i, &mut arena, &mut memo)));
    }
    Ok(V::tuple(vec![first, V::dict_from(items)?]))
}

fn uid_parts(u: &UidO) -> R<(String, String, String)> {
    match &u.b.kind {
        core::UidKind::Uid(s) => core::split_uid(s).ok_or_else(|| Exc::attr_error("'NoneType' object has no attribute 'groupdict'")),
        core::UidKind::UAttr => Ok((String::new(), String::new(), String::new())),
    }
}

fn perr(e: core::PErr) -> Exc {
    to_exc(e)
}

/// `obj.name` on a pgpy value
pub fn attr(v: &V, o: &Obj, name: &str) -> R {
    match o {
        Obj::Module => match name {
            "PGPKey" => Ok(KEY_CLASS.clone()),
            "__name__" => Ok(V::str("pgpy")),
            n if MODULE_NAMES.contains(&n) => Err(unported(&format!("pgpy.{n}"))),
            n => Err(Exc::attr_error(format!("module 'pgpy' has no attribute '{n}'"))),
        },
        Obj::KeyClass => match name {
            "from_blob" => Ok(V::native(Native::MethodOf(v.clone(), "from_blob"))),
            "__name__" | "__qualname__" => Ok(V::str("PGPKey")),
            "__module__" => Ok(V::str("pgpy.pgp")),
            n if KEY_NAMES.contains(&n) || matches!(n, "fingerprint" | "is_public" | "is_primary" | "userids" | "userattributes" | "subkeys") => {
                Err(unported(&format!("PGPKey.{n} read on the class")))
            }
            n => Err(Exc::attr_error(format!("type object 'PGPKey' has no attribute '{n}'"))),
        },
        Obj::Key(k) => match name {
            "is_public" => Ok(V::Bool(k.key.as_ref().is_some_and(|p| p.is_public()))),
            "is_primary" => Ok(V::Bool(k.key.as_ref().is_some_and(|p| p.is_primary()))),
            "fingerprint" => Ok(match &k.key {
                Some(p) => V::native(Native::Pgp(Arc::new(Obj::Fp(Arc::from(p.fp.as_str()))))),
                None => V::None,
            }),
            "userids" => Ok(V::list(k.uids.iter().filter(|u| u.0).map(|u| u.1.clone()).collect())),
            "userattributes" => Ok(V::list(k.uids.iter().filter(|u| !u.0).map(|u| u.1.clone()).collect())),
            "subkeys" => V::dict_from(k.children.iter().map(|(id, c)| (V::str(id), c.clone())).collect()),
            n if KEY_NAMES.contains(&n) => Err(unported(&format!("PGPKey.{n}"))),
            n => Err(Exc::attr_error(format!("'PGPKey' object has no attribute '{n}'"))),
        },
        Obj::Uid(u) => match name {
            "name" => Ok(V::str(uid_parts(u)?.0)),
            "comment" => Ok(V::str(uid_parts(u)?.1)),
            "email" => Ok(V::str(uid_parts(u)?.2)),
            "userid" => Ok(match &u.b.kind {
                core::UidKind::Uid(s) => V::str(s),
                core::UidKind::UAttr => V::None,
            }),
            "is_uid" => Ok(V::Bool(u.b.is_uid())),
            "is_ua" => Ok(V::Bool(!u.b.is_uid())),
            "is_primary" => Ok(V::Bool(u.b.is_primary().map_err(perr)?)),
            n if UID_NAMES.contains(&n) => Err(unported(&format!("PGPUID.{n}"))),
            n => Err(Exc::attr_error(format!("'PGPUID' object has no attribute '{n}'"))),
        },
        Obj::Fp(s) => match name {
            "keyid" => Ok(V::str(&s[s.len().saturating_sub(16)..])),
            "shortid" => Ok(V::str(&s[s.len().saturating_sub(8)..])),
            _ => unreachable!("a Fingerprint's other attributes are its str's"),
        },
    }
}

/// `obj.name(...)` on a pgpy value other than a Fingerprint (whose methods are its str's)
pub fn method(v: &V, o: &Obj, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match (o, name) {
        (Obj::KeyClass, "from_blob") => from_blob(args, kwargs),
        // an attribute that is no method: calling its value
        _ => attr(v, o, name).and_then(|x| Err(Exc::type_error(format!("'{}' object is not callable", x.type_name())))),
    }
}

/// calling a pgpy value (`pgpy.PGPKey()`)
pub fn call(o: &Obj) -> R {
    Err(match o {
        Obj::KeyClass => unported("PGPKey() (a key built rather than parsed)"),
        _ => Exc::type_error(format!("'{}' object is not callable", type_name(o))),
    })
}

/// a Fingerprint as its plain str (the operations of `str` it inherits)
pub fn as_str(v: &V) -> Option<V> {
    match v {
        V::Native(n) => match &**n {
            Native::Pgp(o) => match &**o {
                Obj::Fp(s) => Some(V::Str(s.clone())),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn obj(v: &V) -> Option<&Obj> {
    match v {
        V::Native(n) => match &**n {
            Native::Pgp(o) => Some(o),
            _ => None,
        },
        _ => None,
    }
}

/// `str(x)` of a pgpy value
pub fn str_hook(v: &V) -> Option<R<String>> {
    match obj(v)? {
        Obj::Fp(s) => Some(Ok(s.to_string())),
        _ => Some(repr_text(v)),
    }
}

/// `repr(x)` of a pgpy value
pub fn repr_hook(v: &V) -> Option<R<String>> {
    obj(v)?;
    Some(repr_text(v))
}

fn addr_of(v: &V) -> usize {
    match obj(v) {
        Some(Obj::Key(k)) => Arc::as_ptr(k) as usize,
        Some(Obj::Uid(u)) => Arc::as_ptr(u) as usize,
        _ => 0,
    }
}

fn repr_text(v: &V) -> R<String> {
    let o = obj(v).expect("a pgpy value");
    Ok(match o {
        Obj::Module => format!("<module 'pgpy' from '{}/pgpy/__init__.py'>", DIR.get().map(|d| d.trim_end_matches('/')).unwrap_or("")),
        Obj::KeyClass => "<class 'pgpy.pgp.PGPKey'>".into(),
        Obj::Key(k) => match &k.key {
            Some(p) => format!("<PGPKey [{}][0x{}] at 0x{:02X}>", p.class(), p.keyid(), addr_of(v)),
            None => format!("<PGPKey [unknown] at 0x{:02X}>", addr_of(v)),
        },
        Obj::Uid(u) => {
            let cls = if u.b.is_uid() { "UserID" } else { "UserAttribute" };
            match u.b.selfsig().map_err(perr)? {
                Some(s) => {
                    let created = s.created.ok_or_else(|| Exc::msg(&INDEX_ERROR, "list index out of range"))?;
                    let t = chrono::DateTime::from_timestamp(created, 0).map(|d| d.format("%Y-%m-%d %H:%M:%S+00:00").to_string()).unwrap_or_default();
                    format!("<PGPUID [{cls}][{t}] at 0x{:02X}>", addr_of(v))
                }
                None => format!("<PGPUID [{cls}] at 0x{:02X}>", addr_of(v)),
            }
        }
        Obj::Fp(s) => {
            if s.len() != 40 {
                return Err(Exc::value_error("Expected: String of 40 hex digits"));
            }
            let q: Vec<&str> = (0..10).map(|i| &s[i * 4..i * 4 + 4]).collect();
            format!("Fingerprint({}  {})", q[..5].join(" "), q[5..].join(" "))
        }
    })
}

/// `format(x, spec)`: PGPUID.__format__ (the spec is ignored, a User Attribute returns None)
pub fn format_hook(v: &V) -> Option<R<String>> {
    match obj(v)? {
        Obj::Uid(u) => Some(if u.b.is_uid() {
            uid_parts(u).map(|(n, c, e)| {
                let c = if c.is_empty() { String::new() } else { format!(" ({c})") };
                let e = if e.is_empty() { String::new() } else { format!(" <{e}>") };
                format!("{n}{c}{e}")
            })
        } else {
            Err(Exc::type_error("__format__ must return a str, not NoneType"))
        }),
        _ => None,
    }
}

/// `a == b` with a Fingerprint on either side (`Fingerprint.__eq__`: a str also equals its key id or short id)
pub fn eq_hook(a: &V, b: &V) -> Option<bool> {
    let (fa, fb) = (obj(a), obj(b));
    let text = |o: Option<&Obj>, v: &V| -> Option<String> {
        match (o, v) {
            (Some(Obj::Fp(s)), _) => Some(s.to_string()),
            (None, V::Str(s)) => Some(s.to_string()),
            (None, V::Bytes(b)) => Some(b.iter().map(|&c| c as char).collect()),
            _ => None,
        }
    };
    let (fp, other, other_is_fp) = match (fa, fb) {
        (Some(Obj::Fp(s)), _) => (s.clone(), b, matches!(fb, Some(Obj::Fp(_)))),
        (_, Some(Obj::Fp(s))) => (s.clone(), a, false),
        _ => {
            // pgpy objects compare by identity
            if fa.is_some() || fb.is_some() {
                return Some(match (a, b) {
                    (V::Native(x), V::Native(y)) => Arc::ptr_eq(x, y) || (addr_of(a) != 0 && addr_of(a) == addr_of(b)),
                    _ => false,
                });
            }
            return None;
        }
    };
    let Some(o) = text(obj(other), other) else { return Some(false) };
    if other_is_fp {
        return Some(*fp == *o);
    }
    let o: String = o.chars().filter(|&c| c != ' ').collect();
    Some(*fp == *o || fp[fp.len().saturating_sub(16)..] == o || fp[fp.len().saturating_sub(8)..] == o)
}
