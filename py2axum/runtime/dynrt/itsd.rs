//! `itsdangerous.URLSafeTimedSerializer` and `URLSafeSerializer` (itsdangerous 2.2.0): `dumps(obj, salt)` /
//! `loads(s, max_age, return_timestamp, salt)` (`loads(s, salt)` untimed) with the default signer (django-concat key derivation,
//! HMAC-SHA1), compact JSON payload, zlib when shorter, and the library's exceptions (with their `payload`
//! and `date_signed` attributes) and messages in its order of checks.
use std::io::{Read, Write};

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};

use super::ops;
use super::pyd;
use super::v::*;

pub struct Serializer {
    /// secret keys, the last one signs (`_make_keys_list`)
    pub keys: Vec<Vec<u8>>,
    /// the serializer's salt (None: the Signer default)
    pub salt: Option<Vec<u8>>,
    /// URLSafeTimedSerializer (TimestampSigner) or URLSafeSerializer (Signer)
    pub timed: bool,
}

impl Serializer {
    pub fn class_name(&self) -> &'static str {
        if self.timed { "URLSafeTimedSerializer" } else { "URLSafeSerializer" }
    }
}

fn want_bytes(v: &V, what: &str) -> R<Vec<u8>> {
    match v {
        V::Str(s) => Ok(s.as_bytes().to_vec()),
        V::Bytes(b) => Ok(b.to_vec()),
        other => Err(Exc::type_error(format!("py2axum: {what} must be str or bytes (got {})", other.type_name()))),
    }
}

/// `URLSafeTimedSerializer(secret_key, salt=b"itsdangerous")` (`timed`) or `URLSafeSerializer(...)`
pub fn new(secret_key: &V, salt: Option<&V>, timed: bool) -> R {
    let keys = match secret_key {
        V::Str(_) | V::Bytes(_) => vec![want_bytes(secret_key, "secret_key")?],
        other => ops::iter(other)?.iter().map(|k| want_bytes(k, "secret_key")).collect::<R<Vec<_>>>()?,
    };
    let salt = match salt {
        None => Some(b"itsdangerous".to_vec()),
        Some(V::None) => None,
        Some(s) => Some(want_bytes(s, "salt")?),
    };
    Ok(V::native(Native::Serializer(Serializer { keys, salt, timed })))
}

fn b64(data: &[u8]) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data).into_bytes()
}

/// `base64_decode`: the bytes padded, then decoded leniently (binascii); None = BadData
fn b64d(data: &[u8]) -> Option<Vec<u8>> {
    super::jose::b64_decode(data)
}

fn derive_key(salt: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut h = Sha1::new();
    h.update(salt);
    h.update(b"signer");
    h.update(secret);
    h.finalize().to_vec()
}

fn signature(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("any key length");
    m.update(value);
    m.finalize().into_bytes().to_vec()
}

