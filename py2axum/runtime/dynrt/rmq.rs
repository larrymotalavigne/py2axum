//! `aio_pika` (10.x) publishing over `lapin`: `connect(_robust)`, `channel()`, `declare_queue`,
//! `default_exchange.publish(Message(...), routing_key=)` (publisher confirms), `queue.get()` and the
//! received message, `async with` on a connection. Headers are encoded like pamqp (str → long string,
//! int → 32/64-bit int, float → double, bool, None → void, dict/list nested).
use std::sync::Arc;

use lapin::options::{BasicAckOptions, BasicGetOptions, BasicPublishOptions, ConfirmSelectOptions, QueueDeclareOptions};
use lapin::types::{AMQPValue, FieldArray, FieldTable, LongString, ShortString};
use lapin::{BasicProperties, Connection, ConnectionProperties};

use super::ops;
use super::v::*;

pub struct AConn {
    conn: Connection,
}

pub struct AChan {
    ch: lapin::Channel,
}

pub struct AMsg {
    body: Vec<u8>,
    headers: V,
    props: Vec<(&'static str, V)>,
}

pub struct AIncoming {
    get: tokio::sync::Mutex<Option<lapin::message::BasicGetMessage>>,
    body: Vec<u8>,
    props: BasicProperties,
    routing_key: String,
    no_ack: bool,
}

fn amqp_err(e: lapin::Error, connecting: bool) -> Exc {
    Exc::msg(if connecting { &AMQP_CONNECTION_ERROR } else { &AMQP_ERROR }, e.to_string())
}

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
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
pub async fn connect(args: &[V], kwargs: &[(String, V)]) -> R {
    let url = match (args.first(), kw(kwargs, "url")) {
        (Some(u), _) | (None, Some(u)) => ops::str_(u)?,
        _ => "amqp://guest:guest@localhost:5672/".to_string(),
    };
    for (k, _) in kwargs {
        if !matches!(k.as_str(), "url" | "timeout" | "client_properties" | "reconnect_interval" | "fail_fast") {
            return Err(Exc::type_error(format!("py2axum: aio_pika.connect({k}=) is not supported")));
        }
    }
    let conn = Connection::connect(&normalize_url(&url), ConnectionProperties::default()).await.map_err(|e| amqp_err(e, true))?;
    Ok(V::native(Native::AmqpConn(Arc::new(AConn { conn }))))
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

fn properties(m: &AMsg) -> R<BasicProperties> {
    let mut p = BasicProperties::default();
    if let V::Dict(_) = m.headers {
        p = p.with_headers(table(&m.headers)?);
    }
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
            "message_id" => p.with_message_id(s()?),
            "correlation_id" => p.with_correlation_id(s()?),
            "reply_to" => p.with_reply_to(s()?),
            "type" => p.with_type(s()?),
            "user_id" => p.with_user_id(s()?),
            "app_id" => p.with_app_id(s()?),
            "delivery_mode" => p.with_delivery_mode(int()?),
            _ => p.with_priority(int()?),
        };
    }
    Ok(p)
}

