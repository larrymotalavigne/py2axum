//! `socket.getaddrinfo` (the C library's, like CPython) and `ipaddress` (`ip_address`, `ip_network`,
//! membership, str/repr).
use std::net::IpAddr;

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
