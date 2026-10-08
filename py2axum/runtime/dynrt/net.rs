//! `socket.getaddrinfo` (the C library's, like CPython) and `ipaddress` (`ip_address`, `ip_network`,
//! membership, str/repr).
use std::net::IpAddr;
use std::sync::Arc;

use super::ops;
use super::v::*;

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn int_arg(args: &[V], kwargs: &[(String, V)], i: usize, name: &str) -> R<i32> {
    match args.get(i).or_else(|| kw(kwargs, name)) {
        None => Ok(0),
        Some(V::Int(n)) => Ok(*n as i32),
        Some(V::Bool(b)) => Ok(*b as i32),
        Some(o) => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    }
}

/// `socket.getaddrinfo(host, port, family=0, type=0, proto=0, flags=0)`: the C library's answer, in its order.
/// Families and kinds are plain ints (CPython wraps them in `AddressFamily`/`SocketKind` IntEnums).
pub fn getaddrinfo(args: &[V], kwargs: &[(String, V)]) -> R {
    use std::ffi::{CStr, CString};
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "host" | "port" | "family" | "type" | "proto" | "flags")) {
        return Err(Exc::type_error(format!("getaddrinfo() got an unexpected keyword argument '{k}'")));
    }
    let host = match args.first().or_else(|| kw(kwargs, "host")) {
        None => return Err(Exc::type_error("getaddrinfo() missing required argument 'host' (pos 1)")),
        Some(V::None) => None,
        Some(V::Str(s)) => Some(s.to_string()),
        Some(V::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        Some(o) => return Err(Exc::type_error(format!("getaddrinfo() argument 1 must be string or None, not {}", o.type_name()))),
    };
    let port = match args.get(1).or_else(|| kw(kwargs, "port")) {
        None | Some(V::None) => None,
        Some(V::Int(n)) => Some(n.to_string()),
        Some(V::Str(s)) => Some(s.to_string()),
        Some(V::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        Some(_) => return Err(Exc::msg(&OS_ERROR, "Int or String expected")),
    };
    // CPython encodes the host with IDNA; ASCII names are unchanged
    if host.as_deref().is_some_and(|h| !h.is_ascii()) {
        return Err(Exc::type_error("py2axum: socket.getaddrinfo() of a non-ASCII host name is not supported"));
    }
    let chost = host.map(|h| CString::new(h).map_err(|_| Exc::value_error("embedded null character"))).transpose()?;
    let cport = port.map(|p| CString::new(p).map_err(|_| Exc::value_error("embedded null character"))).transpose()?;
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_family = int_arg(args, kwargs, 2, "family")?;
    hints.ai_socktype = int_arg(args, kwargs, 3, "type")?;
    hints.ai_protocol = int_arg(args, kwargs, 4, "proto")?;
    hints.ai_flags = int_arg(args, kwargs, 5, "flags")?;
    let mut res: *mut libc::addrinfo = std::ptr::null_mut();
    let rc = unsafe {
        libc::getaddrinfo(
            chost.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            cport.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &hints,
            &mut res,
        )
    };
    if rc != 0 {
        let text = unsafe { CStr::from_ptr(libc::gai_strerror(rc)) }.to_string_lossy().into_owned();
        return Err(Exc::msg(&SOCKET_GAIERROR, format!("[Errno {rc}] {text}")));
    }
    let mut out = Vec::new();
    let mut p = res;
    while !p.is_null() {
        let ai = unsafe { &*p };
        let canon = if ai.ai_canonname.is_null() { String::new() } else { unsafe { CStr::from_ptr(ai.ai_canonname) }.to_string_lossy().into_owned() };
        let sockaddr = match ai.ai_family {
            libc::AF_INET => {
                let sa = unsafe { &*(ai.ai_addr as *const libc::sockaddr_in) };
                let ip = std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
                V::tuple(vec![V::str(ip.to_string()), V::Int(u16::from_be(sa.sin_port) as i64)])
            }
            libc::AF_INET6 => {
                let sa = unsafe { &*(ai.ai_addr as *const libc::sockaddr_in6) };
                let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
                V::tuple(vec![
                    V::str(ip.to_string()),
                    V::Int(u16::from_be(sa.sin6_port) as i64),
                    V::Int(u32::from_be(sa.sin6_flowinfo) as i64),
                    V::Int(sa.sin6_scope_id as i64),
                ])
            }
            _ => V::tuple(vec![]),
        };
        out.push(V::tuple(vec![V::Int(ai.ai_family as i64), V::Int(ai.ai_socktype as i64), V::Int(ai.ai_protocol as i64), V::str(canon), sockaddr]));
        p = ai.ai_next;
    }
    unsafe { libc::freeaddrinfo(res) };
    Ok(V::list(out))
}

// ---------------------------------------------------------------- ipaddress

fn not_addr(s: &str, what: &str) -> Exc {
    Exc::value_error(format!("{} does not appear to be an IPv4 or IPv6 {what}", ops::str_repr(s)))
}

fn parse_addr(v: &V) -> R<IpAddr> {
    match v {
        V::Str(s) => s.parse::<IpAddr>().map_err(|_| not_addr(s, "address")),
        V::Int(i) if *i >= 0 && *i <= u32::MAX as i64 => Ok(IpAddr::V4((*i as u32).into())),
        V::Int(i) if *i >= 0 => Ok(IpAddr::V6((*i as u128).into())),
        V::Native(n) => match &**n {
            Native::IpAddr(a) => Ok(*a),
            _ => Err(not_addr(&ops::repr(v)?, "address")),
        },
        o => Err(not_addr(&ops::repr(o)?, "address")),
    }
}

/// `ipaddress.ip_address(x)`
pub fn ip_address(args: &[V]) -> R {
    let [x] = args else {
        return Err(Exc::type_error(format!("ip_address() takes 1 positional argument but {} were given", args.len())));
    };
    Ok(V::native(Native::IpAddr(parse_addr(x)?)))
}

fn bits(a: &IpAddr) -> (u128, u8) {
    match a {
        IpAddr::V4(x) => (u32::from(*x) as u128, 32),
        IpAddr::V6(x) => (u128::from(*x), 128),
    }
}

fn mask(len: u8, width: u8) -> u128 {
    if len == 0 {
        0
    } else {
        (u128::MAX << (128 - len as u32)) >> (128 - width as u32)
    }
}

/// `ipaddress.ip_network(address, strict=True)` (a prefix length after `/`; no netmask form)
pub fn ip_network(args: &[V], kwargs: &[(String, V)]) -> R {
    let x = args.first().or_else(|| kw(kwargs, "address")).ok_or_else(|| Exc::type_error("ip_network() missing 1 required positional argument: 'address'"))?;
    let strict = match args.get(1).or_else(|| kw(kwargs, "strict")) {
        Some(v) => ops::truthy(v)?,
        None => true,
    };
    let V::Str(s) = x else {
        return Err(Exc::type_error("py2axum: ipaddress.ip_network() of a non-string is not supported"));
    };
    let (addr, plen) = match s.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (&**s, None),
    };
    let ip = addr.parse::<IpAddr>().map_err(|_| not_addr(s, "network"))?;
    let width = bits(&ip).1;
    let len = match plen {
        None => width,
        Some(p) if !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()) => match p.parse::<u8>() {
            Ok(n) if n <= width => n,
            _ => return Err(not_addr(s, "network")),
        },
        Some(p) if p.contains('.') || p.contains(':') => return Err(Exc::type_error("py2axum: ipaddress.ip_network() with a netmask is not supported (use a prefix length)")),
        Some(_) => return Err(not_addr(s, "network")),
    };
    let (n, w) = bits(&ip);
    let m = mask(len, w);
    let net = n & m;
    if net != n && strict {
        return Err(Exc::value_error(format!("{s} has host bits set")));
    }
    let base = match ip {
        IpAddr::V4(_) => IpAddr::V4((net as u32).into()),
        IpAddr::V6(_) => IpAddr::V6(net.into()),
    };
    Ok(V::native(Native::IpNet(base, len)))
}

