//! `redis.asyncio` (redis-py 5+): `from_url(...)` / `Redis(...)` clients and the commands projects use,
//! with redis-py's encoding (str/int/float sent as text, bool/None refused), replies (bytes, or str with
//! `decode_responses=True`) and exceptions. The connection is opened on the first command and re-opened
//! after a failure (`retry=Retry(...)` retries a command that failed to connect).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use redis::aio::MultiplexedConnection as ConnectionManager;

use super::ops;
use super::v::*;

pub struct RClient {
    url: String,
    decode: bool,
    retries: usize,
    conn: tokio::sync::Mutex<Option<ConnectionManager>>,
    closed: AtomicBool,
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// options accepted and applied, or accepted with no effect on the replies (connection tuning)
const OPTIONS: &[&str] = &["decode_responses", "encoding", "retry", "retry_on_error", "retry_on_timeout", "health_check_interval",
    "socket_keepalive", "socket_timeout", "socket_connect_timeout", "max_connections", "single_connection_client", "client_name"];

fn client(url: String, kwargs: &[(String, V)]) -> R {
    for (k, _) in kwargs {
        if !OPTIONS.contains(&k.as_str()) {
            return Err(Exc::type_error(format!("py2axum: redis client option {k}= is not supported")));
        }
    }
    if let Some(e) = kw(kwargs, "encoding") {
        if !matches!(e, V::Str(s) if s.eq_ignore_ascii_case("utf-8") || s.eq_ignore_ascii_case("utf8")) {
            return Err(Exc::type_error("py2axum: redis encoding other than utf-8 is not supported"));
        }
    }
    let decode = kw(kwargs, "decode_responses").map(ops::truthy).transpose()?.unwrap_or(false);
    let retries = match kw(kwargs, "retry") {
        Some(V::Native(n)) => match &**n {
            Native::RRetry(k) => *k,
            _ => 0,
        },
        _ => 0,
    };
    Ok(V::native(Native::Redis(Arc::new(RClient { url, decode, retries, conn: tokio::sync::Mutex::new(None), closed: AtomicBool::new(false) }))))
}

/// `redis.asyncio.from_url(url, **options)`
pub fn from_url(args: &[V], kwargs: &[(String, V)]) -> R {
    let url = match (args, kw(kwargs, "url")) {
        ([u], None) | ([], Some(u)) => ops::str_(u)?,
        _ => return Err(Exc::type_error("from_url() missing 1 required positional argument: 'url'")),
    };
    let rest: Vec<(String, V)> = kwargs.iter().filter(|(k, _)| k != "url").cloned().collect();
    client(url, &rest)
}

/// `redis.asyncio.Redis(host="localhost", port=6379, db=0, password=None, username=None, ...)`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: Redis() takes keyword arguments only"));
    }
    let s = |k: &str, d: &str| -> R<String> { kw(kwargs, k).filter(|v| !v.is_none()).map(ops::str_).transpose().map(|x| x.unwrap_or_else(|| d.into())) };
    let (host, port, db) = (s("host", "localhost")?, s("port", "6379")?, s("db", "0")?);
    let auth = match (kw(kwargs, "username").filter(|v| !v.is_none()), kw(kwargs, "password").filter(|v| !v.is_none())) {
        (Some(u), Some(p)) => format!("{}:{}@", ops::str_(u)?, ops::str_(p)?),
        (None, Some(p)) => format!(":{}@", ops::str_(p)?),
        _ => String::new(),
    };
    let rest: Vec<(String, V)> =
        kwargs.iter().filter(|(k, _)| !matches!(k.as_str(), "host" | "port" | "db" | "password" | "username")).cloned().collect();
    client(format!("redis://{auth}{host}:{port}/{db}"), &rest)
}

