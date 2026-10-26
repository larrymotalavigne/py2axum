//! `redis.asyncio` and `redis` (redis-py 8.1): `from_url(...)` / `Redis(...)` / `Redis(connection_pool=...)` clients
//! and the commands projects use,
//! with redis-py's encoding (str/int/float sent as text, bool/None refused), replies (bytes, or str with
//! `decode_responses=True`) and exceptions. The connection is opened on the first command and re-opened
//! after a failure (`retry=Retry(...)` retries a command that failed to connect). A synchronous client (`redis.Redis`)
//! runs the same commands, its methods return their reply (awaiting it is CPython's TypeError, as for a
//! synchronous `Session`), `scan_iter` is a plain iterator and it has no `aclose()`.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use redis::aio::MultiplexedConnection as ConnectionManager;

use super::ops;
use super::v::*;

pub struct RClient {
    pub(super) url: String,
    pub(super) decode: bool,
    retries: usize,
    /// `redis.Redis` rather than `redis.asyncio.Redis`
    pub sync: bool,
    conn: tokio::sync::Mutex<Option<ConnectionManager>>,
    closed: AtomicBool,
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// options accepted and applied, or accepted with no effect on the replies (connection tuning)
const OPTIONS: &[&str] = &["decode_responses", "encoding", "retry", "retry_on_error", "retry_on_timeout", "health_check_interval",
    "socket_keepalive", "socket_timeout", "socket_connect_timeout", "max_connections", "single_connection_client", "client_name"];

fn check_options(kwargs: &[(String, V)]) -> R<()> {
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
    Ok(())
}

fn client(url: String, kwargs: &[(String, V)], sync: bool) -> R {
    check_options(kwargs)?;
    let decode = kw(kwargs, "decode_responses").map(ops::truthy).transpose()?.unwrap_or(false);
    let retries = match kw(kwargs, "retry") {
        Some(V::Native(n)) => match &**n {
            Native::RRetry(k) => *k,
            _ => 0,
        },
        _ => 0,
    };
    Ok(V::native(Native::Redis(Arc::new(RClient { url, decode, retries, sync, conn: tokio::sync::Mutex::new(None), closed: AtomicBool::new(false) }))))
}

fn url_arg(args: &[V], kwargs: &[(String, V)]) -> R<(String, Vec<(String, V)>)> {
    let url = match (args, kw(kwargs, "url")) {
        ([u], None) | ([], Some(u)) => ops::str_(u)?,
        _ => return Err(Exc::type_error("from_url() missing 1 required positional argument: 'url'")),
    };
    Ok((url, kwargs.iter().filter(|(k, _)| k != "url").cloned().collect()))
}

/// `redis.asyncio.from_url(url, **options)`, `Redis.from_url(...)`, `redis.from_url(...)` (sync: `redis`)
pub fn from_url(args: &[V], kwargs: &[(String, V)], sync: bool) -> R {
    let (url, rest) = url_arg(args, kwargs)?;
    client(url, &rest, sync)
}

/// `ConnectionPool.from_url(url, **options)`: the URL and the options, given to the clients built on it
pub struct RPool {
    url: String,
    options: Vec<(String, V)>,
    sync: bool,
}

pub fn pool_from_url(args: &[V], kwargs: &[(String, V)], sync: bool) -> R {
    let (url, options) = url_arg(args, kwargs)?;
    check_options(&options)?;
    Ok(V::native(Native::RPool(Arc::new(RPool { url, options, sync }))))
}

/// the URL of `host=`, `port=`, `db=`, `username=`, `password=` (and `connection_class=SSLConnection`: rediss), the
/// other options left
fn url_of(kwargs: &[(String, V)]) -> R<(String, Vec<(String, V)>)> {
    let s = |k: &str, d: &str| -> R<String> { kw(kwargs, k).filter(|v| !v.is_none()).map(ops::str_).transpose().map(|x| x.unwrap_or_else(|| d.into())) };
    let (host, port, db) = (s("host", "localhost")?, s("port", "6379")?, s("db", "0")?);
    let auth = match (kw(kwargs, "username").filter(|v| !v.is_none()), kw(kwargs, "password").filter(|v| !v.is_none())) {
        (Some(u), Some(p)) => format!("{}:{}@", ops::str_(u)?, ops::str_(p)?),
        (None, Some(p)) => format!(":{}@", ops::str_(p)?),
        _ => String::new(),
    };
    let scheme = match kw(kwargs, "connection_class") {
        None | Some(V::None) => "redis",
        Some(V::Native(n)) if matches!(&**n, Native::ExtType(t) if t.ends_with("SSLConnection")) => "rediss",
        Some(V::Native(n)) if matches!(&**n, Native::ExtType(t) if t.ends_with(".Connection")) => "redis",
        Some(_) => return Err(Exc::type_error("py2axum: redis connection_class= other than Connection/SSLConnection is not supported")),
    };
    let rest: Vec<(String, V)> = kwargs.iter()
        .filter(|(k, _)| !matches!(k.as_str(), "host" | "port" | "db" | "password" | "username" | "connection_class"))
        .cloned().collect();
    Ok((format!("{scheme}://{auth}{host}:{port}/{db}"), rest))
}

/// `ConnectionPool(host=, port=, db=, password=, ..., connection_class=)` (keyword arguments): the clients built on it
pub fn pool_new(args: &[V], kwargs: &[(String, V)], sync: bool) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: ConnectionPool() takes keyword arguments only"));
    }
    let (url, options) = url_of(kwargs)?;
    check_options(&options)?;
    Ok(V::native(Native::RPool(Arc::new(RPool { url, options, sync }))))
}