pub fn contains(net: &(IpAddr, u8), item: &V) -> bool {
    let V::Native(n) = item else { return false };
    let Native::IpAddr(a) = &**n else { return false };
    if a.is_ipv4() != net.0.is_ipv4() {
        return false;
    }
    let (x, w) = bits(a);
    x & mask(net.1, w) == bits(&net.0).0
}

pub fn addr_str(a: &IpAddr) -> String {
    a.to_string()
}

pub fn net_str(n: &(IpAddr, u8)) -> String {
    format!("{}/{}", n.0, n.1)
}

pub fn addr_repr(a: &IpAddr) -> String {
    format!("IPv{}Address('{a}')", if a.is_ipv4() { 4 } else { 6 })
}

pub fn net_repr(n: &(IpAddr, u8)) -> String {
    format!("IPv{}Network('{}')", if n.0.is_ipv4() { 4 } else { 6 }, net_str(n))
}

// ---------------------------------------------------------------- asyncio streams

/// The socket of `asyncio.open_connection()`: its `StreamReader` and `StreamWriter` share it.
pub struct Stream {
    rd: tokio::sync::Mutex<Option<tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>>>,
    wr: tokio::sync::Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
    /// `writer.write()` buffers; `drain()` and the close send it
    buf: parking_lot::Mutex<Vec<u8>>,
    closing: std::sync::atomic::AtomicBool,
    eof: std::sync::atomic::AtomicBool,
    peer: V,
    sock: V,
}