/// `Retry(backoff, retries)` (`redis.asyncio.retry`): the number of retries is kept
pub fn retry(args: &[V], kwargs: &[(String, V)]) -> R {
    let n = match (args.get(1), kw(kwargs, "retries")) {
        (Some(V::Int(i)), _) | (None, Some(V::Int(i))) => (*i).max(0) as usize,
        _ => return Err(Exc::type_error("py2axum: Retry(backoff, retries) needs an int retries")),
    };
    Ok(V::native(Native::RRetry(n)))
}

fn conn_err(e: &redis::RedisError) -> Exc {
    let class = if e.is_timeout() {
        &REDIS_TIMEOUT_ERROR
    } else if e.is_io_error() || e.is_connection_refusal() || e.is_connection_dropped() {
        &REDIS_CONNECTION_ERROR
    } else if matches!(e.kind(), redis::ErrorKind::ResponseError | redis::ErrorKind::ExtensionError | redis::ErrorKind::TypeError) {
        &REDIS_RESPONSE_ERROR
    } else {
        &REDIS_ERROR
    };
    Exc::msg(class, e.to_string())
}

impl RClient {
    async fn manager(&self) -> R<ConnectionManager> {
        let mut g = self.conn.lock().await;
        if let Some(m) = g.as_ref() {
            return Ok(m.clone());
        }
        let c = redis::Client::open(self.url.as_str()).map_err(|e| Exc::value_error(format!("Redis URL: {e}")))?;
        // fails fast when the server is down (redis-py too); reopened after a connection error
        let m = c.get_multiplexed_async_connection().await.map_err(|e| conn_err(&e))?;
        *g = Some(m.clone());
        Ok(m)
    }

    async fn run(&self, cmd: &redis::Cmd) -> R<redis::Value> {
        let mut attempt = 0;
        loop {
            let res = match self.manager().await {
                Ok(mut m) => cmd.query_async::<redis::Value>(&mut m).await.map_err(|e| (conn_err(&e), e.is_io_error() || e.is_connection_dropped())),
                Err(e) => Err((e, true)),
            };
            match res {
                Ok(v) => return Ok(v),
                Err((e, broken)) => {
                    if broken {
                        *self.conn.lock().await = None; // reopened by the next command
                    }
                    if !broken || attempt >= self.retries {
                        return Err(e);
                    }
                    attempt += 1;
                }
            }
        }
    }

    fn reply(&self, v: redis::Value) -> R {
        Ok(match v {
            redis::Value::Nil => V::None,
            redis::Value::Int(i) => V::Int(i),
            redis::Value::BulkString(b) => {
                if self.decode {
                    V::str(String::from_utf8(b).map_err(|_| Exc::msg(&UNICODE_DECODE_ERROR, "'utf-8' codec can't decode a Redis reply"))?)
                } else {
                    V::Bytes(Arc::from(b.as_slice()))
                }
            }
            redis::Value::Array(items) => V::list(items.into_iter().map(|x| self.reply(x)).collect::<R<Vec<_>>>()?),
            redis::Value::SimpleString(s) => {
                if s == "OK" {
                    V::Bool(true)
                } else if self.decode {
                    V::str(s)
                } else {
                    V::Bytes(Arc::from(s.as_bytes()))
                }
            }
            redis::Value::Okay => V::Bool(true),
            redis::Value::Double(f) => V::Float(f),
            redis::Value::Boolean(b) => V::Bool(b),
            redis::Value::Map(kv) => {
                let mut out = Vec::new();
                for (k, x) in kv {
                    out.push((self.reply(k)?, self.reply(x)?));
                }
                V::dict_from(out)?
            }
            redis::Value::ServerError(e) => return Err(Exc::msg(&REDIS_RESPONSE_ERROR, format!("{e:?}"))),
            other => return Err(Exc::type_error(format!("py2axum: unsupported Redis reply {other:?}"))),
        })
    }
}

