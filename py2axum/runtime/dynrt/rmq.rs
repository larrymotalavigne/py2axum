//! `aio_pika` (10.x) over `lapin`: `connect(_robust)`, channels (`set_qos`, `declare_queue`, `declare_exchange`,
//! `get_queue`/`get_exchange`, `queue_delete`/`exchange_delete`), exchanges of every type, `publish` (publisher
//! confirms, aiormq's random `message_id` when there is none), queues (`get`, `bind`/`unbind`, `purge`, `delete`,
//! `consume(callback)`/`cancel`, `iterator()` with its timeout), received messages (`ack`/`nack`/`reject`,
//! `process()`, `info()`), the broker's channel errors as aiormq's classes (`ChannelNotFoundEntity`...).
//! Headers are encoded like pamqp (str → long string, int → 32/64-bit int, float → double, bool, None → void,
//! dict/list nested). A robust connection does not reconnect after a connection loss (documented).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicCancelOptions, BasicConsumeOptions, BasicGetOptions, BasicNackOptions, BasicPublishOptions, BasicQosOptions,
    BasicRejectOptions, ConfirmSelectOptions, ExchangeDeclareOptions, ExchangeDeleteOptions, QueueBindOptions, QueueDeclareOptions,
    QueueDeleteOptions, QueuePurgeOptions,
};
use lapin::types::{AMQPValue, FieldArray, FieldTable, LongString, ShortString};
use lapin::{BasicProperties, Connection, ConnectionProperties, ExchangeKind};
use parking_lot::Mutex;

use super::ops;
use super::v::*;
use super::Cx;

pub struct AConn {
    conn: Connection,
    pub robust: bool,
}

pub struct AChan {
    ch: lapin::Channel,
    pub robust: bool,
}