fn sockaddr(a: &std::net::SocketAddr) -> V {
    match a {
        std::net::SocketAddr::V4(a) => V::tuple(vec![V::str(a.ip().to_string()), V::Int(a.port() as i64)]),
        std::net::SocketAddr::V6(a) => V::tuple(vec![
            V::str(a.ip().to_string()),
            V::Int(a.port() as i64),
            V::Int(a.flowinfo() as i64),
            V::Int(a.scope_id() as i64),
        ]),
    }
}

/// CPython's `OSError(errno, text)`: the subclass its errno selects, `[Errno n] text`
fn os_error(e: &std::io::Error, text: &str) -> Exc {
    let errno = e.raw_os_error().unwrap_or(0);
    let class: &'static Class = match errno {
        libc::ECONNREFUSED => &CONNECTION_REFUSED_ERROR,
        libc::ECONNRESET => &CONNECTION_RESET_ERROR,
        libc::ECONNABORTED => &CONNECTION_ABORTED_ERROR,
        libc::EPIPE => &BROKEN_PIPE_ERROR,
        libc::ETIMEDOUT => &TIMEOUT_ERROR,
        _ => &OS_ERROR,
    };
    Exc::msg(class, format!("[Errno {errno}] {text}"))
}

/// `await asyncio.open_connection(host, port)`: the addresses of `getaddrinfo(host, port, type=SOCK_STREAM)`
/// tried in order, like asyncio's `create_connection` (no happy eyeballs); one failure is raised as is,
/// several as `OSError("Multiple exceptions: ...")` unless they all read the same. A failure reads
/// `[Errno n] Connect call failed (addr)` under asyncio, `[Errno n] <strerror>` under uvloop (`uvloop`).
pub async fn open_connection(args: &[V], kwargs: &[(String, V)], uvloop: bool) -> R {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "host" | "port")) {
        return Err(Exc::type_error(format!("py2axum: asyncio.open_connection({k}=) is not supported (host, port)")));
    }
    let host = args.first().or_else(|| kw(kwargs, "host")).cloned().unwrap_or(V::None);
    let port = args.get(1).or_else(|| kw(kwargs, "port")).cloned().unwrap_or(V::None);
    let infos = getaddrinfo(&[host, port, V::Int(0), V::Int(libc::SOCK_STREAM as i64)], &[])?;
    let mut errors: Vec<Exc> = Vec::new();
    for info in ops::iter(&infos)? {
        let V::Tuple(t) = &info else { continue };
        let V::Tuple(sa) = &t[4] else { continue };
        let ip: IpAddr = match &sa[0] {
            V::Str(s) => s.parse().map_err(|_| Exc::runtime("getaddrinfo returned a bad address"))?,
            _ => continue,
        };
        let port = match &sa[1] {
            V::Int(p) => *p as u16,
            _ => 0,
        };
        let addr = match ip {
            IpAddr::V4(a) => std::net::SocketAddr::V4(std::net::SocketAddrV4::new(a, port)),
            IpAddr::V6(a) => {
                let (flow, scope) = match (sa.get(2), sa.get(3)) {
                    (Some(V::Int(f)), Some(V::Int(s))) => (*f as u32, *s as u32),
                    _ => (0, 0),
                };
                std::net::SocketAddr::V6(std::net::SocketAddrV6::new(a, port, flow, scope))
            }
        };
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => {
                let peer = s.peer_addr().map(|a| sockaddr(&a)).unwrap_or(V::None);
                let sock = s.local_addr().map(|a| sockaddr(&a)).unwrap_or(V::None);
                let (r, w) = s.into_split();
                let st = Arc::new(Stream {
                    rd: tokio::sync::Mutex::new(Some(tokio::io::BufReader::new(r))),
                    wr: tokio::sync::Mutex::new(Some(w)),
                    buf: parking_lot::Mutex::new(Vec::new()),
                    closing: Default::default(),
                    eof: Default::default(),
                    peer,
                    sock,
                });
                return Ok(V::tuple(vec![V::native(Native::Stream(st.clone(), false)), V::native(Native::Stream(st, true))]));
            }
            Err(e) if uvloop => errors.push(os_error(&e, &strerror(e.raw_os_error().unwrap_or(0)))),
            Err(e) => errors.push(os_error(&e, &format!("Connect call failed {}", ops::repr(&V::Tuple(sa.clone()))?))),
        }
    }
    match errors.len() {
        0 => Err(Exc::msg(&OS_ERROR, "getaddrinfo() returned empty list")),
        1 => Err(errors.remove(0)),
        _ => {
            let texts: Vec<String> = errors.iter().map(|e| e.message()).collect();
            if texts.iter().all(|t| *t == texts[0]) {
                return Err(errors.remove(0));
            }
            Err(Exc::msg(&OS_ERROR, format!("Multiple exceptions: {}", texts.join(", "))))
        }
    }
}

