//! `google.oauth2.id_token.verify_oauth2_token` / `verify_token` (google-auth 2.49): the certificates
//! fetched from Google (x509 PEM per key id), RS256/ES256 signature, iat/exp with the clock skew,
//! audience, issuer — with google-auth's exception classes and messages.
use std::sync::Arc;

use base64::Engine as _;

use super::ops;
use super::v::*;

const CERTS_URL: &str = "https://www.googleapis.com/oauth2/v1/certs";
const ISSUERS: [&str; 2] = ["accounts.google.com", "https://accounts.google.com"];

fn malformed(m: impl Into<String>) -> Exc {
    Exc::msg(&GOOGLE_MALFORMED_ERROR, m.into())
}

fn invalid(m: impl Into<String>) -> Exc {
    Exc::msg(&GOOGLE_INVALID_VALUE, m.into())
}

/// `_helpers.padded_urlsafe_b64decode`
fn b64(seg: &[u8]) -> R<Vec<u8>> {
    let mut s = seg.to_vec();
    s.extend(std::iter::repeat_n(b'=', (4 - s.len() % 4) % 4));
    base64::engine::general_purpose::URL_SAFE.decode(&s).map_err(|_| Exc::msg(&BINASCII_ERROR, "Incorrect padding"))
}

fn segment(seg: &[u8]) -> R {
    let bytes = b64(seg)?;
    let text = String::from_utf8(bytes.clone()).map_err(|_| malformed(format!("Can't parse segment: {}", ops::repr(&V::Bytes(Arc::from(&bytes[..]))).unwrap_or_default())))?;
    super::pyd::loads(&text).map_err(|_| malformed(format!("Can't parse segment: {}", ops::repr(&V::Bytes(Arc::from(&bytes[..]))).unwrap_or_default())))
}

fn get(d: &V, k: &str) -> Option<V> {
    match d {
        V::Dict(m) => m.lock().get(&Key::Str(Arc::from(k))).map(|(_, v)| v.clone()),
        _ => None,
    }
}

/// a verifier from a PEM certificate or public key: Ok(true) when the signature matches
fn verify_one(alg: &str, pem: &str, message: &[u8], sig: &[u8]) -> R<bool> {
    use x509_cert::der::{DecodePem, Encode};
    let spki_der = if pem.contains("BEGIN CERTIFICATE") {
        let cert = x509_cert::Certificate::from_pem(pem.as_bytes()).map_err(|e| Exc::value_error(format!("Could not deserialize certificate: {e}")))?;
        cert.tbs_certificate.subject_public_key_info.to_der().map_err(|e| Exc::value_error(e.to_string()))?
    } else {
        let (_, der) = x509_cert::der::pem::decode_vec(pem.as_bytes()).map_err(|e| Exc::value_error(format!("Could not deserialize key data: {e}")))?;
        der
    };
    match alg {
        "RS256" => {
            use rsa::pkcs8::DecodePublicKey;
            use rsa::signature::Verifier;
            let key = rsa::RsaPublicKey::from_public_key_der(&spki_der).map_err(|e| Exc::value_error(e.to_string()))?;
            let vk = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(key);
            let Ok(s) = rsa::pkcs1v15::Signature::try_from(sig) else { return Ok(false) };
            Ok(vk.verify(message, &s).is_ok())
        }
        "ES256" => {
            use p256::pkcs8::DecodePublicKey;
            use p256::ecdsa::signature::Verifier;
            let key = p256::PublicKey::from_public_key_der(&spki_der).map_err(|e| Exc::value_error(e.to_string()))?;
            let vk = p256::ecdsa::VerifyingKey::from(key);
            // google-auth's ES256 signatures are r||s (64 bytes)
            let Ok(s) = p256::ecdsa::Signature::from_slice(sig) else { return Ok(false) };
            Ok(vk.verify(message, &s).is_ok())
        }
        _ => unreachable!(),
    }
}