pub struct AMsg {
    body: Vec<u8>,
    headers: V,
    props: Vec<(&'static str, V)>,
}

/// a declared queue: what its repr shows, and `declaration_result`
pub struct AQueue {
    pub ch: Arc<AChan>,
    pub name: String,
    durable: bool,
    exclusive: bool,
    auto_delete: bool,
    arguments: V,
    counts: (u32, u32),
}

/// an exchange: `channel.default_exchange` (name ""), `declare_exchange`, `get_exchange`
pub struct AExch {
    pub ch: Arc<AChan>,
    pub name: String,
    durable: bool,
    auto_delete: bool,
    arguments: V,
}

/// a received message (`queue.get()`, a consumer's delivery)
pub struct AIncoming {
    ch: Arc<AChan>,
    body: Vec<u8>,
    props: BasicProperties,
    routing_key: String,
    exchange: String,
    redelivered: bool,
    delivery_tag: u64,
    consumer_tag: Option<String>,
    message_count: Option<u32>,
    no_ack: bool,
    processed: AtomicBool,
}

/// `queue.iterator(...)`: a consumer whose deliveries are read one by one
pub struct AIter {
    queue: Arc<AQueue>,
    no_ack: bool,
    timeout: Option<f64>,
    exclusive: bool,
    arguments: V,
    state: tokio::sync::Mutex<Option<(String, lapin::Consumer)>>,
    closed: AtomicBool,
}

/// the class of a broker error: aiormq's `EXCEPTION_MAPPING` for channel errors, `str(e)` its reply text
fn amqp_err(e: lapin::Error, connecting: bool) -> Exc {
    if connecting {
        // aiormq logs the transport's failure before raising it: `error when creating transport: %r`
        let mut src: Option<&(dyn std::error::Error + 'static)> = Some(&e);
        while let Some(x) = src {
            if let Some(io) = x.downcast_ref::<std::io::Error>() {
                if let Some(errno) = io.raw_os_error() {
                    let text = io.to_string();
                    let text = text.split(" (os error").next().unwrap_or(&text).to_string();
                    let repr = format!("<AMQPConnectionError: ({errno}, {})>", ops::str_repr(&text));
                    super::logging::emit_sync("aiormq.connection", super::logging::ERROR, "error when creating transport: %s",
                                              vec![V::str(repr)], vec![], None, &super::logging::NO_LOC);
                }
                break;
            }
            src = x.source();
        }
        return Exc::msg(&AMQP_CONNECTION_ERROR, e.to_string());
    }
    if let lapin::ErrorKind::ProtocolError(p) = e.kind() {
        let text = p.get_message().as_str().to_string();
        let class = match p.get_id() {
            403 => &AMQP_CHANNEL_ACCESS_REFUSED,
            404 => &AMQP_CHANNEL_NOT_FOUND,
            405 => &AMQP_CHANNEL_LOCKED,
            406 => &AMQP_CHANNEL_PRECONDITION,
            code if code < 500 && code != 320 => return Exc::new(&AMQP_CHANNEL_CLOSED, vec![V::Int(code as i64), V::str(text)]),
            code => return Exc::new(&AMQP_CONNECTION_CLOSED, vec![V::Int(code as i64), V::str(text)]),
        };
        return Exc::msg(class, text);
    }
    Exc::msg(&AMQP_ERROR, e.to_string())
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn flag(kwargs: &[(String, V)], n: &str) -> R<bool> {
    kw(kwargs, n).map(ops::truthy).transpose().map(|b| b.unwrap_or(false))
}

fn only(what: &str, kwargs: &[(String, V)], allowed: &[&str]) -> R<()> {
    for (k, _) in kwargs {
        if !allowed.contains(&k.as_str()) {
            return Err(Exc::type_error(format!("py2axum: {what}({k}=) is not supported")));
        }
    }
    Ok(())
}

/// aiormq's default virtual host: a URL path of "" or "/" is the vhost "/"
fn normalize_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if u.path().is_empty() || u.path() == "/" => {
            let mut s = url.trim_end_matches('/').to_string();
            if let Some(q) = u.query() {
                s = s.trim_end_matches(&format!("?{q}")).trim_end_matches('/').to_string();
                return format!("{s}/%2f?{q}");
            }
            format!("{s}/%2f")
        }
        _ => url.to_string(),
    }
}

/// `aio_pika.connect(url)` / `aio_pika.connect_robust(url)` (a fresh connection: reconnection on drop is
/// not reproduced)
pub async fn connect(args: &[V], kwargs: &[(String, V)], robust: bool) -> R {
    let url = match (args.first(), kw(kwargs, "url")) {
        (Some(u), _) | (None, Some(u)) => ops::str_(u)?,
        _ => "amqp://guest:guest@localhost:5672/".to_string(),
    };
    only("aio_pika.connect", kwargs, &["url", "timeout", "client_properties", "reconnect_interval", "fail_fast"])?;
    let conn = Connection::connect(&normalize_url(&url), ConnectionProperties::default()).await.map_err(|e| amqp_err(e, true))?;
    Ok(V::native(Native::AmqpConn(Arc::new(AConn { conn, robust }))))
}

fn value(v: &V) -> R<AMQPValue> {
    Ok(match v {
        V::None => AMQPValue::Void,
        V::Bool(b) => AMQPValue::Boolean(*b),
        V::Int(i) if i32::try_from(*i).is_ok() => AMQPValue::LongInt(*i as i32),
        V::Int(i) => AMQPValue::LongLongInt(*i),
        V::Float(f) => AMQPValue::Double(*f),
        V::Str(s) => AMQPValue::LongString(LongString::from(s.as_bytes().to_vec())),
        V::Bytes(b) => AMQPValue::ByteArray(b.to_vec().into()),
        V::List(_) | V::Tuple(_) => AMQPValue::FieldArray(FieldArray::from(ops::iter(v)?.iter().map(value).collect::<R<Vec<_>>>()?)),
        V::Dict(_) => AMQPValue::FieldTable(table(v)?),
        other => return Err(Exc::value_error(format!("py2axum: AMQP header value of type {}", other.type_name()))),
    })
}

fn table(v: &V) -> R<FieldTable> {
    let mut t = FieldTable::default();
    if let V::Dict(d) = v {
        for (_, (k, x)) in d.lock().iter() {
            t.insert(short(ops::str_(k)?)?, value(x)?);
        }
    }
    Ok(t)
}

fn py_value(v: &AMQPValue) -> V {
    match v {
        AMQPValue::Boolean(b) => V::Bool(*b),
        AMQPValue::ShortShortInt(i) => V::Int(*i as i64),
        AMQPValue::ShortShortUInt(i) => V::Int(*i as i64),
        AMQPValue::ShortInt(i) => V::Int(*i as i64),
        AMQPValue::ShortUInt(i) => V::Int(*i as i64),
        AMQPValue::LongInt(i) => V::Int(*i as i64),
        AMQPValue::LongUInt(i) => V::Int(*i as i64),
        AMQPValue::LongLongInt(i) => V::Int(*i),
        AMQPValue::Float(f) => V::Float(*f as f64),
        AMQPValue::Double(f) => V::Float(*f),
        AMQPValue::ShortString(s) => V::str(s.as_str()),
        AMQPValue::LongString(s) => V::str(String::from_utf8_lossy(s.as_bytes())),
        AMQPValue::FieldArray(a) => V::list(a.as_slice().iter().map(py_value).collect()),
        AMQPValue::FieldTable(t) => py_table(t),
        AMQPValue::ByteArray(b) => V::Bytes(Arc::from(b.as_slice())),
        AMQPValue::Void => V::None,
        _ => V::None,
    }
}

fn py_table(t: &FieldTable) -> V {
    V::dict_from(t.inner().iter().map(|(k, x)| (V::str(k.as_str()), py_value(x))).collect()).unwrap_or(V::None)
}

/// `aio_pika.Message(body, *, headers=, content_type=, delivery_mode=, message_id=, ...)`
pub fn message(args: &[V], kwargs: &[(String, V)]) -> R {
    let body = match args.first().or_else(|| kw(kwargs, "body")) {
        Some(V::Bytes(b)) => b.to_vec(),
        Some(o) => return Err(Exc::type_error(format!("py2axum: Message body must be bytes, not {}", o.type_name()))),
        None => return Err(Exc::type_error("Message.__init__() missing 1 required positional argument: 'body'")),
    };
    let mut props = Vec::new();
    for (k, v) in kwargs {
        match k.as_str() {
            "body" | "headers" => {}
            "content_type" | "content_encoding" | "message_id" | "correlation_id" | "reply_to" | "type" | "user_id" | "app_id"
            | "delivery_mode" | "priority" => {
                if !v.is_none() {
                    props.push((match k.as_str() {
                        "content_type" => "content_type",
                        "content_encoding" => "content_encoding",
                        "message_id" => "message_id",
                        "correlation_id" => "correlation_id",
                        "reply_to" => "reply_to",
                        "type" => "type",
                        "user_id" => "user_id",
                        "app_id" => "app_id",
                        "delivery_mode" => "delivery_mode",
                        _ => "priority",
                    }, v.clone()));
                }
            }
            other => return Err(Exc::type_error(format!("py2axum: Message({other}=) is not supported"))),
        }
    }
    let headers = kw(kwargs, "headers").cloned().unwrap_or(V::None);
    Ok(V::native(Native::AmqpMsg(Arc::new(AMsg { body, headers, props }))))
}


/// what aiormq sends for a Message: aio-pika's defaults (delivery mode 1, priority 0, empty headers), a
/// random UUID hex as `message_id` when there is none
fn properties(m: &AMsg) -> R<BasicProperties> {
    let mut p = BasicProperties::default().with_delivery_mode(1).with_priority(0);
    p = p.with_headers(if let V::Dict(_) = m.headers { table(&m.headers)? } else { FieldTable::default() });
    let mut has_id = false;
    for (k, v) in &m.props {
        let s = || -> R<ShortString> { short(ops::str_(v)?) };
        let int = || -> R<u8> {
            match v {
                V::Int(i) => Ok(*i as u8),
                V::Enum(e, i) => match e.value(*i) {
                    V::Int(x) => Ok(x as u8),
                    _ => Err(Exc::type_error("delivery_mode must be an int")),
                },
                _ => Err(Exc::type_error(format!("{k} must be an int"))),
            }
        };
        p = match *k {
            "content_type" => p.with_content_type(s()?),
            "content_encoding" => p.with_content_encoding(s()?),
            "message_id" => {
                has_id = !ops::str_(v)?.is_empty();
                p.with_message_id(s()?)
            }
            "correlation_id" => p.with_correlation_id(s()?),
            "reply_to" => p.with_reply_to(s()?),
            "type" => p.with_type(s()?),
            "user_id" => p.with_user_id(s()?),
            "app_id" => p.with_app_id(s()?),
            "delivery_mode" => p.with_delivery_mode(int()?),
            _ => p.with_priority(int()?),
        };
    }
    if !has_id {
        let r: u128 = rand::random();
        // UUID(int=getrandbits(128), version=4).hex
        let u = (r & !(0xf000u128 << 64) | (0x4000u128 << 64)) & !(0xc000u128 << 48) | (0x8000u128 << 48);
        p = p.with_message_id(ShortString::from(format!("{u:032x}")));
    }
    Ok(p)
}

fn robust_name(robust: bool, name: &str) -> String {
    if robust { format!("Robust{name}") } else { name.to_string() }
}

pub fn type_name(n: &Native) -> String {
    match n {
        Native::AmqpConn(c) => robust_name(c.robust, "Connection"),
        Native::AmqpChan(c) => robust_name(c.robust, "Channel"),
        Native::AmqpQueue(q) => robust_name(q.ch.robust, "Queue"),
        Native::AmqpExchange(x) => robust_name(x.ch.robust, "Exchange"),
        Native::AmqpIter(_) => "QueueIterator".into(),
        Native::AmqpProcess(..) => "ProcessContext".into(),
        Native::AmqpFrame(name, _) => name.rsplit('.').next().unwrap_or(name).to_string(),
        _ => "object".into(),
    }
}

/// `repr()`: aio-pika's (a queue's lacks its closing `)>`, as in aio-pika)
pub fn repr(n: &Native) -> R<String> {
    Ok(match n {
        Native::AmqpQueue(q) => format!("<{}({}): auto_delete={}, durable={}, exclusive={}, arguments={}", type_name(n), q.name,
                                        py_bool(q.auto_delete), py_bool(q.durable), py_bool(q.exclusive), ops::repr(&q.arguments)?),
        Native::AmqpExchange(x) => format!("<{}({}): auto_delete={}, durable={}, arguments={})>", type_name(n), x.name,
                                           py_bool(x.auto_delete), py_bool(x.durable), ops::repr(&x.arguments)?),
        Native::AmqpFrame(name, _) => format!("<{name} object at {}>", super::ops::addr(n as *const Native as usize)),
        Native::AmqpIter(it) => {
            let tag = it.state.try_lock().ok().and_then(|g| g.as_ref().map(|(t, _)| t.clone()));
            format!("<QueueIterator: queue={} ctag={}>", ops::str_repr(&it.queue.name),
                    tag.map(|t| ops::str_repr(&t)).unwrap_or_else(|| "None".into()))
        }
        _ => format!("<{} object at {}>", type_name(n), super::ops::addr(n as *const Native as usize)),
    })
}

fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

fn frame(name: &'static str, fields: Vec<(&'static str, V)>) -> V {
    V::native(Native::AmqpFrame(name, fields))
}