async fn flush(st: &Stream) -> R<()> {
    use tokio::io::AsyncWriteExt;
    let data = std::mem::take(&mut *st.buf.lock());
    let mut w = st.wr.lock().await;
    if let (Some(w), false) = (w.as_mut(), data.is_empty()) {
        w.write_all(&data).await.map_err(|e| os_error(&e, &e.to_string()))?;
    }
    Ok(())
}

/// `StreamReader` (`writer` false) and `StreamWriter` methods
pub async fn stream_method(st: &Arc<Stream>, writer: bool, name: &str, args: &[V]) -> R {
    use std::sync::atomic::Ordering::SeqCst;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    match (writer, name) {
        (true, "write") => {
            match args.first() {
                Some(V::Bytes(b)) => st.buf.lock().extend_from_slice(b),
                Some(o) => return Err(Exc::type_error(format!("data argument must be a bytes-like object, not '{}'", o.type_name()))),
                None => return Err(Exc::type_error("StreamWriter.write() missing 1 required positional argument: 'data'")),
            }
            Ok(V::None)
        }
        (true, "drain") => {
            flush(st).await?;
            Ok(V::None)
        }
        (true, "is_closing") => Ok(V::Bool(st.closing.load(SeqCst))),
        (true, "close") => {
            if !st.closing.swap(true, SeqCst) {
                // the transport sends what is buffered, then closes (in the background, as asyncio)
                let st = st.clone();
                tokio::spawn(async move {
                    let _ = flush(&st).await;
                    if let Some(mut w) = st.wr.lock().await.take() {
                        let _ = tokio::io::AsyncWriteExt::shutdown(&mut w).await;
                    }
                    st.rd.lock().await.take();
                });
            }
            Ok(V::None)
        }
        (true, "wait_closed") => {
            if st.closing.load(SeqCst) {
                let _ = flush(st).await;
                if let Some(mut w) = st.wr.lock().await.take() {
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut w).await;
                }
                st.rd.lock().await.take();
            }
            Ok(V::None)
        }
        (true, "get_extra_info") => Ok(match args.first() {
            Some(V::Str(s)) if &**s == "peername" => st.peer.clone(),
            Some(V::Str(s)) if &**s == "sockname" => st.sock.clone(),
            _ => args.get(1).cloned().unwrap_or(V::None),
        }),
        (false, "at_eof") => Ok(V::Bool(st.eof.load(SeqCst))),
        (false, "read") => {
            let n = match args.first() {
                None => -1,
                Some(V::Int(n)) => *n,
                Some(o) => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
            };
            let mut g = st.rd.lock().await;
            let Some(r) = g.as_mut() else { return Ok(V::Bytes(Arc::from(&b""[..]))) };
            let mut out = Vec::new();
            if n < 0 {
                r.read_to_end(&mut out).await.map_err(|e| os_error(&e, &e.to_string()))?;
                st.eof.store(true, SeqCst);
            } else if n > 0 {
                out.resize(n as usize, 0);
                let got = r.read(&mut out).await.map_err(|e| os_error(&e, &e.to_string()))?;
                out.truncate(got);
                if got == 0 {
                    st.eof.store(true, SeqCst);
                }
            }
            Ok(V::Bytes(Arc::from(out)))
        }
        (false, "readline") => {
            let mut g = st.rd.lock().await;
            let Some(r) = g.as_mut() else { return Ok(V::Bytes(Arc::from(&b""[..]))) };
            let mut out = Vec::new();
            r.read_until(b'\n', &mut out).await.map_err(|e| os_error(&e, &e.to_string()))?;
            if !out.ends_with(b"\n") {
                st.eof.store(true, SeqCst);
            }
            Ok(V::Bytes(Arc::from(out)))
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if writer { "StreamWriter" } else { "StreamReader" }))),
    }
}

