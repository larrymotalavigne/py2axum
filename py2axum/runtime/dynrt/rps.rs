//! `redis.asyncio` publish/subscribe: `r.pubsub(ignore_subscribe_messages=False)`, `subscribe` / `psubscribe` /
//! `unsubscribe` / `punsubscribe`, `get_message(ignore_subscribe_messages=False, timeout=0.0)`, `listen()` and
//! `aclose()` (`async with r.pubsub() as ps`), as redis-py 8 does them: the commands are sent without reading their
//! replies, and every reply the server pushes (the `subscribe` confirmations included) is a message dict
//! `{"type", "pattern", "channel", "data"}`, read in order by `get_message` / `listen`. The pubsub has its own
//! connection (RESP2), opened by the first command. Message handlers (`subscribe(chan=handler)`), `run()` and the
//! sharded commands are refused.
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use super::ops;
use super::rds::{self, RClient};
use super::v::*;

pub struct RPubSub {
    client: Arc<RClient>,
    ignore_subscribe: bool,
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
    /// the read half and the bytes read but not parsed yet (a read cut by a timeout loses nothing)
    reader: tokio::sync::Mutex<Option<(OwnedReadHalf, Vec<u8>)>>,
    subs: parking_lot::Mutex<Subs>,
}

#[derive(Default)]
struct Subs {
    channels: Vec<Vec<u8>>,
    patterns: Vec<Vec<u8>>,
    pending_channels: Vec<Vec<u8>>,
    pending_patterns: Vec<Vec<u8>>,
}

/// `r.pubsub(**kwargs)`
pub fn new(c: &Arc<RClient>, args: &[V], kwargs: &[(String, V)]) -> R {
    if c.sync {
        return Err(Exc::type_error("py2axum: pubsub() of a synchronous redis client is not supported (use redis.asyncio)"));
    }
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: Redis.pubsub() takes keyword arguments only"));
    }
    let mut ignore_subscribe = false;
    for (k, v) in kwargs {
        match k.as_str() {
            "ignore_subscribe_messages" => ignore_subscribe = ops::truthy(v)?,
            _ => return Err(Exc::type_error(format!("py2axum: Redis.pubsub({k}=) is not supported"))),
        }
    }
    Ok(V::native(Native::RPubSub(Arc::new(RPubSub {
        client: c.clone(),
        ignore_subscribe,
        writer: tokio::sync::Mutex::new(None),
        reader: tokio::sync::Mutex::new(None),
        subs: parking_lot::Mutex::new(Subs::default()),
    }))))
}

/// a RESP2 reply
enum Resp {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Resp>>),
}

/// one complete reply at the start of `buf` and its length, or None while incomplete
fn parse(buf: &[u8]) -> R<Option<(Resp, usize)>> {
    let Some(end) = buf.windows(2).position(|w| w == b"\r\n") else { return Ok(None) };
    if buf.is_empty() {
        return Ok(None);
    }
    let line = std::str::from_utf8(&buf[1..end]).map_err(|_| Exc::msg(&REDIS_ERROR, "Protocol Error: bad reply"))?;
    let num = || line.parse::<i64>().map_err(|_| Exc::msg(&REDIS_ERROR, format!("Protocol Error: {line}")));
    let next = end + 2;
    Ok(Some(match buf[0] {
        b'+' => (Resp::Simple(line.to_string()), next),
        b'-' => (Resp::Error(line.to_string()), next),
        b':' => (Resp::Int(num()?), next),
        b'$' => {
            let n = num()?;
            if n < 0 {
                (Resp::Bulk(None), next)
            } else {
                let n = n as usize;
                if buf.len() < next + n + 2 {
                    return Ok(None);
                }
                (Resp::Bulk(Some(buf[next..next + n].to_vec())), next + n + 2)
            }
        }
        b'*' => {
            let n = num()?;
            if n < 0 {
                (Resp::Array(None), next)
            } else {
                let mut items = Vec::with_capacity(n as usize);
                let mut at = next;
                for _ in 0..n {
                    match parse(&buf[at..])? {
                        Some((r, used)) => {
                            items.push(r);
                            at += used;
                        }
                        None => return Ok(None),
                    }
                }
                (Resp::Array(Some(items)), at)
            }
        }
        c => return Err(Exc::msg(&REDIS_ERROR, format!("Protocol Error: {}", c as char))),
    }))
}

