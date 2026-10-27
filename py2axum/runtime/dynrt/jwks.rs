//! PyJWT 2.15's JSON Web Keys: `PyJWK`, `PyJWKSet` (`api_jwk.py`), `PyJWKClient` (`jwks_client.py`, its JWK Set
//! cache `jwk_set_cache.py` and the `lru_cache` of `cache_keys=True`), and the RSA public keys they hold
//! (`RSAAlgorithm.from_jwk` / `verify`, `RSAPSSAlgorithm.verify`), used by `jwt.decode` (pyjwt.rs). Verification
//! only: RSA keys from a JWK (`kty: RSA`, `n`/`e`) and HMAC keys (`kty: oct`); EC and OKP keys are parsed as
//! PyJWT does but verifying with one raises a py2axum RuntimeError, as does a private JWK.
//!
//! The endpoint is fetched with reqwest, without redirects and with the client's timeout, as urllib's opener
//! of PyJWT does; the `PyJWKClientConnectionError` message carries the error as urllib words it for an HTTP
//! status (`HTTP Error 404: Not Found`), the transport's own text otherwise (a refused connection, a timeout:
//! the exception's message differs from CPython's, its class does not).
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use num_integer::Integer;

use super::ops;
use super::v::*;

pub enum KeyMaterial {
    Rsa(Arc<rsa::RsaPublicKey>),
    Hmac(Vec<u8>),
    /// EC / OKP / an RSA private key: parsed, not reproduced (a py2axum RuntimeError when used)
    Other(&'static str),
}

pub struct Jwk {
    pub data: V,
    pub alg: String,
    pub key: KeyMaterial,
}

static PYJWT_MINOR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(15);

/// The PyJWT 2.x minor the project locks (13, 14 or 15; main() sets it): what changed in 2.14 (the refresh
/// cooldown, redirects refused) and 2.15 (key errors as InvalidKeyError, members that are not objects skipped,
/// the payload checked before it is cached)
pub fn set_pyjwt(minor: u32) {
    PYJWT_MINOR.store(minor, std::sync::atomic::Ordering::Relaxed);
}

fn minor() -> u32 {
    PYJWT_MINOR.load(std::sync::atomic::Ordering::Relaxed)
}

fn exc(class: &'static Class, msg: impl AsRef<str>) -> Exc {
    Exc::msg(class, msg)
}

fn get(d: &V, name: &str) -> Option<V> {
    match d {
        V::Dict(d) => d.lock().get(&Key::Str(Arc::from(name))).map(|(_, v)| v.clone()),
        _ => None,
    }
}

fn get_str(d: &V, name: &str) -> Option<String> {
    match get(d, name) {
        Some(V::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// `jwt.utils.from_base64url_uint`: base64url (padding added), big-endian
fn b64url_uint(v: &V) -> R<rsa::BigUint> {
    let s = match v {
        V::Str(s) => s.to_string(),
        V::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err(Exc::type_error("Expected a string value")),
    };
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('=').as_bytes())
        .map_err(|_| Exc::value_error("Incorrect padding"))?;
    Ok(rsa::BigUint::from_bytes_be(&raw))
}

fn same(a: &V, b: &V) -> R<bool> {
    ops::truthy(&ops::eq(a, b)?)
}

fn repr(v: &V) -> String {
    ops::repr(v).unwrap_or_else(|_| "?".into())
}

/// `PyJWK(jwk_data, algorithm=None)`
pub fn jwk_new(data: &V, algorithm: Option<&V>) -> R<Arc<Jwk>> {
    let kty = get(data, "kty").filter(|v| ops::truthy(v).unwrap_or(false));
    let Some(kty) = kty else {
        return Err(exc(&PYJWT_INVALID_KEY, format!("kty is not found: {}", repr(data))));
    };
    let mut alg: Option<String> = match algorithm {
        Some(V::Str(s)) if !s.is_empty() => Some(s.to_string()),
        Some(V::None) | None => None,
        Some(V::Str(_)) => None,
        Some(other) => return Err(Exc::type_error(format!("py2axum: PyJWK(algorithm={}) is not supported", other.type_name()))),
    };
    if alg.is_none() && matches!(data, V::Dict(_)) {
        alg = match get(data, "alg") {
            Some(V::Str(s)) if !s.is_empty() => Some(s.to_string()),
            Some(V::None) | None => None,
            Some(V::Str(_)) => None,
            Some(other) => return Err(exc(&PYJWK_ERROR, format!("Unable to find an algorithm for key: {}", repr(&other)))),
        };
    }
    let kty_s = match &kty {
        V::Str(s) => s.to_string(),
        other => repr(other),
    };
    let alg = match alg {
        Some(a) => a,
        None => {
            let crv = get(data, "crv").filter(|v| ops::truthy(v).unwrap_or(false));
            let crv_s = crv.as_ref().map(|c| match c {
                V::Str(s) => s.to_string(),
                o => repr(o),
            });
            match kty_s.as_str() {
                "EC" => match crv_s.as_deref() {
                    None | Some("P-256") => "ES256".into(),
                    Some("P-384") => "ES384".into(),
                    Some("P-521") => "ES512".into(),
                    Some("secp256k1") => "ES256K".into(),
                    Some(_) => return Err(exc(&PYJWT_INVALID_KEY, format!("Unsupported crv: {}", repr(crv.as_ref().unwrap())))),
                },
                "RSA" => "RS256".into(),
                "oct" => "HS256".into(),
                "OKP" => match crv_s.as_deref() {
                    None => return Err(exc(&PYJWT_INVALID_KEY, format!("crv is not found: {}", repr(data)))),
                    Some("Ed25519") => "EdDSA".into(),
                    Some(_) => return Err(exc(&PYJWT_INVALID_KEY, format!("Unsupported crv: {}", repr(crv.as_ref().unwrap())))),
                },
                _ => return Err(exc(&PYJWT_INVALID_KEY, format!("Unsupported kty: {}", repr(&kty)))),
            }
        }
    };
    // get_default_algorithms() with cryptography installed
    const KNOWN: &[&str] = &["none", "HS256", "HS384", "HS512", "RS256", "RS384", "RS512", "ES256", "ES256K", "ES384",
                             "ES521", "ES512", "PS256", "PS384", "PS512", "EdDSA"];
    if !KNOWN.contains(&alg.as_str()) {
        return Err(exc(&PYJWK_ERROR, format!("Unable to find an algorithm for key: {}", repr(data))));
    }
    // PyJWK wraps the ValueError of Algorithm.from_jwk from 2.14, its TypeError from 2.15; else it propagates
    let construct = |msg: String| {
        let type_error = msg == "Expected a string value";
        if minor() >= 15 || (minor() == 14 && !type_error) {
            exc(&PYJWT_INVALID_KEY, format!("Unable to construct key from JWK: {msg}"))
        } else if type_error {
            Exc::type_error(msg)
        } else {
            Exc::value_error(msg)
        }
    };
    let key = match alg.as_str() {
        "RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512" => {
            // RSAAlgorithm.from_jwk
            if get_str(data, "kty").as_deref() != Some("RSA") {
                return Err(exc(&PYJWT_INVALID_KEY, "Not an RSA key"));
            }
            let (d, e, n) = (get(data, "d"), get(data, "e"), get(data, "n"));
            if d.is_some() && e.is_some() && n.is_some() {
                if get(data, "oth").is_some() {
                    return Err(exc(&PYJWT_INVALID_KEY, "Unsupported RSA private key: > 2 primes not supported"));
                }
                let found: Vec<bool> = ["p", "q", "dp", "dq", "qi"].iter().map(|p| get(data, p).is_some()).collect();
                if found.iter().any(|x| *x) && !found.iter().all(|x| *x) {
                    return Err(exc(&PYJWT_INVALID_KEY, "RSA key must include all parameters if any are present besides d"));
                }
                KeyMaterial::Other("an RSA private key")
            } else if let (Some(n), Some(e)) = (n, e) {
                let (n, e) = match (b64url_uint(&n), b64url_uint(&e)) {
                    (Ok(n), Ok(e)) => (n, e),
                    (Err(err), _) | (_, Err(err)) => {
                        if err.isinstance(&TYPE_ERROR) || err.isinstance(&VALUE_ERROR) {
                            return Err(construct(err.message()));
                        }
                        return Err(err);
                    }
                };
                // cryptography's RSAPublicNumbers.public_key() checks: 3 <= e < n, e odd, n odd
                let three = rsa::BigUint::from(3u32);
                if e < three || e >= n || e.is_even() || n.is_even() {
                    return Err(construct("Invalid public numbers".into()));
                }
                match rsa::RsaPublicKey::new_with_max_size(n, e, 16384) {
                    Ok(k) => KeyMaterial::Rsa(Arc::new(k)),
                    Err(err) => return Err(construct(err.to_string())),
                }
            } else {
                return Err(exc(&PYJWT_INVALID_KEY, "Not a public or private key"));
            }
        }
        "HS256" | "HS384" | "HS512" => {
            // HMACAlgorithm.from_jwk
            if get_str(data, "kty").as_deref() != Some("oct") {
                return Err(exc(&PYJWT_INVALID_KEY, "Not an HMAC key"));
            }
            let k = get(data, "k").ok_or_else(|| Exc::new(&KEY_ERROR, vec![V::str("k")]))?;
            let s = match &k {
                V::Str(s) => s.to_string(),
                _ => return Err(construct("Expected a string value".into())),
            };
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s.trim_end_matches('=').as_bytes())
                .map_err(|_| construct("Incorrect padding".into()))?;
            KeyMaterial::Hmac(raw)
        }
        "none" => KeyMaterial::Other("the 'none' algorithm"),
        _ => KeyMaterial::Other("an EC or OKP key"),
    };
    Ok(Arc::new(Jwk { data: data.clone(), alg, key }))
}

/// `PyJWKSet(keys)` / `PyJWKSet.from_dict(obj)`: the usable keys, in order
pub fn set_new(keys: &V) -> R<Arc<Vec<Arc<Jwk>>>> {
    if !ops::truthy(keys)? {
        return Err(exc(&PYJWK_SET_ERROR, "The JWK Set did not contain any keys"));
    }
    let V::List(l) = keys else {
        return Err(exc(&PYJWK_SET_ERROR, "Invalid JWK Set value"));
    };
    let mut out = Vec::new();
    for k in l.lock().clone() {
        if !matches!(k, V::Dict(_)) {
            if minor() < 15 {
                // PyJWK(key) reads `key.get("kty")`
                return Err(Exc::attr_error(format!("'{}' object has no attribute 'get'", k.type_name())));
            }
            continue;
        }
        match jwk_new(&k, None) {
            Ok(j) => out.push(j),
            Err(e) if e.isinstance(&PYJWT_MISSING_CRYPTOGRAPHY) => return Err(e),
            Err(e) if e.isinstance(&PYJWT_ERROR) => continue,
            Err(e) => return Err(e),
        }
    }
    if out.is_empty() {
        return Err(exc(&PYJWK_SET_ERROR, "The JWK Set did not contain any usable keys. Perhaps 'cryptography' is not installed?"));
    }
    Ok(Arc::new(out))
}

pub fn set_from_dict(obj: &V) -> R<Arc<Vec<Arc<Jwk>>>> {
    let keys = match obj {
        V::Dict(_) => get(obj, "keys").unwrap_or_else(|| V::list(vec![])),
        other => return Err(Exc::attr_error(format!("'{}' object has no attribute 'get'", other.type_name()))),
    };
    set_new(&keys)
}

pub fn jwk_value(j: R<Arc<Jwk>>) -> R {
    j.map(|j| V::native(Native::Jwk(j)))
}

pub fn set_value(s: R<Arc<Vec<Arc<Jwk>>>>) -> R {
    s.map(|s| V::native(Native::JwkSet(s)))
}

fn key_id(j: &Jwk) -> V {
    get(&j.data, "kid").unwrap_or(V::None)
}

pub fn jwk_attr(j: &Arc<Jwk>, name: &str) -> R {
    match name {
        "key" => Ok(match &j.key {
            KeyMaterial::Rsa(k) => V::native(Native::RsaPub(k.clone())),
            KeyMaterial::Hmac(b) => V::Bytes(Arc::from(b.as_slice())),
            KeyMaterial::Other(what) => return Err(Exc::runtime(format!("py2axum: the key of a PyJWK holding {what} is not supported"))),
        }),
        "key_type" => Ok(get(&j.data, "kty").unwrap_or(V::None)),
        "key_id" => Ok(key_id(j)),
        "public_key_use" => Ok(get(&j.data, "use").unwrap_or(V::None)),
        "algorithm_name" => Ok(V::str(&j.alg)),
        "_jwk_data" => Ok(j.data.clone()),
        _ => Err(Exc::attr_error(format!("'PyJWK' object has no attribute '{name}'"))),
    }
}

pub fn set_attr(s: &Arc<Vec<Arc<Jwk>>>, name: &str) -> R {
    match name {
        "keys" => Ok(V::list(s.iter().map(|j| V::native(Native::Jwk(j.clone()))).collect())),
        _ => Err(Exc::attr_error(format!("'PyJWKSet' object has no attribute '{name}'"))),
    }
}

/// `jwk_set[kid]`
pub fn set_getitem(s: &Arc<Vec<Arc<Jwk>>>, kid: &V) -> R {
    for j in s.iter() {
        if same(&key_id(j), kid)? {
            return Ok(V::native(Native::Jwk(j.clone())));
        }
    }
    Err(Exc::new(&KEY_ERROR, vec![V::str(format!("keyset has no key for kid: {}", ops::str_(kid)?))]))
}

pub fn rsa_attr(k: &rsa::RsaPublicKey, name: &str) -> R {
    use rsa::traits::PublicKeyParts;
    match name {
        "key_size" => Ok(V::Int(k.n().bits() as i64)),
        _ => Err(Exc::attr_error(format!("'RSAPublicKey' object has no attribute '{name}'"))),
    }
}

/// `RSAAlgorithm.verify` / `RSAPSSAlgorithm.verify` (PSS: MGF1 of the same hash, salt length = digest size)
pub fn rsa_verify(alg: &str, key: &rsa::RsaPublicKey, msg: &[u8], sig: &[u8]) -> bool {
    use rsa::signature::Verifier;
    macro_rules! pkcs1 {
        ($h:ty) => {{
            let vk = rsa::pkcs1v15::VerifyingKey::<$h>::new(key.clone());
            match rsa::pkcs1v15::Signature::try_from(sig) {
                Ok(s) => vk.verify(msg, &s).is_ok(),
                Err(_) => false,
            }
        }};
    }
    macro_rules! pss {
        ($h:ty) => {{
            let vk = rsa::pss::VerifyingKey::<$h>::new(key.clone());
            match rsa::pss::Signature::try_from(sig) {
                Ok(s) => vk.verify(msg, &s).is_ok(),
                Err(_) => false,
            }
        }};
    }
    match alg {
        "RS256" => pkcs1!(sha2::Sha256),
        "RS384" => pkcs1!(sha2::Sha384),
        "RS512" => pkcs1!(sha2::Sha512),
        "PS256" => pss!(sha2::Sha256),
        "PS384" => pss!(sha2::Sha384),
        "PS512" => pss!(sha2::Sha512),
        _ => false,
    }
}

// ---------------------------------------------------------------- PyJWKClient

pub struct Client {
    pub uri: String,
    headers: Vec<(String, String)>,
    timeout: f64,
    lifespan: Option<f64>,
    cache_keys: Option<Option<usize>>,
    cooldown: f64,
    state: tokio::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    set: Option<(Arc<Vec<Arc<Jwk>>>, Instant)>,
    /// before 2.15 the JWK Set cache holds the payload as received, parsed again by each read
    raw: Option<(V, Instant)>,
    last_fetch: Option<Instant>,
    /// lru_cache of get_signing_key: (kid, key), most recent last
    lru: Vec<(V, Arc<Jwk>)>,
}

fn num(name: &str, v: &V) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Bool(b) => Ok(*b as i64 as f64),
        V::Float(f) => Ok(*f),
        o => Err(Exc::type_error(format!("py2axum: PyJWKClient({name}=) of type {} is not supported", o.type_name()))),
    }
}