fn signer_salt<'a>(s: &'a Serializer, salt: &'a Option<Vec<u8>>) -> &'a [u8] {
    // make_signer(salt): None -> the serializer's salt; a None salt -> Signer's default
    salt.as_deref().or(s.salt.as_deref()).unwrap_or(b"itsdangerous.Signer")
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn bad(class: &'static Class, msg: impl AsRef<str>) -> Exc {
    Exc::msg(class, msg)
}

/// `BadSignature(message, payload)` / `BadTimeSignature(message, payload, date_signed)`: the attributes
/// the library sets in `__init__`
fn bad_sig(class: &'static Class, msg: impl AsRef<str>, payload: Option<&[u8]>, date_signed: Option<i64>) -> Exc {
    let e = Exc::msg(class, msg);
    let mut attrs = e.0.attrs.lock();
    attrs.insert("payload".into(), payload.map(|p| V::Bytes(p.to_vec().into())).unwrap_or(V::None));
    if class.is_subclass(&BAD_TIME_SIGNATURE) {
        attrs.insert("date_signed".into(), date_signed.map(date_of).unwrap_or(V::None));
    }
    drop(attrs);
    e
}

/// `TimestampSigner.timestamp_to_datetime`: an aware datetime in `timezone.utc`
fn date_of(ts: i64) -> V {
    let utc = chrono::DateTime::from_timestamp(ts, 0).expect("checked against year 9999").naive_utc();
    V::DateTime(super::dt::DateTime::aware(utc, super::dt::Tz::Utc))
}

/// the largest timestamp `datetime.fromtimestamp` takes (9999-12-31T23:59:59Z)
const MAX_TS: u64 = 253_402_300_799;

fn salt_kw(kwargs: &[(String, V)]) -> R<Option<Vec<u8>>> {
    match kwargs.iter().find(|(k, _)| k == "salt").map(|(_, v)| v) {
        None | Some(V::None) => Ok(None),
        Some(v) => Ok(Some(want_bytes(v, "salt")?)),
    }
}

fn check_kwargs(s: &Serializer, name: &str, kwargs: &[(String, V)], allowed: &[&str]) -> R<()> {
    match kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(Exc::type_error(format!("py2axum: {}.{name}({k}=) is not supported", s.class_name()))),
        None => Ok(()),
    }
}

pub fn method(s: &Serializer, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "dumps" => {
            check_kwargs(s, name, kwargs, &["salt"])?;
            if args.len() > 2 {
                return Err(Exc::type_error(format!("Serializer.dumps() takes from 2 to 3 positional arguments but {} were given", args.len() + 1)));
            }
            let obj = args.first().ok_or_else(|| Exc::type_error("Serializer.dumps() missing 1 required positional argument: 'obj'"))?;
            let salt = if let Some(v) = args.get(1) { if matches!(v, V::None) { None } else { Some(want_bytes(v, "salt")?) } } else { salt_kw(kwargs)? };
            dumps(s, obj, &salt)
        }
        // Serializer.loads(s, salt=None, **kwargs): the other keywords are accepted and ignored
        "loads" if !s.timed => {
            if args.len() > 2 {
                return Err(Exc::type_error(format!("Serializer.loads() takes from 2 to 3 positional arguments but {} were given", args.len() + 1)));
            }
            if args.len() == 2 && kwargs.iter().any(|(k, _)| k == "salt") {
                return Err(Exc::type_error("Serializer.loads() got multiple values for argument 'salt'"));
            }
            let token = match args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "s").map(|(_, v)| v)) {
                Some(t) => t,
                None => return Err(Exc::type_error("Serializer.loads() missing 1 required positional argument: 's'")),
            };
            let salt = match args.get(1) {
                Some(V::None) => None,
                Some(v) => Some(want_bytes(v, "salt")?),
                None => salt_kw(kwargs)?,
            };
            loads_untimed(s, token, &salt)
        }
        "loads" => {
            check_kwargs(s, name, kwargs, &["max_age", "return_timestamp", "salt"])?;
            // TimedSerializer.loads(s, max_age=None, return_timestamp=False, salt=None)
            let params = ["s", "max_age", "return_timestamp", "salt"];
            if args.len() > params.len() {
                return Err(Exc::type_error(format!("TimedSerializer.loads() takes from 2 to 5 positional arguments but {} were given", args.len() + 1)));
            }
            if let Some((k, _)) = kwargs.iter().find(|(k, _)| params[..args.len()].contains(&k.as_str())) {
                return Err(Exc::type_error(format!("TimedSerializer.loads() got multiple values for argument '{k}'")));
            }
            let arg = |i: usize| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == params[i]).map(|(_, v)| v)).cloned().unwrap_or(V::None);
            let token = args.first().ok_or_else(|| Exc::type_error("TimedSerializer.loads() missing 1 required positional argument: 's'"))?;
            let salt = match arg(3) {
                V::None => None,
                v => Some(want_bytes(&v, "salt")?),
            };
            let (payload, ts) = loads(s, token, &arg(1), &salt)?;
            if ops::truthy(&arg(2))? { Ok(V::tuple(vec![payload, ts])) } else { Ok(payload) }
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", s.class_name()))),
    }
}