pub async fn conn_method(c: &Arc<AConn>, name: &str, _args: &[V]) -> R {
    match name {
        "channel" => {
            let ch = c.conn.create_channel().await.map_err(|e| amqp_err(e, false))?;
            ch.confirm_select(ConfirmSelectOptions::default()).await.map_err(|e| amqp_err(e, false))?;
            Ok(V::native(Native::AmqpChan(Arc::new(AChan { ch, robust: c.robust }))))
        }
        "close" => {
            if c.conn.status().connected() {
                c.conn.close(200, "OK".into()).await.map_err(|e| amqp_err(e, false))?;
            }
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(c.robust, "Connection")))),
    }
}

pub fn conn_attr(c: &AConn, name: &str) -> R {
    match name {
        "is_closed" => Ok(V::Bool(!c.conn.status().connected())),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(c.robust, "Connection")))),
    }
}

fn opt_name(args: &[V], kwargs: &[(String, V)], i: usize) -> R<String> {
    match args.get(i).or_else(|| kw(kwargs, "name")) {
        Some(n) if !n.is_none() => ops::str_(n),
        _ => Ok(String::new()),
    }
}

/// `ExchangeType.X` (a str enum) or its value
fn exchange_kind(v: Option<&V>) -> R<(ExchangeKind, String)> {
    let s = match v {
        None => "direct".to_string(),
        Some(V::Enum(e, i)) => ops::str_(&e.value(*i))?,
        Some(o) => ops::str_(o)?,
    };
    Ok((match s.as_str() {
        "direct" => ExchangeKind::Direct,
        "fanout" => ExchangeKind::Fanout,
        "topic" => ExchangeKind::Topic,
        "headers" => ExchangeKind::Headers,
        o => ExchangeKind::Custom(o.to_string()),
    }, s))
}