/// `redis.asyncio.Redis(host="localhost", port=6379, db=0, password=None, username=None, ...)` (sync: `redis.Redis`);
/// `Redis(connection_pool=pool)`: the pool's URL and options (redis-py ignores the client's other options then)
pub fn new(args: &[V], kwargs: &[(String, V)], sync: bool) -> R {
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: Redis() takes keyword arguments only"));
    }
    if let Some(p) = kw(kwargs, "connection_pool").filter(|v| !v.is_none()) {
        let pool = match p {
            V::Native(n) => match &**n {
                Native::RPool(pool) => pool.clone(),
                _ => return Err(Exc::type_error("py2axum: Redis(connection_pool=) needs a ConnectionPool")),
            },
            _ => return Err(Exc::type_error("py2axum: Redis(connection_pool=) needs a ConnectionPool")),
        };
        if pool.sync != sync {
            return Err(Exc::type_error("py2axum: a redis.asyncio client needs a redis.asyncio ConnectionPool (and a redis client a redis one)"));
        }
        return client(pool.url.clone(), &pool.options, sync);
    }
    let (url, rest) = url_of(kwargs)?;
    client(url, &rest, sync)
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
    if std::ptr::eq(class, &REDIS_RESPONSE_ERROR) {
        if let Some(code) = e.code() {
            return Exc::msg(class, server_msg(code, e.detail()));
        }
    }
    Exc::msg(class, e.to_string())
}

/// redis-py's message of an error reply: `ERR` (and the other codes it maps to a class) dropped, the rest kept
/// (`WRONGTYPE Operation against a key...`)
pub(super) fn server_msg(code: &str, detail: Option<&str>) -> String {
    let detail = detail.unwrap_or("");
    const STRIPPED: &[&str] = &["ERR", "OOM", "WRONGPASS", "EXECABORT", "LOADING", "NOSCRIPT", "READONLY", "NOAUTH", "NOPERM",
        "ASK", "TRYAGAIN", "MOVED", "CLUSTERDOWN", "CROSSSLOT", "MASTERDOWN"];
    if STRIPPED.contains(&code) { detail.to_string() } else if detail.is_empty() { code.to_string() } else { format!("{code} {detail}") }
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
        self.retrying(|mut m| async move { cmd.query_async::<redis::Value>(&mut m).await }).await
    }

    /// the replies of a pipeline, error replies kept in place (`MULTI`/`EXEC` around it for a transaction)
    async fn run_pipe(&self, cmds: &[redis::Cmd], transaction: bool) -> R<Vec<redis::Value>> {
        use redis::aio::ConnectionLike;
        let mut p = redis::pipe();
        if transaction {
            p.atomic();
        }
        for c in cmds {
            p.add_command(c.clone());
        }
        let (offset, count) = if transaction { (cmds.len() + 1, 1) } else { (0, cmds.len()) };
        let p = &p;
        let mut vals = self.retrying(|mut m| async move { m.req_packed_commands(p, offset, count).await }).await?;
        if !transaction {
            return Ok(vals);
        }
        match vals.pop() {
            Some(redis::Value::Array(a)) => Ok(a),
            Some(redis::Value::ServerError(e)) => Err(Exc::msg(&REDIS_RESPONSE_ERROR, server_msg(e.code(), e.details()))),
            _ => Err(Exc::msg(&REDIS_ERROR, "Watched variable changed.")),
        }
    }

    async fn retrying<T, F, Fut>(&self, f: F) -> R<T>
    where
        F: Fn(ConnectionManager) -> Fut,
        Fut: std::future::Future<Output = redis::RedisResult<T>>,
    {
        let mut attempt = 0;
        loop {
            let res = match self.manager().await {
                Ok(m) => f(m).await.map_err(|e| (conn_err(&e), e.is_io_error() || e.is_connection_dropped())),
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
            redis::Value::ServerError(e) => return Err(Exc::msg(&REDIS_RESPONSE_ERROR, server_msg(e.code(), e.details()))),
            other => return Err(Exc::type_error(format!("py2axum: unsupported Redis reply {other:?}"))),
        })
    }
}

