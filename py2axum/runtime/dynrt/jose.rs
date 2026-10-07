//! `jose.jwt.encode` / `jose.jwt.decode` (python-jose 3.5.0), HMAC algorithms only: same tokens byte
//! for byte, same exception classes and messages, same claim checks in the same order.
use std::sync::{Arc, OnceLock};

use base64::Engine;
use hmac::{Hmac, Mac};

use super::libs;
use super::ops;
use super::pyd;
use super::v::*;

const SUPPORTED: &[&str] = &[
    "HS256", "HS384", "HS512", "RS256", "RS384", "RS512", "ES256", "ES384", "ES512", "dir", "A128CBC-HS256",
    "A192CBC-HS384", "A256CBC-HS512", "A128GCM", "A192GCM", "A256GCM", "RSA1_5", "RSA-OAEP", "RSA-OAEP-256",
    "A128KW", "A192KW", "A256KW",
];
const ASYMMETRIC: &[&str] = &["RS256", "RS384", "RS512", "ES256", "ES384", "ES512"];

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// `base64url_decode`: pad to a multiple of 4, then `urlsafe_b64decode` (binascii's lenient mode:
/// characters outside the alphabet are skipped, decoding stops at complete padding).
pub(super) fn b64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut s = input.to_vec();
    let rem = s.len() % 4;
    if rem > 0 {
        s.extend(std::iter::repeat_n(b'=', 4 - rem));
    }
    b64_decode_unpadded(&s)
}

/// `base64.urlsafe_b64decode` as is (binascii's lenient mode, no padding added)
pub(super) fn b64_decode_unpadded(s: &[u8]) -> Option<Vec<u8>> {
    let (mut out, mut quad, mut pads, mut left) = (Vec::new(), 0u32, 0u32, 0u8);
    for &c in s {
        if c == b'=' {
            if quad >= 2 {
                pads += 1;
                if quad + pads >= 4 {
                    return Some(out);
                }
            }
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => continue,
        };
        match quad {
            0 => left = v,
            1 => {
                out.push((left << 2) | (v >> 4));
                left = v & 0xf;
            }
            2 => {
                out.push((left << 4) | (v >> 2));
                left = v & 0x3;
            }
            _ => out.push((left << 6) | v),
        }
        quad = (quad + 1) % 4;
    }
    if quad == 0 { Some(out) } else { None }
}

fn exc(class: &'static Class, msg: impl AsRef<str>) -> Exc {
    Exc::msg(class, msg)
}