fn fmt_num(v: &V) -> String {
    ops::str_(v).unwrap_or_default()
}

/// `PyJWKClient(uri, cache_keys=False, max_cached_keys=16, cache_jwk_set=True, lifespan=300, headers=None,
/// timeout=30, ssl_context=None, cooldown_duration=30)`
pub fn client_new(args: &[V], kwargs: &[(String, V)]) -> R {
    const ALL: &[&str] = &["uri", "cache_keys", "max_cached_keys", "cache_jwk_set", "lifespan", "headers", "timeout",
                           "ssl_context", "cooldown_duration"];
    // `cooldown_duration` appeared in 2.14
    let names = if minor() >= 14 { ALL } else { &ALL[..8] };
    if args.len() > names.len() {
        return Err(Exc::type_error(format!("PyJWKClient.__init__() takes from 2 to {} positional arguments but {} were given",
                                           names.len() + 1, args.len() + 1)));
    }
    let mut vals: Vec<Option<V>> = vec![None; ALL.len()];
    for (i, a) in args.iter().enumerate() {
        vals[i] = Some(a.clone());
    }
    for (k, v) in kwargs {
        let Some(i) = names.iter().position(|n| n == k) else {
            return Err(Exc::type_error(format!("PyJWKClient.__init__() got an unexpected keyword argument '{k}'")));
        };
        if vals[i].is_some() {
            return Err(Exc::type_error(format!("PyJWKClient.__init__() got multiple values for argument '{k}'")));
        }
        vals[i] = Some(v.clone());
    }
    let uri = match &vals[0] {
        Some(V::Str(s)) => s.to_string(),
        Some(o) => return Err(Exc::type_error(format!("py2axum: PyJWKClient(uri) of type {} is not supported", o.type_name()))),
        None => return Err(Exc::type_error("PyJWKClient.__init__() missing 1 required positional argument: 'uri'")),
    };
    let headers = match vals[5].clone().unwrap_or(V::None) {
        V::None => vec![],
        V::Dict(d) => d.lock().values().map(|(k, v)| Ok((ops::str_(k)?, ops::str_(v)?))).collect::<R<Vec<_>>>()?,
        o => return Err(Exc::type_error(format!("py2axum: PyJWKClient(headers=) of type {} is not supported", o.type_name()))),
    };
    // urlparse(uri).scheme.lower()
    let scheme = match uri.find(':') {
        Some(i) if i > 0 && uri[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            && uri[..1].chars().all(|c| c.is_ascii_alphabetic()) => uri[..i].to_ascii_lowercase(),
        _ => String::new(),
    };
    if scheme != "http" && scheme != "https" {
        return Err(exc(&PYJWK_CLIENT_ERROR, format!(
            "Invalid JWKS URI scheme {}: only 'http' and 'https' are supported.", repr(&V::str(&scheme)))));
    }
    if !matches!(vals[7], None | Some(V::None)) {
        return Err(Exc::type_error("py2axum: PyJWKClient(ssl_context=) is not supported"));
    }
    let timeout_v = vals[6].clone().unwrap_or(V::Int(30));
    let timeout = num("timeout", &timeout_v)?;
    let cooldown_v = vals[8].clone().unwrap_or(V::Int(30));
    let cooldown = num("cooldown_duration", &cooldown_v)?;
    if cooldown < 0.0 {
        return Err(exc(&PYJWK_CLIENT_ERROR, format!(
            "Cooldown duration must be greater than or equal to 0, the input is \"{}\"", fmt_num(&cooldown_v))));
    }
    if !cooldown.is_finite() {
        return Err(exc(&PYJWK_CLIENT_ERROR, format!("Cooldown duration must be finite, the input is \"{}\"", fmt_num(&cooldown_v))));
    }
    let lifespan = if ops::truthy(&vals[3].clone().unwrap_or(V::Bool(true)))? {
        let l = vals[4].clone().unwrap_or(V::Int(300));
        let lf = num("lifespan", &l)?;
        if lf <= 0.0 {
            return Err(exc(&PYJWK_CLIENT_ERROR, format!("Lifespan must be greater than 0, the input is \"{}\"", fmt_num(&l))));
        }
        Some(lf)
    } else {
        None
    };
    let cache_keys = if ops::truthy(&vals[1].clone().unwrap_or(V::Bool(false)))? {
        Some(match vals[2].clone().unwrap_or(V::Int(16)) {
            V::None => None,
            V::Int(i) => Some(i.max(0) as usize),
            V::Bool(b) => Some(b as usize),
            o => return Err(Exc::type_error(format!("Expected first argument to be an integer, a callable, or None, not {}", o.type_name()))),
        })
    } else {
        None
    };
    Ok(V::native(Native::JwkClient(Arc::new(Client { uri, headers, timeout, lifespan, cache_keys, cooldown,
                                                         state: tokio::sync::Mutex::new(State::default()) }))))
}

fn reason(code: u16) -> &'static str {
    axum::http::StatusCode::from_u16(code).ok().and_then(|s| s.canonical_reason()).unwrap_or("")
}