async fn declare_queue(c: &Arc<AChan>, qname: String, kwargs: &[(String, V)], passive: bool) -> R {
    let opts = QueueDeclareOptions { durable: flag(kwargs, "durable")?, exclusive: flag(kwargs, "exclusive")?, passive,
                                     auto_delete: flag(kwargs, "auto_delete")?, nowait: false };
    let arguments = kw(kwargs, "arguments").cloned().unwrap_or(V::None);
    pamqp_name("queue", &qname)?;
    let q = c.ch.queue_declare(short(qname)?, opts, table(&arguments)?).await.map_err(|e| amqp_err(e, false))?;
    Ok(V::native(Native::AmqpQueue(Arc::new(AQueue {
        ch: c.clone(), name: q.name().as_str().to_string(), durable: opts.durable, exclusive: opts.exclusive, auto_delete: opts.auto_delete,
        arguments, counts: (q.message_count(), q.consumer_count()),
    }))))
}

async fn declare_exchange(c: &Arc<AChan>, xname: String, kind: Option<&V>, kwargs: &[(String, V)], passive: bool) -> R {
    let (kind, _) = exchange_kind(kind)?;
    let opts = ExchangeDeclareOptions { passive, durable: flag(kwargs, "durable")?, auto_delete: flag(kwargs, "auto_delete")?,
                                        internal: flag(kwargs, "internal")?, nowait: false };
    let arguments = match kw(kwargs, "arguments") {
        Some(a) if ops::truthy(a)? => a.clone(),
        _ => V::empty_dict(),
    };
    pamqp_name("exchange", &xname)?;
    c.ch.exchange_declare(short(xname.clone())?, kind, opts, table(&arguments)?).await.map_err(|e| amqp_err(e, false))?;
    Ok(V::native(Native::AmqpExchange(Arc::new(AExch { ch: c.clone(), name: xname, durable: opts.durable, auto_delete: opts.auto_delete, arguments }))))
}

pub async fn chan_method(c: &Arc<AChan>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "declare_queue" => {
            only("declare_queue", kwargs, &["name", "durable", "exclusive", "passive", "auto_delete", "arguments", "timeout"])?;
            let qname = opt_name(args, kwargs, 0)?;
            declare_queue(c, qname, kwargs, flag(kwargs, "passive")?).await
        }
        "get_queue" => {
            only("get_queue", kwargs, &["name", "ensure"])?;
            let qname = opt_name(args, kwargs, 0)?;
            declare_queue(c, qname, &[], true).await
        }
        "declare_exchange" => {
            only("declare_exchange", kwargs, &["name", "type", "durable", "auto_delete", "internal", "passive", "arguments", "timeout"])?;
            let xname = opt_name(args, kwargs, 0)?;
            declare_exchange(c, xname, args.get(1).or_else(|| kw(kwargs, "type")), kwargs, flag(kwargs, "passive")?).await
        }
        "get_exchange" => {
            only("get_exchange", kwargs, &["name", "ensure"])?;
            let xname = opt_name(args, kwargs, 0)?;
            declare_exchange(c, xname, None, &[], true).await
        }
        "exchange_delete" => {
            only("exchange_delete", kwargs, &["exchange_name", "if_unused", "timeout", "nowait"])?;
            let x = ops::str_(args.first().or_else(|| kw(kwargs, "exchange_name")).unwrap_or(&V::None))?;
            c.ch.exchange_delete(short(x)?, ExchangeDeleteOptions { if_unused: flag(kwargs, "if_unused")?, nowait: false })
                .await.map_err(|e| amqp_err(e, false))?;
            Ok(frame("Exchange.DeleteOk", vec![]))
        }
        "queue_delete" => {
            only("queue_delete", kwargs, &["queue_name", "if_unused", "if_empty", "timeout", "nowait"])?;
            let q = ops::str_(args.first().or_else(|| kw(kwargs, "queue_name")).unwrap_or(&V::None))?;
            let n = c.ch.queue_delete(short(q)?, QueueDeleteOptions { if_unused: flag(kwargs, "if_unused")?, if_empty: flag(kwargs, "if_empty")?, nowait: false })
                .await.map_err(|e| amqp_err(e, false))?;
            Ok(frame("Queue.DeleteOk", vec![("message_count", V::Int(n as i64))]))
        }
        "set_qos" => {
            only("set_qos", kwargs, &["prefetch_count", "prefetch_size", "global_", "timeout", "all_channels"])?;
            let count = match args.first().or_else(|| kw(kwargs, "prefetch_count")) {
                Some(V::Int(n)) => *n as u16,
                _ => 0,
            };
            if !matches!(kw(kwargs, "prefetch_size"), None | Some(V::Int(0))) {
                return Err(Exc::type_error("py2axum: set_qos(prefetch_size=) is not supported"));
            }
            c.ch.basic_qos(count, BasicQosOptions { global: flag(kwargs, "global_")? }).await.map_err(|e| amqp_err(e, false))?;
            Ok(frame("Basic.QosOk", vec![]))
        }
        "close" => {
            if c.ch.status().connected() {
                c.ch.close(200, "OK".into()).await.map_err(|e| amqp_err(e, false))?;
            }
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(c.robust, "Channel")))),
    }
}