/// redis-py's encoder: bytes as is, str as UTF-8, int/float as their repr; other types refused
pub(super) fn arg(v: &V) -> R<Vec<u8>> {
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

/// How redis-py turns a command's reply into its value (its response callbacks, RESP2)
#[derive(Clone, Copy)]
enum Post {
    Plain,
    /// `bool(reply)`: EXPIRE, SETEX, HEXISTS, PERSIST (SISMEMBER stays an int in redis-py 8.1)
    Bool,
    /// `True` for OK: FLUSHDB, LTRIM
    Ok_,
    Ping,
    /// HGETALL: the pairs as a dict
    Dict,
    /// SMEMBERS
    Set,
    /// ZRANGE/ZREVRANGE `withscores=True`: (member, float) tuples
    Scores,
    /// ZSCORE: float or None
    Float,
}

/// A command built from a method call: its words (redis-py's command tuple, for a pipeline's error) and its
/// response callback
pub struct Built {
    words: Vec<Vec<u8>>,
    post: Post,
}

impl Built {
    fn cmd(&self) -> redis::Cmd {
        let mut c = redis::Cmd::new();
        for w in &self.words {
            c.arg(w.as_slice());
        }
        c
    }
}

fn words(name: &str, args: &[V]) -> R<Vec<Vec<u8>>> {
    let mut w = vec![name.as_bytes().to_vec()];
    for a in args {
        w.push(arg(a)?);
    }
    Ok(w)
}

fn need<'a>(args: &'a [V], n: usize, method: &str, names: &[&str]) -> R<&'a [V]> {
    if args.len() < n {
        let missing = &names[args.len()..n];
        let list = match missing {
            [a] => format!("'{a}'"),
            [a, b] => format!("'{a}' and '{b}'"),
            _ => {
                let q: Vec<String> = missing.iter().map(|x| format!("'{x}'")).collect();
                format!("{}, and {}", q[..q.len() - 1].join(", "), q[q.len() - 1])
            }
        };
        return Err(Exc::type_error(format!("{method}() missing {} required positional argument{}: {list}",
            missing.len(), if missing.len() == 1 { "" } else { "s" })));
    }
    if args.len() > names.len() {
        return Err(Exc::type_error(format!("{method}() takes {} positional arguments but {} were given", names.len() + 1, args.len() + 1)));
    }
    Ok(args)
}

/// the keyword arguments each command takes (others: TypeError, as redis-py's signatures)
fn allowed_kwargs(name: &str) -> &'static [&'static str] {
    match name {
        "set" => &["ex", "px", "nx", "xx", "keepttl", "get"],
        "scan_iter" => &["match", "count", "_type"],
        "incr" | "incrby" | "decr" | "decrby" => &["amount"],
        "keys" => &["pattern"],
        "hset" => &["key", "value", "mapping", "items"],
        "zadd" => &["nx", "xx", "ch", "gt", "lt"],
        "zrange" => &["desc", "withscores"],
        "zrevrange" => &["withscores"],
        "pipeline" => &["transaction"],
        "pubsub" => &["ignore_subscribe_messages"],
        "expire" | "pexpire" => &["time", "nx", "xx", "gt", "lt"],
        _ => &[],
    }
}