/// `fetch_data()`: the endpoint's JSON object, cached when the JWK Set cache is on
async fn fetch_data(c: &Client, st: &mut State) -> R<V> {
    let conn = |m: String| exc(&PYJWK_CLIENT_CONNECTION_ERROR, format!("Fail to fetch data from the url, err: \"{m}\""));
    let client = reqwest::Client::builder()
        // 2.13's urlopen follows redirects (HTTPRedirectHandler: at most 10); 2.14 refuses them
        .redirect(if minor() < 14 { reqwest::redirect::Policy::limited(10) } else { reqwest::redirect::Policy::none() })
        .timeout(Duration::from_secs_f64(c.timeout.max(0.0)))
        .build()
        .map_err(|e| conn(e.to_string()))?;
    let mut req = client.get(&c.uri).header("User-Agent", format!("Python-urllib/{}.{}", super::python().0, super::python().1));
    for (k, v) in &c.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() { conn("timed out".into()) } else { conn(format!("<urlopen error {e}>")) }
    })?;
    let status = resp.status().as_u16();
    if status >= 300 {
        // urllib raises HTTPError for any status its handlers leave (redirects included: _NoRedirectHandler)
        return Err(conn(format!("HTTP Error {status}: {}", reason(status))));
    }
    let body = resp.bytes().await.map_err(|e| {
        if e.is_timeout() { conn("timed out".into()) } else { conn(format!("<urlopen error {e}>")) }
    })?;
    let data = super::libs::json_loads(&V::Bytes(Arc::from(&body[..])))?;
    if minor() < 15 {
        // the payload cached as it is (checked and parsed by get_jwk_set)
        if c.lifespan.is_some() {
            st.raw = Some((data.clone(), Instant::now()));
        }
        st.last_fetch = Some(Instant::now());
        return Ok(data);
    }
    if !matches!(data, V::Dict(_)) {
        return Err(exc(&PYJWK_CLIENT_ERROR, "The JWKS endpoint did not return a JSON object"));
    }
    if c.lifespan.is_some() {
        // JWKSetCache.put: parsed once (an unusable set raises here, before the cache changes)
        let set = set_from_dict(&data)?;
        st.set = Some((set, Instant::now()));
    }
    st.last_fetch = Some(Instant::now());
    Ok(data)
}