pub fn chan_attr(c: &Arc<AChan>, name: &str) -> R {
    match name {
        "default_exchange" => Ok(V::native(Native::AmqpExchange(Arc::new(AExch {
            ch: c.clone(), name: String::new(), durable: false, auto_delete: false, arguments: V::empty_dict() })))),
        "is_closed" => Ok(V::Bool(!c.ch.status().connected())),
        "number" => Ok(V::Int(c.ch.id() as i64)),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(c.robust, "Channel")))),
    }
}

pub fn exchange_attr(x: &AExch, name: &str) -> R {
    match name {
        "name" => Ok(V::str(&x.name)),
        "durable" => Ok(V::Bool(x.durable)),
        "auto_delete" => Ok(V::Bool(x.auto_delete)),
        "arguments" => Ok(x.arguments.clone()),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(x.ch.robust, "Exchange")))),
    }
}

pub async fn exchange_method(x: &Arc<AExch>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "publish" => publish(&x.ch, &x.name, args, kwargs).await,
        "delete" => {
            only("delete", kwargs, &["if_unused", "timeout"])?;
            x.ch.ch.exchange_delete(short(x.name.clone())?, ExchangeDeleteOptions { if_unused: flag(kwargs, "if_unused")?, nowait: false })
                .await.map_err(|e| amqp_err(e, false))?;
            Ok(frame("Exchange.DeleteOk", vec![]))
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(x.ch.robust, "Exchange")))),
    }
}

/// `exchange.publish(message, routing_key, mandatory=True)`: waits for the broker's confirmation
pub async fn publish(c: &Arc<AChan>, exchange: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let msg = match args.first().or_else(|| kw(kwargs, "message")) {
        Some(V::Native(n)) => match &**n {
            Native::AmqpMsg(m) => m.clone(),
            _ => return Err(Exc::type_error("publish() needs an aio_pika.Message")),
        },
        _ => return Err(Exc::type_error("publish() needs an aio_pika.Message")),
    };
    let key = match args.get(1).or_else(|| kw(kwargs, "routing_key")) {
        Some(k) => ops::str_(k)?,
        None => return Err(Exc::type_error("publish() missing 1 required argument: 'routing_key'")),
    };
    only("publish", kwargs, &["message", "routing_key", "mandatory", "immediate", "timeout"])?;
    let props = properties(&msg)?;
    pamqp_name("exchange", exchange)?;
    let confirm = c
        .ch
        .basic_publish(short(exchange.to_string())?, short(key)?, BasicPublishOptions::default(), &msg.body, props)
        .await
        .map_err(|e| amqp_err(e, false))?;
    confirm.await.map_err(|e| amqp_err(e, false))?;
    Ok(V::None)
}

pub fn queue_attr(q: &Arc<AQueue>, name: &str) -> R {
    match name {
        "name" => Ok(V::str(&q.name)),
        "durable" => Ok(V::Bool(q.durable)),
        "exclusive" => Ok(V::Bool(q.exclusive)),
        "auto_delete" => Ok(V::Bool(q.auto_delete)),
        "arguments" => Ok(q.arguments.clone()),
        "declaration_result" => Ok(frame("Queue.DeclareOk", vec![("queue", V::str(&q.name)), ("message_count", V::Int(q.counts.0 as i64)),
                                                                  ("consumer_count", V::Int(q.counts.1 as i64))])),
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(q.ch.robust, "Queue")))),
    }
}

/// the name of an exchange given as an object or by name (aio-pika's `get_exchange_name`)
fn exchange_name(v: &V) -> R<String> {
    match v {
        V::Native(n) => match &**n {
            Native::AmqpExchange(x) => Ok(x.name.clone()),
            _ => ops::str_(v),
        },
        _ => ops::str_(v),
    }
}

/// a consumer tag as aiormq makes it: `ctag<channel>.<uuid4 hex>`
fn consumer_tag(c: &AChan) -> String {
    let r: u128 = rand::random();
    let u = (r & !(0xf000u128 << 64) | (0x4000u128 << 64)) & !(0xc000u128 << 48) | (0x8000u128 << 48);
    format!("ctag{}.{u:032x}", c.ch.id())
}