/// `r.<name>(*args, **kwargs)` as the command redis-py sends
fn build(name: &str, args: &[V], kwargs: &[(String, V)]) -> R<Built> {
    let k = |n: &str| kw(kwargs, n).filter(|v| !v.is_none()).cloned();
    let flag = |n: &str| -> R<bool> { k(n).map(|v| ops::truthy(&v)).transpose().map(|b| b.unwrap_or(false)) };
    let plain = |w| Ok(Built { words: w, post: Post::Plain });
    match name {
        "ping" => {
            need(args, 0, name, &[])?;
            Ok(Built { words: words("PING", &[])?, post: Post::Ping })
        }
        "get" | "ttl" | "llen" | "hlen" | "hgetall" | "hkeys" | "hvals" | "smembers" | "scard" | "zcard" | "type" => {
            let a = need(args, 1, name, &["name"])?;
            let post = match name {
                "hgetall" => Post::Dict,
                "smembers" => Post::Set,
                _ => Post::Plain,
            };
            Ok(Built { words: words(&name.to_ascii_uppercase(), a)?, post })
        }
        "set" => {
            let [key, val] = need(args, 2, name, &["name", "value"])? else { unreachable!() };
            let mut w = words("SET", &[key.clone(), val.clone()])?;
            if let Some(ex) = k("ex") {
                w.push(b"EX".to_vec());
                w.push(seconds(&ex)?.to_string().into_bytes());
            }
            // px: milliseconds, an int or a timedelta (redis-py's `int(td.total_seconds() * 1000)`)
            match k("px") {
                None => {}
                Some(V::Int(px)) => {
                    w.push(b"PX".to_vec());
                    w.push(px.to_string().into_bytes());
                }
                Some(V::Delta(d)) => {
                    w.push(b"PX".to_vec());
                    w.push(super::dt::micros(&d).div_euclid(1000).to_string().into_bytes());
                }
                Some(o) => return Err(Exc::msg(&REDIS_ERROR, format!("px must be datetime.timedelta or int (got {})", o.type_name()))),
            }
            for (opt, word) in [("nx", "NX"), ("xx", "XX"), ("keepttl", "KEEPTTL"), ("get", "GET")] {
                if flag(opt)? {
                    w.push(word.as_bytes().to_vec());
                }
            }
            plain(w)
        }
        "setex" => {
            let [key, t, val] = need(args, 3, name, &["name", "time", "value"])? else { unreachable!() };
            let mut w = words("SETEX", std::slice::from_ref(key))?;
            w.push(seconds(t)?.to_string().into_bytes());
            w.push(arg(val)?);
            Ok(Built { words: w, post: Post::Bool })
        }
        "delete" | "exists" | "unlink" | "mget" => {
            let ks = keys_of(args)?;
            let word = match name { "delete" => "DEL", n => &n.to_ascii_uppercase() };
            plain(words(word, &ks)?)
        }
        "incr" | "incrby" | "decr" | "decrby" => {
            let a = need(args, 1, name, &["name", "amount"])?;
            let amount = a.get(1).cloned().or_else(|| k("amount")).unwrap_or(V::Int(1));
            plain(words(if name.starts_with("incr") { "INCRBY" } else { "DECRBY" }, &[a[0].clone(), amount])?)
        }
        "expire" | "pexpire" => {
            // expire(name, time, nx=False, xx=False, gt=False, lt=False): seconds (pexpire: milliseconds), an int or a
            // timedelta
            let given: Vec<V> = match (args.len(), k("time")) {
                (1, Some(t)) => vec![args[0].clone(), t],
                _ => args.to_vec(),
            };
            let [key, t] = need(&given, 2, name, &["name", "time"])? else { unreachable!() };
            let mut w = words(&name.to_ascii_uppercase(), std::slice::from_ref(key))?;
            let n = match (name, t) {
                ("pexpire", V::Delta(d)) => super::dt::micros(d).div_euclid(1000),
                ("pexpire", V::Int(i)) => *i,
                ("pexpire", o) => return Err(Exc::type_error(format!("py2axum: pexpire() time must be an int or a timedelta, not {}", o.type_name()))),
                _ => seconds(t)?,
            };
            w.push(n.to_string().into_bytes());
            for (opt, word) in [("nx", "NX"), ("xx", "XX"), ("gt", "GT"), ("lt", "LT")] {
                if flag(opt)? {
                    w.push(word.as_bytes().to_vec());
                }
            }
            Ok(Built { words: w, post: Post::Bool })
        }
        "psetex" => {
            // psetex(name, time_ms, value): milliseconds, an int or a timedelta
            let [key, t, val] = need(args, 3, name, &["name", "time_ms", "value"])? else { unreachable!() };
            let ms = match t {
                V::Delta(d) => super::dt::micros(d).div_euclid(1000),
                V::Int(i) => *i,
                o => return Err(Exc::type_error(format!("py2axum: psetex() time_ms must be an int or a timedelta, not {}", o.type_name()))),
            };
            let mut w = words("PSETEX", std::slice::from_ref(key))?;
            w.push(ms.to_string().into_bytes());
            w.push(arg(val)?);
            Ok(Built { words: w, post: Post::Bool })
        }
        "pfadd" => {
            if args.is_empty() {
                need(args, 1, name, &["name"])?;
            }
            plain(words("PFADD", args)?)
        }
        "pfcount" => {
            if args.is_empty() {
                return Err(Exc::msg(&REDIS_RESPONSE_ERROR, "wrong number of arguments for 'pfcount' command"));
            }
            plain(words("PFCOUNT", args)?)
        }
        "eval" => {
            // eval(script, numkeys, *keys_and_args): the reply as is
            if args.len() < 2 {
                need(args, 2, name, &["script", "numkeys"])?;
            }
            plain(words("EVAL", args)?)
        }
        "execute_command" => {
            // the command's words as given; the reply through redis-py's callback of that command (RESP2)
            let Some(first) = args.first() else {
                return Err(Exc::type_error("py2axum: execute_command() needs the command name"));
            };
            let cmd = ops::str_(first)?.to_ascii_uppercase();
            let post = match cmd.as_str() {
                "AUTH" | "COPY" | "EXPIRE" | "EXPIREAT" | "HEXISTS" | "HMSET" | "MOVE" | "MSETNX" | "PERSIST" | "PSETEX" | "PEXPIRE"
                | "PEXPIREAT" | "RENAMENX" | "SETEX" | "SETNX" | "SMOVE" => Post::Bool,
                "ASKING" | "FLUSHALL" | "FLUSHDB" | "LSET" | "LTRIM" | "MSET" | "PFMERGE" | "READONLY" | "READWRITE" | "RENAME" | "SAVE"
                | "SELECT" | "SHUTDOWN" | "SLAVEOF" | "SWAPDB" | "WATCH" | "UNWATCH" => Post::Ok_,
                "PING" => Post::Ping,
                "HGETALL" => Post::Dict,
                "SMEMBERS" | "SDIFF" | "SINTER" | "SUNION" => Post::Set,
                "INCRBYFLOAT" | "HINCRBYFLOAT" | "ZSCORE" => Post::Float,
                _ => Post::Plain,
            };
            let mut w = vec![cmd.into_bytes()];
            for a in &args[1..] {
                w.push(arg(a)?);
            }
            Ok(Built { words: w, post })
        }
        "persist" => Ok(Built { words: words("PERSIST", need(args, 1, name, &["name"])?)?, post: Post::Bool }),
        "keys" => {
            let a = need(args, 0, name, &["pattern"])?;
            plain(words("KEYS", &[a.first().cloned().or_else(|| k("pattern")).unwrap_or(V::str("*"))])?)
        }
        "flushdb" => {
            need(args, 0, name, &[])?;
            Ok(Built { words: words("FLUSHDB", &[])?, post: Post::Ok_ })
        }
        "hget" | "hexists" => {
            let a = need(args, 2, name, &["name", "key"])?;
            Ok(Built { words: words(&name.to_ascii_uppercase(), a)?, post: if name == "hexists" { Post::Bool } else { Post::Plain } })
        }
        "hdel" | "hmget" => {
            // hdel(name, *keys), hmget(name, keys, *args)
            let Some((first, rest)) = args.split_first() else { need(args, 1, name, &["name"])?; unreachable!() };
            let mut items = vec![first.clone()];
            items.extend(if name == "hmget" { keys_of(rest)? } else { rest.to_vec() });
            plain(words(&name.to_ascii_uppercase(), &items)?)
        }
        "hincrby" => {
            let a = need(args, 2, name, &["name", "key", "amount"])?;
            plain(words("HINCRBY", &[a[0].clone(), a[1].clone(), a.get(2).cloned().unwrap_or(V::Int(1))])?)
        }
        "hset" => {
            // hset(name, key=None, value=None, mapping=None, items=None): items, then key/value, then mapping
            let a = need(args, 1, name, &["name", "key", "value", "mapping", "items"])?;
            let at = |i: usize, n: &str| a.get(i).cloned().filter(|v| !v.is_none()).or_else(|| k(n));
            let (key, value, mapping, items) = (at(1, "key"), at(2, "value"), at(3, "mapping"), at(4, "items"));
            let empty = |v: &Option<V>| -> R<bool> { Ok(match v { None => true, Some(x) => !ops::truthy(x)? }) };
            if key.is_none() && empty(&mapping)? && empty(&items)? {
                return Err(Exc::msg(&REDIS_DATA_ERROR, "'hset' with no key value pairs"));
            }
            let mut pieces = vec![a[0].clone()];
            if let Some(it) = items {
                pieces.extend(ops::iter(&it)?);
            }
            if let Some(kk) = key {
                pieces.push(kk);
                pieces.push(value.unwrap_or(V::None));
            }
            if let Some(m) = mapping {
                let V::Dict(d) = &m else { return Err(Exc::type_error(format!("py2axum: hset(mapping=) needs a dict, not {}", m.type_name()))) };
                for (kk, vv) in d.lock().values() {
                    pieces.push(kk.clone());
                    pieces.push(vv.clone());
                }
            }
            plain(words("HSET", &pieces)?)
        }
        "lpush" | "rpush" | "sadd" | "srem" | "zrem" => {
            // name, *values
            if args.len() < 2 {
                return Err(Exc::type_error(format!("py2axum: {name}() needs a name and at least one value")));
            }
            plain(words(&name.to_ascii_uppercase(), args)?)
        }
        "lrange" | "ltrim" => {
            let a = need(args, 3, name, &["name", "start", "end"])?;
            Ok(Built { words: words(&name.to_ascii_uppercase(), a)?, post: if name == "ltrim" { Post::Ok_ } else { Post::Plain } })
        }
        "lpop" | "rpop" => plain(words(&name.to_ascii_uppercase(), need(args, 1, name, &["name"])?)?),
        "sismember" => plain(words("SISMEMBER", need(args, 2, name, &["name", "value"])?)?),
        "zscore" => Ok(Built { words: words("ZSCORE", need(args, 2, name, &["name", "value"])?)?, post: Post::Float }),
        "zremrangebyscore" => plain(words("ZREMRANGEBYSCORE", need(args, 3, name, &["name", "min", "max"])?)?),
        "zremrangebyrank" => plain(words("ZREMRANGEBYRANK", need(args, 3, name, &["name", "min", "max"])?)?),
        "zadd" => {
            // zadd(name, mapping, nx=False, xx=False, ch=False, incr=False, gt=False, lt=False)
            let a = need(args, 2, name, &["name", "mapping"])?;
            let V::Dict(d) = &a[1] else { return Err(Exc::type_error(format!("py2axum: zadd() needs a dict mapping, not {}", a[1].type_name()))) };
            let pairs: Vec<(V, V)> = d.lock().values().cloned().collect();
            if pairs.is_empty() {
                return Err(Exc::msg(&REDIS_DATA_ERROR, "ZADD requires at least one element/score pair"));
            }
            let (nx, xx, gt, lt) = (flag("nx")?, flag("xx")?, flag("gt")?, flag("lt")?);
            if nx && xx {
                return Err(Exc::msg(&REDIS_DATA_ERROR, "ZADD allows either 'nx' or 'xx', not both"));
            }
            if gt && lt {
                return Err(Exc::msg(&REDIS_DATA_ERROR, "ZADD allows either 'gt' or 'lt', not both"));
            }
            if nx && (gt || lt) {
                return Err(Exc::msg(&REDIS_DATA_ERROR, "Only one of 'nx', 'lt', or 'gr' may be defined."));
            }
            let mut w = words("ZADD", &a[..1])?;
            for (opt, word) in [("nx", "NX"), ("xx", "XX"), ("ch", "CH"), ("gt", "GT"), ("lt", "LT")] {
                if flag(opt)? {
                    w.push(word.as_bytes().to_vec());
                }
            }
            for (member, score) in pairs {
                w.push(arg(&score)?);
                w.push(arg(&member)?);
            }
            plain(w)
        }
        "zrange" | "zrevrange" => {
            let a = need(args, 3, name, &["name", "start", "end"])?;
            // zrange(desc=True) is ZREVRANGE (redis-py 8: `ZRANGE ... REV`)
            let desc = name == "zrange" && flag("desc")?;
            let mut w = words(if name == "zrevrange" { "ZREVRANGE" } else { "ZRANGE" }, a)?;
            if desc {
                w.push(b"REV".to_vec());
            }
            let scores = flag("withscores")?;
            if scores {
                w.push(b"WITHSCORES".to_vec());
            }
            Ok(Built { words: w, post: if scores { Post::Scores } else { Post::Plain } })
        }
        "publish" => plain(words("PUBLISH", need(args, 2, name, &["channel", "message"])?)?),
        _ => Err(Exc::attr_error(format!("'Redis' object has no attribute '{name}'"))),
    }
}