fn encode(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// redis-py's message of a failed connect: `Error 111 connecting to localhost:6379. Connection refused.`
fn connect_err(host: &str, port: u16, e: &std::io::Error) -> Exc {
    let text = e.to_string();
    let text = match text.find(" (os error") {
        Some(i) => text[..i].to_string(),
        None => text,
    };
    match e.raw_os_error() {
        Some(n) => Exc::msg(&REDIS_CONNECTION_ERROR, format!("Error {n} connecting to {host}:{port}. {text}.")),
        None => Exc::msg(&REDIS_CONNECTION_ERROR, format!("Error connecting to {host}:{port}. {text}.")),
    }
}

impl RPubSub {
    fn value(&self, r: Resp) -> R {
        Ok(match r {
            Resp::Simple(s) => self.text(s.into_bytes())?,
            Resp::Error(e) => {
                let (code, detail) = e.split_once(' ').unwrap_or((e.as_str(), ""));
                return Err(Exc::msg(&REDIS_RESPONSE_ERROR, rds::server_msg(code, Some(detail))));
            }
            Resp::Int(i) => V::Int(i),
            Resp::Bulk(None) | Resp::Array(None) => V::None,
            Resp::Bulk(Some(b)) => self.text(b)?,
            Resp::Array(Some(items)) => V::list(items.into_iter().map(|x| self.value(x)).collect::<R<Vec<_>>>()?),
        })
    }

    /// bytes, or str with `decode_responses=True`
    fn text(&self, b: Vec<u8>) -> R {
        if self.client.decode {
            Ok(V::str(String::from_utf8(b).map_err(|_| Exc::msg(&UNICODE_DECODE_ERROR, "'utf-8' codec can't decode a Redis reply"))?))
        } else {
            Ok(V::Bytes(Arc::from(b.as_slice())))
        }
    }

    /// the command sent, its reply left to `get_message` / `listen`; the connection opened (and authenticated) first
    async fn execute(&self, parts: Vec<Vec<u8>>) -> R<()> {
        let mut w = self.writer.lock().await;
        if w.is_none() {
            let info = redis::Client::open(self.client.url.as_str())
                .map_err(|e| Exc::value_error(format!("Redis URL: {e}")))?
                .get_connection_info()
                .clone();
            let redis::ConnectionAddr::Tcp(host, port) = &info.addr else {
                return Err(Exc::type_error("py2axum: redis pubsub over TLS or a unix socket is not supported"));
            };
            let stream = tokio::net::TcpStream::connect((host.as_str(), *port)).await.map_err(|e| connect_err(host, *port, &e))?;
            let (mut rd, mut wr) = stream.into_split();
            let mut buf = Vec::new();
            if let Some(pw) = &info.redis.password {
                let mut auth = vec![b"AUTH".to_vec()];
                if let Some(u) = &info.redis.username {
                    auth.push(u.as_bytes().to_vec());
                }
                auth.push(pw.as_bytes().to_vec());
                wr.write_all(&encode(&auth)).await.map_err(|e| connect_err(host, *port, &e))?;
                let reply = read_reply(&mut rd, &mut buf).await?;
                if let Resp::Error(_) = reply {
                    self.value(reply)?;
                }
            }
            *self.reader.lock().await = Some((rd, buf));
            *w = Some(wr);
        }
        let wr = w.as_mut().unwrap();
        if let Err(e) = wr.write_all(&encode(&parts)).await {
            *w = None;
            *self.reader.lock().await = None;
            return Err(Exc::msg(&REDIS_CONNECTION_ERROR, format!("Error while writing to socket. {e}.")));
        }
        Ok(())
    }

    /// the next reply: waits for one (`timeout` None), at most `timeout` seconds, or None
    async fn read(&self, timeout: Option<f64>) -> R<Option<Resp>> {
        let mut g = self.reader.lock().await;
        let Some((rd, buf)) = g.as_mut() else {
            return Err(Exc::msg(&RUNTIME_ERROR, "pubsub connection not set: did you forget to call subscribe() or psubscribe()?"));
        };
        match timeout {
            None => read_reply(rd, buf).await.map(Some),
            Some(t) => match tokio::time::timeout(Duration::from_secs_f64(t.max(0.0)), read_reply(rd, buf)).await {
                Ok(r) => r.map(Some),
                Err(_) => Ok(None),
            },
        }
    }

    /// redis-py's `handle_message`: the message dict, or None for a (un)subscribe confirmation ignored
    fn handle(&self, r: Resp, ignore: bool) -> R<Option<V>> {
        let parts = match self.value(r)? {
            V::List(l) => l.lock().clone(),
            other => vec![other],
        };
        let kind = match parts.first() {
            Some(V::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
            Some(V::Str(s)) => s.to_string(),
            _ => return Err(Exc::msg(&REDIS_ERROR, "py2axum: unexpected pubsub reply")),
        };
        let at = |i: usize| parts.get(i).cloned().unwrap_or(V::None);
        let (pattern, channel, data) = if kind == "pmessage" { (at(1), at(2), at(3)) } else { (V::None, at(1), at(2)) };
        let raw = |v: &V| -> Vec<u8> {
            match v {
                V::Bytes(b) => b.to_vec(),
                V::Str(s) => s.as_bytes().to_vec(),
                _ => Vec::new(),
            }
        };
        if kind == "unsubscribe" || kind == "punsubscribe" {
            let name = raw(&channel);
            let mut s = self.subs.lock();
            let s = &mut *s;
            let (pending, subscribed) =
                if kind == "unsubscribe" { (&mut s.pending_channels, &mut s.channels) } else { (&mut s.pending_patterns, &mut s.patterns) };
            if let Some(i) = pending.iter().position(|x| *x == name) {
                pending.remove(i);
                subscribed.retain(|x| *x != name);
            }
        }
        if !matches!(kind.as_str(), "message" | "pmessage") && (ignore || self.ignore_subscribe) {
            return Ok(None);
        }
        Ok(Some(V::dict_from(vec![
            (V::str("type"), V::str(kind)),
            (V::str("pattern"), pattern),
            (V::str("channel"), channel),
            (V::str("data"), data),
        ])?))
    }

    pub fn subscribed(&self) -> bool {
        let s = self.subs.lock();
        !s.channels.is_empty() || !s.patterns.is_empty()
    }

    async fn close(&self) {
        *self.writer.lock().await = None;
        *self.reader.lock().await = None;
        *self.subs.lock() = Subs::default();
    }

    /// `listen()`'s next message: None once nothing is subscribed
    pub async fn listen_next(&self) -> R<Option<V>> {
        while self.subscribed() {
            let Some(r) = self.read(None).await? else { continue };
            if let Some(m) = self.handle(r, false)? {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }
}

async fn read_reply(rd: &mut OwnedReadHalf, buf: &mut Vec<u8>) -> R<Resp> {
    loop {
        if let Some((r, used)) = parse(buf)? {
            buf.drain(..used);
            return Ok(r);
        }
        // `read_buf` is cancel safe: the bytes read are in `buf` when a timeout drops this future
        let n = rd.read_buf(buf).await.map_err(|e| Exc::msg(&REDIS_CONNECTION_ERROR, format!("Error while reading from socket: {e}")))?;
        if n == 0 {
            return Err(Exc::msg(&REDIS_CONNECTION_ERROR, "Connection closed by server."));
        }
    }
}

/// `list_or_args(args[0], args[1:])`: the names, a first list or tuple spread
fn names(args: &[V]) -> R<Vec<Vec<u8>>> {
    let mut items = Vec::new();
    if let Some(first) = args.first() {
        match first {
            V::List(_) | V::Tuple(_) | V::Set(_) | V::Frozen(_) => items.extend(ops::iter(first)?),
            other => items.push(other.clone()),
        }
        items.extend(args[1..].iter().cloned());
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    for v in &items {
        let b = rds::arg(v)?;
        if !out.contains(&b) {
            out.push(b);
        }
    }
    Ok(out)
}

pub async fn method(p: &Arc<RPubSub>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let no_kwargs = |what: &str| -> R<()> {
        match kwargs.first() {
            Some((k, _)) => Err(Exc::type_error(format!("py2axum: PubSub.{what}({k}=...) is not supported"))),
            None => Ok(()),
        }
    };
    match name {
        "subscribe" | "psubscribe" => {
            // `subscribe(chan=handler)`: handlers are not supported
            no_kwargs(name)?;
            let new = names(&args)?;
            let mut cmd = vec![name.to_uppercase().into_bytes()];
            cmd.extend(new.iter().cloned());
            p.execute(cmd).await?;
            let mut s = p.subs.lock();
            let s = &mut *s;
            let (subscribed, pending) =
                if name == "subscribe" { (&mut s.channels, &mut s.pending_channels) } else { (&mut s.patterns, &mut s.pending_patterns) };
            for n in new {
                pending.retain(|x| *x != n);
                if !subscribed.contains(&n) {
                    subscribed.push(n);
                }
            }
            Ok(V::None)
        }
        "unsubscribe" | "punsubscribe" => {
            no_kwargs(name)?;
            let given = names(&args)?;
            {
                let mut s = p.subs.lock();
                let s = &mut *s;
                let (subscribed, pending) =
                    if name == "unsubscribe" { (&s.channels, &mut s.pending_channels) } else { (&s.patterns, &mut s.pending_patterns) };
                let these = if given.is_empty() { subscribed.clone() } else { given.clone() };
                for n in these {
                    if !pending.contains(&n) {
                        pending.push(n);
                    }
                }
            }
            let mut cmd = vec![name.to_uppercase().into_bytes()];
            cmd.extend(given);
            p.execute(cmd).await?;
            Ok(V::None)
        }
        "get_message" => {
            // get_message(ignore_subscribe_messages=False, timeout=0.0)
            if args.len() > 2 {
                return Err(Exc::type_error(format!("PubSub.get_message() takes from 1 to 3 positional arguments but {} were given", args.len() + 1)));
            }
            let mut ignore = args.first().cloned();
            let mut timeout = args.get(1).cloned();
            for (k, v) in &kwargs {
                match k.as_str() {
                    "ignore_subscribe_messages" if ignore.is_none() => ignore = Some(v.clone()),
                    "timeout" if timeout.is_none() => timeout = Some(v.clone()),
                    _ => return Err(Exc::type_error(format!("PubSub.get_message() got an unexpected keyword argument '{k}'"))),
                }
            }
            let ignore = ignore.map(|v| ops::truthy(&v)).transpose()?.unwrap_or(false);
            let timeout = match timeout {
                None => Some(0.0),
                Some(V::None) => None,
                Some(V::Int(i)) => Some(i as f64),
                Some(V::Float(f)) => Some(f),
                Some(o) => return Err(Exc::type_error(format!("py2axum: get_message(timeout=) of a '{}' is not supported", o.type_name()))),
            };
            match p.read(timeout).await? {
                Some(r) => Ok(p.handle(r, ignore)?.unwrap_or(V::None)),
                None => Ok(V::None),
            }
        }
        "listen" if args.is_empty() && kwargs.is_empty() => Ok(V::native(Native::RPubSubListen(p.clone()))),
        "aclose" | "close" | "reset" if args.is_empty() && kwargs.is_empty() => {
            p.close().await;
            Ok(V::None)
        }
        "run" | "ssubscribe" | "sunsubscribe" | "get_sharded_message" | "ping" | "execute_command" => {
            Err(Exc::type_error(format!("py2axum: PubSub.{name}() is not supported")))
        }
        _ => Err(Exc::attr_error(format!("'PubSub' object has no attribute '{name}'"))),
    }
}