fn dumps(s: &Serializer, obj: &V, salt: &Option<Vec<u8>>) -> R {
    let json = super::libs::json_dumps(
        obj,
        &[("ensure_ascii".into(), V::Bool(false)), ("separators".into(), V::tuple(vec![V::str(","), V::str(":")]))],
    )?;
    let json = ops::str_(&json)?.into_bytes();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    z.write_all(&json).expect("in-memory zlib");
    let compressed = z.finish().expect("in-memory zlib");
    let mut payload = Vec::new();
    if compressed.len() + 1 < json.len() {
        payload.push(b'.');
        payload.extend(b64(&compressed));
    } else {
        payload.extend(b64(&json));
    }
    // TimestampSigner.sign: value.timestamp.signature (Signer.sign: value.signature)
    let mut value = payload;
    if s.timed {
        let ts = (now() as u64).to_be_bytes();
        let ts = &ts[ts.iter().position(|b| *b != 0).unwrap_or(ts.len())..];
        value.push(b'.');
        value.extend(b64(ts));
    }
    let key = derive_key(signer_salt(s, salt), s.keys.last().expect("a secret key"));
    let sig = b64(&signature(&key, &value));
    value.push(b'.');
    value.extend(sig);
    Ok(V::str(String::from_utf8(value).expect("ascii")))
}

/// `Signer.unsign`'s check: None when a key signed `value`, else BadSignature's message and payload
fn check_sig(s: &Serializer, signed: &[u8], salt: &[u8]) -> Option<(String, Option<Vec<u8>>)> {
    match signed.iter().rposition(|c| *c == b'.') {
        None => Some(("No b'.' found in value".into(), None)),
        Some(i) => {
            let (value, sig) = (&signed[..i], &signed[i + 1..]);
            let ok = b64d(sig).is_some_and(|sig| {
                s.keys.iter().rev().any(|k| {
                    let expected = signature(&derive_key(salt, k), value);
                    bool::from(subtle::ConstantTimeEq::ct_eq(expected.as_slice(), sig.as_slice()))
                })
            });
            if ok {
                None
            } else {
                let repr = ops::repr(&V::Bytes(sig.to_vec().into())).unwrap_or_default();
                Some((format!("Signature {repr} does not match"), Some(value.to_vec())))
            }
        }
    }
}

/// `Serializer.loads` with the default Signer (no fallback signers)
/// `want_bytes(s)` of `loads`: a str or bytes; a number reaches `b"." not in s` (Signer.unsign)
fn token_bytes(token: &V) -> R<Vec<u8>> {
    match token {
        V::Int(_) | V::Float(_) | V::Bool(_) | V::None => {
            Err(Exc::type_error(ops::not_container(token.type_name())))
        }
        _ => want_bytes(token, "s"),
    }
}

fn loads_untimed(s: &Serializer, token: &V, salt: &Option<Vec<u8>>) -> R {
    let signed = token_bytes(token)?;
    if let Some((msg, payload)) = check_sig(s, &signed, signer_salt(s, salt)) {
        return Err(bad_sig(&BAD_SIGNATURE, msg, payload.as_deref(), None));
    }
    load_payload(&signed[..signed.iter().rposition(|c| *c == b'.').unwrap()])
}

