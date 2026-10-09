//! Password and second-factor libraries: pyca/bcrypt 5.0 (itself built on the `bcrypt` crate: same
//! salt parsing, same errors) and pyotp 2.9 (TOTP), with `base64.b32decode` as CPython has it.
use base64::Engine as _;

use super::ops;
use super::v::*;

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

fn hashpw(password: &[u8], salt: &[u8]) -> R<Vec<u8>> {
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

/// `bcrypt.gensalt` / `hashpw` / `checkpw`
pub fn call(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
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
            Ok(V::Bytes(std::sync::Arc::from(&super::thread::blocking(|| hashpw(&pw, &salt))?[..])))
        }
        "checkpw" => {
            let names = ["password", "hashed_password"];
            let pw = bytes_arg(arg(name, args, kwargs, 0, "password", &names)?, "password")?;
            let hashed = bytes_arg(arg(name, args, kwargs, 1, "hashed_password", &names)?, "hashed_password")?;
            use subtle::ConstantTimeEq;
            Ok(V::Bool(super::thread::blocking(|| hashpw(&pw, &hashed))?.ct_eq(&hashed).into()))
        }
        _ => Err(Exc::attr_error(format!("module 'bcrypt' has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- base32

/// `base64.b32decode(s, casefold)` (binascii.Error on bad input)
pub fn b32decode(s: &str, casefold: bool) -> R<Vec<u8>> {
    let err = |m: &str| Exc::msg(&BINASCII_ERROR, m);
    let mut s: Vec<u8> = s.bytes().collect();
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

// ---------------------------------------------------------------- pwdlib (Argon2)

/// `pwdlib.PasswordHash.recommended()`: one Argon2 hasher with argon2-cffi's defaults (argon2id, t=3,
/// m=65536 KiB, p=4, 32-byte hash, 16-byte salt).
pub fn pwd_recommended() -> R {
    Ok(V::native(Native::PwdHash))
}

const ARGON2_T: u32 = 3;
const ARGON2_M: u32 = 65536;
const ARGON2_P: u32 = 4;
const ARGON2_LEN: usize = 32;

fn str_or_bytes(v: Option<&V>, name: &str) -> R<Vec<u8>> {
    match v {
        Some(V::Str(s)) => Ok(s.as_bytes().to_vec()),
        Some(V::Bytes(b)) => Ok(b.to_vec()),
        Some(o) => Err(Exc::type_error(format!("{name} must be str or bytes, not {}", o.type_name()))),
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

fn argon2_hash(pw: &[u8]) -> R<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let params = argon2::Params::new(ARGON2_M, ARGON2_T, ARGON2_P, Some(ARGON2_LEN)).map_err(|e| Exc::runtime(e.to_string()))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let salt: [u8; 16] = rand::random();
    let salt = SaltString::encode_b64(&salt).map_err(|e| Exc::runtime(e.to_string()))?;
    super::thread::blocking(|| a.hash_password(pw, &salt).map(|h| h.to_string())).map_err(|e| Exc::runtime(e.to_string()))
}

/// argon2-cffi's verify: a mismatch or an invalid hash is False for pwdlib
fn argon2_verify(pw: &[u8], h: &[u8]) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(s) = std::str::from_utf8(h) else { return false };
    let Ok(parsed) = PasswordHash::new(s) else { return false };
    super::thread::blocking(|| argon2::Argon2::default().verify_password(pw, &parsed).is_ok())
}

/// argon2-cffi's check_needs_rehash: the hash's parameters differ from the hasher's
fn argon2_needs_rehash(h: &[u8]) -> bool {
    use argon2::password_hash::PasswordHash;
    let Ok(s) = std::str::from_utf8(h) else { return true };
    let Ok(p) = PasswordHash::new(s) else { return true };
    let get = |k: &str| p.params.get_decimal(k);
    p.algorithm.as_str() != "argon2id" || p.version != Some(0x13) || get("m") != Some(ARGON2_M) || get("t") != Some(ARGON2_T)
        || get("p") != Some(ARGON2_P) || p.hash.map(|x| x.len()) != Some(ARGON2_LEN) || p.salt.map(|x| x.as_str().len()) != Some(22)
}

pub fn pwd_method(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let get = |i: usize, k: &str| args.get(i).or_else(|| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v));
    let known: &[&str] = match name {
        "hash" => &["password"],
        "verify" | "verify_and_update" => &["password", "hash"],
        _ => return Err(Exc::attr_error(format!("'PasswordHash' object has no attribute '{name}'"))),
    };
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !known.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("py2axum: PasswordHash.{name}({k}=) is not supported")));
    }
    if args.len() > known.len() {
        return Err(Exc::type_error(format!("PasswordHash.{name}() takes {} positional arguments", known.len() + 1)));
    }
    let pw = str_or_bytes(get(0, "password"), "password")?;
    if name == "hash" {
        return Ok(V::str(argon2_hash(&pw)?));
    }
    let h = str_or_bytes(get(1, "hash"), "hash")?;
    if !argon2_identify(&h) {
        return Err(Exc::msg(&UNKNOWN_HASH_ERROR, format!("This hash can't be identified. Make sure it's valid and that its \
corresponding hasher is enabled. {}", String::from_utf8_lossy(&h))));
    }
    let ok = argon2_verify(&pw, &h);
    if name == "verify" {
        return Ok(V::Bool(ok));
    }
    let updated = if ok && argon2_needs_rehash(&h) { V::str(argon2_hash(&pw)?) } else { V::None };
    Ok(V::tuple(vec![V::Bool(ok), updated]))
}