/// `get_jwk_set(refresh=False)`
async fn get_jwk_set(c: &Client, st: &mut State, refresh: bool) -> R<Arc<Vec<Arc<Jwk>>>> {
    if minor() < 15 {
        let cached = match (c.lifespan, refresh, &st.raw) {
            (Some(life), false, Some((raw, at))) if at.elapsed().as_secs_f64() <= life => Some(raw.clone()),
            _ => None,
        };
        let data = match cached {
            Some(d) => d,
            None => fetch_data(c, st).await?,
        };
        if !matches!(data, V::Dict(_)) {
            return Err(exc(&PYJWK_CLIENT_ERROR, "The JWKS endpoint did not return a JSON object"));
        }
        return set_from_dict(&data);
    }
    if let (Some(life), false) = (c.lifespan, refresh) {
        // JWKSetCache.get: None once expired (`monotonic() > timestamp + lifespan`)
        if let Some((set, at)) = &st.set {
            if at.elapsed().as_secs_f64() <= life {
                return Ok(set.clone());
            }
        }
    }
    let data = fetch_data(c, st).await?;
    match (&st.set, c.lifespan) {
        // parsed and cached by fetch_data (get_jwk_set parses the same payload again and caches it: the same keys)
        (Some((s, _)), Some(_)) => Ok(s.clone()),
        _ => set_from_dict(&data),
    }
}

