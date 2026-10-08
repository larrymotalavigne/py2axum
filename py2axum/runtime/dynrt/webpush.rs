//! `pywebpush.webpush` (2.3.0): RFC 8291 encryption in the aes128gcm content coding (http-ece 1.2.1),
//! VAPID authorization (py-vapid 1.9.4, RFC 8292), then a POST like `requests` sends it.
use std::sync::Arc;

use aes_gcm::aead::{AeadInPlace, KeyInit};
use base64::Engine as _;
use p256::ecdsa::signature::Signer;
use p256::elliptic_curve::sec1::ToEncodedPoint;

use super::ops;
use super::v::*;

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn wpe(msg: impl Into<String>) -> Exc {
    let msg = msg.into();
    let e = Exc::msg(&WEBPUSH_EXCEPTION, msg.clone());
    let mut a = e.0.attrs.lock();
    a.insert("message".into(), V::str(msg));
    a.insert("response".into(), V::None);
    drop(a);
    e
}

fn vapid_exc(msg: &str) -> Exc {
    Exc::msg(&VAPID_EXCEPTION, msg)
}

/// `base64.urlsafe_b64decode` after py-vapid / pywebpush re-padding (padding and alphabet lenient)
fn b64url_decode(s: &str) -> R<Vec<u8>> {
    let t = s.trim_end_matches('=');
    base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent).with_decode_allow_trailing_bits(true),
    )
    .decode(t)
    .map_err(|_| Exc::msg(&BINASCII_ERROR, "Incorrect padding"))
}

fn get<'a>(d: &'a V, key: &str) -> R<Option<V>> {
    match d {
        V::Dict(m) => Ok(m.lock().get(&Key::Str(Arc::from(key))).map(|(_, v)| v.clone())),
        o => Err(Exc::type_error(format!("argument of type '{}' is not iterable", o.type_name()))),
    }
}

fn key_bytes(v: &V) -> R<String> {
    match v {
        V::Str(s) => Ok(s.to_string()),
        V::Bytes(b) => Ok(String::from_utf8_lossy(b).into_owned()),
        o => Err(Exc::type_error(format!("py2axum: key of type {}", o.type_name()))),
    }
}

/// `Vapid.from_string`: a raw 32-byte scalar or a DER private key, base64url
fn vapid_key(s: &str) -> R<p256::SecretKey> {
    let raw = b64url_decode(&s.replace('\n', ""))?;
    if raw.len() == 32 {
        return p256::SecretKey::from_slice(&raw).map_err(|_| Exc::value_error("Invalid private key"));
    }
    use p256::pkcs8::DecodePrivateKey;
    p256::SecretKey::from_pkcs8_der(&raw)
        .or_else(|_| p256::SecretKey::from_sec1_der(&raw))
        .map_err(|_| Exc::value_error("Could not deserialize key data. The data may be in an incorrect format, the provided password may be incorrect, it may be encrypted with an unsupported algorithm, or it may be an unsupported key type."))
}

/// py-vapid's `_check_sub`
fn check_sub(sub: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)^(?:(mailto:.+@((localhost|[%\w-]+(\.[%\w-]+)+|([0-9a-f]{1,4}):+([0-9a-f]{1,4})?)))|https://(localhost|[\w-]+\.[\w\.-]+|([0-9a-f]{1,4}:+)+([0-9a-f]{1,4})?)$)").unwrap()
    })
    .is_match(sub)
}

/// `Vapid02.sign(claims)`: `vapid t=<ES256 JWT>,k=<public key>`
fn vapid_header(key: &p256::SecretKey, claims: &V) -> R<String> {
    let mut c: Vec<(V, V)> = match claims {
        V::Dict(d) => d.lock().values().cloned().collect(),
        _ => return Err(Exc::type_error("vapid_claims must be a dict")),
    };
    if !c.iter().any(|(k, v)| matches!(k, V::Str(s) if &**s == "exp") && ops::truthy(v).unwrap_or(false)) {
        c.retain(|(k, _)| !matches!(k, V::Str(s) if &**s == "exp"));
        c.push((V::str("exp"), V::Int(chrono::Utc::now().timestamp() + 86400)));
    }
    let find = |n: &str| c.iter().find(|(k, _)| matches!(k, V::Str(s) if &**s == n)).map(|(_, v)| v.clone());
    let sub = find("sub").map(|v| ops::str_(&v)).transpose()?.unwrap_or_default();
    if !check_sub(&sub) {
        return Err(vapid_exc("Missing 'sub' from claims. 'sub' is your admin email as a mailto: link."));
    }
    let aud = find("aud").map(|v| ops::str_(&v)).transpose()?.unwrap_or_default();
    static AUD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    if !AUD.get_or_init(|| regex::Regex::new(r"(?i)^https?://[^/:]+(:\d+)?$").unwrap()).is_match(&aud) {
        return Err(vapid_exc("Missing 'aud' from claims. 'aud' is the scheme, host and optional port for this transaction e.g. https://example.com:8080"));
    }
    // json.dumps(claims, separators=(',', ':'), sort_keys=True)
    let mut sorted: Vec<(String, V)> = c.into_iter().map(|(k, v)| Ok((ops::str_(&k)?, v))).collect::<R<_>>()?;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let body = V::dict_from(sorted.into_iter().map(|(k, v)| (V::str(k), v)).collect())?;
    let json = super::pyd::to_json(&body, &super::pyd::JsonStyle { ensure_ascii: true, item_sep: ",", key_sep: ":", nan_null: false }, false)?;
    let token = format!("{}.{}", B64URL.encode(br#"{"typ":"JWT","alg":"ES256"}"#), B64URL.encode(json.as_bytes()));
    let sk = p256::ecdsa::SigningKey::from(key.clone());
    let sig: p256::ecdsa::Signature = sk.sign(token.as_bytes());
    let public = key.public_key().to_encoded_point(false);
    Ok(format!("vapid t={token}.{},k={}", B64URL.encode(sig.to_bytes()), B64URL.encode(public.as_bytes())))
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let h = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), ikm);
    let mut out = vec![0u8; len];
    h.expand(info, &mut out).expect("hkdf length");
    out
}

