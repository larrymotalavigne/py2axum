//! Password and second-factor libraries: pyca/bcrypt 4.3 and 5.0 (itself built on the `bcrypt` crate: same
//! salt parsing, same errors), passlib 1.7.4's CryptContext of bcrypt over it, and pyotp 2.9 (TOTP), with
//! `base64.b32decode` as CPython has it.
use base64::Engine as _;

use std::sync::Arc;

use super::ops;
use super::v::*;
use super::Cx;

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn arg<'a>(fname: &str, args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str, names: &[&str]) -> R<Option<&'a V>> {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !names.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("{fname}() got an unexpected keyword argument '{k}'")));
    }
    Ok(args.get(i).or_else(|| kw(kwargs, name)))
}

/// pyo3's conversion of a `bytes` parameter
fn bytes_arg(v: Option<&V>, name: &str) -> R<Vec<u8>> {
    match v {
        Some(V::Bytes(b)) => Ok(b.to_vec()),
        Some(o) => Err(Exc::type_error(format!("argument '{name}': '{}' object cannot be converted to 'PyBytes'", o.type_name()))),
        None => Err(Exc::type_error(format!("missing required positional argument '{name}'"))),
    }
}

// ---------------------------------------------------------------- bcrypt

const BCRYPT_ALPHABET: base64::alphabet::Alphabet = match base64::alphabet::Alphabet::new("./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789") {
    Ok(a) => a,
    Err(_) => panic!("bcrypt alphabet"),
};

fn bcrypt_b64() -> base64::engine::GeneralPurpose {
    base64::engine::GeneralPurpose::new(
        &BCRYPT_ALPHABET,
        base64::engine::GeneralPurposeConfig::new()
            .with_encode_padding(false)
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    )
}

fn invalid_salt() -> Exc {
    Exc::value_error("Invalid salt")
}

/// pyca/bcrypt's `hashpw`: 5.0 refuses a password over 72 bytes, 4.x hashes its first 72 bytes
fn hashpw(password: &[u8], salt: &[u8], bcrypt5: bool) -> R<Vec<u8>> {
    let password = if !bcrypt5 && password.len() > 72 { &password[..72] } else { password };
    if password.len() > 72 {
        return Err(Exc::value_error("password cannot be longer than 72 bytes, truncate manually if necessary (e.g. my_password[:72])"));
    }
    let parts: Vec<&[u8]> = salt.split(|&b| b == b'$').filter(|s| !s.is_empty()).collect();
    if parts.len() != 3 {
        return Err(invalid_salt());
    }
    let version = match parts[0] {
        b"2y" => bcrypt::Version::TwoY,
        b"2b" => bcrypt::Version::TwoB,
        b"2a" => bcrypt::Version::TwoA,
        b"2x" => bcrypt::Version::TwoX,
        _ => return Err(invalid_salt()),
    };
    let cost: u32 = std::str::from_utf8(parts[1]).ok().and_then(|s| s.parse().ok()).ok_or_else(invalid_salt)?;
    if parts[2].len() < 22 {
        return Err(invalid_salt());
    }
    let raw: [u8; 16] = bcrypt_b64().decode(&parts[2][..22]).ok().and_then(|v| v.try_into().ok()).ok_or_else(invalid_salt)?;
    let hashed = bcrypt::hash_with_salt(password, cost, raw).map_err(|_| invalid_salt())?;
    Ok(hashed.format_for_version(version).into_bytes())
}