fn signing_keys_of(set: &[Arc<Jwk>]) -> R<Vec<Arc<Jwk>>> {
    let keys: Vec<Arc<Jwk>> = set
        .iter()
        .filter(|j| matches!(get(&j.data, "use"), None | Some(V::None)) || get_str(&j.data, "use").as_deref() == Some("sig"))
        .filter(|j| ops::truthy(&key_id(j)).unwrap_or(false))
        .cloned()
        .collect();
    if keys.is_empty() {
        return Err(exc(&PYJWK_CLIENT_ERROR, "The JWKS endpoint did not contain any signing keys"));
    }
    Ok(keys)
}

fn match_kid(keys: &[Arc<Jwk>], kid: &V) -> R<Option<Arc<Jwk>>> {
    for k in keys {
        if same(&key_id(k), kid)? {
            return Ok(Some(k.clone()));
        }
    }
    Ok(None)
}

/// `get_signing_key(kid)` (behind `lru_cache(maxsize=max_cached_keys)` with `cache_keys=True`)
async fn get_signing_key(c: &Client, kid: &V) -> R<Arc<Jwk>> {
    let mut st = c.state.lock().await;
    if let Some(max) = c.cache_keys {
        Key::dict_key(kid)?; // lru_cache hashes its argument
        if let Some(i) = st.lru.iter().position(|(k, _)| same(k, kid).unwrap_or(false)) {
            let hit = st.lru.remove(i);
            let key = hit.1.clone();
            st.lru.push(hit);
            return Ok(key);
        }
        let key = signing_key_uncached(c, &mut st, kid).await?;
        if max != Some(0) {
            st.lru.push((kid.clone(), key.clone()));
            if let Some(m) = max {
                while st.lru.len() > m {
                    st.lru.remove(0);
                }
            }
        }
        return Ok(key);
    }
    signing_key_uncached(c, &mut st, kid).await
}