fn float_of(v: &V) -> R {
    let s = match v {
        V::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        V::Str(s) => s.to_string(),
        V::Float(f) => return Ok(V::Float(*f)),
        V::None => return Ok(V::None),
        o => return Err(Exc::type_error(format!("py2axum: unexpected Redis score {}", o.type_name()))),
    };
    s.trim().parse::<f64>().map(V::Float).map_err(|_| Exc::value_error(format!("could not convert string to float: {}", ops::repr(&V::str(&s)).unwrap_or_default())))
}

fn post(c: &RClient, b: &Built, raw: redis::Value) -> R {
    let simple_ok = matches!(&raw, redis::Value::Okay) || matches!(&raw, redis::Value::SimpleString(s) if s == "OK");
    let pong = matches!(&raw, redis::Value::SimpleString(s) if s == "PONG");
    let v = c.reply(raw)?;
    Ok(match b.post {
        Post::Plain => v,
        Post::Bool => V::Bool(ops::truthy(&v)?),
        Post::Ok_ => V::Bool(simple_ok),
        Post::Ping => V::Bool(pong || matches!(&v, V::Str(s) if &**s == "PONG") || matches!(&v, V::Bytes(s) if &**s == b"PONG")),
        Post::Dict => {
            let items = match &v { V::List(l) => l.lock().clone(), _ => vec![] };
            let mut out = Vec::new();
            for p in items.chunks(2) {
                if let [a, b] = p {
                    out.push((a.clone(), b.clone()));
                }
            }
            V::dict_from(out)?
        }
        Post::Set => super::methods::set_of(match &v { V::List(l) => l.lock().clone(), _ => vec![] })?,
        Post::Scores => {
            let items = match &v { V::List(l) => l.lock().clone(), _ => vec![] };
            let mut out = Vec::new();
            for p in items.chunks(2) {
                if let [m, s] = p {
                    out.push(V::tuple(vec![m.clone(), float_of(s)?]));
                }
            }
            V::list(out)
        }
        Post::Float => float_of(&v)?,
    })
}