/// `TimedSerializer.loads`: the payload and the signing instant (`return_timestamp=True`'s pair)
fn loads(s: &Serializer, token: &V, max_age: &V, salt: &Option<Vec<u8>>) -> R<(V, V)> {
    let signed = token_bytes(token)?;
    let salt = signer_salt(s, salt);
    // Signer.unsign: BadSignature("No b'.' found in value") has no payload, a bad signature the value
    let sig_error = check_sig(s, &signed, salt);
    // TimestampSigner.unsign: `result = e.payload or b""`
    let result: Vec<u8> = match &sig_error {
        None => signed[..signed.iter().rposition(|c| *c == b'.').unwrap()].to_vec(),
        Some((_, payload)) => payload.clone().unwrap_or_default(),
    };
    let Some(i) = result.iter().rposition(|c| *c == b'.') else {
        return Err(match sig_error {
            Some((msg, payload)) => bad_sig(&BAD_SIGNATURE, msg, payload.as_deref(), None),
            None => bad_sig(&BAD_TIME_SIGNATURE, "timestamp missing", Some(&result), None),
        });
    };
    let (value, ts_bytes) = (&result[..i], &result[i + 1..]);
    // bytes_to_int(base64_decode(ts)) = struct.unpack(">Q", b.rjust(8)): more than 8 bytes is an error (swallowed)
    let ts: Option<u64> = b64d(ts_bytes).filter(|b| b.len() <= 8).map(|b| b.iter().fold(0u64, |acc, x| (acc << 8) | *x as u64));
    // a valid date: Some(seconds); None = timestamp_to_datetime would raise (past year 9999)
    let date = |t: u64| (t <= MAX_TS).then_some(t as i64);
    if let Some((msg, _)) = sig_error {
        let date_signed = match ts {
            None => None,
            Some(t) => match date(t) {
                Some(d) => Some(d),
                None => return Err(bad_sig(&BAD_TIME_SIGNATURE, "Malformed timestamp", Some(value), None)),
            },
        };
        return Err(bad_sig(&BAD_TIME_SIGNATURE, msg, Some(value), date_signed));
    }
    let Some(ts) = ts else {
        return Err(bad_sig(&BAD_TIME_SIGNATURE, "Malformed timestamp", Some(value), None));
    };
    // a signed timestamp past year 9999 (only the secret's holder can make one): ValueError, like CPython
    let out_of_range = || Exc::value_error("year 10000 is out of range");
    if !matches!(max_age, V::None) {
        let age = V::Int((now() as i128 - ts as i128) as i64);
        if ops::cmp(&age, max_age)? == std::cmp::Ordering::Greater {
            let d = date(ts).ok_or_else(out_of_range)?;
            return Err(bad_sig(&SIGNATURE_EXPIRED, format!("Signature age {} > {} seconds", ops::str_(&age)?, ops::str_(max_age)?), Some(value), Some(d)));
        }
        if ops::cmp(&age, &V::Int(0))? == std::cmp::Ordering::Less {
            let d = date(ts).ok_or_else(out_of_range)?;
            return Err(bad_sig(&SIGNATURE_EXPIRED, format!("Signature age {} < 0 seconds", ops::str_(&age)?), Some(value), Some(d)));
        }
    }
    // TimedSerializer.loads always asks the signer for the timestamp (return_timestamp=True)
    let stamp = date_of(date(ts).ok_or_else(out_of_range)?);
    Ok((load_payload(value)?, stamp))
}

/// `URLSafeSerializerMixin.load_payload`, then the json serializer's
fn load_payload(value: &[u8]) -> R {
    let (compressed, b) = match value.strip_prefix(b".") {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let mut json = b64d(b).ok_or_else(|| bad(&BAD_PAYLOAD, "Could not base64 decode the payload because of an exception"))?;
    if compressed {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(json.as_slice())
            .read_to_end(&mut out)
            .map_err(|_| bad(&BAD_PAYLOAD, "Could not zlib decompress the payload before decoding the payload"))?;
        json = out;
    }
    let unserializing = || bad(&BAD_PAYLOAD, "Could not load the payload because an exception occurred on unserializing the data.");
    let text = String::from_utf8(json).map_err(|_| unserializing())?;
    pyd::loads(&text).map_err(|_| unserializing())
}