async fn signing_key_uncached(c: &Client, st: &mut State, kid: &V) -> R<Arc<Jwk>> {
    let set = get_jwk_set(c, st, false).await?;
    let keys = signing_keys_of(&set)?;
    if let Some(k) = match_kid(&keys, kid)? {
        return Ok(k);
    }
    // the refresh cooldown appeared in 2.14
    let cooling = minor() >= 14 && c.lifespan.is_some() && st.last_fetch.is_some_and(|t| t.elapsed().as_secs_f64() < c.cooldown);
    if !cooling {
        let set = get_jwk_set(c, st, true).await?;
        let keys = signing_keys_of(&set)?;
        st.last_fetch = Some(Instant::now());
        if let Some(k) = match_kid(&keys, kid)? {
            return Ok(k);
        }
    }
    Err(exc(&PYJWK_CLIENT_ERROR, format!("Unable to find a signing key that matches: \"{}\"", ops::str_(kid)?)))
}

pub fn client_attr(c: &Arc<Client>, name: &str) -> R {
    match name {
        "uri" => Ok(V::str(&c.uri)),
        "timeout" => Ok(V::Float(c.timeout)),
        "cooldown_duration" => Ok(V::Float(c.cooldown)),
        _ => Err(Exc::attr_error(format!("'PyJWKClient' object has no attribute '{name}'"))),
    }
}