/// redis-py's encoder: bytes as is, str as UTF-8, int/float as their repr; other types refused
fn arg(v: &V) -> R<Vec<u8>> {
    Ok(match v {
        V::Bytes(b) => b.to_vec(),
        V::Str(s) => s.as_bytes().to_vec(),
        V::Int(i) => i.to_string().into_bytes(),
        V::Float(f) => ops::repr(&V::Float(*f))?.into_bytes(),
        V::Decimal(_) => ops::str_(v)?.into_bytes(),
        other => {
            return Err(Exc::msg(&REDIS_DATA_ERROR, format!(
                "Invalid input of type: '{}'. Convert to a bytes, string, int or float first.",
                match other { V::None => "NoneType", o => o.type_name() })))
        }
    })
}

fn keys_of(args: &[V]) -> R<Vec<V>> {
    // mget(keys) / delete(*keys): one list argument or several keys
    match args {
        [V::List(l)] => Ok(l.lock().clone()),
        [V::Tuple(t)] => Ok(t.to_vec()),
        _ => Ok(args.to_vec()),
    }
}

fn seconds(v: &V) -> R<i64> {
    match v {
        V::Int(i) => Ok(*i),
        V::Delta(d) => Ok(super::dt::micros(d) / 1_000_000),
        V::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
        other => Err(Exc::msg(&REDIS_DATA_ERROR, format!("ex must be an int or timedelta, not {}", other.type_name()))),
    }
}