fn check_kwargs(name: &str, kwargs: &[(String, V)], what: &str) -> R<()> {
    let allowed = allowed_kwargs(name);
    if let Some((x, _)) = kwargs.iter().find(|(x, _)| !allowed.contains(&x.as_str())) {
        return Err(Exc::type_error(format!("py2axum: {what}.{name}({x}=) is not supported")));
    }
    Ok(())
}

pub async fn method(c: &Arc<RClient>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    check_kwargs(name, &kwargs, "Redis")?;
    let k = |n: &str| kw(&kwargs, n).filter(|v| !v.is_none()).cloned();
    match name {
        "aclose" if c.sync => return Err(Exc::attr_error("'Redis' object has no attribute 'aclose'")),
        "aclose" | "close" => {
            c.closed.store(true, Ordering::SeqCst);
            *c.conn.lock().await = None;
            return Ok(V::None);
        }
        "pubsub" => return super::rps::new(c, &args, &kwargs),
        "pipeline" => {
            // pipeline(transaction=True, shard_hint=None)
            if args.len() > 1 {
                return Err(Exc::type_error("py2axum: Redis.pipeline() takes transaction= only"));
            }
            let transaction = match args.first().cloned().or_else(|| k("transaction")) {
                Some(v) => ops::truthy(&v)?,
                None => true,
            };
            return Ok(V::native(Native::RPipe(Arc::new(RPipe { client: c.clone(), transaction, stack: parking_lot::Mutex::new(Vec::new()) }))));
        }
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
            if c.sync {
                return Ok(V::native(Native::Iter(parking_lot::Mutex::new(out.into()))));
            }
            return Ok(V::native(Native::AsyncItems(parking_lot::Mutex::new(Some(out)))));
        }
        _ => {}
    }
    // `delete()` of no key: redis-py sends nothing
    if name == "delete" && keys_of(&args)?.is_empty() {
        return Ok(V::Int(0));
    }
    let b = build(name, &args, &kwargs)?;
    let raw = c.run(&b.cmd()).await?;
    post(c, &b, raw)
}

