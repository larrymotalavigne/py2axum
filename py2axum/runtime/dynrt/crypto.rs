//! `cryptography`'s RSA keys as DKIM code uses them: `rsa.generate_private_key`, `private_bytes`,
//! `public_key`, `public_bytes`, `key_size` (crate `rsa`; PEM and DER as OpenSSL writes them).
use std::sync::Arc;

use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::traits::PublicKeyParts;

use super::v::*;

pub enum Key {
    Private(rsa::RsaPrivateKey),
    Public(rsa::RsaPublicKey),
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn int(v: Option<&V>, name: &str) -> R<i64> {
    match v {
        Some(V::Int(n)) => Ok(*n),
        Some(o) => Err(Exc::type_error(format!("argument '{name}': '{}' object cannot be interpreted as an integer", o.type_name()))),
        None => Err(Exc::type_error(format!("generate_private_key() missing required argument '{name}'"))),
    }
}

/// `rsa.generate_private_key(public_exponent, key_size, backend=None)`
pub async fn generate_private_key(args: &[V], kwargs: &[(String, V)]) -> R {
    let e = int(args.first().or_else(|| kw(kwargs, "public_exponent")), "public_exponent")?;
    let bits = int(args.get(1).or_else(|| kw(kwargs, "key_size")), "key_size")?;
    if e != 3 && e != 65537 {
        return Err(Exc::value_error(
            "public_exponent must be either 3 (for legacy compatibility) or 65537. Almost everyone should choose 65537 here!",
        ));
    }
    if bits < 1024 {
        return Err(Exc::value_error("key_size must be at least 1024-bits."));
    }
    if bits > 16384 {
        return Err(Exc::value_error("py2axum: an RSA key above 16384 bits is not supported"));
    }
    let key = tokio::task::spawn_blocking(move || {
        rsa::RsaPrivateKey::new_with_exp(&mut rand::thread_rng(), bits as usize, &rsa::BigUint::from(e as u64))
    })
    .await
    .map_err(|e| Exc::runtime(e.to_string()))?
    .map_err(|e| Exc::value_error(e.to_string()))?;
    Ok(V::native(Native::Rsa(Arc::new(Key::Private(key)))))
}

fn enc_err(e: impl std::fmt::Display) -> Exc {
    Exc::value_error(e.to_string())
}

/// the `serialization` constants are strings here ("Encoding.PEM", "PrivateFormat.PKCS8", ...)
fn constant(v: Option<&V>, what: &str) -> R<String> {
    match v {
        Some(V::Str(s)) if s.starts_with(what) => Ok(s.to_string()),
        Some(_) => Err(Exc::type_error(format!("{} must be an element in {}", what.to_lowercase(), what))),
        None => Err(Exc::type_error(format!("missing required argument '{}'", what.to_lowercase()))),
    }
}

/// `key.key_size`
pub fn attr(k: &Arc<Key>, name: &str) -> Option<R> {
    let bits = match &**k {
        Key::Private(p) => p.size() * 8,
        Key::Public(p) => p.size() * 8,
    };
    (name == "key_size").then(|| Ok(V::Int(bits as i64)))
}

pub fn method(k: &Arc<Key>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match (&**k, name) {
        (Key::Private(p), "public_key") => Ok(V::native(Native::Rsa(Arc::new(Key::Public(p.to_public_key()))))),
        (Key::Private(p), "private_bytes") => {
            let enc = constant(args.first().or_else(|| kw(kwargs, "encoding")), "Encoding")?;
            let fmt = constant(args.get(1).or_else(|| kw(kwargs, "format")), "PrivateFormat")?;
            match args.get(2).or_else(|| kw(kwargs, "encryption_algorithm")) {
                Some(V::Str(s)) if &**s == "NoEncryption" => {}
                _ => return Err(Exc::type_error("Encryption algorithm must be a KeySerializationEncryption instance")),
            }
            if enc != "Encoding.PEM" && enc != "Encoding.DER" {
                return Err(Exc::value_error("format is invalid with this key"));
            }
            let pem = enc == "Encoding.PEM";
            let out: Vec<u8> = match fmt.as_str() {
                "PrivateFormat.PKCS8" if pem => p.to_pkcs8_pem(LineEnding::LF).map_err(enc_err)?.as_bytes().to_vec(),
                "PrivateFormat.PKCS8" => p.to_pkcs8_der().map_err(enc_err)?.as_bytes().to_vec(),
                "PrivateFormat.TraditionalOpenSSL" if pem => p.to_pkcs1_pem(LineEnding::LF).map_err(enc_err)?.as_bytes().to_vec(),
                "PrivateFormat.TraditionalOpenSSL" => p.to_pkcs1_der().map_err(enc_err)?.as_bytes().to_vec(),
                _ => return Err(Exc::value_error("format is invalid with this key")),
            };
            Ok(V::Bytes(Arc::from(out)))
        }
        (Key::Public(p), "public_bytes") => {
            let enc = constant(args.first().or_else(|| kw(kwargs, "encoding")), "Encoding")?;
            let fmt = constant(args.get(1).or_else(|| kw(kwargs, "format")), "PublicFormat")?;
            if enc != "Encoding.PEM" && enc != "Encoding.DER" {
                return Err(Exc::type_error("encoding must be Encoding.DER or Encoding.PEM"));
            }
            let pem = enc == "Encoding.PEM";
            let out: Vec<u8> = match fmt.as_str() {
                "PublicFormat.SubjectPublicKeyInfo" if pem => p.to_public_key_pem(LineEnding::LF).map_err(enc_err)?.into_bytes(),
                "PublicFormat.SubjectPublicKeyInfo" => p.to_public_key_der().map_err(enc_err)?.as_bytes().to_vec(),
                "PublicFormat.PKCS1" if pem => p.to_pkcs1_pem(LineEnding::LF).map_err(enc_err)?.into_bytes(),
                "PublicFormat.PKCS1" => p.to_pkcs1_der().map_err(enc_err)?.as_bytes().to_vec(),
                _ => return Err(Exc::value_error("format is invalid with this key")),
            };
            Ok(V::Bytes(Arc::from(out)))
        }
        _ => Err(Exc::attr_error(format!(
            "'{}' object has no attribute '{name}'",
            if matches!(&**k, Key::Private(_)) { "RSAPrivateKey" } else { "RSAPublicKey" }
        ))),
    }
}
