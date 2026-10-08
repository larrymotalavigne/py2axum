//! PyJWT 2.15 (`import jwt`): `encode`, `decode`, `decode_complete`, `get_unverified_header` with the HMAC
//! algorithms and `none`: same tokens byte for byte, same exception classes and messages, same checks in the
//! same order (`api_jws.py`, `api_jwt.py`, `algorithms.HMACAlgorithm`). Asymmetric algorithms raise a
//! py2axum RuntimeError (refused at transpile time when written as literals).
use std::sync::Arc;

use base64::Engine;
use hmac::{Hmac, Mac};

use super::libs;
use super::ops;
use super::v::*;

/// the algorithms of `get_default_algorithms()` that need `cryptography` (not reproduced)
const ASYMMETRIC: &[&str] =
    &["RS256", "RS384", "RS512", "ES256", "ES256K", "ES384", "ES521", "ES512", "PS256", "PS384", "PS512", "EdDSA"];
const JWK_MSG: &str =
    "The specified key looks like a JWK and should not be used directly as an HMAC secret. Load it via PyJWK / HMACAlgorithm.from_jwk first.";
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

enum Alg {
    None,
    Hmac(&'static str),
}

fn exc(class: &'static Class, msg: impl AsRef<str>) -> Exc {
    Exc::msg(class, msg)
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn get(d: &V, name: &str) -> Option<V> {
    match d {
        V::Dict(d) => d.lock().get(&Key::Str(Arc::from(name))).map(|(_, v)| v.clone()),
        _ => None,
    }
}

fn has(d: &V, name: &str) -> bool {
    get(d, name).is_some()
}

/// `dict.get(name)` on a value that must be a mapping (`options.get(...)`)
fn dict_get(d: &V, name: &str) -> R<Option<V>> {
    match d {
        V::Dict(_) => Ok(get(d, name)),
        other => Err(Exc::attr_error(format!("'{}' object has no attribute 'get'", other.type_name()))),
    }
}

/// `MissingRequiredClaimError(claim)`: `args` = (claim,) (set by `BaseException.__new__`), `str()` = its `__str__`, `.claim`
fn missing(claim: &str) -> Exc {
    let e = Exc::new(&PYJWT_MISSING_REQUIRED_CLAIM, vec![V::str(claim)]);
    e.0.attrs.lock().insert("claim".into(), V::str(claim));
    e
}

/// `PyJWS.get_algorithm_by_name(alg)`: `None` when the algorithm is unknown (NotImplementedError)
fn lookup(alg: &V) -> R<Option<Alg>> {
    Key::dict_key(alg)?;
    Ok(match alg {
        V::Str(s) => match &**s {
            "none" => Some(Alg::None),
            "HS256" => Some(Alg::Hmac("HS256")),
            "HS384" => Some(Alg::Hmac("HS384")),
            "HS512" => Some(Alg::Hmac("HS512")),
            a if ASYMMETRIC.contains(&a) => {
                return Err(Exc::runtime(format!("py2axum: jwt algorithm {a} is not supported (HS256, HS384, HS512 and none only)")))
            }
            _ => None,
        },
        _ => None,
    })
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

/// `utils.is_pem_format`: a BEGIN marker followed by the END marker of the same label
fn is_pem(key: &[u8]) -> bool {
    const LABELS: &[&str] = &[
        "CERTIFICATE", "TRUSTED CERTIFICATE", "PRIVATE KEY", "PUBLIC KEY", "ENCRYPTED PRIVATE KEY", "OPENSSH PRIVATE KEY",
        "DSA PRIVATE KEY", "RSA PRIVATE KEY", "RSA PUBLIC KEY", "EC PRIVATE KEY", "DH PARAMETERS", "NEW CERTIFICATE REQUEST",
        "CERTIFICATE REQUEST", "SSH2 PUBLIC KEY", "SSH2 ENCRYPTED PRIVATE KEY", "X509 CRL",
    ];
    let mut begins: Vec<(&str, usize)> = Vec::new();
    // the lookahead regex reports a match at every position: scan them all
    for i in 0..key.len() {
        let rest = &key[i..];
        if !(rest.starts_with(b"----") && rest.len() > 5 && matches!(rest[4], b'-' | b' ')) {
            continue;
        }
        let r = &rest[5..];
        let (kind, r) = if let Some(r) = r.strip_prefix(b"BEGIN ") {
            ("BEGIN", r)
        } else if let Some(r) = r.strip_prefix(b"END ") {
            ("END", r)
        } else {
            continue;
        };
        // the alternation takes the first label that matches, in the set's order (CPython's set order differs,
        // but no label followed by `[- ]----` is a prefix of another one followed by the same)
        for label in LABELS {
            let Some(after) = r.strip_prefix(label.as_bytes()) else { continue };
            if after.len() >= 5 && matches!(after[0], b'-' | b' ') && after[1..5] == *b"----" {
                let end = i + (rest.len() - after.len()) + 5;
                if kind == "BEGIN" {
                    match begins.iter_mut().find(|(l, _)| l == label) {
                        Some(b) => b.1 = end,
                        None => begins.push((label, end)),
                    }
                } else if begins.iter().any(|(l, e)| l == label && *e < i) {
                    return true;
                }
                break;
            }
        }
    }
    false
}

/// a complete DER SEQUENCE spanning the key: what `load_der_public_key`/`load_der_x509_certificate`
/// could accept (not reproduced)
fn der_shaped(k: &[u8]) -> bool {
    if k.len() < 2 || k[0] != 0x30 {
        return false;
    }
    let (len, hdr) = match k[1] {
        n if n < 0x80 => (n as usize, 2),
        n @ 0x81..=0x84 => {
            let c = (n - 0x80) as usize;
            if k.len() < 2 + c {
                return false;
            }
            (k[2..2 + c].iter().fold(0usize, |a, b| (a << 8) | *b as usize), 2 + c)
        }
        _ => return false,
    };
    hdr + len == k.len()
}

/// `HMACAlgorithm.prepare_key`
fn hmac_key(key: &V) -> R<Vec<u8>> {
    let k: Vec<u8> = match key {
        V::Str(s) => s.as_bytes().to_vec(),
        V::Bytes(b) => b.to_vec(),
        _ => return Err(Exc::type_error("Expected a string or bytes value")),
    };
    if k.is_empty() {
        return Err(exc(&PYJWT_INVALID_KEY, "HMAC key must not be empty."));
    }
    const SSH: &[&str] = &["ssh-ed25519", "ssh-rsa", "ssh-dss", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"];
    if is_pem(&k) || SSH.iter().any(|p| k.starts_with(p.as_bytes())) {
        return Err(exc(
            &PYJWT_INVALID_KEY,
            "The specified key is an asymmetric key or x509 certificate and should not be used as an HMAC secret.",
        ));
    }
    if der_shaped(&k) {
        return Err(Exc::runtime("py2axum: an HMAC key shaped like a DER structure is not supported (PyJWT tries it as a public key)"));
    }
    // a JWK given as the secret (a "kty" member anywhere in a JSON key)
    let jwk = match libs::json_loads(&V::Bytes(Arc::from(k.as_slice()))) {
        Ok(v) => v,
        // json.loads(..., parse_int=lambda _: 0) never overflows: without a "kty" anywhere it is no JWK
        Err(e) if e.isinstance(&OVERFLOW_ERROR) && !k.windows(3).any(|w| w == b"kty") => V::None,
        Err(e) if e.isinstance(&RECURSION_ERROR) => {
            // too deep to load: PyJWT looks at the text (a JSON object, or an array with a "kty" member)
            let text = String::from_utf8_lossy(&k);
            let t = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
            if t.starts_with('{') {
                return Err(exc(&PYJWT_INVALID_KEY, JWK_MSG));
            }
            if t.starts_with('[') && text.contains("\"kty\"") {
                return Err(Exc::runtime("py2axum: an HMAC key that is a deeply nested JSON array naming \"kty\" is not supported"));
            }
            V::None
        }
        Err(e) if e.isinstance(&OVERFLOW_ERROR) => {
            return Err(Exc::runtime("py2axum: an HMAC key that is JSON with integers beyond 64 bits and a \"kty\" is not supported"))
        }
        Err(_) => V::None,
    };
    let mut todo = vec![jwk];
    while let Some(o) = todo.pop() {
        match &o {
            V::Dict(d) => {
                if has(&o, "kty") {
                    return Err(exc(&PYJWT_INVALID_KEY, JWK_MSG));
                }
                todo.extend(d.lock().values().map(|(_, v)| v.clone()));
            }
            V::List(l) => todo.extend(l.lock().iter().cloned()),
            _ => {}
        }
    }
    Ok(k)
}

/// `Algorithm.prepare_key` then `check_key_length` (a warning, an error under `enforce_minimum_key_length`)
fn prepare(alg: &Alg, key: &V, enforce: bool) -> R<Option<Vec<u8>>> {
    match alg {
        Alg::None => {
            if !(key.is_none() || matches!(key, V::Str(s) if s.is_empty())) {
                return Err(exc(&PYJWT_INVALID_KEY, "When alg = \"none\", key value must be None."));
            }
            Ok(None)
        }
        Alg::Hmac(name) => {
            let k = hmac_key(key)?;
            let min = match *name {
                "HS256" => 32,
                "HS384" => 48,
                _ => 64,
            };
            if enforce && k.len() < min {
                return Err(exc(
                    &PYJWT_INVALID_KEY,
                    format!(
                        "The HMAC key is {} bytes long, which is below the minimum recommended length of {min} bytes for SHA{}. See RFC 7518 Section 3.2.",
                        k.len(),
                        &name[2..]
                    ),
                ));
            }
            Ok(Some(k))
        }
    }
}

/// `PyJWS._validate_headers`
fn validate_headers(h: &V, encoding: bool) -> R<()> {
    if let Some(kid) = get(h, "kid") {
        if !matches!(kid, V::Str(_)) {
            return Err(exc(&PYJWT_INVALID_TOKEN, "Key ID header parameter must be a string"));
        }
    }
    if !encoding {
        if let Some(crit) = get(h, "crit") {
            let items = match &crit {
                V::List(l) if !l.lock().is_empty() => l.lock().clone(),
                _ => return Err(exc(&PYJWT_INVALID_TOKEN, "Invalid 'crit' header: must be a non-empty list")),
            };
            for ext in items {
                let V::Str(s) = &ext else {
                    return Err(exc(&PYJWT_INVALID_TOKEN, "Invalid 'crit' header: values must be strings"));
                };
                if &**s != "b64" {
                    return Err(exc(&PYJWT_INVALID_TOKEN, format!("Unsupported critical extension: {s}")));
                }
                if !has(h, s) {
                    return Err(exc(&PYJWT_INVALID_TOKEN, format!("Critical extension '{s}' is missing from headers")));
                }
            }
        }
    }
    Ok(())
}

fn is_false(v: &Option<V>) -> bool {
    matches!(v, Some(V::Bool(false)))
}

fn json_dumps(v: &V, sort_keys: Option<&V>) -> R<String> {
    let mut kw = vec![("separators".to_string(), V::tuple(vec![V::str(","), V::str(":")]))];
    if let Some(s) = sort_keys {
        kw.push(("sort_keys".into(), s.clone()));
    }
    ops::str_(&libs::json_dumps(v, &kw)?)
}

/// `jwt.encode(payload, key, algorithm="HS256", headers=None, sort_headers=True)`
pub fn encode(payload: &V, key: &V, algorithm: Option<&V>, headers: &V, sort_headers: &V) -> R {
    let V::Dict(d) = payload else {
        return Err(Exc::type_error("Expecting a dict object, as JWT only supports JSON objects as payloads."));
    };
    // a copy: the caller's dict is left as is
    let copy = V::Dict(Arc::new(parking_lot::Mutex::new(d.lock().clone())));
    if let V::Dict(c) = &copy {
        let mut g = c.lock();
        for name in ["exp", "iat", "nbf"] {
            let k = Key::Str(Arc::from(name));
            if let Some((kv, V::DateTime(dt))) = g.get(&k).cloned() {
                g.insert(k, (kv, V::Int(dt.utc().and_utc().timestamp())));
            }
        }
    }
    if let Some(iss) = get(&copy, "iss") {
        if !matches!(iss, V::Str(_)) {
            return Err(Exc::type_error("Issuer (iss) must be a string."));
        }
    }
    let json_payload = json_dumps(&copy, None)?;
    // PyJWS.encode
    let mut alg = match algorithm {
        None => V::str("HS256"),
        Some(V::None) => V::str("none"),
        Some(a) => a.clone(),
    };
    let headers_truthy = !headers.is_none() && ops::truthy(headers)?;
    let mut detached = false;
    if headers_truthy {
        if let Some(a) = dict_get(headers, "alg")? {
            if ops::truthy(&a)? {
                alg = a;
            }
        }
        if is_false(&dict_get(headers, "b64")?) {
            detached = true;
        }
    }
    let header = V::dict_from(vec![(V::str("typ"), V::str("JWT")), (V::str("alg"), alg.clone())])?;
    let V::Dict(hd) = &header else { unreachable!() };
    if headers_truthy {
        validate_headers(headers, true)?;
        let V::Dict(src) = headers else { unreachable!() };
        let items: Vec<(Key, (V, V))> = src.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut g = hd.lock();
        for (k, v) in items {
            g.insert(k, v);
        }
    }
    if !ops::truthy(&get(&header, "typ").unwrap_or(V::None))? {
        hd.lock().shift_remove(&Key::Str(Arc::from("typ")));
    }
    if detached {
        hd.lock().insert(Key::Str(Arc::from("b64")), (V::str("b64"), V::Bool(false)));
        let crit = get(&header, "crit").unwrap_or_else(|| V::list(vec![]));
        let V::List(l) = &crit else {
            return Err(exc(&PYJWT_INVALID_TOKEN, "Invalid 'crit' header: must be a list"));
        };
        if !l.lock().iter().any(|x| matches!(x, V::Str(s) if &**s == "b64")) {
            let mut items = l.lock().clone();
            items.push(V::str("b64"));
            hd.lock().insert(Key::Str(Arc::from("crit")), (V::str("crit"), V::list(items)));
        }
    } else {
        hd.lock().shift_remove(&Key::Str(Arc::from("b64")));
    }
    let json_header = json_dumps(&header, Some(sort_headers))?;
    let h64 = b64(json_header.as_bytes());
    let msg_payload = if detached { json_payload.clone() } else { b64(json_payload.as_bytes()) };
    let signing_input = format!("{h64}.{msg_payload}");
    let a = lookup(&alg)?.ok_or_else(|| exc(&NOT_IMPLEMENTED_ERROR, "Algorithm not supported"))?;
    let k = prepare(&a, key, false)?;
    let sig = match (&a, &k) {
        (Alg::Hmac(name), Some(k)) => hmac_sign(name, k, signing_input.as_bytes()),
        _ => vec![],
    };
    let middle = if detached { "" } else { msg_payload.as_str() };
    Ok(V::str(format!("{h64}.{middle}.{}", b64(&sig))))
}

/// `PyJWS._decode_base64url_segment`
fn segment(seg: &[u8], name: &str) -> R<Vec<u8>> {
    let bad = || exc(&PYJWT_DECODE_ERROR, format!("Invalid {name} padding"));
    let stripped = {
        let mut s = seg;
        let mut padding = 0;
        while let Some(r) = s.strip_suffix(b"=") {
            s = r;
            padding += 1;
            if padding > 2 {
                return Err(bad());
            }
        }
        if padding > 0 && seg.len() % 4 != 0 {
            return Err(bad());
        }
        s
    };
    if stripped.len() % 4 == 1 || stripped.iter().any(|c| !ALPHABET.contains(c)) {
        return Err(bad());
    }
    let decoded = super::jose::b64_decode(stripped).ok_or_else(bad)?;
    if b64(&decoded).as_bytes() != stripped {
        return Err(bad());
    }
    Ok(decoded)
}

/// `json.loads(bytes)` wrapped as PyJWT does (`Invalid {what} string: {e}`), a json object required
fn json_object(bytes: &[u8], what: &str) -> R<V> {
    let v = libs::json_loads(&V::Bytes(Arc::from(bytes))).map_err(|e| {
        if e.isinstance(&VALUE_ERROR) || e.isinstance(&RECURSION_ERROR) {
            exc(&PYJWT_DECODE_ERROR, format!("Invalid {what} string: {}", e.message()))
        } else {
            e
        }
    })?;
    if !matches!(v, V::Dict(_)) {
        return Err(exc(&PYJWT_DECODE_ERROR, format!("Invalid {what} string: must be a json object")));
    }
    Ok(v)
}

/// `PyJWS._load`: (payload, signing_input, header, signature)
fn load(token: &V) -> R<(Vec<u8>, Vec<u8>, V, Vec<u8>)> {
    let raw: Vec<u8> = match token {
        V::Str(s) => s.as_bytes().to_vec(),
        V::Bytes(b) => b.to_vec(),
        _ => return Err(exc(&PYJWT_DECODE_ERROR, "Invalid token type. Token must be a <class 'bytes'>")),
    };
    let few = || exc(&PYJWT_DECODE_ERROR, "Not enough segments");
    let dot = raw.iter().rposition(|c| *c == b'.').ok_or_else(few)?;
    let (signing_input, crypto) = (&raw[..dot], &raw[dot + 1..]);
    let first = signing_input.iter().position(|c| *c == b'.').ok_or_else(few)?;
    let (h, p) = (&signing_input[..first], &signing_input[first + 1..]);
    let header = json_object(&segment(h, "header")?, "header")?;
    let payload = if is_false(&get(&header, "b64")) {
        if !p.is_empty() {
            return Err(exc(&PYJWT_DECODE_ERROR, "Payload segment must be empty when 'b64' is false."));
        }
        vec![]
    } else {
        segment(p, "payload")?
    };
    let signature = segment(crypto, "crypto")?;
    Ok((payload, signing_input.to_vec(), header, signature))
}

/// `jwt.get_unverified_header(token)`
pub fn get_unverified_header(token: &V) -> R {
    let (_, _, header, _) = load(token)?;
    validate_headers(&header, false)?;
    Ok(header)
}

/// The options of a `decode` call merged over PyJWT's defaults (`PyJWT._merge_options`).
fn merge_options(options: &V) -> R<Vec<(String, V)>> {
    let mut opts: Vec<(String, V)> = ["verify_signature", "verify_exp", "verify_nbf", "verify_iat", "verify_aud", "verify_iss", "verify_sub", "verify_jti"]
        .iter()
        .map(|k| (k.to_string(), V::Bool(true)))
        .chain([
            ("require".to_string(), V::list(vec![])),
            ("strict_aud".to_string(), V::Bool(false)),
            ("enforce_minimum_key_length".to_string(), V::Bool(false)),
        ])
        .collect();
    let V::Dict(d) = options else {
        if options.is_none() {
            return Ok(opts);
        }
        // dict(options)
        return Err(Exc::type_error(format!("'{}' object is not iterable", options.type_name())));
    };
    let mut given: Vec<(String, V)> = Vec::new();
    for (k, v) in d.lock().values() {
        let V::Str(k) = k else { continue };
        given.push((k.to_string(), v.clone()));
    }
    let sig = given.iter().find(|(k, _)| k == "verify_signature").map(|(_, v)| ops::truthy(v)).transpose()?.unwrap_or(true);
    if !sig {
        for k in ["verify_exp", "verify_nbf", "verify_iat", "verify_aud", "verify_iss", "verify_sub", "verify_jti"] {
            if !given.iter().any(|(x, _)| x == k) {
                given.push((k.to_string(), V::Bool(false)));
            }
        }
    }
    for (k, v) in given {
        match opts.iter_mut().find(|(x, _)| *x == k) {
            Some(e) => e.1 = v,
            None => opts.push((k, v)),
        }
    }
    Ok(opts)
}

fn opt(opts: &[(String, V)], k: &str) -> V {
    opts.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone()).unwrap_or(V::Bool(false))
}

/// `jwt.decode_complete(...)`: {"payload": claims, "header": header, "signature": bytes}
#[allow(clippy::too_many_arguments)]
pub fn decode_complete(token: &V, key: &V, algorithms: &V, options: &V, audience: &V, issuer: &V, subject: &V, leeway: &V) -> R {
    let verify_signature = match options {
        V::None => V::Bool(true),
        o => dict_get(o, "verify_signature")?.unwrap_or(V::Bool(true)),
    };
    let merged = merge_options(options)?;
    let enforce = ops::truthy(&opt(&merged, "enforce_minimum_key_length"))?;
    let verify = ops::truthy(&verify_signature)?;
    // PyJWS.decode_complete
    if verify && !ops::truthy(algorithms)? {
        return Err(exc(&PYJWT_DECODE_ERROR, "It is required that you pass in a value for the \"algorithms\" argument when calling decode()."));
    }
    let (payload, signing_input, header, signature) = load(token)?;
    validate_headers(&header, false)?;
    if is_false(&get(&header, "b64")) {
        let crit = get(&header, "crit").unwrap_or(V::None);
        let crit = if ops::truthy(&crit)? { crit } else { V::list(vec![]) };
        let ok = matches!(&crit, V::List(l) if l.lock().iter().any(|x| matches!(x, V::Str(s) if &**s == "b64")));
        if !ok {
            return Err(exc(&PYJWT_INVALID_TOKEN, "The 'b64' header parameter requires 'b64' to be listed in 'crit'."));
        }
        return Err(exc(
            &PYJWT_DECODE_ERROR,
            "It is required that you pass in a value for the \"detached_payload\" argument to decode a message having the b64 header set to false.",
        ));
    }
    if verify {
        // PyJWS._verify_signature
        let alg = get(&header, "alg").ok_or_else(|| exc(&PYJWT_INVALID_ALGORITHM, "Algorithm not specified"))?;
        if !ops::truthy(&alg)? || (!algorithms.is_none() && !ops::contains(algorithms, &alg)?) {
            return Err(exc(&PYJWT_INVALID_ALGORITHM, "The specified alg value is not allowed"));
        }
        let a = lookup(&alg)?.ok_or_else(|| exc(&PYJWT_INVALID_ALGORITHM, "Algorithm not supported"))?;
        let k = prepare(&a, key, enforce)?;
        let ok = match (&a, &k) {
            (Alg::Hmac(name), Some(k)) => {
                bool::from(subtle::ConstantTimeEq::ct_eq(signature.as_slice(), hmac_sign(name, k, &signing_input).as_slice()))
            }
            _ => false,
        };
        if !ok {
            return Err(exc(&PYJWT_INVALID_SIGNATURE, "Signature verification failed"));
        }
    }
    let claims = json_object(&payload, "payload")?;
    validate_claims(&claims, &merged, audience, issuer, subject, leeway)?;
    V::dict_from(vec![(V::str("payload"), claims), (V::str("header"), header), (V::str("signature"), V::Bytes(Arc::from(signature.as_slice())))])
}

/// `jwt.decode(...)`: the claims
#[allow(clippy::too_many_arguments)]
pub fn decode(token: &V, key: &V, algorithms: &V, options: &V, audience: &V, issuer: &V, subject: &V, leeway: &V) -> R {
    let d = decode_complete(token, key, algorithms, options, audience, issuer, subject, leeway)?;
    Ok(get(&d, "payload").unwrap_or(V::None))
}

/// `int(x)` as the claim checks call it: ValueError, TypeError and OverflowError become `err`
fn claim_int(v: &V, err: Exc) -> R<f64> {
    match super::methods::b_int(std::slice::from_ref(v)) {
        Ok(V::Int(i)) => Ok(i as f64),
        Ok(_) => Err(Exc::runtime("py2axum: int() beyond 64 bits in a JWT claim")),
        Err(e) if e.isinstance(&VALUE_ERROR) || e.isinstance(&TYPE_ERROR) || e.isinstance(&OVERFLOW_ERROR) => Err(err),
        Err(e) => Err(e),
    }
}

/// `now + leeway` / `now - leeway` with the leeway as given (a float, an int or a timedelta)
fn leeway_secs(leeway: &V, op: &str) -> R<f64> {
    match leeway {
        V::Int(i) => Ok(*i as f64),
        V::Bool(b) => Ok(*b as i64 as f64),
        V::Float(f) => Ok(*f),
        V::Delta(d) => Ok(d.num_microseconds().map(|m| m as f64 / 1e6).unwrap_or(d.num_seconds() as f64)),
        other => Err(Exc::type_error(format!("unsupported operand type(s) for {op}: 'float' and '{}'", other.type_name()))),
    }
}

/// `PyJWT._validate_claims`
fn validate_claims(payload: &V, opts: &[(String, V)], audience: &V, issuer: &V, subject: &V, leeway: &V) -> R<()> {
    if !matches!(audience, V::None | V::Str(_)) && ops::iter(audience).is_err() {
        return Err(Exc::type_error("audience must be a string, iterable or None"));
    }
    for claim in ops::iter(&opt(opts, "require"))? {
        if get(payload, &ops::str_(&claim)?).unwrap_or(V::None).is_none() {
            return Err(missing(&ops::str_(&claim)?));
        }
    }
    let now = chrono::Utc::now().timestamp_micros() as f64 / 1e6;
    let on = |k: &str| ops::truthy(&opt(opts, k));
    if let Some(iat) = get(payload, "iat") {
        if on("verify_iat")? {
            let iat = claim_int(&iat, exc(&PYJWT_INVALID_ISSUED_AT, "Issued At claim (iat) must be an integer."))?;
            if iat > now + leeway_secs(leeway, "+")? {
                return Err(exc(&PYJWT_IMMATURE_SIGNATURE, "The token is not yet valid (iat)"));
            }
        }
    }
    if let Some(nbf) = get(payload, "nbf") {
        if on("verify_nbf")? {
            let nbf = claim_int(&nbf, exc(&PYJWT_DECODE_ERROR, "Not Before claim (nbf) must be an integer."))?;
            if nbf > now + leeway_secs(leeway, "+")? {
                return Err(exc(&PYJWT_IMMATURE_SIGNATURE, "The token is not yet valid (nbf)"));
            }
        }
    }
    if let Some(exp) = get(payload, "exp") {
        if on("verify_exp")? {
            let exp = claim_int(&exp, exc(&PYJWT_DECODE_ERROR, "Expiration Time claim (exp) must be an integer."))?;
            if exp <= now - leeway_secs(leeway, "-")? {
                return Err(exc(&PYJWT_EXPIRED_SIGNATURE, "Signature has expired"));
            }
        }
    }
    if on("verify_iss")? && !issuer.is_none() {
        let iss = get(payload, "iss").ok_or_else(|| missing("iss"))?;
        if !matches!(iss, V::Str(_)) {
            return Err(exc(&PYJWT_INVALID_ISSUER, "Payload Issuer (iss) must be a string"));
        }
        let found = match issuer {
            V::Str(_) => ops::eq_bool(&iss, issuer),
            other => match ops::contains(other, &iss) {
                Ok(b) => b,
                Err(e) if e.isinstance(&TYPE_ERROR) => {
                    return Err(exc(&PYJWT_INVALID_ISSUER, "Issuer param must be \"str\" or \"Container[str]\""))
                }
                Err(e) => return Err(e),
            },
        };
        if !found {
            return Err(exc(&PYJWT_INVALID_ISSUER, "Invalid issuer"));
        }
    }
    if on("verify_aud")? {
        validate_aud(payload, audience, ops::truthy(&opt(opts, "strict_aud"))?)?;
    }
    if on("verify_sub")? {
        if let Some(sub) = get(payload, "sub") {
            if !matches!(sub, V::Str(_)) {
                return Err(exc(&PYJWT_INVALID_SUBJECT, "Subject must be a string"));
            }
            if !subject.is_none() && !ops::eq_bool(&sub, subject) {
                return Err(exc(&PYJWT_INVALID_SUBJECT, "Invalid subject"));
            }
        }
    }
    if on("verify_jti")? {
        if let Some(jti) = get(payload, "jti") {
            if !matches!(jti, V::Str(_)) {
                return Err(exc(&PYJWT_INVALID_JTI, "JWT ID must be a string"));
            }
        }
    }
    Ok(())
}

/// `PyJWT._validate_aud`
fn validate_aud(payload: &V, audience: &V, strict: bool) -> R<()> {
    let aud = get(payload, "aud");
    let present = match &aud {
        Some(a) => ops::truthy(a)?,
        None => false,
    };
    if audience.is_none() {
        if !present {
            return Ok(());
        }
        return Err(exc(&PYJWT_INVALID_AUDIENCE, "Invalid audience"));
    }
    let Some(claims) = aud.filter(|_| present) else {
        return Err(missing("aud"));
    };
    if strict {
        if !matches!(audience, V::Str(_)) {
            return Err(exc(&PYJWT_INVALID_AUDIENCE, "Invalid audience (strict)"));
        }
        if !matches!(claims, V::Str(_)) {
            return Err(exc(&PYJWT_INVALID_AUDIENCE, "Invalid claim format in token (strict)"));
        }
        if !ops::eq_bool(audience, &claims) {
            return Err(exc(&PYJWT_INVALID_AUDIENCE, "Audience doesn't match (strict)"));
        }
        return Ok(());
    }
    let list = match &claims {
        V::Str(_) => vec![claims.clone()],
        V::List(l) => l.lock().clone(),
        _ => return Err(exc(&PYJWT_INVALID_AUDIENCE, "Invalid claim format in token")),
    };
    if list.iter().any(|c| !matches!(c, V::Str(_))) {
        return Err(exc(&PYJWT_INVALID_AUDIENCE, "Invalid claim format in token"));
    }
    let wanted = match audience {
        V::Str(_) => vec![audience.clone()],
        other => ops::iter(other)?,
    };
    if wanted.iter().all(|a| !list.iter().any(|c| ops::eq_bool(c, a))) {
        return Err(exc(&PYJWT_INVALID_AUDIENCE, "Audience doesn't match"));
    }
    Ok(())
}