/// `bcrypt.gensalt` / `hashpw` / `checkpw` (`bcrypt5`: the locked release is 5.x)
pub fn call(name: &str, bcrypt5: bool, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "gensalt" => {
            let names = ["rounds", "prefix"];
            let rounds = match arg(name, args, kwargs, 0, "rounds", &names)? {
                None => 12,
                Some(V::Int(i)) => *i,
                Some(o) => return Err(Exc::type_error(format!("argument 'rounds': '{}' object cannot be interpreted as an integer", o.type_name()))),
            };
            let prefix = match arg(name, args, kwargs, 1, "prefix", &names)? {
                None => b"2b".to_vec(),
                v => bytes_arg(v, "prefix")?,
            };
            if prefix != b"2a" && prefix != b"2b" {
                return Err(Exc::value_error("Supported prefixes are b'2a' or b'2b'"));
            }
            if !(4..=31).contains(&rounds) {
                return Err(Exc::value_error("Invalid rounds"));
            }
            let salt: [u8; 16] = rand::random();
            let s = format!("${}${:02}${}", String::from_utf8_lossy(&prefix), rounds, bcrypt_b64().encode(salt));
            Ok(V::Bytes(std::sync::Arc::from(s.as_bytes())))
        }
        "hashpw" => {
            let names = ["password", "salt"];
            let pw = bytes_arg(arg(name, args, kwargs, 0, "password", &names)?, "password")?;
            let salt = bytes_arg(arg(name, args, kwargs, 1, "salt", &names)?, "salt")?;
            // CPU-bound for ~0.25 s at cost 12: off the loop worker, as pyca/bcrypt releases the GIL
            Ok(V::Bytes(std::sync::Arc::from(&super::thread::blocking(|| hashpw(&pw, &salt, bcrypt5))?[..])))
        }
        "checkpw" => {
            let names = ["password", "hashed_password"];
            let pw = bytes_arg(arg(name, args, kwargs, 0, "password", &names)?, "password")?;
            let hashed = bytes_arg(arg(name, args, kwargs, 1, "hashed_password", &names)?, "hashed_password")?;
            use subtle::ConstantTimeEq;
            Ok(V::Bool(super::thread::blocking(|| hashpw(&pw, &hashed, bcrypt5))?.ct_eq(&hashed).into()))
        }
        _ => Err(Exc::attr_error(format!("module 'bcrypt' has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- base32

/// `base64.b32decode(s, casefold)` (binascii.Error on bad input)
pub fn b32decode(s: &str, casefold: bool) -> R<Vec<u8>> {
    let err = |m: &str| Exc::msg(&BINASCII_ERROR, m);
    let mut s: Vec<u8> = s.bytes().collect();
    if super::python() >= (3, 15) {
        if casefold {
            s.make_ascii_uppercase();
        }
        return a2b_base32(&s);
    }
    if s.len() % 8 != 0 {
        return Err(err("Incorrect padding"));
    }
    if casefold {
        s.make_ascii_uppercase();
    }
    let l = s.len();
    while s.last() == Some(&b'=') {
        s.pop();
    }
    let padchars = l - s.len();
    let mut out: Vec<u8> = Vec::new();
    let mut acc: u64 = 0;
    for quanta in s.chunks(8) {
        acc = 0;
        for &c in quanta {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'2'..=b'7' => c - b'2' + 26,
                _ => return Err(err("Non-base32 digit found")),
            };
            acc = (acc << 5) + v as u64;
        }
        out.extend_from_slice(&acc.to_be_bytes()[3..]);
    }
    if l % 8 != 0 || ![0, 1, 3, 4, 6].contains(&padchars) {
        return Err(err("Incorrect padding"));
    }
    if padchars > 0 && !out.is_empty() {
        acc <<= 5 * padchars;
        let last = acc.to_be_bytes();
        let leftover = (43 - 5 * padchars) / 8;
        let n = out.len();
        out.truncate(n - 5);
        out.extend_from_slice(&last[3..3 + leftover]);
    }
    Ok(out)
}

/// Python 3.15's `binascii.a2b_base32(s)` (padded, standard alphabet, no ignorechars), behind
/// `base64.b32decode`: a port of Modules/binascii.c, messages included
fn a2b_base32(s: &[u8]) -> R<Vec<u8>> {
    let err = |m: String| Exc::msg(&BINASCII_ERROR, m);
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'2'..=b'7' => Some(c - b'2' + 26),
        _ => None,
    };
    let mut out: Vec<u8> = Vec::with_capacity(s.len().div_ceil(8) * 5);
    let (mut left, mut pos, mut pads) = (0u8, 0usize, 0usize);
    for &c in s {
        if c == b'=' {
            pads += 1;
            if matches!(pos, 2 | 4 | 5 | 7) && pos + pads <= 8 {
                continue;
            }
            if matches!(pos, 1 | 3 | 6) {
                break;
            }
            return Err(err(if pos == 0 && out.is_empty() { "Leading padding not allowed" } else { "Excess padding not allowed" }.into()));
        }
        let Some(v) = val(c) else {
            return Err(err("Only base32 data is allowed".into()));
        };
        if pads > 0 {
            return Err(err(if pos + pads == 8 { "Excess data after padding" } else { "Discontinuous padding not allowed" }.into()));
        }
        match pos {
            0 => left = v,
            1 => {
                out.push((left << 3) | (v >> 2));
                left = v & 0x03;
            }
            2 => left = (left << 5) | v,
            3 => {
                out.push((left << 1) | (v >> 4));
                left = v & 0x0f;
            }
            4 => {
                out.push((left << 4) | (v >> 1));
                left = v & 0x01;
            }
            5 => left = (left << 5) | v,
            6 => {
                out.push((left << 2) | (v >> 3));
                left = v & 0x07;
            }
            _ => {
                out.push((left << 5) | v);
                left = 0;
            }
        }
        pos = (pos + 1) % 8;
    }
    if matches!(pos, 1 | 3 | 6) {
        return Err(err(format!(
            "Invalid base32-encoded string: number of data characters ({}) cannot be 1, 3, or 6 more than a multiple of 8",
            out.len() / 5 * 8 + pos
        )));
    }
    if pos != 0 && pos + pads < 8 {
        return Err(err("Incorrect padding".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------- pyotp

pub struct Totp {
    secret: String,
    digits: i64,
    interval: i64,
    name: String,
    issuer: Option<String>,
}

/// `pyotp.random_base32(length=32)`
pub fn random_base32(args: &[V], kwargs: &[(String, V)]) -> R {
    let length = match arg("random_base32", args, kwargs, 0, "length", &["length"])? {
        None => 32,
        Some(V::Int(i)) => *i,
        Some(o) => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    };
    if length < 32 {
        return Err(Exc::value_error("Secrets should be at least 160 bits"));
    }
    use rand::Rng;
    let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut rng = rand::thread_rng();
    Ok(V::str((0..length).map(|_| chars[rng.gen_range(0..32)] as char).collect::<String>()))
}

/// `pyotp.TOTP(s, digits=6, digest=None, name=None, issuer=None, interval=30)`
pub fn totp_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let names = ["s", "digits", "digest", "name", "issuer", "interval"];
    let get = |i: usize, n: &str| arg("TOTP", args, kwargs, i, n, &names);
    let secret = ops::str_(get(0, "s")?.ok_or_else(|| Exc::type_error("TOTP.__init__() missing 1 required positional argument: 's'"))?)?;
    if get(2, "digest")?.is_some_and(|d| !d.is_none()) {
        return Err(Exc::type_error("py2axum: TOTP(digest=) is not supported (sha1 only)"));
    }
    let int = |v: Option<&V>, d: i64| -> R<i64> {
        match v {
            None => Ok(d),
            Some(V::Int(i)) => Ok(*i),
            Some(o) => Err(Exc::type_error(format!("py2axum: TOTP integer option, got {}", o.type_name()))),
        }
    };
    let digits = int(get(1, "digits")?, 6)?;
    if digits > 10 {
        return Err(Exc::value_error("digits must be no greater than 10"));
    }
    let interval = int(get(5, "interval")?, 30)?;
    let opt = |v: Option<&V>| -> R<Option<String>> { v.filter(|v| !v.is_none()).map(ops::str_).transpose() };
    let name = opt(get(3, "name")?)?.filter(|n| !n.is_empty()).unwrap_or_else(|| "Secret".into());
    let issuer = opt(get(4, "issuer")?)?;
    Ok(V::native(Native::Totp(Totp { secret, digits, interval, name, issuer })))
}

impl Totp {
    fn generate(&self, input: i64) -> R<String> {
        use hmac::Mac;
        if input < 0 {
            return Err(Exc::value_error("input must be positive integer"));
        }
        let mut secret = self.secret.clone();
        if secret.len() % 8 != 0 {
            secret += &"=".repeat(8 - secret.len() % 8);
        }
        let key = b32decode(&secret, true)?;
        let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(&key).expect("hmac key");
        mac.update(&(input as u64).to_be_bytes());
        let h = mac.finalize().into_bytes();
        let offset = (h[h.len() - 1] & 0xF) as usize;
        let code = ((h[offset] & 0x7F) as u64) << 24 | (h[offset + 1] as u64) << 16 | (h[offset + 2] as u64) << 8 | h[offset + 3] as u64;
        let s = (10_000_000_000u64 + code % 10u64.pow(self.digits as u32)).to_string();
        Ok(s[s.len() - self.digits as usize..].to_string())
    }

    fn timecode(&self, for_time: Option<&V>) -> R<i64> {
        let secs = match for_time {
            None | Some(V::None) => chrono::Utc::now().timestamp(),
            Some(V::Int(i)) => *i,
            Some(V::DateTime(d)) => match d.tz {
                Some(_) => d.utc().and_utc().timestamp(),
                // time.mktime: the wall time in the local zone
                None => {
                    use chrono::TimeZone;
                    chrono::Local.from_local_datetime(&d.wall).single().map(|x| x.timestamp()).unwrap_or_else(|| d.wall.and_utc().timestamp())
                }
            },
            Some(o) => return Err(Exc::type_error(format!("py2axum: TOTP time as {}", o.type_name()))),
        };
        Ok((secs as f64 / self.interval as f64) as i64)
    }
}

/// `pyotp.utils.strings_equal`: NFKC-normalised, constant time
fn strings_equal(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    use unicode_normalization::UnicodeNormalization;
    let a: String = a.nfkc().collect();
    let b: String = b.nfkc().collect();
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// `urllib.parse.quote(s)` (safe="/")
fn quote(s: &str) -> String {
    super::resp::quote(s, "/")
}

/// `urllib.parse.quote_plus` as `urlencode` applies it
fn quote_plus(s: &str) -> String {
    super::resp::quote(s, " ").replace(' ', "+")
}

pub fn totp_attr(t: &Totp, name: &str) -> R {
    Ok(match name {
        "secret" => V::str(&t.secret),
        "digits" => V::Int(t.digits),
        "interval" => V::Int(t.interval),
        "name" => V::str(&t.name),
        "issuer" => t.issuer.as_deref().map(V::str).unwrap_or(V::None),
        _ => return Err(Exc::attr_error(format!("'TOTP' object has no attribute '{name}'"))),
    })
}

pub fn totp_method(t: &Totp, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "now" => Ok(V::str(t.generate(t.timecode(None)?)?)),
        "at" => {
            let names = ["for_time", "counter_offset"];
            let ft = arg(name, args, kwargs, 0, "for_time", &names)?;
            let off = match arg(name, args, kwargs, 1, "counter_offset", &names)? {
                Some(V::Int(i)) => *i,
                _ => 0,
            };
            Ok(V::str(t.generate(t.timecode(Some(ft.unwrap_or(&V::None)))? + off)?))
        }
        "verify" => {
            let names = ["otp", "for_time", "valid_window"];
            let otp = ops::str_(arg(name, args, kwargs, 0, "otp", &names)?.ok_or_else(|| Exc::type_error("verify() missing 1 required positional argument: 'otp'"))?)?;
            let tc = t.timecode(arg(name, args, kwargs, 1, "for_time", &names)?)?;
            let window = match arg(name, args, kwargs, 2, "valid_window", &names)? {
                Some(V::Int(i)) => *i,
                _ => 0,
            };
            if window != 0 {
                for i in -window..=window {
                    if strings_equal(&otp, &t.generate(tc + i)?) {
                        return Ok(V::Bool(true));
                    }
                }
                return Ok(V::Bool(false));
            }
            Ok(V::Bool(strings_equal(&otp, &t.generate(tc)?)))
        }
        "provisioning_uri" => {
            let names = ["name", "issuer_name", "image"];
            let opt = |i: usize, n: &str| -> R<Option<String>> {
                arg(name, args, kwargs, i, n, &names)?.filter(|v| !v.is_none()).map(ops::str_).transpose().map(|o| o.filter(|s| !s.is_empty()))
            };
            if opt(2, "image")?.is_some() {
                return Err(Exc::type_error("py2axum: provisioning_uri(image=) is not supported"));
            }
            let account = opt(0, "name")?.unwrap_or_else(|| t.name.clone());
            let issuer = opt(1, "issuer_name")?.or_else(|| t.issuer.clone());
            let mut label = quote(&account);
            let mut q = vec![format!("secret={}", quote_plus(&t.secret))];
            if let Some(i) = &issuer {
                label = format!("{}:{label}", quote(i));
                q.push(format!("issuer={}", quote_plus(i)));
            }
            if t.digits != 6 {
                q.push(format!("digits={}", t.digits));
            }
            if t.interval != 30 {
                q.push(format!("period={}", t.interval));
            }
            Ok(V::str(format!("otpauth://totp/{label}?{}", q.join("&").replace('+', "%20"))))
        }
        _ => Err(Exc::attr_error(format!("'TOTP' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- pwdlib (Argon2, bcrypt)

/// a pwdlib hasher: `Argon2Hasher(time_cost=3, memory_cost=65536, parallelism=4, hash_len=32, salt_len=16,
/// type=Type.ID)` (argon2-cffi's defaults) or `BcryptHasher(rounds=12, prefix="2b")`
pub enum PwdHasher {
    Argon2 { t: u32, m: u32, p: u32, len: usize, salt: usize, alg: argon2::Algorithm },
    Bcrypt { rounds: u32, prefix: String },
}

/// `pwdlib.PasswordHash(hashers)`: the first hasher hashes, each verifies the hashes it identifies (`bcrypt5`:
/// the locked bcrypt release is 5.x)
pub struct PwdHash {
    hashers: Vec<Arc<PwdHasher>>,
    bcrypt5: bool,
}

const ARGON2_DEFAULT: PwdHasher = PwdHasher::Argon2 { t: 3, m: 65536, p: 4, len: 32, salt: 16, alg: argon2::Algorithm::Argon2id };

/// `pwdlib.PasswordHash.recommended()`: one Argon2 hasher with argon2-cffi's defaults (argon2id, t=3,
/// m=65536 KiB, p=4, 32-byte hash, 16-byte salt).
pub fn pwd_recommended(bcrypt5: bool) -> R {
    Ok(V::native(Native::PwdHash(Arc::new(PwdHash { hashers: vec![Arc::new(ARGON2_DEFAULT)], bcrypt5 }))))
}

/// `pwdlib.PasswordHash(hashers)` (a sequence of hashers, at least one)
pub fn pwd_new(bcrypt5: bool, args: &[V], kwargs: &[(String, V)]) -> R {
    let seq = match (args, kwargs) {
        ([v], []) => v.clone(),
        ([], [(k, v)]) if k == "hashers" => v.clone(),
        _ => return Err(Exc::type_error("PasswordHash.__init__() takes 1 positional argument: 'hashers'")),
    };
    let mut hashers = Vec::new();
    for h in ops::iter(&seq)? {
        match &h {
            V::Native(n) if matches!(&**n, Native::PwdHasher(_)) => {
                let Native::PwdHasher(x) = &**n else { unreachable!() };
                hashers.push(x.clone());
            }
            o => return Err(Exc::type_error(format!("py2axum: PasswordHash() of a '{}' (only pwdlib's Argon2Hasher and BcryptHasher)", o.type_name()))),
        }
    }
    if hashers.is_empty() {
        return Err(Exc::new(&ASSERTION_ERROR, vec![V::str("You must specify at least one hasher.")]));
    }
    Ok(V::native(Native::PwdHash(Arc::new(PwdHash { hashers, bcrypt5 }))))
}

fn int_arg(v: Option<&V>, name: &str, default: i64) -> R<i64> {
    match v {
        None => Ok(default),
        Some(V::Int(i)) => Ok(*i),
        Some(o) => Err(Exc::type_error(format!("py2axum: {name}= must be an int, not {}", o.type_name()))),
    }
}

/// `Argon2Hasher(...)` / `BcryptHasher(...)`
pub fn pwd_hasher(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let names: &[&str] = if kind == "argon2" {
        &["time_cost", "memory_cost", "parallelism", "hash_len", "salt_len", "type"]
    } else {
        &["rounds", "prefix"]
    };
    if args.len() > names.len() {
        return Err(Exc::type_error(format!("too many positional arguments ({} given)", args.len())));
    }
    let mut got: Vec<Option<V>> = vec![None; names.len()];
    for (i, a) in args.iter().enumerate() {
        got[i] = Some(a.clone());
    }
    for (k, v) in kwargs {
        let Some(i) = names.iter().position(|n| n == k) else {
            return Err(Exc::type_error(format!("__init__() got an unexpected keyword argument '{k}'")));
        };
        got[i] = Some(v.clone());
    }
    let h = if kind == "argon2" {
        let alg = match got[5].as_ref() {
            None => argon2::Algorithm::Argon2id,
            Some(V::Str(t)) => match &**t {
                "ID" => argon2::Algorithm::Argon2id,
                "I" => argon2::Algorithm::Argon2i,
                "D" => argon2::Algorithm::Argon2d,
                o => return Err(Exc::value_error(format!("py2axum: argon2 type {o}"))),
            },
            Some(o) => return Err(Exc::type_error(format!("py2axum: type= must be an argon2.Type, not {}", o.type_name()))),
        };
        PwdHasher::Argon2 {
            t: int_arg(got[0].as_ref(), "time_cost", 3)? as u32,
            m: int_arg(got[1].as_ref(), "memory_cost", 65536)? as u32,
            p: int_arg(got[2].as_ref(), "parallelism", 4)? as u32,
            len: int_arg(got[3].as_ref(), "hash_len", 32)? as usize,
            salt: int_arg(got[4].as_ref(), "salt_len", 16)? as usize,
            alg,
        }
    } else {
        let prefix = match got[1].as_ref() {
            None => "2b".to_string(),
            Some(V::Str(p)) => p.to_string(),
            Some(o) => return Err(Exc::type_error(format!("py2axum: prefix= must be a str, not {}", o.type_name()))),
        };
        PwdHasher::Bcrypt { rounds: int_arg(got[0].as_ref(), "rounds", 12)? as u32, prefix }
    };
    Ok(V::native(Native::PwdHasher(Arc::new(h))))
}

fn str_or_bytes(v: Option<&V>, name: &str) -> R<Vec<u8>> {
    match v {
        Some(V::Str(s)) => Ok(s.as_bytes().to_vec()),
        Some(V::Bytes(b)) => Ok(b.to_vec()),
        // pwdlib's validate_str_or_bytes
        Some(_) => Err(Exc::type_error(format!("{name} must be str or bytes"))),
        None => Err(Exc::type_error(format!("missing required argument: '{name}'"))),
    }
}

/// pwdlib's ARGON2_ENCODED_HASH_REGEX (identify)
fn argon2_identify(h: &[u8]) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^\$(argon2(id|i|d))\$(?:v=\d+\$)?m=\d+,t=\d+,p=\d+(?:\$[^$]+(?:\$.+?)?)?$").unwrap()
    });
    std::str::from_utf8(h).map(|s| re.is_match(s)).unwrap_or(false)
}

/// pwdlib's bcrypt `_IDENTIFY_REGEX` (`re.match`: a trailing newline is accepted before `$`): (prefix, rounds)
fn bcrypt_parse(h: &[u8]) -> Option<(String, u32)> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^\$(2[abxy])\$(\d{2})\$[A-Za-z0-9+/.]{22}[A-Za-z0-9+/.]{31}\n?$").unwrap());
    let s = std::str::from_utf8(h).ok()?;
    let c = re.captures(s)?;
    Some((c[1].to_string(), c[2].parse().ok()?))
}

impl PwdHasher {
    fn identify(&self, h: &[u8]) -> bool {
        match self {
            PwdHasher::Argon2 { .. } => argon2_identify(h),
            PwdHasher::Bcrypt { .. } => bcrypt_parse(h).is_some(),
        }
    }

    fn hash(&self, pw: &[u8], bcrypt5: bool) -> R<String> {
        match self {
            PwdHasher::Argon2 { t, m, p, len, salt, alg } => {
                use argon2::password_hash::{PasswordHasher, SaltString};
                let params = argon2::Params::new(*m, *t, *p, Some(*len)).map_err(|e| Exc::runtime(e.to_string()))?;
                let a = argon2::Argon2::new(*alg, argon2::Version::V0x13, params);
                let raw: Vec<u8> = (0..*salt).map(|_| rand::random::<u8>()).collect();
                let salt = SaltString::encode_b64(&raw).map_err(|e| Exc::runtime(e.to_string()))?;
                super::thread::blocking(|| a.hash_password(pw, &salt).map(|h| h.to_string())).map_err(|e| Exc::runtime(e.to_string()))
            }
            PwdHasher::Bcrypt { rounds, prefix } => {
                let salt: [u8; 16] = rand::random();
                let s = format!("${prefix}${rounds:02}${}", bcrypt_b64().encode(salt));
                let out = super::thread::blocking(|| hashpw(pw, s.as_bytes(), bcrypt5))?;
                Ok(String::from_utf8_lossy(&out).into_owned())
            }
        }
    }

    /// argon2-cffi's verify (a mismatch or an invalid hash is False for pwdlib); bcrypt.checkpw
    fn verify(&self, pw: &[u8], h: &[u8], bcrypt5: bool) -> R<bool> {
        match self {
            PwdHasher::Argon2 { .. } => {
                use argon2::password_hash::{PasswordHash, PasswordVerifier};
                let Ok(s) = std::str::from_utf8(h) else { return Ok(false) };
                let Ok(parsed) = PasswordHash::new(s) else { return Ok(false) };
                Ok(super::thread::blocking(|| argon2::Argon2::default().verify_password(pw, &parsed).is_ok()))
            }
            PwdHasher::Bcrypt { .. } => {
                use subtle::ConstantTimeEq;
                Ok(super::thread::blocking(|| hashpw(pw, h, bcrypt5))?.ct_eq(h).into())
            }
        }
    }

    /// argon2-cffi's check_needs_rehash (the hash's parameters differ from the hasher's); bcrypt: rounds or prefix
    fn needs_rehash(&self, h: &[u8]) -> bool {
        match self {
            PwdHasher::Argon2 { t, m, p, len, salt, alg } => {
                use argon2::password_hash::PasswordHash;
                let Ok(s) = std::str::from_utf8(h) else { return true };
                let Ok(ph) = PasswordHash::new(s) else { return true };
                let get = |k: &str| ph.params.get_decimal(k);
                ph.algorithm.as_str() != alg.as_str() || ph.version != Some(0x13) || get("m") != Some(*m) || get("t") != Some(*t)
                    || get("p") != Some(*p) || ph.hash.map(|x| x.len()) != Some(*len)
                    || ph.salt.map(|x| x.as_str().len()) != Some((salt * 4).div_ceil(3))
            }
            PwdHasher::Bcrypt { rounds, prefix } => match bcrypt_parse(h) {
                None => true,
                Some((pf, r)) => r != *rounds || pf != *prefix,
            },
        }
    }
}

pub fn pwd_method(ph: &PwdHash, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let get = |i: usize, k: &str| args.get(i).or_else(|| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v));
    let known: &[&str] = match name {
        "hash" => &["password"],
        "verify" | "verify_and_update" => &["password", "hash"],
        _ => return Err(Exc::attr_error(format!("'PasswordHash' object has no attribute '{name}'"))),
    };
    if name == "hash" && kwargs.iter().any(|(k, v)| k == "salt" && !v.is_none()) {
        return Err(Exc::type_error("py2axum: PasswordHash.hash(salt=) is not supported (a random salt)"));
    }
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !known.contains(&k.as_str()) && !(name == "hash" && k == "salt")) {
        return Err(Exc::type_error(format!("py2axum: PasswordHash.{name}({k}=) is not supported")));
    }
    if args.len() > known.len() {
        return Err(Exc::type_error(format!("PasswordHash.{name}() takes {} positional arguments", known.len() + 1)));
    }
    let pw = str_or_bytes(get(0, "password"), "password")?;
    let current = &ph.hashers[0];
    if name == "hash" {
        return Ok(V::str(current.hash(&pw, ph.bcrypt5)?));
    }
    let h = str_or_bytes(get(1, "hash"), "hash")?;
    let Some((i, hasher)) = ph.hashers.iter().enumerate().find(|(_, x)| x.identify(&h)) else {
        let e = Exc::msg(&UNKNOWN_HASH_ERROR, "This hash can't be identified. Make sure it's valid and that its \
corresponding hasher is enabled.");
        // `exc.hash`: what it failed to identify
        e.0.attrs.lock().insert("hash".into(), get(1, "hash").cloned().unwrap_or(V::None));
        return Err(e);
    };
    let ok = hasher.verify(&pw, &h, ph.bcrypt5)?;
    if name == "verify" {
        return Ok(V::Bool(ok));
    }
    // a hash of another hasher than the current one, or with other parameters, is hashed again
    let updated = if ok && (i != 0 || hasher.needs_rehash(&h)) { V::str(current.hash(&pw, ph.bcrypt5)?) } else { V::None };
    Ok(V::tuple(vec![V::Bool(ok), updated]))
}

// ---------------------------------------------------------------- passlib 1.7.4 (CryptContext of bcrypt)

/// `passlib.context.CryptContext(schemes=["bcrypt"], deprecated="auto")` (the only configuration translated):
/// its `bcrypt` handler over the `bcrypt` package. `bcrypt`: the locked pyca/bcrypt release, which decides how
/// passlib loads it (4.1 and later no longer have `bcrypt.__about__`; 5.0's `hashpw` refuses the 255-byte
/// password of passlib's self-test, so the backend never loads and every hash raises).
pub struct PassCtx {
    bcrypt: (u32, u32),
    /// `CryptContext._dummy_hash` (a memoized property)
    dummy: parking_lot::Mutex<Option<String>>,
}

/// passlib's `_BcryptBackend` is loaded once per process (`bcrypt.set_backend()`)
static PASSLIB_LOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn passlib_context(bcrypt_version: &str) -> R {
    let mut it = bcrypt_version.split('.').map(|x| x.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().unwrap_or(0));
    let bcrypt = (it.next().unwrap_or(5), it.next().unwrap_or(0));
    Ok(V::native(Native::PassCtx(std::sync::Arc::new(PassCtx { bcrypt, dummy: parking_lot::Mutex::new(None) }))))
}

const BCRYPT64: &str = "./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const IDENTS: [&str; 5] = ["$2$", "$2a$", "$2x$", "$2y$", "$2b$"];
/// `passlib.utils.MAX_PASSWORD_SIZE` (PASSLIB_MAX_PASSWORD_SIZE unset)
const MAX_PASSWORD_SIZE: usize = 4096;

/// `passlib.exc.type_name`: the module-qualified name of a value's class, but for builtins and None
async fn passlib_type_name(cx: &Cx, v: &V) -> String {
    match v {
        V::None => "None".into(),
        V::Inst(_) | V::Obj(_) | V::Native(_) => {
            if let Ok(cls) = super::methods::getattr(cx, v, "__class__").await {
                if let (Ok(V::Str(m)), Ok(V::Str(n))) =
                    (super::methods::getattr(cx, &cls, "__module__").await, super::methods::getattr(cx, &cls, "__name__").await)
                {
                    if !m.is_empty() && &*m != "builtins" {
                        return format!("{m}.{n}");
                    }
                    return n.to_string();
                }
            }
            v.type_name().to_string()
        }
        _ => v.type_name().to_string(),
    }
}

async fn expected_string(cx: &Cx, v: &V, param: &str) -> Exc {
    Exc::type_error(format!("{param} must be unicode or bytes, not {}", passlib_type_name(cx, v).await))
}

fn password_value_error(msg: &str) -> Exc {
    Exc::msg(&PASSLIB_PASSWORD_VALUE_ERROR, msg)
}

fn password_size_error() -> Exc {
    let e = Exc::msg(&PASSLIB_PASSWORD_SIZE_ERROR, "password exceeds maximum allowed size");
    e.0.attrs.lock().insert("max_size".into(), V::Int(MAX_PASSWORD_SIZE as i64));
    e
}

/// `passlib.utils.handlers.validate_secret`
async fn validate_secret(cx: &Cx, secret: &V) -> R<()> {
    let n = match secret {
        V::Str(s) => s.chars().count(),
        V::Bytes(b) => b.len(),
        o => return Err(expected_string(cx, o, "secret").await),
    };
    if n > MAX_PASSWORD_SIZE {
        return Err(password_size_error());
    }
    Ok(())
}

fn unknown_hash() -> Exc {
    let msg = "hash could not be identified";
    let e = Exc::new(&PASSLIB_UNKNOWN_HASH_ERROR, vec![V::str(msg), V::None]);
    let mut a = e.0.attrs.lock();
    a.insert("message".into(), V::str(msg));
    a.insert("value".into(), V::None);
    drop(a);
    e
}

/// `CryptContext._identify_record` (a single bcrypt record): the hash's type, then `bcrypt.identify`
/// (`to_unicode_for_identify`: UTF-8, else Latin-1, then the ident prefixes)
async fn identify(cx: &Cx, hash: &V, required: bool) -> R<bool> {
    let s: String = match hash {
        V::Str(s) => s.to_string(),
        V::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => b.iter().map(|&c| c as char).collect(),
        },
        o => return Err(expected_string(cx, o, "hash").await),
    };
    if IDENTS.iter().any(|i| s.starts_with(i)) {
        return Ok(true);
    }
    if required {
        return Err(unknown_hash());
    }
    Ok(false)
}

/// `to_unicode(hash, "ascii", "hash")`
async fn hash_text(cx: &Cx, hash: &V) -> R<String> {
    match hash {
        V::Str(s) => Ok(s.to_string()),
        V::Bytes(b) => match b.iter().position(|&c| c >= 0x80) {
            None => Ok(b.iter().map(|&c| c as char).collect()),
            Some(i) => Err(Exc::new(&UNICODE_DECODE_ERROR, vec![V::str("ascii"), V::Bytes(b.clone()), V::Int(i as i64),
                                                                 V::Int(i as i64 + 1), V::str("ordinal not in range(128)")])),
        },
        o => Err(expected_string(cx, o, "hash").await),
    }
}

/// `bcrypt64.check_repair_unused`: the padding bits of the last character cleared (`mask`: 15 for a 22-char
/// salt, 3 for a 31-char checksum)
fn repair_unused(s: &str, mask: usize) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    if let Some(last) = chars.last_mut() {
        if let Some(i) = BCRYPT64.find(*last) {
            *last = BCRYPT64.as_bytes()[i & !mask] as char;
        }
    }
    chars.into_iter().collect()
}

/// Whether CPython's `int(s)` accepts `s` (base 10): surrounding whitespace, a sign, decimal digits with
/// single underscores between them
fn py_int_accepts(s: &str) -> bool {
    let t = s.trim_matches(super::methods::py_space);
    let t = t.strip_prefix(['+', '-']).unwrap_or(t);
    let dec = |c: char| super::methods::str_class(c, |t| t.decimal, |c| c.is_ascii_digit());
    !t.is_empty() && !t.starts_with('_') && !t.ends_with('_') && !t.contains("__") && t.chars().all(|c| c == '_' || dec(c))
}

struct BcryptHash {
    ident: &'static str,
    rounds: u32,
    salt: String,
    checksum: Option<String>,
}

/// `bcrypt.from_string(hash)` and the handler's constructor: the ident, the cost field, then the checksum, the
/// salt and the rounds as the mixins normalize them (GenericHandler first)
async fn parse_bcrypt(cx: &Cx, hash: &V) -> R<BcryptHash> {
    let h = hash_text(cx, hash).await?;
    let ident = *IDENTS.iter().find(|i| h.starts_with(**i)).ok_or_else(|| Exc::value_error("not a valid bcrypt hash"))?;
    if ident == "$2x$" {
        return Err(Exc::value_error("crypt_blowfish's buggy '2x' hashes are not currently supported"));
    }
    let tail = &h[ident.len()..];
    let parts = super::ops::unpack(&V::list(tail.split('$').map(V::str).collect()), 2)?;
    let (V::Str(rounds_str), V::Str(data)) = (&parts[0], &parts[1]) else { unreachable!() };
    // `int(rounds_str)` then `rounds_str != "%02d" % rounds`
    let canonical = {
        let d = rounds_str.strip_prefix('-').unwrap_or(rounds_str);
        let neg = d.len() != rounds_str.len();
        !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
            && if neg { !d.starts_with('0') } else { d.len() == 2 || !d.starts_with('0') && d.len() > 2 }
    };
    if !canonical {
        if py_int_accepts(rounds_str) {
            return Err(Exc::value_error("malformed bcrypt hash (malformed cost field)"));
        }
        return Err(Exc::value_error(format!("invalid literal for int() with base 10: {}", super::ops::str_repr(rounds_str))));
    }
    let chars: Vec<char> = data.chars().collect();
    let salt: String = chars.iter().take(22).collect();
    let chk: String = chars.iter().skip(22).collect();
    let checksum = if chk.is_empty() {
        None
    } else {
        if chk.chars().count() != 31 {
            return Err(Exc::value_error("malformed bcrypt hash (checksum must be exactly 31 chars)"));
        }
        if chk.chars().any(|c| !BCRYPT64.contains(c)) {
            return Err(Exc::value_error("invalid characters in bcrypt checksum"));
        }
        // a checksum with padding bits set: repaired (with a PasslibHashWarning, not printed by the binary)
        Some(repair_unused(&chk, 3))
    };
    if salt.chars().any(|c| !BCRYPT64.contains(c)) {
        return Err(Exc::value_error("invalid characters in bcrypt salt"));
    }
    if salt.chars().count() < 22 {
        return Err(Exc::value_error("salt too small (bcrypt requires exactly 22 chars)"));
    }
    let salt = repair_unused(&salt, 15);
    let neg = rounds_str.starts_with('-');
    let digits = rounds_str.trim_start_matches('-');
    let small: Option<u32> = if neg { None } else { digits.parse().ok() };
    match small {
        Some(r) if (4..=31).contains(&r) => Ok(BcryptHash { ident, rounds: r, salt, checksum }),
        Some(r) if r < 4 => Err(Exc::value_error(format!("bcrypt: rounds ({r}) is too low, must be at least 4"))),
        _ if neg => Err(Exc::value_error(format!("bcrypt: rounds ({rounds_str}) is too low, must be at least 4"))),
        _ => Err(Exc::value_error(format!("bcrypt: rounds ({digits}) is too large, cannot be more than 31"))),
    }
}

impl PassCtx {
    /// `_NoBackend._stub_requires_backend()`: loads the `bcrypt` backend the first time (its version read
    /// from `bcrypt.__about__`, trapped and logged from bcrypt 4.1); bcrypt 5.0 fails passlib's wraparound
    /// self-test (a 255-byte password), so it is retried, and fails, at every call
    fn backend(&self) -> R<()> {
        if PASSLIB_LOADED.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        if self.bcrypt >= (4, 1) {
            super::logging::emit_sync("passlib.handlers.bcrypt", super::logging::WARNING, "(trapped) error reading bcrypt version",
                                      vec![], vec![], Some(Exc::attr_error("module 'bcrypt' has no attribute '__about__'")),
                                      &super::logging::NO_LOC);
        }
        if self.bcrypt.0 >= 5 {
            return Err(Exc::value_error("password cannot be longer than 72 bytes, truncate manually if necessary (e.g. my_password[:72])"));
        }
        PASSLIB_LOADED.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// `bcrypt._calc_checksum(secret)` of the `bcrypt` backend (`_norm_digest_args`, then `bcrypt.hashpw`)
    async fn checksum(&self, cx: &Cx, secret: &V, ident: &'static str, rounds: u32, salt: &str) -> R<String> {
        self.backend()?;
        let mut pw: Vec<u8> = match secret {
            V::Str(s) => s.as_bytes().to_vec(),
            V::Bytes(b) => b.to_vec(),
            o => return Err(expected_string(cx, o, "secret").await),
        };
        validate_secret(cx, &V::Bytes(std::sync::Arc::from(&pw[..]))).await?;
        if pw.contains(&0) {
            return Err(password_value_error("bcrypt does not allow NULL bytes in password"));
        }
        let mut ident = ident;
        if ident == "$2$" {
            // `$2$` is not supported by the `bcrypt` package: the password repeated up to 72 bytes, as `$2b$`
            if !pw.is_empty() {
                pw = pw.iter().cycle().take(72).copied().collect();
            }
            ident = "$2b$";
        }
        let config = format!("{ident}{rounds:02}${salt}");
        let bcrypt5 = self.bcrypt.0 >= 5;
        let out = super::thread::blocking(|| hashpw(&pw, config.as_bytes(), bcrypt5))?;
        Ok(String::from_utf8_lossy(&out[out.len() - 31..]).into_owned())
    }

    /// `bcrypt.hash(secret)` (a new salt, cost 12, `$2b$`)
    async fn hash(&self, cx: &Cx, secret: &V) -> R<String> {
        validate_secret(cx, secret).await?;
        let raw: [u8; 16] = rand::random();
        let salt = bcrypt_b64().encode(raw);
        let chk = self.checksum(cx, secret, "$2b$", 12, &salt).await?;
        Ok(format!("$2b$12${salt}{chk}"))
    }

    /// `bcrypt.verify(secret, hash)` (the hash already identified)
    async fn verify(&self, cx: &Cx, secret: &V, hash: &V) -> R<bool> {
        validate_secret(cx, secret).await?;
        let h = parse_bcrypt(cx, hash).await?;
        let Some(chk) = &h.checksum else {
            return Err(Exc::value_error("expected bcrypt hash, got bcrypt config string instead"));
        };
        let calc = self.checksum(cx, secret, h.ident, h.rounds, &h.salt).await?;
        use subtle::ConstantTimeEq;
        Ok(calc.as_bytes().ct_eq(chk.as_bytes()).into())
    }

    /// `bcrypt.needs_update(hash)`: a `$2a$` hash whose salt has padding bits set (passlib issue 25)
    async fn needs_update(&self, cx: &Cx, hash: &V) -> R<bool> {
        let h = match hash {
            V::Bytes(_) => hash_text(cx, hash).await?,
            V::Str(s) => s.to_string(),
            o => return Err(expected_string(cx, o, "hash").await),
        };
        if h.starts_with("$2a$") {
            let c = h.chars().nth(28).ok_or_else(|| Exc::msg(&INDEX_ERROR, "string index out of range"))?;
            if !".Oeu".contains(c) {
                return Ok(true);
            }
        }
        parse_bcrypt(cx, hash).await?;
        Ok(false)
    }

    /// `CryptContext.dummy_verify()`: verify against a memoized hash of "too many secrets"
    async fn dummy_verify(&self, cx: &Cx) -> R<()> {
        let secret = V::str("too many secrets");
        let cached = self.dummy.lock().clone();
        let h = match cached {
            Some(h) => h,
            None => {
                let h = self.hash(cx, &secret).await?;
                *self.dummy.lock() = Some(h.clone());
                h
            }
        };
        self.verify(cx, &secret, &V::str(h)).await?;
        Ok(())
    }
}

/// The methods of a CryptContext (passlib 1.7.4's signatures; `scheme`, `category` and handler keywords are
/// refused)
pub async fn passlib_method(cx: &Cx, c: &PassCtx, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let params: &[&str] = match name {
        "hash" => &["secret"],
        "verify" | "verify_and_update" => &["secret", "hash"],
        "needs_update" => &["hash"],
        "identify" => &["hash"],
        "dummy_verify" => &[],
        "encrypt" | "genconfig" | "genhash" | "hash_needs_update" | "is_enabled" | "disable" | "enable" | "handler"
        | "schemes" | "default_scheme" | "using" | "copy" | "replace" | "update" | "load" | "load_path" | "to_dict"
        | "to_string" | "from_string" | "from_path" | "reset_min_verify_time" | "mvt_estimate_max_samples" => {
            return Err(Exc::type_error(format!("py2axum: CryptContext.{name}() is not supported (hash, verify, verify_and_update, \
needs_update, identify and dummy_verify are)")));
        }
        _ => return Err(Exc::attr_error(format!("'CryptContext' object has no attribute '{name}'"))),
    };
    // keyword-only extras that keep the default behaviour
    let extra: &[&str] = match name {
        "needs_update" => &["secret"],
        "identify" => &["required"],
        _ => &[],
    };
    if args.len() > params.len() {
        return Err(Exc::type_error(format!("py2axum: CryptContext.{name}() with {} positional arguments is not supported", args.len())));
    }
    for (k, _) in kwargs {
        if !params.contains(&k.as_str()) && !extra.contains(&k.as_str()) {
            return Err(Exc::type_error(format!("py2axum: CryptContext.{name}({k}=) is not supported")));
        }
        if let Some(i) = params.iter().position(|p| p == k) {
            if i < args.len() {
                return Err(Exc::type_error(format!("CryptContext.{name}() got multiple values for argument '{k}'")));
            }
        }
    }
    let get = |i: usize| -> R<V> {
        args.get(i).or_else(|| kw(kwargs, params[i])).cloned().ok_or_else(|| {
            Exc::type_error(format!("CryptContext.{name}() missing 1 required positional argument: '{}'", params[i]))
        })
    };
    match name {
        "hash" => Ok(V::str(c.hash(cx, &get(0)?).await?)),
        "verify" => {
            let (secret, hash) = (get(0)?, get(1)?);
            if hash.is_none() {
                c.dummy_verify(cx).await?;
                return Ok(V::Bool(false));
            }
            identify(cx, &hash, true).await?;
            Ok(V::Bool(c.verify(cx, &secret, &hash).await?))
        }
        "verify_and_update" => {
            let (secret, hash) = (get(0)?, get(1)?);
            if hash.is_none() {
                c.dummy_verify(cx).await?;
                return Ok(V::tuple(vec![V::Bool(false), V::None]));
            }
            identify(cx, &hash, true).await?;
            if !c.verify(cx, &secret, &hash).await? {
                return Ok(V::tuple(vec![V::Bool(false), V::None]));
            }
            if c.needs_update(cx, &hash).await? {
                return Ok(V::tuple(vec![V::Bool(true), V::str(c.hash(cx, &secret).await?)]));
            }
            Ok(V::tuple(vec![V::Bool(true), V::None]))
        }
        "needs_update" => {
            let hash = get(0)?;
            identify(cx, &hash, true).await?;
            Ok(V::Bool(c.needs_update(cx, &hash).await?))
        }
        "identify" => {
            let required = match kw(kwargs, "required") {
                Some(v) => super::ops::truthy(v)?,
                None => false,
            };
            Ok(if identify(cx, &get(0)?, required).await? { V::str("bcrypt") } else { V::None })
        }
        _ => {
            c.dummy_verify(cx).await?;
            Ok(V::Bool(false))
        }
    }
}