/// http_ece.encrypt(version="aes128gcm", rs=4096) with a fresh sender key and salt
fn encrypt(data: &[u8], receiver: &[u8], auth: &[u8]) -> R<Vec<u8>> {
    let receiver_key = p256::PublicKey::from_sec1_bytes(receiver).map_err(|_| Exc::value_error("Invalid EC key."))?;
    let sender = p256::ecdh::EphemeralSecret::random(&mut rand::rngs::OsRng);
    let sender_pub = p256::EncodedPoint::from(sender.public_key());
    let shared = sender.diffie_hellman(&receiver_key);
    let mut info = b"WebPush: info\x00".to_vec();
    info.extend_from_slice(receiver);
    info.extend_from_slice(sender_pub.as_bytes());
    let ikm = hkdf(auth, shared.raw_secret_bytes(), &info, 32);
    let salt: [u8; 16] = rand::random();
    let key = hkdf(&salt, &ikm, b"Content-Encoding: aes128gcm\x00", 16);
    let nonce = hkdf(&salt, &ikm, b"Content-Encoding: nonce\x00", 12);
    let cipher = aes_gcm::Aes128Gcm::new_from_slice(&key).expect("key length");
    let rs: u32 = 4096;
    let chunk = (rs - 17) as usize;
    let mut out = salt.to_vec();
    out.extend_from_slice(&rs.to_be_bytes());
    out.push(sender_pub.as_bytes().len() as u8);
    out.extend_from_slice(sender_pub.as_bytes());
    let mut counter: u64 = 0;
    let mut i = 0;
    while i < data.len() {
        let last = i + chunk >= data.len();
        let mut rec = data[i..(i + chunk).min(data.len())].to_vec();
        rec.push(if last { 2 } else { 1 });
        let mask = u64::from_be_bytes(nonce[4..12].try_into().unwrap());
        let mut iv = nonce[..4].to_vec();
        iv.extend_from_slice(&(counter ^ mask).to_be_bytes());
        cipher
            .encrypt_in_place(aes_gcm::Nonce::from_slice(&iv), b"", &mut rec)
            .map_err(|_| Exc::runtime("py2axum: AES-GCM encryption failed"))?;
        out.extend_from_slice(&rec);
        counter += 1;
        i += chunk;
    }
    Ok(out)
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// `webpush(subscription_info, data, vapid_private_key, vapid_claims, ...)`
pub async fn webpush(args: &[V], kwargs: &[(String, V)]) -> R {
    let names = ["subscription_info", "data", "vapid_private_key", "vapid_claims", "content_encoding", "curl", "timeout", "ttl", "verbose", "headers"];
    if args.len() > names.len() {
        return Err(Exc::type_error("webpush() takes too many positional arguments"));
    }
    let mut vals: Vec<Option<V>> = args.iter().cloned().map(Some).collect();
    vals.resize(names.len(), None);
    for (k, v) in kwargs {
        let i = names.iter().position(|n| n == k).ok_or_else(|| Exc::type_error(format!("webpush() got an unexpected keyword argument '{k}'")))?;
        vals[i] = Some(v.clone());
    }
    let arg = |i: usize| vals[i].clone().filter(|v| !v.is_none());
    let sub = arg(0).ok_or_else(|| Exc::type_error("webpush() missing 1 required positional argument: 'subscription_info'"))?;
    if let Some(ce) = arg(4) {
        if ops::str_(&ce)? != "aes128gcm" {
            return Err(Exc::type_error("py2axum: webpush(content_encoding=) other than aes128gcm is not supported"));
        }
    }
    if arg(5).map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false) {
        return Err(Exc::type_error("py2axum: webpush(curl=True) is not supported"));
    }
    let ttl = match arg(7) {
        None => 0,
        Some(V::Int(i)) => i,
        Some(o) => return Err(Exc::type_error(format!("py2axum: webpush(ttl=) of type {}", o.type_name()))),
    };
    let mut headers: Vec<(String, String)> = match arg(9) {
        None => vec![],
        Some(V::Dict(d)) => d.lock().values().map(|(k, v)| Ok((ops::str_(k)?, ops::str_(v)?))).collect::<R<_>>()?,
        Some(o) => return Err(Exc::type_error(format!("py2axum: webpush(headers=) of type {}", o.type_name()))),
    };
    let set = |h: &mut Vec<(String, String)>, k: &str, v: String| {
        h.retain(|(x, _)| !x.eq_ignore_ascii_case(k));
        h.push((k.to_string(), v));
    };
    let endpoint_v = get(&sub, "endpoint")?;
    // the VAPID claims are completed in place (the caller's dict), as pywebpush does
    if let Some(claims) = arg(3).filter(|c| ops::truthy(c).unwrap_or(false)) {
        let has = |k: &str| get(&claims, k).map(|v| v.is_some_and(|v| ops::truthy(&v).unwrap_or(false)));
        if !has("aud")? {
            let ep = ops::str_(endpoint_v.as_ref().unwrap_or(&V::None))?;
            let (scheme, netloc) = match ep.split_once("://") {
                Some((s, rest)) => (s.to_string(), rest.split(['/', '?', '#']).next().unwrap_or("").to_string()),
                None => (String::new(), String::new()),
            };
            ops::setitem(&claims, &V::str("aud"), V::str(format!("{scheme}://{netloc}")))?;
        }
        let now = chrono::Utc::now().timestamp();
        let exp = get(&claims, "exp")?;
        let stale = match &exp {
            None => true,
            Some(v) if !ops::truthy(v)? => true,
            Some(V::Int(e)) => *e < now,
            Some(v) => super::methods::b_int(&[v.clone()]).ok().and_then(|x| match x { V::Int(i) => Some(i), _ => None }).unwrap_or(0) < now,
        };
        if stale {
            ops::setitem(&claims, &V::str("exp"), V::Int(now + 12 * 60 * 60))?;
        }
        let pk = arg(2).ok_or_else(|| wpe("VAPID dict missing 'private_key'"))?;
        let pk = ops::str_(&pk)?;
        if std::path::Path::new(&pk).is_file() {
            return Err(Exc::type_error("py2axum: a VAPID key file path is not supported (pass the key itself)"));
        }
        let key = vapid_key(&pk)?;
        set(&mut headers, "Authorization", vapid_header(&key, &claims)?);
    }
    // WebPusher(subscription_info)
    let endpoint = ops::str_(&endpoint_v.ok_or_else(|| wpe("subscription_info missing endpoint URL"))?)?;
    let mut receiver = None;
    if let Some(keys) = get(&sub, "keys")? {
        let p256dh = get(&keys, "p256dh")?.filter(|v| !v.is_none()).ok_or_else(|| wpe("Missing keys value: p256dh"))?;
        let auth = get(&keys, "auth")?.filter(|v| !v.is_none()).ok_or_else(|| wpe("Missing keys value: auth"))?;
        let raw = b64url_decode(&key_bytes(&p256dh)?)?;
        if raw.len() != 65 {
            return Err(wpe("Invalid p256dh key specified"));
        }
        receiver = Some((raw, b64url_decode(&key_bytes(&auth)?)?));
    }
    let data = match arg(1) {
        None => None,
        Some(V::Str(s)) if s.is_empty() => None,
        Some(V::Bytes(b)) if b.is_empty() => None,
        Some(V::Str(s)) => Some(s.as_bytes().to_vec()),
        Some(V::Bytes(b)) => Some(b.to_vec()),
        Some(o) => return Err(Exc::type_error(format!("py2axum: webpush(data=) of type {}", o.type_name()))),
    };
    let mut body = None;
    if let Some(d) = data {
        let (recv, auth) = receiver.ok_or_else(|| wpe("No keys specified in subscription info"))?;
        body = Some(encrypt(&d, &recv, &auth)?);
        set(&mut headers, "content-encoding", "aes128gcm".into());
    }
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("ttl")) || ttl != 0 {
        set(&mut headers, "ttl", ttl.to_string());
    }
    let mut rb = http().post(&endpoint);
    for (k, v) in &headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    rb = rb.header("user-agent", "python-requests/2.32.5");
    if let Some(t) = arg(6) {
        let s = match t {
            V::Int(i) => i as f64,
            V::Float(f) => f,
            o => return Err(Exc::type_error(format!("py2axum: webpush(timeout=) of type {}", o.type_name()))),
        };
        rb = rb.timeout(std::time::Duration::from_secs_f64(s));
    }
    if let Some(b) = body {
        rb = rb.body(b);
    }
    let resp = rb.send().await.map_err(|e| Exc::msg(&REQUESTS_CONNECTION_ERROR, e.to_string()))?;
    let resp = super::http::from_reqwest(resp).await?;
    let status = super::http::status_of(&resp);
    if status > 202 {
        let e = wpe(format!("Push failed: {} {}\nResponse body:{}", status, super::status_phrase(status).unwrap_or(""), super::http::text_of(&resp)));
        e.0.attrs.lock().insert("response".into(), resp);
        return Err(e);
    }
    Ok(resp)
}