pub async fn client_method(c: &Arc<Client>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let arg = |i: usize, n: &str| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v)).cloned();
    match name {
        "get_signing_key_from_jwt" => {
            let token = arg(0, "token").ok_or_else(|| Exc::type_error("PyJWKClient.get_signing_key_from_jwt() missing 1 required positional argument: 'token'"))?;
            let opts = V::dict_from(vec![(V::str("verify_signature"), V::Bool(false))])?;
            let d = super::pyjwt::decode_complete(&token, &V::str(""), &V::None, &opts, &V::None, &V::None, &V::None, &V::Int(0))?;
            let header = get(&d, "header").unwrap_or(V::None);
            let kid = get(&header, "kid").unwrap_or(V::None);
            Ok(V::native(Native::Jwk(get_signing_key(c, &kid).await?)))
        }
        "get_signing_key" => {
            let kid = arg(0, "kid").ok_or_else(|| Exc::type_error("PyJWKClient.get_signing_key() missing 1 required positional argument: 'kid'"))?;
            Ok(V::native(Native::Jwk(get_signing_key(c, &kid).await?)))
        }
        "get_signing_keys" | "get_jwk_set" => {
            let refresh = arg(0, "refresh").map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false);
            let mut st = c.state.lock().await;
            let set = get_jwk_set(c, &mut st, refresh).await?;
            if name == "get_jwk_set" {
                return Ok(V::native(Native::JwkSet(set)));
            }
            Ok(V::list(signing_keys_of(&set)?.into_iter().map(|j| V::native(Native::Jwk(j))).collect()))
        }
        "fetch_data" => {
            let mut st = c.state.lock().await;
            fetch_data(c, &mut st).await
        }
        _ => Err(Exc::attr_error(format!("'PyJWKClient' object has no attribute '{name}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // a 2048-bit key and an RS256/PS256 token signed by PyJWT 2.15.1 + cryptography (tools: see the commit message)
    const N: &str = include_str!("testdata/jwk_n.txt");
    const RS256: &str = include_str!("testdata/jwt_rs256.txt");
    const PS256: &str = include_str!("testdata/jwt_ps256.txt");

    fn key() -> rsa::RsaPublicKey {
        let n = b64url_uint(&V::str(N.trim())).unwrap();
        rsa::RsaPublicKey::new(n, rsa::BigUint::from(65537u32)).unwrap()
    }

    fn parts(t: &str) -> (Vec<u8>, Vec<u8>) {
        let t = t.trim();
        let i = t.rfind('.').unwrap();
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&t[i + 1..]).unwrap();
        (t[..i].as_bytes().to_vec(), sig)
    }

    #[test]
    fn verifies_pyjwt_signatures() {
        let k = key();
        let (msg, sig) = parts(RS256);
        assert!(rsa_verify("RS256", &k, &msg, &sig));
        assert!(!rsa_verify("RS384", &k, &msg, &sig));
        let mut bad = msg.clone();
        bad[0] ^= 1;
        assert!(!rsa_verify("RS256", &k, &bad, &sig));
        let (msg, sig) = parts(PS256);
        assert!(rsa_verify("PS256", &k, &msg, &sig));
        assert!(!rsa_verify("RS256", &k, &msg, &sig));
    }
}