async fn start_consumer(q: &Arc<AQueue>, no_ack: bool, exclusive: bool, arguments: &V, tag: Option<String>) -> R<(String, lapin::Consumer)> {
    let tag = tag.unwrap_or_else(|| consumer_tag(&q.ch));
    let consumer = q.ch.ch.basic_consume(short(q.name.clone())?, short(tag.clone())?,
                                         BasicConsumeOptions { no_local: false, no_ack, exclusive, nowait: false }, table(arguments)?)
        .await.map_err(|e| amqp_err(e, false))?;
    Ok((tag, consumer))
}

fn incoming(ch: &Arc<AChan>, d: lapin::message::Delivery, tag: Option<String>, count: Option<u32>, no_ack: bool) -> V {
    V::native(Native::AmqpIncoming(Arc::new(AIncoming {
        ch: ch.clone(), body: d.data, props: d.properties, routing_key: d.routing_key.as_str().to_string(),
        exchange: d.exchange.as_str().to_string(), redelivered: d.redelivered, delivery_tag: d.delivery_tag, consumer_tag: tag,
        message_count: count, processed: AtomicBool::new(no_ack), no_ack,
    })))
}

pub async fn queue_method(cx: &Cx, q: &Arc<AQueue>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "get" => queue_get(q, kwargs).await,
        "bind" | "unbind" => {
            only(name, kwargs, &["exchange", "routing_key", "arguments", "timeout"])?;
            let x = exchange_name(args.first().or_else(|| kw(kwargs, "exchange")).unwrap_or(&V::None))?;
            let rk = match args.get(1).or_else(|| kw(kwargs, "routing_key")) {
                Some(k) if !k.is_none() => ops::str_(k)?,
                _ => q.name.clone(),
            };
            let arguments = table(kw(kwargs, "arguments").unwrap_or(&V::None))?;
            if name == "bind" {
                q.ch.ch.queue_bind(short(q.name.clone())?, short(x)?, short(rk)?, QueueBindOptions { nowait: false }, arguments)
                    .await.map_err(|e| amqp_err(e, false))?;
                Ok(frame("Queue.BindOk", vec![]))
            } else {
                q.ch.ch.queue_unbind(short(q.name.clone())?, short(x)?, short(rk)?, arguments).await.map_err(|e| amqp_err(e, false))?;
                Ok(frame("Queue.UnbindOk", vec![]))
            }
        }
        "purge" => {
            only("purge", kwargs, &["no_wait", "timeout"])?;
            let n = q.ch.ch.queue_purge(short(q.name.clone())?, QueuePurgeOptions { nowait: false }).await.map_err(|e| amqp_err(e, false))?;
            Ok(frame("Queue.PurgeOk", vec![("message_count", V::Int(n as i64))]))
        }
        "delete" => {
            only("delete", kwargs, &["if_unused", "if_empty", "timeout"])?;
            // aio-pika's defaults: if_unused=True, if_empty=True
            let opt = |n: &str| -> R<bool> { kw(kwargs, n).map(ops::truthy).transpose().map(|b| b.unwrap_or(true)) };
            let n = q.ch.ch.queue_delete(short(q.name.clone())?, QueueDeleteOptions { if_unused: opt("if_unused")?, if_empty: opt("if_empty")?, nowait: false })
                .await.map_err(|e| amqp_err(e, false))?;
            Ok(frame("Queue.DeleteOk", vec![("message_count", V::Int(n as i64))]))
        }
        "consume" => {
            only("consume", kwargs, &["callback", "no_ack", "exclusive", "arguments", "consumer_tag", "timeout"])?;
            let cb = args.first().or_else(|| kw(kwargs, "callback")).cloned()
                .ok_or_else(|| Exc::type_error("consume() missing 1 required positional argument: 'callback'"))?;
            let no_ack = flag(kwargs, "no_ack")?;
            let tag = kw(kwargs, "consumer_tag").filter(|t| !t.is_none()).map(ops::str_).transpose()?;
            let (tag, mut consumer) = start_consumer(q, no_ack, flag(kwargs, "exclusive")?, kw(kwargs, "arguments").unwrap_or(&V::None), tag).await?;
            let (cx2, ch, t2) = (cx.clone(), q.ch.clone(), tag.clone());
            // aiormq runs each delivery's callback as a task of its own
            tokio::spawn(async move {
                while let Some(Ok(d)) = consumer.next().await {
                    let m = incoming(&ch, d, Some(t2.clone()), None, no_ack);
                    let _ = super::web::spawn_task(&cx2, cb.clone(), vec![m], vec![]);
                }
            });
            Ok(V::str(tag))
        }
        "cancel" => {
            only("cancel", kwargs, &["consumer_tag", "timeout", "nowait"])?;
            let tag = ops::str_(args.first().or_else(|| kw(kwargs, "consumer_tag")).unwrap_or(&V::None))?;
            q.ch.ch.basic_cancel(short(tag.clone())?, BasicCancelOptions { nowait: false }).await.map_err(|e| amqp_err(e, false))?;
            Ok(frame("Basic.CancelOk", vec![("consumer_tag", V::str(tag))]))
        }
        "iterator" => {
            only("iterator", kwargs, &["no_ack", "timeout", "exclusive", "arguments", "consumer_tag"])?;
            let timeout = match kw(kwargs, "timeout") {
                None | Some(V::None) => None,
                Some(V::Int(i)) => Some(*i as f64),
                Some(V::Float(f)) => Some(*f),
                Some(o) => return Err(Exc::type_error(format!("py2axum: iterator(timeout=) of type {}", o.type_name()))),
            };
            Ok(V::native(Native::AmqpIter(Arc::new(AIter {
                queue: q.clone(), no_ack: flag(kwargs, "no_ack")?, timeout, exclusive: flag(kwargs, "exclusive")?,
                arguments: kw(kwargs, "arguments").cloned().unwrap_or(V::None), state: tokio::sync::Mutex::new(None), closed: AtomicBool::new(false),
            }))))
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", robust_name(q.ch.robust, "Queue")))),
    }
}

