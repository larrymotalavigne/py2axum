//! starlette-context 0.5.1: `RawContextMiddleware(app, plugins=(RequestIdPlugin(), CorrelationIdPlugin()))`.
//! Each plugin reads its header (`request.headers.get(key)`); empty or absent (or `force_new_uuid`) → a new
//! `uuid4().hex`; with `validate` (the default) the value must parse as `uuid.UUID(value)`, else the
//! middleware answers itself with the default error response (`Response(status_code=400)`: empty body,
//! `content-length: 0`). On the way back, each plugin appends `key: value` to `http.response.start`
//! (`MutableHeaders.append`, lower-cased name), in plugin order. `context` itself is not readable from the
//! translated code (the library's `context` object is not mapped).
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue};
use axum::response::Response;

use super::v::*;
use super::Cx;

pub struct Plugin {
    pub key: &'static str,
    pub force_new_uuid: bool,
    pub validate: bool,
}

pub struct RawContext {
    pub plugins: Vec<Plugin>,
}

/// `uuid.UUID(s)` accepts it (version= only overwrites bits): `urn:`/`uuid:` removed, `{}` stripped at
/// both ends, `-` removed, 32 characters left, `int(h, 16)` in 0..2**128
pub fn valid_uuid(s: &str) -> bool {
    let h = s.replace("urn:", "").replace("uuid:", "");
    let h = h.trim_matches(|c| c == '{' || c == '}').replace('-', "");
    if h.chars().count() != 32 {
        return false;
    }
    // int(h, 16): surrounding whitespace, a sign, an optional 0x prefix, single underscores between digits
    let t = h.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    let (neg, t) = match t.as_bytes().first() {
        Some(b'+') => (false, &t[1..]),
        Some(b'-') => (true, &t[1..]),
        _ => (false, t),
    };
    let (t, prefixed) = if t.len() >= 2 && (t.starts_with("0x") || t.starts_with("0X")) { (&t[2..], true) } else { (t, false) };
    let t = if prefixed { t.strip_prefix('_').unwrap_or(t) } else { t };
    if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return false;
    }
    let digits: String = t.chars().filter(|c| *c != '_').collect();
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    // at most 32 hex digits: always under 2**128
    let Ok(v) = u128::from_str_radix(&digits, 16) else { return false };
    !neg || v == 0
}

fn new_uuid_hex() -> String {
    match super::pathio::uuid4() {
        V::Native(n) => match &*n {
            Native::Uuid(u) => super::pathio::uuid_hex(*u),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

impl RawContext {
    pub async fn call<F>(&self, cx: &Cx, next: F) -> R<Response>
    where
        F: std::future::Future<Output = R<Response>>,
    {
        let mut values = Vec::with_capacity(self.plugins.len());
        for p in &self.plugins {
            let found = if p.force_new_uuid { None } else { cx.req.header(p.key) };
            let v = match found {
                Some(v) if !v.is_empty() => v,
                _ => new_uuid_hex(),
            };
            if p.validate && !valid_uuid(&v) {
                // MiddleWareValidationError: the default error response, sent by the middleware itself
                return Ok(Response::builder().status(400).header("content-length", "0").body(Body::empty()).unwrap());
            }
            values.push((p.key.to_ascii_lowercase(), v));
        }
        let mut r = next.await?;
        for (k, v) in values {
            let k = HeaderName::from_bytes(k.as_bytes()).map_err(|e| Exc::value_error(e.to_string()))?;
            let v = HeaderValue::from_str(&v).map_err(|e| Exc::value_error(e.to_string()))?;
            r.headers_mut().append(k, v);
        }
        Ok(r)
    }
}