fn key_bytes(key: &V) -> R<Vec<u8>> {
    let k: Vec<u8> = match key {
        V::Str(s) => s.as_bytes().to_vec(),
        V::Bytes(b) => b.to_vec(),
        V::Dict(_) => return Err(Exc::runtime("py2axum: JWK dictionaries are not supported as jose keys")),
        _ => return Err(exc(&JWK_ERROR, "Expecting a string- or bytes-formatted key.")),
    };
    static PEM: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let pem = PEM.get_or_init(|| {
        regex::bytes::Regex::new(
            "----[- ]BEGIN (CERTIFICATE|TRUSTED CERTIFICATE|PRIVATE KEY|PUBLIC KEY|ENCRYPTED PRIVATE KEY|OPENSSH PRIVATE KEY|DSA PRIVATE KEY|RSA PRIVATE KEY|RSA PUBLIC KEY|EC PRIVATE KEY|DH PARAMETERS|NEW CERTIFICATE REQUEST|CERTIFICATE REQUEST|SSH2 PUBLIC KEY|SSH2 ENCRYPTED PRIVATE KEY|X509 CRL)[- ]----",
        )
        .unwrap()
    });
    static SSH: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let ssh = SSH.get_or_init(|| regex::bytes::Regex::new(r"\A(\S+)[ \t]+(\S+)").unwrap());
    let has = |needle: &[u8]| k.windows(needle.len()).any(|w| w == needle);
    let ssh_key = ["ssh-ed25519", "ssh-rsa", "ssh-dss", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"]
        .iter()
        .any(|f| has(f.as_bytes()))
        || ssh.captures(&k).is_some_and(|c| c[1].ends_with(b"-cert-v01@openssh.com"));
    if pem.is_match(&k) || ssh_key {
        return Err(exc(
            &JWK_ERROR,
            "The specified key is an asymmetric key or x509 certificate and should not be used as an HMAC secret.",
        ));
    }
    Ok(k)
}

fn hmac_sign(alg: &str, key: &[u8], msg: &[u8]) -> Vec<u8> {
    macro_rules! mac {
        ($h:ty) => {{
            let mut m = <Hmac<$h> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
            m.update(msg);
            m.finalize().into_bytes().to_vec()
        }};
    }
    match alg {
        "HS256" => mac!(sha2::Sha256),
        "HS384" => mac!(sha2::Sha384),
        _ => mac!(sha2::Sha512),
    }
}

/// `jwk.construct(key, alg)` for a key that is not HMAC: python-jose builds an RSA/EC key from the
/// string (error) or finds no key class.
fn no_hmac(alg: &str) -> Exc {
    if ASYMMETRIC.contains(&alg) {
        exc(&JWK_ERROR, format!("py2axum: algorithm {alg} is not supported (HS256, HS384, HS512 only)"))
    } else {
        exc(&JWK_ERROR, "Unable to find an algorithm for key")
    }
}

/// `calendar.timegm(dt.utctimetuple())`
fn timegm(d: &super::dt::DateTime) -> i64 {
    d.utc().and_utc().timestamp()
}

/// `jwt.encode(claims, key, algorithm="HS256")`
pub fn encode(claims: &V, key: &V, algorithm: Option<&V>) -> R {
    let d = match claims {
        V::Dict(d) => d,
        other => return Err(Exc::attr_error(format!("'{}' object has no attribute 'get'", other.type_name()))),
    };
    for name in ["exp", "iat", "nbf"] {
        // python-jose rewrites the caller's dict in place
        let k = Key::Str(Arc::from(name));
        let mut g = d.lock();
        let hit = match g.get(&k) {
            Some((kv, V::DateTime(dt))) => Some((kv.clone(), timegm(dt))),
            _ => None,
        };
        if let Some((kv, ts)) = hit {
            g.insert(k, (kv, V::Int(ts)));
        }
    }
    let alg = match algorithm {
        None => "HS256".to_string(),
        Some(a) => ops::str_(a)?,
    };
    if !SUPPORTED.contains(&alg.as_str()) {
        return Err(exc(&JWS_ERROR, format!("Algorithm {alg} not supported.")));
    }
    // json.dumps({"typ": "JWT", "alg": alg}, separators=(",", ":"), sort_keys=True); alg is one of SUPPORTED
    let header = b64(format!("{{\"alg\":\"{alg}\",\"typ\":\"JWT\"}}").as_bytes());
    let payload = libs::json_dumps(claims, &[("separators".into(), V::tuple(vec![V::str(","), V::str(":")]))])?;
    let payload = b64(ops::str_(&payload)?.as_bytes());
    let signing_input = format!("{header}.{payload}");
    if !alg.starts_with("HS") || alg.contains('-') {
        return Err(exc(&JWS_ERROR, no_hmac(&alg).message()));
    }
    let k = key_bytes(key).map_err(|e| if e.isinstance(&JWK_ERROR) { exc(&JWS_ERROR, e.message()) } else { e })?;
    Ok(V::str(format!("{signing_input}.{}", b64(&hmac_sign(&alg, &k, signing_input.as_bytes())))))
}

/// The message of CPython's `json.loads` error when the document has no valid value start (the common
/// case: empty segment, plain text); other syntax errors keep serde_json's text (README).
fn json_error(text: &str, fallback: String) -> String {
    let chars: Vec<char> = text.chars().collect();
    let pos = chars.iter().position(|c| !matches!(c, ' ' | '\t' | '\n' | '\r')).unwrap_or(chars.len());
    let rest: String = chars[pos..].iter().take(5).collect();
    let starts = match chars.get(pos) {
        None => false,
        Some('n') => rest.starts_with("null"),
        Some('t') => rest.starts_with("true"),
        Some('f') => rest.starts_with("false"),
        Some('N') => rest.starts_with("NaN"),
        Some('I') => rest.starts_with("Infin"),
        Some('-') => matches!(chars.get(pos + 1), Some('0'..='9' | 'I')),
        Some(c) => matches!(c, '{' | '[' | '"' | '0'..='9'),
    };
    if starts {
        return fallback;
    }
    let line = chars[..pos].iter().filter(|c| **c == '\n').count() + 1;
    let col = pos - chars[..pos].iter().rposition(|c| *c == '\n').map(|i| i + 1).unwrap_or(0) + 1;
    format!("Expecting value: line {line} column {col} (char {pos})")
}

fn json_object(bytes: &[u8], what: &str) -> R<V> {
    let text = std::str::from_utf8(bytes).map_err(|e| exc(&JWS_ERROR, format!("Invalid {what} string: 'utf-8' codec can't decode bytes: {e}")))?;
    let v = pyd::loads(text).map_err(|e| exc(&JWS_ERROR, format!("Invalid {what} string: {}", json_error(text, e.message()))))?;
    if !matches!(v, V::Dict(_)) {
        return Err(exc(&JWS_ERROR, format!("Invalid {what} string: must be a json object")));
    }
    Ok(v)
}

fn get(d: &V, name: &str) -> Option<V> {
    match d {
        V::Dict(d) => d.lock().get(&Key::Str(Arc::from(name))).map(|(_, v)| v.clone()),
        _ => None,
    }
}

/// `_get_keys(key)`: python-jose first tries `json.loads(key, parse_int=str, parse_float=str)`.
fn the_key(key: &V) -> R<V> {
    let text = match key {
        V::Str(s) => s.to_string(),
        V::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => return Ok(key.clone()),
        },
        _ => return Ok(key.clone()),
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Err(_) => Ok(key.clone()),
        Ok(serde_json::Value::String(s)) => Ok(V::str(s)),
        Ok(serde_json::Value::Number(_)) => Ok(V::str(text.trim())),
        Ok(serde_json::Value::Object(_) | serde_json::Value::Array(_)) => {
            Err(Exc::runtime("py2axum: JWK / JWK set keys are not supported as jose keys"))
        }
        Ok(_) => Err(exc(&JWK_ERROR, "Expecting a string- or bytes-formatted key.")),
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `int(x)` as the claim checks call it: only ValueError is turned into a claims error.
fn claim_int(v: &V, msg: &str) -> R<i64> {
    match super::methods::b_int(std::slice::from_ref(v)) {
        Ok(V::Int(i)) => Ok(i),
        Ok(_) => Err(Exc::runtime("py2axum: int() out of range in a JWT claim")),
        Err(e) if e.isinstance(&VALUE_ERROR) => Err(exc(&JWT_CLAIMS_ERROR, msg)),
        Err(e) => Err(e),
    }
}

/// `jwt.decode(token, key, algorithms=...)` with the default options.
pub fn decode(token: &V, key: &V, algorithms: Option<&V>) -> R {
    decode_with(token, key, algorithms, &V::None, &V::None, &V::None, &V::None)
}

/// `jwt.decode(token, key, algorithms=None, options=None, audience=None, issuer=None, subject=None)`
pub fn decode_with(token: &V, key: &V, algorithms: Option<&V>, options: &V, audience: &V, issuer: &V, subject: &V) -> R {
    // the options over python-jose's defaults
    let mut opts: Vec<(String, V)> = [
        "verify_signature", "verify_aud", "verify_iat", "verify_exp", "verify_nbf", "verify_iss", "verify_sub", "verify_jti", "verify_at_hash",
    ]
    .iter()
    .map(|k| (k.to_string(), V::Bool(true)))
    .chain(["require_aud", "require_iat", "require_exp", "require_nbf", "require_iss", "require_sub", "require_jti", "require_at_hash"].iter().map(|k| (k.to_string(), V::Bool(false))))
    .chain(std::iter::once(("leeway".to_string(), V::Int(0))))
    .collect();
    if let V::Dict(d) = options {
        for (k, v) in d.lock().values() {
            let k = ops::str_(k)?;
            match opts.iter_mut().find(|(x, _)| *x == k) {
                Some(e) => e.1 = v.clone(),
                None => opts.push((k, v.clone())),
            }
        }
    } else if !options.is_none() {
        return Err(Exc::attr_error(format!("'{}' object has no attribute 'get'", options.type_name())));
    }
    let flag = |opts: &Vec<(String, V)>, k: &str| -> R<bool> { opts.iter().find(|(x, _)| x == k).map(|(_, v)| ops::truthy(v)).transpose().map(|b| b.unwrap_or(false)) };
    let verify_signature = flag(&opts, "verify_signature")?;
    let wrap = |e: Exc| if e.isinstance(&JWS_ERROR) { exc(&JWT_ERROR, e.message()) } else { e };
    let raw: Vec<u8> = match token {
        V::Str(s) => s.as_bytes().to_vec(),
        V::Bytes(b) => b.to_vec(),
        other => return Err(Exc::attr_error(format!("'{}' object has no attribute 'rsplit'", other.type_name()))),
    };
    let (header, payload, signing_input, signature) = (|| -> R<(V, Vec<u8>, Vec<u8>, Vec<u8>)> {
        let dot = raw.iter().rposition(|c| *c == b'.').ok_or_else(|| exc(&JWS_ERROR, "Not enough segments"))?;
        let (signing_input, crypto) = (&raw[..dot], &raw[dot + 1..]);
        let first = signing_input.iter().position(|c| *c == b'.').ok_or_else(|| exc(&JWS_ERROR, "Not enough segments"))?;
        let (h, c) = (&signing_input[..first], &signing_input[first + 1..]);
        // binascii.Error is a ValueError: caught by the "Not enough segments" clause first
        let header_data = b64_decode(h).ok_or_else(|| exc(&JWS_ERROR, "Not enough segments"))?;
        let header = json_object(&header_data, "header")?;
        let payload = b64_decode(c).ok_or_else(|| exc(&JWS_ERROR, "Invalid payload padding"))?;
        let signature = b64_decode(crypto).ok_or_else(|| exc(&JWS_ERROR, "Invalid crypto padding"))?;
        Ok((header, payload, signing_input.to_vec(), signature))
    })()
    .map_err(wrap)?;
    if !verify_signature {
        // jws.verify(verify=False): no signature check; the header's alg is still read
        if get(&header, "alg").is_none() {
            return Err(Exc::new(&KEY_ERROR, vec![V::str("alg")]));
        }
        return claims_of(&payload, &mut opts, audience, issuer, subject);
    }
    // _verify_signature
    let alg = get(&header, "alg").unwrap_or(V::None);
    if !ops::truthy(&alg)? {
        return Err(exc(&JWT_ERROR, "No algorithm was specified in the JWS header."));
    }
    if let Some(algs) = algorithms {
        if !matches!(algs, V::None) && !ops::contains(algs, &alg)? {
            return Err(exc(&JWT_ERROR, "The specified alg value is not allowed"));
        }
    }
    let k = the_key(key)?;
    let alg = match &alg {
        V::Str(s) if ["HS256", "HS384", "HS512"].contains(&&**s) => s.to_string(),
        V::Str(s) => return Err(no_hmac(s)),
        _ => return Err(exc(&JWK_ERROR, "Unable to find an algorithm for key")),
    };
    let kb = key_bytes(&k)?;
    let expected = hmac_sign(&alg, &kb, &signing_input);
    if !bool::from(subtle::ConstantTimeEq::ct_eq(expected.as_slice(), signature.as_slice())) {
        return Err(exc(&JWT_ERROR, "Signature verification failed."));
    }
    claims_of(&payload, &mut opts, audience, issuer, subject)
}

/// `_validate_claims` with the options (require_* forces its verify_*, leeway on exp/nbf)
fn claims_of(payload: &[u8], opts: &mut Vec<(String, V)>, audience: &V, issuer: &V, subject: &V) -> R {
    let text = std::str::from_utf8(payload).map_err(|e| exc(&JWT_ERROR, format!("Invalid payload string: 'utf-8' codec can't decode bytes: {e}")))?;
    let claims = pyd::loads(text).map_err(|e| exc(&JWT_ERROR, format!("Invalid payload string: {}", json_error(text, e.message()))))?;
    if !matches!(claims, V::Dict(_)) {
        return Err(exc(&JWT_ERROR, "Invalid payload string: must be a json object"));
    }
    let leeway = match opts.iter().find(|(k, _)| k == "leeway").map(|(_, v)| v.clone()) {
        Some(V::Int(i)) => i,
        Some(V::Float(f)) => f as i64,
        Some(V::Delta(d)) => d.num_seconds(),
        _ => 0,
    };
    // required claims force their verification
    let required: Vec<String> = opts.iter().filter(|(k, v)| k.starts_with("require_") && ops::truthy(v).unwrap_or(false)).map(|(k, _)| k["require_".len()..].to_string()).collect();
    for r in required {
        if get(&claims, &r).is_none() {
            return Err(exc(&JWT_ERROR, format!("missing required key \"{r}\" among claims")));
        }
        let key = format!("verify_{r}");
        match opts.iter_mut().find(|(k, _)| *k == key) {
            Some(e) => e.1 = V::Bool(true),
            None => opts.push((key, V::Bool(true))),
        }
    }
    if !matches!(audience, V::Str(_) | V::None) {
        return Err(exc(&JWT_ERROR, "audience must be a string or None"));
    }
    let on = |k: &str| opts.iter().find(|(x, _)| x == k).map(|(_, v)| ops::truthy(v).unwrap_or(false)).unwrap_or(false);
    if on("verify_iat") {
        if let Some(iat) = get(&claims, "iat") {
            claim_int(&iat, "Issued At claim (iat) must be an integer.")?;
        }
    }
    if on("verify_nbf") {
        if let Some(nbf) = get(&claims, "nbf") {
            if claim_int(&nbf, "Not Before claim (nbf) must be an integer.")? > now() + leeway {
                return Err(exc(&JWT_CLAIMS_ERROR, "The token is not yet valid (nbf)"));
            }
        }
    }
    if on("verify_exp") {
        if let Some(exp) = get(&claims, "exp") {
            if claim_int(&exp, "Expiration Time claim (exp) must be an integer.")? < now() - leeway {
                return Err(exc(&EXPIRED_SIGNATURE_ERROR, "Signature has expired."));
            }
        }
    }
    if on("verify_aud") {
        if let Some(aud) = get(&claims, "aud") {
            let list = match &aud {
                V::Str(_) => vec![aud.clone()],
                V::List(l) => l.lock().clone(),
                _ => return Err(exc(&JWT_CLAIMS_ERROR, "Invalid claim format in token")),
            };
            if list.iter().any(|c| !matches!(c, V::Str(_))) {
                return Err(exc(&JWT_CLAIMS_ERROR, "Invalid claim format in token"));
            }
            if !list.iter().any(|c| ops::eq_bool(c, audience)) {
                return Err(exc(&JWT_CLAIMS_ERROR, "Invalid audience"));
            }
        }
    }
    if on("verify_iss") && !issuer.is_none() {
        let accepted = match issuer {
            V::Str(_) => vec![issuer.clone()],
            other => ops::iter(other)?,
        };
        let iss = get(&claims, "iss").unwrap_or(V::None);
        if !accepted.iter().any(|x| ops::eq_bool(x, &iss)) {
            return Err(exc(&JWT_CLAIMS_ERROR, "Invalid issuer"));
        }
    }
    if on("verify_sub") {
        if let Some(sub) = get(&claims, "sub") {
            if !matches!(sub, V::Str(_)) {
                return Err(exc(&JWT_CLAIMS_ERROR, "Subject must be a string."));
            }
            if !subject.is_none() && !ops::eq_bool(&sub, subject) {
                return Err(exc(&JWT_CLAIMS_ERROR, "Invalid subject"));
            }
        }
    }
    if on("verify_jti") {
        if let Some(jti) = get(&claims, "jti") {
            if !matches!(jti, V::Str(_)) {
                return Err(exc(&JWT_CLAIMS_ERROR, "JWT ID must be a string."));
            }
        }
    }
    if on("verify_at_hash") && get(&claims, "at_hash").is_some() {
        return Err(exc(&JWT_CLAIMS_ERROR, "No access_token provided to compare against at_hash claim."));
    }
    Ok(claims)
}