/// `google.auth.jwt.decode(token, certs, audience=, clock_skew_in_seconds=)`
fn decode(token: &[u8], certs: &V, audience: &V, skew: i64) -> R {
    if token.iter().filter(|&&c| c == b'.').count() != 2 {
        return Err(malformed(format!("Wrong number of segments in token: {}", ops::repr(&V::Bytes(Arc::from(token))).unwrap_or_default())));
    }
    let parts: Vec<&[u8]> = token.split(|&c| c == b'.').collect();
    let signed = &token[..parts[0].len() + 1 + parts[1].len()];
    let signature = b64(parts[2])?;
    let header = segment(parts[0])?;
    let payload = segment(parts[1])?;
    if !matches!(header, V::Dict(_)) {
        return Err(malformed(format!("Header segment should be a JSON object: {}", ops::repr(&V::Bytes(Arc::from(parts[0]))).unwrap_or_default())));
    }
    if !matches!(payload, V::Dict(_)) {
        return Err(malformed(format!("Payload segment should be a JSON object: {}", ops::repr(&V::Bytes(Arc::from(parts[1]))).unwrap_or_default())));
    }
    let alg = get(&header, "alg").unwrap_or(V::None);
    let alg = match &alg {
        V::Str(a) if &**a == "RS256" || &**a == "ES256" => a.to_string(),
        other => return Err(invalid(format!("Unsupported signature algorithm {}", ops::str_(other).unwrap_or_default()))),
    };
    let kid = get(&header, "kid").filter(|k| ops::truthy(k).unwrap_or(false));
    let to_check: Vec<V> = match (certs, &kid) {
        (V::Dict(_), Some(k)) => match certs {
            V::Dict(m) => match m.lock().get(&Key::of(k)?) {
                Some((_, c)) => vec![c.clone()],
                None => return Err(malformed(format!("Certificate for key id {} not found.", ops::str_(k)?))),
            },
            _ => unreachable!(),
        },
        (V::Dict(m), None) => m.lock().values().map(|(_, c)| c.clone()).collect(),
        (other, _) => vec![other.clone()],
    };
    let mut ok = false;
    for c in to_check {
        if verify_one(&alg, &ops::str_(&c)?, signed, &signature)? {
            ok = true;
            break;
        }
    }
    if !ok {
        return Err(malformed("Could not verify token signature."));
    }
    let now = chrono::Utc::now().timestamp();
    for k in ["iat", "exp"] {
        if get(&payload, k).is_none() {
            return Err(malformed(format!("Token does not contain required claim {k}")));
        }
    }
    let iat = get(&payload, "iat").unwrap();
    let earliest = ops::sub(&iat, &V::Int(skew))?;
    if ops::cmp(&V::Int(now), &earliest)? == std::cmp::Ordering::Less {
        return Err(invalid(format!("Token used too early, {} < {}. Check that your computer's clock is set correctly.", now, ops::str_(&iat)?)));
    }
    let exp = get(&payload, "exp").unwrap();
    let latest = ops::add(&exp, &V::Int(skew))?;
    if ops::cmp(&latest, &V::Int(now))? == std::cmp::Ordering::Less {
        return Err(invalid(format!("Token expired, {} < {}", ops::str_(&latest)?, now)));
    }
    if !audience.is_none() {
        let claim = get(&payload, "aud").unwrap_or(V::None);
        let auds = match audience {
            V::Str(_) => V::list(vec![audience.clone()]),
            other => other.clone(),
        };
        if !ops::contains(&auds, &claim)? {
            return Err(invalid(format!("Token has wrong audience {}, expected one of {}", ops::str_(&claim)?, ops::str_(&auds)?)));
        }
    }
    Ok(payload)
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

async fn fetch_certs(url: &str) -> R {
    let transport = || Exc::msg(&GOOGLE_TRANSPORT_ERROR, format!("Could not fetch certificates at {url}"));
    let resp = http().get(url).send().await.map_err(|_| transport())?;
    if resp.status().as_u16() != 200 {
        return Err(transport());
    }
    let text = resp.text().await.map_err(|_| transport())?;
    super::pyd::loads(&text)
}

/// `verify_token(id_token, request, audience=None, certs_url=..., clock_skew_in_seconds=0)` and
/// `verify_oauth2_token(id_token, request, audience=None, clock_skew_in_seconds=0)`
pub async fn verify(oauth2: bool, args: &[V], kwargs: &[(String, V)]) -> R {
    let names: &[&str] = if oauth2 {
        &["id_token", "request", "audience", "clock_skew_in_seconds"]
    } else {
        &["id_token", "request", "audience", "certs_url", "clock_skew_in_seconds"]
    };
    let mut vals: Vec<Option<V>> = args.iter().cloned().map(Some).collect();
    if vals.len() > names.len() {
        return Err(Exc::type_error("verify_token() takes too many positional arguments"));
    }
    vals.resize(names.len(), None);
    for (k, v) in kwargs {
        let i = names.iter().position(|n| n == k).ok_or_else(|| Exc::type_error(format!("got an unexpected keyword argument '{k}'")))?;
        vals[i] = Some(v.clone());
    }
    let at = |n: &str| names.iter().position(|x| *x == n).and_then(|i| vals[i].clone());
    let token = match at("id_token").ok_or_else(|| Exc::type_error("missing 'id_token'"))? {
        V::Str(s) => s.as_bytes().to_vec(),
        V::Bytes(b) => b.to_vec(),
        o => return Err(Exc::value_error(format!("{} could not be converted to bytes", ops::repr(&o)?))),
    };
    let audience = at("audience").unwrap_or(V::None);
    let skew = match at("clock_skew_in_seconds") {
        None => 0,
        Some(V::Int(i)) => i,
        Some(o) => return Err(Exc::type_error(format!("py2axum: clock_skew_in_seconds of type {}", o.type_name()))),
    };
    let url = match at("certs_url") {
        Some(u) => ops::str_(&u)?,
        None => CERTS_URL.to_string(),
    };
    let certs = fetch_certs(&url).await?;
    if get(&certs, "keys").is_some() {
        return Err(Exc::type_error("py2axum: JWK certificate sets (pyjwt) are not supported"));
    }
    let info = decode(&token, &certs, &audience, skew)?;
    if oauth2 {
        let iss = get(&info, "iss").ok_or_else(|| Exc::new(&KEY_ERROR, vec![V::str("iss")]))?;
        if !ISSUERS.iter().any(|i| matches!(&iss, V::Str(s) if &**s == *i)) {
            return Err(Exc::msg(&GOOGLE_AUTH_ERROR, "Wrong issuer. 'iss' should be one of the following: ['accounts.google.com', 'https://accounts.google.com']"));
        }
    }
    Ok(info)
}