// ---------------------------------------------------------------- socket.create_connection

/// A connected TCP socket of `socket.create_connection()` (closed by `close()` or leaving `with`)
pub struct Sock {
    stream: parking_lot::Mutex<Option<std::net::TcpStream>>,
    peer: V,
    local: V,
}

fn strerror(errno: i32) -> String {
    unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) }.to_string_lossy().into_owned()
}

/// `socket.create_connection((host, port), timeout=None)`: the `getaddrinfo` addresses in order, the last
/// failure raised (CPython without `all_errors`); a connection that outlasts `timeout` is `TimeoutError("timed out")`.
pub async fn create_connection(args: &[V], kwargs: &[(String, V)]) -> R {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "address" | "timeout" | "source_address" | "all_errors")) {
        return Err(Exc::type_error(format!("create_connection() got an unexpected keyword argument '{k}'")));
    }
    let opt = |i: usize, name: &str| args.get(i).or_else(|| kw(kwargs, name)).cloned().unwrap_or(V::None);
    if !opt(2, "source_address").is_none() || ops::truthy(&opt(3, "all_errors"))? {
        return Err(Exc::type_error("py2axum: create_connection(source_address=, all_errors=True) is not supported"));
    }
    let addr = opt(0, "address");
    let parts = ops::iter(&addr)?;
    let [host, port] = parts.as_slice() else {
        return Err(Exc::type_error("py2axum: create_connection() needs a (host, port) address"));
    };
    let timeout = match opt(1, "timeout") {
        V::None => None,
        V::Int(n) => Some(n as f64),
        V::Float(f) => Some(f),
        o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer or float", o.type_name()))),
    };
    if timeout.is_some_and(|t| t < 0.0) {
        return Err(Exc::value_error("Timeout value out of range"));
    }
    if timeout == Some(0.0) {
        return Err(Exc::type_error("py2axum: create_connection(timeout=0) (a non-blocking socket) is not supported"));
    }
    let infos = getaddrinfo(&[host.clone(), port.clone(), V::Int(0), V::Int(libc::SOCK_STREAM as i64)], &[])?;
    let mut addrs = Vec::new();
    for info in ops::iter(&infos)? {
        let V::Tuple(t) = &info else { continue };
        let V::Tuple(sa) = &t[4] else { continue };
        let (V::Str(ip), V::Int(p)) = (&sa[0], &sa[1]) else { continue };
        let ip: IpAddr = ip.parse().map_err(|_| Exc::runtime("getaddrinfo returned a bad address"))?;
        addrs.push(std::net::SocketAddr::new(ip, *p as u16));
    }
    let res = tokio::task::spawn_blocking(move || {
        let mut last = None;
        for a in addrs {
            let r = match timeout {
                None => std::net::TcpStream::connect(a),
                Some(t) => std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_secs_f64(t)),
            };
            match r {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
        }
        Err(last)
    })
    .await
    .map_err(|e| Exc::runtime(e.to_string()))?;
    match res {
        Ok(s) => {
            let peer = s.peer_addr().map(|a| sockaddr(&a)).unwrap_or(V::None);
            let local = s.local_addr().map(|a| sockaddr(&a)).unwrap_or(V::None);
            Ok(V::native(Native::Socket(Arc::new(Sock { stream: parking_lot::Mutex::new(Some(s)), peer, local }))))
        }
        Err(None) => Err(Exc::msg(&OS_ERROR, "getaddrinfo returns an empty list")),
        Err(Some(e)) => Err(match e.raw_os_error() {
            Some(n) => os_error(&e, &strerror(n)),
            None if e.kind() == std::io::ErrorKind::TimedOut => Exc::msg(&TIMEOUT_ERROR, "timed out"),
            None => Exc::msg(&OS_ERROR, e.to_string()),
        }),
    }
}

pub fn sock_method(s: &Arc<Sock>, name: &str) -> R {
    match name {
        "close" | "__exit__" => {
            s.stream.lock().take();
            Ok(if name == "close" { V::None } else { V::Bool(false) })
        }
        "getpeername" => Ok(s.peer.clone()),
        "getsockname" => Ok(s.local.clone()),
        _ => Err(Exc::attr_error(format!("'socket' object has no attribute '{name}'"))),
    }
}