/// `queue.get(no_ack=False, fail=True)`: the next message, or `QueueEmpty`
async fn queue_get(q: &Arc<AQueue>, kwargs: &[(String, V)]) -> R {
    only("get", kwargs, &["no_ack", "fail", "timeout"])?;
    let no_ack = flag(kwargs, "no_ack")?;
    let fail = kw(kwargs, "fail").map(ops::truthy).transpose()?.unwrap_or(true);
    pamqp_name("queue", &q.name)?;
    let got = q.ch.ch.basic_get(short(q.name.clone())?, BasicGetOptions { no_ack }).await.map_err(|e| amqp_err(e, false))?;
    match got {
        Some(m) => {
            let count = m.message_count;
            Ok(incoming(&q.ch, m.delivery, None, Some(count), no_ack))
        }
        None if fail => Err(Exc::msg(&AMQP_QUEUE_EMPTY, "")),
        None => Ok(V::None),
    }
}

/// `async with queue.iterator() as it`: the consumer starts
pub async fn iter_enter(it: &Arc<AIter>) -> R<()> {
    let mut st = it.state.lock().await;
    if st.is_none() && !it.closed.load(Ordering::SeqCst) {
        *st = Some(start_consumer(&it.queue, it.no_ack, it.exclusive, &it.arguments, None).await?);
    }
    Ok(())
}

/// `QueueIterator.close()`: the consumer cancelled (deliveries not read yet go back to the queue)
pub async fn iter_close(it: &Arc<AIter>) -> R<()> {
    it.closed.store(true, Ordering::SeqCst);
    let taken = it.state.lock().await.take();
    if let Some((tag, _consumer)) = taken {
        if it.queue.ch.ch.status().connected() {
            it.queue.ch.ch.basic_cancel(short(tag)?, BasicCancelOptions { nowait: false }).await.map_err(|e| amqp_err(e, false))?;
        }
    }
    Ok(())
}

/// `__anext__`: the next delivery; at the timeout the iterator closes and `TimeoutError` is raised
pub async fn iter_next(it: &Arc<AIter>) -> R<Option<V>> {
    if it.closed.load(Ordering::SeqCst) {
        return Ok(None);
    }
    iter_enter(it).await?;
    let next = {
        let mut st = it.state.lock().await;
        let Some((tag, consumer)) = st.as_mut() else { return Ok(None) };
        let tag = tag.clone();
        let fut = consumer.next();
        let got = match it.timeout {
            Some(t) if t > 0.0 => match tokio::time::timeout(std::time::Duration::from_secs_f64(t), fut).await {
                Ok(x) => Some(x),
                Err(_) => None,
            },
            _ => Some(fut.await),
        };
        got.map(|x| (x, tag))
    };
    match next {
        None => {
            iter_close(it).await?;
            Err(Exc::new(&TIMEOUT_ERROR, vec![]))
        }
        Some((Some(Ok(d)), tag)) => Ok(Some(incoming(&it.queue.ch, d, Some(tag), None, it.no_ack))),
        Some((Some(Err(e)), _)) => Err(amqp_err(e, false)),
        Some((None, _)) => Ok(None),
    }
}

pub fn incoming_attr(m: &AIncoming, name: &str) -> R {
    let s = |x: &Option<ShortString>| x.as_ref().map(|v| V::str(v.as_str())).unwrap_or(V::None);
    Ok(match name {
        "body" => V::Bytes(Arc::from(m.body.as_slice())),
        "body_size" => V::Int(m.body.len() as i64),
        "headers" => m.props.headers().as_ref().map(py_table).unwrap_or_else(V::empty_dict),
        "message_id" => s(m.props.message_id()),
        "content_type" => s(m.props.content_type()),
        "content_encoding" => s(m.props.content_encoding()),
        "correlation_id" => s(m.props.correlation_id()),
        "reply_to" => s(m.props.reply_to()),
        "type" => s(m.props.kind()),
        "app_id" => s(m.props.app_id()),
        "user_id" => s(m.props.user_id()),
        "cluster_id" => V::str(m.props.cluster_id().as_ref().map(|c| c.as_str()).unwrap_or("")),
        "delivery_mode" => V::Int(m.props.delivery_mode().unwrap_or(1) as i64),
        "priority" => V::Int(m.props.priority().unwrap_or(0) as i64),
        "routing_key" => V::str(&m.routing_key),
        "exchange" => V::str(&m.exchange),
        "redelivered" => V::Bool(m.redelivered),
        "delivery_tag" => V::Int(m.delivery_tag as i64),
        "consumer_tag" => m.consumer_tag.as_ref().map(V::str).unwrap_or(V::None),
        "message_count" => m.message_count.map(|n| V::Int(n as i64)).unwrap_or(V::None),
        "processed" | "locked" => V::Bool(m.processed.load(Ordering::SeqCst)),
        _ => return Err(Exc::attr_error(format!("'IncomingMessage' object has no attribute '{name}'"))),
    })
}