/// `r.pipeline(transaction=True)`: the commands queued (each call returns the pipeline), sent by `execute()`
pub struct RPipe {
    pub client: Arc<RClient>,
    transaction: bool,
    stack: parking_lot::Mutex<Vec<Built>>,
}

/// redis-py's `truncate_text`: `textwrap.shorten(txt, 100, placeholder="...", break_long_words=True)`
fn shorten(txt: &str) -> String {
    let words: Vec<&str> = txt.split_whitespace().collect();
    let joined = words.join(" ");
    if joined.chars().count() <= 100 {
        return joined;
    }
    // the words that fit with the placeholder (" ..." after a word, a word cut when it alone is too long)
    let mut out = String::new();
    for w in &words {
        let next = if out.is_empty() { w.to_string() } else { format!("{out} {w}") };
        if next.chars().count() + 4 > 100 {
            if out.is_empty() {
                out = w.chars().take(96).collect();
            }
            break;
        }
        out = next;
    }
    format!("{out} ...")
}

pub async fn pipe_method(p: &Arc<RPipe>, recv: &V, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    match name {
        "execute" => {
            // execute(raise_on_error=True)
            if !args.is_empty() || !kwargs.is_empty() {
                return Err(Exc::type_error("py2axum: Pipeline.execute() takes no argument here"));
            }
            let stack = std::mem::take(&mut *p.stack.lock());
            if stack.is_empty() {
                return Ok(V::list(vec![]));
            }
            let cmds: Vec<redis::Cmd> = stack.iter().map(|b| b.cmd()).collect();
            let replies = p.client.run_pipe(&cmds, p.transaction).await?;
            // the first error reply raised, annotated with its command (the others' effects stay)
            for (i, (r, b)) in replies.iter().zip(&stack).enumerate() {
                if let redis::Value::ServerError(e) = r {
                    let msg = server_msg(e.code(), e.details());
                    let cmd: Vec<String> = b.words.iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect();
                    // the async pipeline formats the exception's args tuple, the sync one its message
                    let shown = if p.client.sync { msg } else { format!("({},)", ops::repr(&V::str(&msg))?) };
                    return Err(Exc::msg(&REDIS_RESPONSE_ERROR, format!(
                        "Command # {} ({}) of pipeline caused error: {shown}", i + 1, shorten(&cmd.join(" ")))));
                }
            }
            let mut out = Vec::new();
            for (r, b) in replies.into_iter().zip(&stack) {
                out.push(post(&p.client, b, r)?);
            }
            Ok(V::list(out))
        }
        "reset" => {
            p.stack.lock().clear();
            Ok(V::None)
        }
        "aclose" if p.client.sync => Err(Exc::attr_error("'Pipeline' object has no attribute 'aclose'")),
        "aclose" | "close" => {
            p.stack.lock().clear();
            Ok(V::None)
        }
        "pipeline" | "scan_iter" | "multi" | "watch" => Err(Exc::type_error(format!("py2axum: Pipeline.{name}() is not supported"))),
        _ => {
            check_kwargs(name, &kwargs, "Pipeline")?;
            if name == "delete" && keys_of(&args)?.is_empty() {
                return Err(Exc::type_error("py2axum: Pipeline.delete() of no key is not supported"));
            }
            let b = build(name, &args, &kwargs)?;
            p.stack.lock().push(b);
            Ok(recv.clone())
        }
    }
}