pub async fn method(c: &Arc<RClient>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let k = |n: &str| kw(&kwargs, n).filter(|v| !v.is_none()).cloned();
    let allowed: &[&str] = match name {
        "set" => &["ex", "px", "nx", "xx", "keepttl", "get"],
        "scan_iter" => &["match", "count", "_type"],
        "incr" | "incrby" | "decr" | "decrby" => &["amount"],
        "keys" => &["pattern"],
        _ => &[],
    };
    if let Some((x, _)) = kwargs.iter().find(|(x, _)| !allowed.contains(&x.as_str())) {
        return Err(Exc::type_error(format!("py2axum: Redis.{name}({x}=) is not supported")));
    }
    let mut cmd;
    match name {
        "aclose" | "close" => {
            c.closed.store(true, Ordering::SeqCst);
            *c.conn.lock().await = None;
            return Ok(V::None);
        }
        "ping" => cmd = redis::cmd("PING"),
        "get" => {
            cmd = redis::cmd("GET");
            cmd.arg(arg(&args[0])?);
        }
        "set" => {
            let [key, val] = &args[..] else { return Err(Exc::type_error("set() missing required arguments: 'name' and 'value'")) };
            cmd = redis::cmd("SET");
            cmd.arg(arg(key)?).arg(arg(val)?);
            if let Some(ex) = k("ex") {
                cmd.arg("EX").arg(seconds(&ex)?);
            }
            // px: milliseconds, an int or a timedelta (redis-py's `int(td.total_seconds() * 1000)`)
            match k("px") {
                None | Some(V::None) => {}
                Some(V::Int(px)) => {
                    cmd.arg("PX").arg(px);
                }
                Some(V::Delta(d)) => {
                    cmd.arg("PX").arg(super::dt::micros(&d).div_euclid(1000));
                }
                Some(o) => return Err(Exc::msg(&REDIS_ERROR, format!("px must be datetime.timedelta or int (got {})", o.type_name()))),
            }
            for (opt, word) in [("nx", "NX"), ("xx", "XX"), ("keepttl", "KEEPTTL"), ("get", "GET")] {
                if let Some(v) = k(opt) {
                    if ops::truthy(&v)? {
                        cmd.arg(word);
                    }
                }
            }
        }
        "setex" => {
            let [key, t, val] = &args[..] else { return Err(Exc::type_error("setex() takes name, time and value")) };
            cmd = redis::cmd("SETEX");
            cmd.arg(arg(key)?).arg(seconds(t)?).arg(arg(val)?);
        }
        "delete" | "exists" | "unlink" => {
            let ks = keys_of(&args)?;
            if ks.is_empty() && name == "delete" {
                return Ok(V::Int(0));
            }
            cmd = redis::cmd(&name.to_ascii_uppercase().replace("DELETE", "DEL"));
            for x in ks {
                cmd.arg(arg(&x)?);
            }
        }
        "mget" => {
            cmd = redis::cmd("MGET");
            for x in keys_of(&args)? {
                cmd.arg(arg(&x)?);
            }
        }
        "incr" | "incrby" | "decr" | "decrby" => {
            let amount = args.get(1).cloned().or_else(|| k("amount")).unwrap_or(V::Int(1));
            cmd = redis::cmd(if name.starts_with("incr") { "INCRBY" } else { "DECRBY" });
            cmd.arg(arg(&args[0])?).arg(arg(&amount)?);
        }
        "expire" => {
            cmd = redis::cmd("EXPIRE");
            cmd.arg(arg(&args[0])?).arg(seconds(&args[1])?);
        }
        "ttl" => {
            cmd = redis::cmd("TTL");
            cmd.arg(arg(&args[0])?);
        }
        "keys" => {
            cmd = redis::cmd("KEYS");
            cmd.arg(arg(&args.first().cloned().or_else(|| k("pattern")).unwrap_or(V::str("*")))?);
        }
        "hget" | "hdel" | "hgetall" | "hset" => {
            cmd = redis::cmd(&name.to_ascii_uppercase());
            for x in &args {
                cmd.arg(arg(x)?);
            }
        }
        "flushdb" => cmd = redis::cmd("FLUSHDB"),
        "scan_iter" => {
            // the whole SCAN, collected (an `async for` over it reads them in order)
            // scan_iter(match=None, count=None, _type=None), positional or keyword
            if args.len() > 3 {
                return Err(Exc::type_error(format!("scan_iter() takes from 1 to 4 positional arguments but {} were given", args.len() + 1)));
            }
            let at = |i: usize, name: &str| args.get(i).cloned().or_else(|| k(name)).filter(|v| !v.is_none());
            let (pattern, count, kind) = (at(0, "match"), at(1, "count"), at(2, "_type"));
            let mut cursor: u64 = 0;
            let mut out = Vec::new();
            loop {
                let mut sc = redis::cmd("SCAN");
                sc.arg(cursor);
                if let Some(p) = &pattern {
                    sc.arg("MATCH").arg(arg(p)?);
                }
                if let Some(n) = &count {
                    sc.arg("COUNT").arg(arg(n)?);
                }
                if let Some(t) = &kind {
                    sc.arg("TYPE").arg(arg(t)?);
                }
                let reply = c.run(&sc).await?;
                let redis::Value::Array(mut parts) = reply else { return Err(Exc::msg(&REDIS_ERROR, "bad SCAN reply")) };
                if parts.len() != 2 {
                    return Err(Exc::msg(&REDIS_ERROR, "bad SCAN reply"));
                }
                let keys = parts.pop().unwrap();
                let next = match parts.pop().unwrap() {
                    redis::Value::BulkString(b) => String::from_utf8_lossy(&b).parse().unwrap_or(0),
                    _ => 0,
                };
                if let V::List(l) = c.reply(keys)? {
                    out.extend(l.lock().iter().cloned());
                }
                cursor = next;
                if cursor == 0 {
                    break;
                }
            }
            return Ok(V::native(Native::AsyncItems(parking_lot::Mutex::new(Some(out)))));
        }
        _ => return Err(Exc::attr_error(format!("'Redis' object has no attribute '{name}'"))),
    }
    let v = c.run(&cmd).await?;
    let out = c.reply(v)?;
    Ok(match (name, &out) {
        ("set", V::None) | ("set", V::Bool(_)) => out,
        ("expire", V::Int(i)) => V::Bool(*i == 1),
        ("ping", _) => V::Bool(true),
        _ => out,
    })
}