fn already_processed() -> Exc {
    // MessageProcessError("Message already processed", message): its str() is the args tuple
    Exc::new(&AMQP_MESSAGE_PROCESS_ERROR, vec![V::str("Message already processed"), V::str("<IncomingMessage>")])
}

/// `ack`, `nack`, `reject` (aio-pika's checks, in its order)
async fn settle(m: &Arc<AIncoming>, how: &str, multiple: bool, requeue: bool) -> R {
    if m.no_ack {
        return Err(Exc::type_error(match how {
            "ack" => "Can't ack message with \"no_ack\" flag",
            "nack" => "Can't nack message with \"no_ack\" flag",
            _ => "This message has \"no_ack\" flag.",
        }));
    }
    if m.processed.swap(true, Ordering::SeqCst) {
        return Err(already_processed());
    }
    let ch = &m.ch.ch;
    let r = match how {
        "ack" => ch.basic_ack(m.delivery_tag, BasicAckOptions { multiple }).await,
        "nack" => ch.basic_nack(m.delivery_tag, BasicNackOptions { multiple, requeue }).await,
        _ => ch.basic_reject(m.delivery_tag, BasicRejectOptions { requeue }).await,
    };
    r.map_err(|e| amqp_err(e, false))?;
    Ok(V::None)
}

pub async fn incoming_method(m: &Arc<AIncoming>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let get = |i: usize, n: &str, d: bool| -> R<bool> {
        args.get(i).or_else(|| kw(kwargs, n)).map(ops::truthy).transpose().map(|b| b.unwrap_or(d))
    };
    match name {
        "ack" => settle(m, "ack", get(0, "multiple", false)?, false).await,
        "nack" => settle(m, "nack", get(0, "multiple", false)?, get(1, "requeue", true)?).await,
        "reject" => settle(m, "reject", false, get(0, "requeue", false)?).await,
        "process" => Ok(V::native(Native::AmqpProcess(m.clone(), get(0, "requeue", false)?, get(1, "reject_on_redelivered", false)?,
                                                      get(2, "ignore_processed", false)?))),
        "info" => {
            let attrs = ["app_id", "body_size", "cluster_id", "consumer_tag", "content_encoding", "content_type", "correlation_id",
                         "delivery_mode", "delivery_tag", "exchange", "expiration", "headers", "message_id", "priority", "redelivered",
                         "reply_to", "routing_key", "timestamp", "type", "user_id"];
            let mut items = vec![];
            for a in attrs {
                let v = match a {
                    "expiration" | "timestamp" => V::None,
                    "type" => V::str(ops::str_(&incoming_attr(m, a)?)?),
                    _ => incoming_attr(m, a)?,
                };
                items.push((V::str(a), v));
            }
            V::dict_from(items)
        }
        _ => Err(Exc::attr_error(format!("'IncomingMessage' object has no attribute '{name}'"))),
    }
}

/// leaving `async with message.process(...)`: aio-pika's ProcessContext.__aexit__
pub async fn process_exit(m: &Arc<AIncoming>, requeue: bool, reject_on_redelivered: bool, ignore_processed: bool, exc: Option<&Exc>) -> R<()> {
    if exc.is_none() {
        if !ignore_processed || !m.processed.load(Ordering::SeqCst) {
            settle(m, "ack", false, false).await?;
        }
        return Ok(());
    }
    // a closed channel: aio-pika logs a warning (its own logger) and sends nothing
    if (!ignore_processed || !m.processed.load(Ordering::SeqCst)) && m.ch.ch.status().connected() {
        settle(m, "reject", false, requeue && !(reject_on_redelivered && m.redelivered)).await?;
    }
    Ok(())
}

/// pamqp validates names before sending a frame (`Frame.validate`): at most 256 (127) characters of
/// `^[a-zA-Z0-9-_.:@#,/+ ]*$`, a ValueError otherwise (the server never sees the request).
fn pamqp_name(what: &str, name: &str) -> R<()> {
    // Basic.Publish checks its exchange against 127, the queue frames against 256
    if name.chars().count() > if what == "exchange" { 127 } else { 256 } {
        return Err(Exc::value_error(format!("Max length exceeded for {what}")));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:@#,/+ ".contains(c)) {
        return Err(Exc::value_error(format!("Invalid value for {what}")));
    }
    Ok(())
}

/// pamqp's `short_string` encoder: at most 255 UTF-8 bytes, a TypeError otherwise (lapin would panic).
fn short(s: impl Into<String>) -> R<ShortString> {
    let s: String = s.into();
    if s.len() > 255 {
        return Err(Exc::type_error("string exceeds maximum length of 255 bytes"));
    }
    Ok(ShortString::from(s))
}