pub async fn conn_method(c: &Arc<AConn>, name: &str, _args: &[V]) -> R {
    match name {
        "channel" => {
            let ch = c.conn.create_channel().await.map_err(|e| amqp_err(e, false))?;
            ch.confirm_select(ConfirmSelectOptions::default()).await.map_err(|e| amqp_err(e, false))?;
            Ok(V::native(Native::AmqpChan(Arc::new(AChan { ch }))))
        }
        "close" => {
            if c.conn.status().connected() {
                c.conn.close(200, "OK".into()).await.map_err(|e| amqp_err(e, false))?;
            }
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'RobustConnection' object has no attribute '{name}'"))),
    }
}

pub fn conn_attr(c: &AConn, name: &str) -> R {
    match name {
        "is_closed" => Ok(V::Bool(!c.conn.status().connected())),
        _ => Err(Exc::attr_error(format!("'RobustConnection' object has no attribute '{name}'"))),
    }
}

pub async fn chan_method(c: &Arc<AChan>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "declare_queue" => {
            let qname = match args.first().or_else(|| kw(kwargs, "name")) {
                Some(n) if !n.is_none() => ops::str_(n)?,
                _ => String::new(),
            };
            let flag = |n: &str| -> R<bool> { kw(kwargs, n).map(ops::truthy).transpose().map(|b| b.unwrap_or(false)) };
            for (k, _) in kwargs {
                if !matches!(k.as_str(), "name" | "durable" | "exclusive" | "passive" | "auto_delete" | "arguments" | "timeout") {
                    return Err(Exc::type_error(format!("py2axum: declare_queue({k}=) is not supported")));
                }
            }
            let opts = QueueDeclareOptions { durable: flag("durable")?, exclusive: flag("exclusive")?, passive: flag("passive")?, auto_delete: flag("auto_delete")?, nowait: false };
            let arguments = match kw(kwargs, "arguments") {
                Some(a @ V::Dict(_)) => table(a)?,
                _ => FieldTable::default(),
            };
            pamqp_name("queue", &qname)?;
            let q = c.ch.queue_declare(short(qname)?, opts, arguments).await.map_err(|e| amqp_err(e, false))?;
            Ok(V::native(Native::AmqpQueue(c.clone(), q.name().as_str().to_string())))
        }
        "close" => {
            c.ch.close(200, "OK".into()).await.map_err(|e| amqp_err(e, false))?;
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'RobustChannel' object has no attribute '{name}'"))),
    }
}

pub fn chan_attr(c: &Arc<AChan>, name: &str) -> R {
    match name {
        "default_exchange" => Ok(V::native(Native::AmqpExchange(c.clone(), String::new()))),
        "is_closed" => Ok(V::Bool(!c.ch.status().connected())),
        _ => Err(Exc::attr_error(format!("'RobustChannel' object has no attribute '{name}'"))),
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
    for (k, _) in kwargs {
        if !matches!(k.as_str(), "message" | "routing_key" | "mandatory" | "immediate" | "timeout") {
            return Err(Exc::type_error(format!("py2axum: publish({k}=) is not supported")));
        }
    }
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

/// `queue.get(no_ack=False, fail=True)`: the next message, or `QueueEmpty`
pub async fn queue_get(c: &Arc<AChan>, queue: &str, kwargs: &[(String, V)]) -> R {
    let no_ack = kw(kwargs, "no_ack").map(ops::truthy).transpose()?.unwrap_or(false);
    let fail = kw(kwargs, "fail").map(ops::truthy).transpose()?.unwrap_or(true);
    pamqp_name("queue", queue)?;
    let got = c.ch.basic_get(short(queue.to_string())?, BasicGetOptions { no_ack }).await.map_err(|e| amqp_err(e, false))?;
    match got {
        Some(m) => {
            let (body, props, routing_key) = (m.delivery.data.clone(), m.delivery.properties.clone(), m.delivery.routing_key.as_str().to_string());
            Ok(V::native(Native::AmqpIncoming(Arc::new(AIncoming { get: tokio::sync::Mutex::new(Some(m)), body, props, routing_key, no_ack }))))
        }
        None if fail => Err(Exc::msg(&AMQP_QUEUE_EMPTY, "")),
        None => Ok(V::None),
    }
}

pub fn incoming_attr(m: &AIncoming, name: &str) -> R {
    let s = |x: &Option<ShortString>| x.as_ref().map(|v| V::str(v.as_str())).unwrap_or(V::None);
    Ok(match name {
        "body" => V::Bytes(Arc::from(m.body.as_slice())),
        "headers" => m.props.headers().as_ref().map(py_table).unwrap_or_else(|| V::dict_from(vec![]).unwrap()),
        "message_id" => s(m.props.message_id()),
        "content_type" => s(m.props.content_type()),
        "correlation_id" => s(m.props.correlation_id()),
        "reply_to" => s(m.props.reply_to()),
        "type" => s(m.props.kind()),
        "app_id" => s(m.props.app_id()),
        "delivery_mode" => m.props.delivery_mode().map(|d| V::Int(d as i64)).unwrap_or(V::None),
        "priority" => m.props.priority().map(|d| V::Int(d as i64)).unwrap_or(V::None),
        "routing_key" => V::str(&m.routing_key),
        _ => return Err(Exc::attr_error(format!("'IncomingMessage' object has no attribute '{name}'"))),
    })
}

pub async fn incoming_method(m: &Arc<AIncoming>, name: &str) -> R {
    match name {
        "ack" => {
            if m.no_ack {
                return Err(Exc::type_error("Can't ack message with \"no_ack\" flag"));
            }
            match m.get.lock().await.take() {
                Some(g) => {
                    g.delivery.ack(BasicAckOptions::default()).await.map_err(|e| amqp_err(e, false))?;
                }
                None => return Err(Exc::msg(&AMQP_ERROR, "Message already processed")),
            }
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'IncomingMessage' object has no attribute '{name}'"))),
    }
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
