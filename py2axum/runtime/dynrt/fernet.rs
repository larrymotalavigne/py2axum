//! `cryptography.fernet.Fernet` (cryptography 50): `encrypt` / `decrypt` per the Fernet spec,
//! compatible both ways with tokens written by the Python application (data at rest).
use std::sync::Arc;

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

use super::v::*;

pub struct Fernet {
    signing: [u8; 16],
    encryption: [u8; 16],
}

const KEY_ERROR: &str = "Fernet key must be 32 url-safe base64-encoded bytes.";

/// `base64.urlsafe_b64decode` of a str/bytes argument (lenient like binascii; None = binascii.Error)
fn urlsafe_decode(v: &V) -> R<Option<Vec<u8>>> {
    let raw: Vec<u8> = match v {
        V::Bytes(b) => b.to_vec(),
        V::Str(s) if s.is_ascii() => s.as_bytes().to_vec(),
        V::Str(_) => return Err(Exc::value_error("string argument should contain only ASCII characters")),
        other => {
            return Err(Exc::type_error(format!(
                "argument should be a bytes-like object or ASCII string, not '{}'",
                other.type_name()
            )))
        }
    };
    Ok(super::jose::b64_decode_unpadded(&raw))
}

/// `Fernet(key)`
pub fn new(key: &V) -> R {
    let k = urlsafe_decode(key)?.ok_or_else(|| Exc::value_error(KEY_ERROR))?;
    if k.len() != 32 {
        return Err(Exc::value_error(KEY_ERROR));
    }
    let mut f = Fernet { signing: [0; 16], encryption: [0; 16] };
    f.signing.copy_from_slice(&k[..16]);
    f.encryption.copy_from_slice(&k[16..]);
    Ok(V::native(Native::Fernet(f)))
}

fn mac(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn invalid() -> Exc {
    Exc::new(&INVALID_TOKEN, vec![])
}

pub fn method(f: &Fernet, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    if let Some((k, _)) = kwargs.first() {
        return Err(Exc::type_error(format!("py2axum: Fernet.{name}({k}=) is not supported")));
    }
    if matches!(name, "encrypt" | "decrypt") && args.len() != 1 {
        return Err(Exc::type_error(format!("py2axum: Fernet.{name}() takes exactly one argument here")));
    }
    match name {
        "encrypt" => {
            let V::Bytes(data) = &args[0] else {
                return Err(Exc::type_error("data must be bytes"));
            };
            let mut iv = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut iv);
            let ct = cbc::Encryptor::<aes::Aes128>::new(&f.encryption.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(data);
            let mut out = vec![0x80u8];
            out.extend((chrono::Utc::now().timestamp() as u64).to_be_bytes());
            out.extend(iv);
            out.extend(ct);
            let h = mac(&f.signing, &out);
            out.extend(h);
            Ok(V::Bytes(Arc::from(base64::engine::general_purpose::URL_SAFE.encode(out).as_bytes())))
        }
        "decrypt" => {
            let token = &args[0];
            if !matches!(token, V::Str(_) | V::Bytes(_)) {
                return Err(Exc::type_error("token must be bytes or str"));
            }
            let data = urlsafe_decode(token)?.ok_or_else(invalid)?;
            if data.first() != Some(&0x80) || data.len() < 9 {
                return Err(invalid());
            }
            let split = data.len().saturating_sub(32);
            let expected = mac(&f.signing, &data[..split]);
            if !bool::from(subtle::ConstantTimeEq::ct_eq(expected.as_slice(), &data[split..])) {
                return Err(invalid());
            }
            if data.len() < 25 + 32 {
                return Err(invalid());
            }
            let iv: [u8; 16] = data[9..25].try_into().unwrap();
            let plain = cbc::Decryptor::<aes::Aes128>::new(&f.encryption.into(), &iv.into())
                .decrypt_padded_vec_mut::<Pkcs7>(&data[25..split])
                .map_err(|_| invalid())?;
            Ok(V::Bytes(Arc::from(plain)))
        }
        _ => Err(Exc::attr_error(format!("'Fernet' object has no attribute '{name}'"))),
    }
}
