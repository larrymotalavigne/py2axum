//! dnspython 2.8's stub resolver (`dns.resolver`): `Resolver()` configured from /etc/resolv.conf (or `filename=`,
//! or `configure=False`), its settings (nameservers, port, timeout, lifetime, search...), `resolve()` and the
//! module's `resolve()` on the default resolver: the same queries over UDP (TCP on truncation or `tcp=True`), the
//! same retries, backoff, timeouts and nameserver eviction (`_Resolution`), CNAME chaining, and the same
//! exceptions with their messages. Answers hold MX, TXT, A, AAAA, CNAME and NS rdata (`dns.rdtypes`); names are
//! ASCII (dnspython's IDNA codecs are not ported: a non-ASCII name is refused at run time).
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use super::ops;
use super::v::*;

// ---------------------------------------------------------------- names (dns.name)

#[derive(Clone, Debug)]
pub struct Name {
    pub labels: Vec<Vec<u8>>,
}

impl PartialEq for Name {
    /// dnspython compares names case-insensitively (ASCII)
    fn eq(&self, o: &Name) -> bool {
        self.labels.len() == o.labels.len() && self.labels.iter().zip(&o.labels).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

fn dexc(c: &'static Class, msg: &str) -> Exc {
    Exc::msg(c, msg)
}

const MSG_EMPTY_LABEL: &str = "A DNS label is empty.";
const MSG_BAD_ESCAPE: &str = "An escaped code in a text format of DNS name is invalid.";
const MSG_LABEL_TOO_LONG: &str = "A DNS label is > 63 octets long.";
const MSG_NAME_TOO_LONG: &str = "A DNS name is > 255 octets long.";

impl Name {
    pub fn root() -> Name {
        Name { labels: vec![vec![]] }
    }

    /// `Name(labels)`: `_validate_labels`
    pub fn new(labels: Vec<Vec<u8>>) -> R<Name> {
        let l = labels.len();
        let mut total = 0;
        let mut first_empty: Option<usize> = None;
        for (j, label) in labels.iter().enumerate() {
            total += label.len() + 1;
            if label.len() > 63 {
                return Err(dexc(&DNS_LABEL_TOO_LONG, MSG_LABEL_TOO_LONG));
            }
            if first_empty.is_none() && label.is_empty() {
                first_empty = Some(j);
            }
        }
        if total > 255 {
            return Err(dexc(&DNS_NAME_TOO_LONG, MSG_NAME_TOO_LONG));
        }
        if let Some(i) = first_empty {
            if i != l - 1 {
                return Err(dexc(&DNS_EMPTY_LABEL, MSG_EMPTY_LABEL));
            }
        }
        Ok(Name { labels })
    }

    pub fn is_absolute(&self) -> bool {
        self.labels.last().is_some_and(|l| l.is_empty())
    }

    /// `dns.name.from_text(text, origin)` of an ASCII text
    pub fn from_text(text: &str, origin: Option<&Name>) -> R<Name> {
        if !text.is_ascii() {
            return Err(Exc::type_error("py2axum: a non-ASCII DNS name (IDNA) is not supported"));
        }
        let mut text = text.as_bytes();
        let mut labels: Vec<Vec<u8>> = Vec::new();
        if text == b"@" {
            text = b"";
        }
        if !text.is_empty() {
            if text == b"." {
                return Name::new(vec![vec![]]);
            }
            let mut label: Vec<u8> = Vec::new();
            let mut escaping = false;
            let mut edigits = 0;
            let mut total: u32 = 0;
            for &c in text {
                if escaping {
                    if edigits == 0 {
                        if c.is_ascii_digit() {
                            total = (c - b'0') as u32;
                            edigits += 1;
                        } else {
                            label.push(c);
                            escaping = false;
                        }
                    } else {
                        if !c.is_ascii_digit() {
                            return Err(dexc(&DNS_BAD_ESCAPE, MSG_BAD_ESCAPE));
                        }
                        total = total * 10 + (c - b'0') as u32;
                        edigits += 1;
                        if edigits == 3 {
                            escaping = false;
                            if total > 255 {
                                // struct.pack("!B", total)
                                return Err(Exc::msg(&STRUCT_ERROR, "'B' format requires 0 <= number <= 255"));
                            }
                            label.push(total as u8);
                        }
                    }
                } else if c == b'.' {
                    if label.is_empty() {
                        return Err(dexc(&DNS_EMPTY_LABEL, MSG_EMPTY_LABEL));
                    }
                    labels.push(std::mem::take(&mut label));
                } else if c == b'\\' {
                    escaping = true;
                    edigits = 0;
                    total = 0;
                } else {
                    label.push(c);
                }
            }
            if escaping {
                return Err(dexc(&DNS_BAD_ESCAPE, MSG_BAD_ESCAPE));
            }
            labels.push(label);
        }
        if (labels.is_empty() || !labels.last().unwrap().is_empty()) && origin.is_some() {
            labels.extend(origin.unwrap().labels.iter().cloned());
        }
        Name::new(labels)
    }

    pub fn to_text(&self, omit_final_dot: bool) -> String {
        if self.labels.is_empty() {
            return "@".into();
        }
        if self.labels.len() == 1 && self.labels[0].is_empty() {
            return ".".into();
        }
        let l = if omit_final_dot && self.is_absolute() { &self.labels[..self.labels.len() - 1] } else { &self.labels[..] };
        l.iter().map(|x| escapify_label(x)).collect::<Vec<_>>().join(".")
    }

    /// `self.concatenate(root)` of a relative name
    fn absolute(&self) -> R<Name> {
        let mut labels = self.labels.clone();
        labels.push(vec![]);
        Name::new(labels)
    }

    fn to_wire(&self, out: &mut Vec<u8>) {
        for l in &self.labels {
            out.push(l.len() as u8);
            out.extend_from_slice(l);
        }
    }
}

fn escapify_label(label: &[u8]) -> String {
    let mut t = String::new();
    for &c in label {
        if b"\"().;\\@$".contains(&c) {
            t.push('\\');
            t.push(c as char);
        } else if c > 0x20 && c < 0x7f {
            t.push(c as char);
        } else {
            t += &format!("\\{c:03}");
        }
    }
    t
}

// ---------------------------------------------------------------- types (dns.rdatatype)

const TYPES: &[(&str, u16)] = &[
    ("NONE", 0), ("A", 1), ("NS", 2), ("MD", 3), ("MF", 4), ("CNAME", 5), ("SOA", 6), ("MB", 7), ("MG", 8), ("MR", 9),
    ("NULL", 10), ("WKS", 11), ("PTR", 12), ("HINFO", 13), ("MINFO", 14), ("MX", 15), ("TXT", 16), ("RP", 17),
    ("AFSDB", 18), ("X25", 19), ("ISDN", 20), ("RT", 21), ("NSAP", 22), ("NSAP_PTR", 23), ("SIG", 24), ("KEY", 25),
    ("PX", 26), ("GPOS", 27), ("AAAA", 28), ("LOC", 29), ("NXT", 30), ("SRV", 33), ("NAPTR", 35), ("KX", 36),
    ("CERT", 37), ("A6", 38), ("DNAME", 39), ("OPT", 41), ("APL", 42), ("DS", 43), ("SSHFP", 44), ("IPSECKEY", 45),
    ("RRSIG", 46), ("NSEC", 47), ("DNSKEY", 48), ("DHCID", 49), ("NSEC3", 50), ("NSEC3PARAM", 51), ("TLSA", 52),
    ("SMIMEA", 53), ("HIP", 55), ("NINFO", 56), ("CDS", 59), ("CDNSKEY", 60), ("OPENPGPKEY", 61), ("CSYNC", 62),
    ("ZONEMD", 63), ("SVCB", 64), ("HTTPS", 65), ("DSYNC", 66), ("SPF", 99), ("UNSPEC", 103), ("NID", 104),
    ("L32", 105), ("L64", 106), ("LP", 107), ("EUI48", 108), ("EUI64", 109), ("TKEY", 249), ("TSIG", 250),
    ("IXFR", 251), ("AXFR", 252), ("MAILB", 253), ("MAILA", 254), ("ANY", 255), ("URI", 256), ("CAA", 257),
    ("AVC", 258), ("AMTRELAY", 260), ("RESINFO", 261), ("WALLET", 262), ("TA", 32768), ("DLV", 32769),
];

fn type_text(t: u16) -> String {
    TYPES.iter().find(|(_, v)| *v == t).map(|(n, _)| n.replace('_', "-")).unwrap_or_else(|| format!("TYPE{t}"))
}

/// `dns.rdatatype.RdataType.make(value)`
fn make_type(v: &V) -> R<u16> {
    let t = match v {
        V::Str(s) => {
            let up = s.to_uppercase().replace('-', "_");
            match TYPES.iter().find(|(n, _)| *n == up) {
                Some((_, t)) => *t,
                None => match up.strip_prefix("TYPE").and_then(|d| if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) { d.parse::<u64>().ok() } else { None }) {
                    Some(n) if n <= 65535 => n as u16,
                    Some(_) => return Err(Exc::value_error("type must be an int between >= 0 and <= 65535")),
                    None => return Err(dexc(&DNS_UNKNOWN_RDATATYPE, "DNS resource record type is unknown.")),
                },
            }
        }
        V::Int(i) if (0..=65535).contains(i) => *i as u16,
        V::Int(_) => return Err(Exc::value_error("type must be an int between >= 0 and <= 65535")),
        other => return Err(Exc::type_error(format!("py2axum: a DNS record type given as a {} is not supported (str or int)", other.type_name()))),
    };
    Ok(t)
}

fn is_metatype(t: u16) -> bool {
    (128..256).contains(&t) || t == 41
}

// ---------------------------------------------------------------- rdata

#[derive(Clone, Debug)]
pub enum Rdata {
    Mx { preference: u16, exchange: Name },
    Txt { strings: Vec<Vec<u8>> },
    A(String),
    Aaaa(String),
    /// CNAME and NS: a name (`target`)
    Target(u16, Name),
}

impl PartialEq for Rdata {
    fn eq(&self, o: &Rdata) -> bool {
        match (self, o) {
            (Rdata::Mx { preference: a, exchange: x }, Rdata::Mx { preference: b, exchange: y }) => a == b && x == y,
            (Rdata::Txt { strings: a }, Rdata::Txt { strings: b }) => a == b,
            (Rdata::A(a), Rdata::A(b)) | (Rdata::Aaaa(a), Rdata::Aaaa(b)) => a == b,
            (Rdata::Target(t, a), Rdata::Target(u, b)) => t == u && a == b,
            _ => false,
        }
    }
}

fn rdata_escapify(s: &[u8]) -> String {
    let mut t = String::new();
    for &c in s {
        if c == b'"' || c == b'\\' {
            t.push('\\');
            t.push(c as char);
        } else if (0x20..0x7f).contains(&c) {
            t.push(c as char);
        } else {
            t += &format!("\\{c:03}");
        }
    }
    t
}

impl Rdata {
    pub fn rdtype(&self) -> u16 {
        match self {
            Rdata::Mx { .. } => 15,
            Rdata::Txt { .. } => 16,
            Rdata::A(_) => 1,
            Rdata::Aaaa(_) => 28,
            Rdata::Target(t, _) => *t,
        }
    }

    pub fn to_text(&self) -> String {
        match self {
            Rdata::Mx { preference, exchange } => format!("{preference} {}", exchange.to_text(false)),
            Rdata::Txt { strings } => strings.iter().map(|s| format!("\"{}\"", rdata_escapify(s))).collect::<Vec<_>>().join(" "),
            Rdata::A(a) | Rdata::Aaaa(a) => a.clone(),
            Rdata::Target(_, n) => n.to_text(false),
        }
    }
}

// ---------------------------------------------------------------- wire format (dns.message)

const QR: u16 = 0x8000;
const TC: u16 = 0x0200;

#[derive(Clone, Debug)]
struct Rr {
    name: Name,
    rdtype: u16,
    rdclass: u16,
    rdata: Option<Rdata>,
}

#[derive(Clone, Debug)]
struct RRset {
    name: Name,
    rdtype: u16,
    rdclass: u16,
    rdatas: Vec<Rdata>,
    /// records of a type whose rdata is not ported (skipped by their length)
    opaque: bool,
}

#[derive(Clone, Debug)]
pub struct Message {
    id: u16,
    flags: u16,
    rcode: u16,
    question: Vec<(Name, u16, u16)>,
    answer: Vec<RRset>,
}

enum WireErr {
    /// a dns.exception.FormError subclass (its class and message)
    Form(&'static Class, &'static str),
}

struct Reader<'a> {
    w: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn need(&self, n: usize) -> Result<(), WireErr> {
        if self.i + n > self.w.len() {
            Err(WireErr::Form(&DNS_FORM_ERROR, "DNS message is malformed."))
        } else {
            Ok(())
        }
    }
    fn u8(&mut self) -> Result<u8, WireErr> {
        self.need(1)?;
        self.i += 1;
        Ok(self.w[self.i - 1])
    }
    fn u16(&mut self) -> Result<u16, WireErr> {
        self.need(2)?;
        self.i += 2;
        Ok(u16::from_be_bytes([self.w[self.i - 2], self.w[self.i - 1]]))
    }
    fn u32(&mut self) -> Result<u32, WireErr> {
        self.need(4)?;
        self.i += 4;
        Ok(u32::from_be_bytes(self.w[self.i - 4..self.i].try_into().unwrap()))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], WireErr> {
        self.need(n)?;
        self.i += n;
        Ok(&self.w[self.i - n..self.i])
    }

    /// `Parser.get_name()`: labels and backward compression pointers
    fn name(&mut self) -> Result<Name, WireErr> {
        let mut labels: Vec<Vec<u8>> = Vec::new();
        let mut biggest_pointer = self.i;
        let mut saved: Option<usize> = None;
        loop {
            let count = self.u8()?;
            if count < 64 {
                if count == 0 {
                    labels.push(vec![]);
                    break;
                }
                labels.push(self.bytes(count as usize)?.to_vec());
            } else if count >= 192 {
                let lo = self.u8()?;
                let current = ((count as usize & 0x3f) << 8) | lo as usize;
                if current >= biggest_pointer {
                    return Err(WireErr::Form(&DNS_BAD_POINTER, "A DNS compression pointer points forward instead of backward."));
                }
                biggest_pointer = current;
                if saved.is_none() {
                    saved = Some(self.i);
                }
                self.i = current;
            } else {
                return Err(WireErr::Form(&DNS_BAD_LABEL_TYPE, "The label type in DNS name wire format is unknown."));
            }
        }
        if let Some(s) = saved {
            self.i = s;
        }
        Name::new(labels).map_err(|_| WireErr::Form(&DNS_NAME_TOO_LONG, MSG_NAME_TOO_LONG))
    }
}

fn form() -> WireErr {
    WireErr::Form(&DNS_FORM_ERROR, "DNS message is malformed.")
}

fn parse_rdata(r: &mut Reader, rdtype: u16, rdlen: usize) -> Result<Option<Rdata>, WireErr> {
    let end = r.i + rdlen;
    r.need(rdlen)?;
    let rd = match rdtype {
        15 => {
            let preference = r.u16()?;
            let exchange = r.name()?;
            Some(Rdata::Mx { preference, exchange })
        }
        16 => {
            let mut strings = Vec::new();
            while r.i < end {
                let n = r.u8()? as usize;
                if r.i + n > end {
                    return Err(form());
                }
                strings.push(r.bytes(n)?.to_vec());
            }
            if strings.is_empty() {
                return Err(WireErr::Form(&DNS_FORM_ERROR, "DNS message is malformed."));
            }
            Some(Rdata::Txt { strings })
        }
        1 => {
            if rdlen != 4 {
                return Err(form());
            }
            let b = r.bytes(4)?;
            Some(Rdata::A(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])))
        }
        28 => {
            if rdlen != 16 {
                return Err(form());
            }
            let b: [u8; 16] = r.bytes(16)?.try_into().unwrap();
            Some(Rdata::Aaaa(ipv6_text(&b)))
        }
        5 | 2 => {
            let n = r.name()?;
            Some(Rdata::Target(rdtype, n))
        }
        _ => {
            r.i = end;
            None
        }
    };
    if r.i != end {
        return Err(form());
    }
    Ok(rd)
}

/// dns.ipv6.inet_ntoa: the longest run of zero groups (at least two) shortened, an IPv4 suffix for ::ffff: and ::
fn ipv6_text(b: &[u8; 16]) -> String {
    let addr = std::net::Ipv6Addr::from(*b);
    let s = addr.segments();
    // dnspython writes IPv4-mapped and IPv4-compatible (non-trivial) addresses with a dotted quad
    if s[..5].iter().all(|x| *x == 0) && (s[5] == 0xffff || s[5] == 0 && (s[6] != 0 || s[7] > 1)) {
        let v4 = format!("{}.{}.{}.{}", b[12], b[13], b[14], b[15]);
        return if s[5] == 0xffff { format!("::ffff:{v4}") } else { format!("::{v4}") };
    }
    let hex: Vec<String> = s.iter().map(|x| format!("{x:x}")).collect();
    // longest run of "0" groups
    let (mut best_start, mut best_len, mut start, mut len) = (0usize, 0usize, 0usize, 0usize);
    for (i, h) in hex.iter().enumerate() {
        if h == "0" {
            if len == 0 {
                start = i;
            }
            len += 1;
            if len > best_len {
                best_len = len;
                best_start = start;
            }
        } else {
            len = 0;
        }
    }
    if best_len > 1 {
        let left = hex[..best_start].join(":");
        let right = hex[best_start + best_len..].join(":");
        format!("{left}::{right}")
    } else {
        hex.join(":")
    }
}

enum Parsed {
    Ok(Message),
    /// raise_on_truncation: the message had the TC flag
    Truncated(Message),
    Err(WireErr),
}

/// `dns.message.from_wire(wire, raise_on_truncation=...)`
fn from_wire(wire: &[u8], raise_on_truncation: bool) -> Parsed {
    if wire.len() < 12 {
        return Parsed::Err(WireErr::Form(&DNS_SHORT_HEADER, "The DNS packet passed to from_wire() is too short."));
    }
    let mut r = Reader { w: wire, i: 0 };
    let h: Vec<u16> = (0..6).map(|_| r.u16().ok().unwrap()).collect();
    let mut m = Message { id: h[0], flags: h[1], rcode: h[1] & 0xf, question: vec![], answer: vec![] };
    let res = (|| -> Result<(), WireErr> {
        for _ in 0..h[2] {
            let n = r.name()?;
            let t = r.u16()?;
            let c = r.u16()?;
            if !m.question.iter().any(|(qn, qt, qc)| *qn == n && *qt == t && *qc == c) {
                m.question.push((n, t, c));
            }
        }
        for section in 0..3 {
            let count = h[3 + section];
            for _ in 0..count {
                let name = r.name()?;
                let rdtype = r.u16()?;
                let rdclass = r.u16()?;
                let ttl = r.u32()?;
                let rdlen = r.u16()? as usize;
                if rdtype == 41 {
                    // OPT: EDNS, only in the additional section; its ttl carries the extended rcode
                    if section != 2 || !name.labels.iter().all(|l| l.is_empty()) {
                        return Err(WireErr::Form(&DNS_BAD_EDNS, "An OPT record occurred somewhere other than\nthe additional data section."));
                    }
                    m.rcode = (((ttl >> 24) & 0xff) as u16) << 4 | (m.flags & 0xf);
                    r.bytes(rdlen)?;
                    continue;
                }
                let rdata = parse_rdata(&mut r, rdtype, rdlen)?;
                if section == 0 {
                    let rr = Rr { name, rdtype, rdclass, rdata };
                    let set = m.answer.iter_mut().find(|s| s.name == rr.name && s.rdtype == rr.rdtype && s.rdclass == rr.rdclass);
                    match (set, rr.rdata) {
                        (Some(s), Some(d)) => {
                            if !s.rdatas.contains(&d) {
                                s.rdatas.push(d);
                            }
                        }
                        (Some(s), None) => s.opaque = true,
                        (None, d) => {
                            let opaque = d.is_none();
                            m.answer.push(RRset { name: rr.name, rdtype: rr.rdtype, rdclass: rr.rdclass, rdatas: d.into_iter().collect(), opaque })
                        }
                    }
                }
            }
        }
        if r.i != wire.len() {
            return Err(WireErr::Form(&DNS_TRAILING_JUNK, "The DNS packet passed to from_wire() has extra junk at the end of it."));
        }
        Ok(())
    })();
    let tc = m.flags & TC != 0;
    match res {
        Err(_) if tc && raise_on_truncation => Parsed::Truncated(m),
        Err(e) => Parsed::Err(e),
        Ok(()) if tc && raise_on_truncation => Parsed::Truncated(m),
        Ok(()) => Parsed::Ok(m),
    }
}

/// `query.is_response(other)`
fn is_response(q: &Message, r: &Message) -> bool {
    if r.flags & QR == 0 || q.id != r.id || (q.flags >> 11) & 0xf != (r.flags >> 11) & 0xf {
        return false;
    }
    if matches!(r.rcode, 1 | 2 | 4 | 5) && r.question.is_empty() {
        return true;
    }
    let has = |set: &[(Name, u16, u16)], x: &(Name, u16, u16)| set.iter().any(|y| y.0 == x.0 && y.1 == x.1 && y.2 == x.2);
    q.question.iter().all(|x| has(&r.question, x)) && r.question.iter().all(|x| has(&q.question, x))
}

fn rcode_text(r: u16) -> String {
    match r {
        0 => "NOERROR", 1 => "FORMERR", 2 => "SERVFAIL", 3 => "NXDOMAIN", 4 => "NOTIMP", 5 => "REFUSED", 6 => "YXDOMAIN",
        7 => "YXRRSET", 8 => "NXRRSET", 9 => "NOTAUTH", 10 => "NOTZONE", 11 => "DSOTYPENI", 16 => "BADVERS", 17 => "BADKEY",
        18 => "BADTIME", 19 => "BADMODE", 20 => "BADNAME", 21 => "BADALG", 22 => "BADTRUNC", 23 => "BADCOOKIE",
        n => return n.to_string(),
    }
    .to_string()
}

// ---------------------------------------------------------------- resolver

struct Conf {
    nameservers: V,
    nameserver_ports: V,
    port: V,
    search: Vec<Name>,
    domain: Name,
    use_search_by_default: V,
    timeout: V,
    lifetime: V,
    edns: V,
    ednsflags: V,
    payload: V,
    flags: V,
    retry_servfail: V,
    rotate: V,
    ndots: V,
}

pub struct Resolver {
    c: Mutex<Conf>,
}

/// `BaseResolver.reset()`
fn reset() -> Conf {
    let host = super::sentry::hostname().unwrap_or_default();
    let domain = match Name::from_text(&host, Some(&Name::root())) {
        Ok(n) if n.labels.len() > 1 => Name { labels: n.labels[1..].to_vec() },
        _ => Name::root(),
    };
    Conf {
        nameservers: V::list(vec![]),
        nameserver_ports: V::dict_from(vec![]).unwrap(),
        port: V::Int(53),
        search: vec![],
        domain,
        use_search_by_default: V::Bool(false),
        timeout: V::Float(2.0),
        lifetime: V::Float(5.0),
        edns: V::Int(-1),
        ednsflags: V::Int(0),
        payload: V::Int(0),
        flags: V::None,
        retry_servfail: V::Bool(false),
        rotate: V::Bool(false),
        ndots: V::None,
    }
}

/// `Resolver.read_resolv_conf(filename)`
fn read_resolv_conf(c: &mut Conf, filename: &str) -> R<()> {
    let text = std::fs::read(filename).map_err(|_| dexc(&DNS_NO_RESOLVER_CONFIGURATION, &format!("cannot open {filename}")))?;
    let text = String::from_utf8(text).map_err(|e| {
        Exc::msg(&UNICODE_DECODE_ERROR, format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
            e.as_bytes()[e.utf8_error().valid_up_to()], e.utf8_error().valid_up_to()))
    })?;
    let mut nameservers = Vec::new();
    for line in text.split_inclusive('\n') {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let tokens: Vec<&str> = line.split(super::methods::py_space).filter(|t| !t.is_empty()).collect();
        if tokens.len() < 2 {
            continue;
        }
        match tokens[0] {
            "nameserver" => nameservers.push(V::str(tokens[1])),
            "domain" => {
                c.domain = Name::from_text(tokens[1], Some(&Name::root()))?;
                c.search = vec![];
            }
            "search" => {
                c.search = vec![];
                for s in &tokens[1..] {
                    c.search.push(Name::from_text(s, Some(&Name::root()))?);
                }
            }
            "options" => {
                for opt in &tokens[1..] {
                    let num = || opt.split(':').nth(1).and_then(|x| py_int(x));
                    if *opt == "rotate" {
                        c.rotate = V::Bool(true);
                    } else if *opt == "edns0" {
                        c.edns = V::Int(0);
                        c.ednsflags = V::Int(0);
                        c.payload = V::Int(1232);
                    } else if opt.contains("timeout") {
                        if let Some(n) = num() {
                            c.timeout = V::Int(n);
                        }
                    } else if opt.contains("ndots") {
                        if let Some(n) = num() {
                            c.ndots = V::Int(n);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if nameservers.is_empty() {
        return Err(dexc(&DNS_NO_RESOLVER_CONFIGURATION, "no nameservers"));
    }
    c.nameservers = V::list(nameservers);
    Ok(())
}

/// `int(text)` (Python's: blanks and underscores between digits allowed)
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim_matches(super::methods::py_space);
    let (neg, d) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if d.is_empty() || d.starts_with('_') || d.ends_with('_') || d.contains("__") || !d.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return None;
    }
    let n: i64 = d.replace('_', "").parse().ok()?;
    Some(if neg { -n } else { n })
}

/// `dns.resolver.Resolver(filename='/etc/resolv.conf', configure=True)`
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    let mut filename = args.first().cloned().unwrap_or(V::str("/etc/resolv.conf"));
    let mut configure = args.get(1).cloned().unwrap_or(V::Bool(true));
    if args.len() > 2 {
        return Err(Exc::type_error(format!("Resolver.__init__() takes from 1 to 3 positional arguments but {} were given", args.len() + 1)));
    }
    for (k, v) in kwargs {
        match k.as_str() {
            "filename" => filename = v.clone(),
            "configure" => configure = v.clone(),
            _ => return Err(Exc::type_error(format!("BaseResolver.__init__() got an unexpected keyword argument '{k}'"))),
        }
    }
    let mut c = reset();
    if ops::truthy(&configure)? && ops::truthy(&filename)? {
        let f = match &filename {
            V::Str(s) => s.to_string(),
            _ => return Err(Exc::type_error("py2axum: Resolver(filename=) must be a path (str)")),
        };
        read_resolv_conf(&mut c, &f)?;
    }
    Ok(V::native(Native::DnsResolver(Arc::new(Resolver { c: Mutex::new(c) }))))
}

static DEFAULT: Mutex<Option<Arc<Resolver>>> = Mutex::new(None);

/// `dns.resolver.get_default_resolver()`
pub fn get_default_resolver() -> R {
    let mut d = DEFAULT.lock();
    if d.is_none() {
        let r = new(&[], &[])?;
        if let V::Native(n) = &r {
            if let Native::DnsResolver(r) = &**n {
                *d = Some(r.clone());
            }
        }
    }
    Ok(V::native(Native::DnsResolver(d.clone().unwrap())))
}

/// `dns.resolver.reset_default_resolver()`
pub fn reset_default_resolver() -> R {
    let r = new(&[], &[])?;
    if let V::Native(n) = &r {
        if let Native::DnsResolver(r) = &**n {
            *DEFAULT.lock() = Some(r.clone());
        }
    }
    Ok(V::None)
}

/// `dns.resolver.resolve(...)`: on the default resolver
pub async fn resolve_default(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let r = get_default_resolver()?;
    let V::Native(n) = &r else { unreachable!() };
    let Native::DnsResolver(r) = &**n else { unreachable!() };
    resolve(r, args, kwargs).await
}

/// a nameserver of `_enrich_nameservers`: its address and port
#[derive(Clone)]
struct Ns {
    addr: IpAddr,
    text: String,
    port: u16,
}

impl Ns {
    fn label(&self) -> String {
        format!("Do53:{}@{}", self.text, self.port)
    }
}

fn type_repr(v: &V) -> String {
    format!("<class '{}'>", v.type_name())
}

/// `dns.inet.is_address(text)`
fn is_address(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok()
}

/// `_enrich_nameservers(nameservers, nameserver_ports, port)`
fn enrich(nameservers: &V, ports: &V, default_port: &V) -> R<Vec<Ns>> {
    let items = match nameservers {
        V::List(_) | V::Tuple(_) => ops::iter(nameservers)?,
        other => return Err(Exc::value_error(format!("nameservers must be a list or tuple (not a {})", type_repr(other)))),
    };
    let mut out = Vec::new();
    for ns in items {
        let s = match &ns {
            V::Str(s) => s.to_string(),
            _ => return Err(Exc::type_error("py2axum: a nameserver given as an object (dns.nameserver.Nameserver) is not supported")),
        };
        match is_address(&s) {
            Some(addr) => {
                let port = match ops::getitem(ports, &ns) {
                    Ok(p) => p,
                    Err(_) => default_port.clone(),
                };
                let port = match port {
                    V::Int(p) if (0..=65535).contains(&p) => p as u16,
                    _ => return Err(Exc::type_error("py2axum: a nameserver port must be an int in 0..65535")),
                };
                out.push(Ns { addr, text: s, port });
            }
            None => {
                if s.starts_with("https:") || s.to_ascii_lowercase().starts_with("https:") {
                    return Err(Exc::type_error("py2axum: DNS over HTTPS nameservers are not supported"));
                }
                return Err(Exc::value_error(format!(
                    "nameserver {s} is not a dns.nameserver.Nameserver instance or text form, IP address, nor a valid https URL"
                )));
            }
        }
    }
    Ok(out)
}

fn num(v: &V, what: &str) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Float(f) => Ok(*f),
        V::Bool(b) => Ok(*b as i64 as f64),
        _ => Err(Exc::type_error(format!("py2axum: Resolver.{what} must be a number"))),
    }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// an error of the trace (`ErrorTuple`): the server and what it answered
struct ErrorTuple {
    server: String,
    text: String,
}

fn errors_text(errors: &[ErrorTuple]) -> String {
    errors.iter().map(|e| format!("Server {} answered {}", e.server, e.text)).collect::<Vec<_>>().join("; ")
}

fn lifetime_timeout(duration: f64, errors: &[ErrorTuple]) -> Exc {
    dexc(&DNS_LIFETIME_TIMEOUT, &format!("The resolution lifetime expired after {} seconds: {}", fmt3(duration), errors_text(errors)))
}

fn fmt3(x: f64) -> String {
    format!("{x:.3}")
}

/// What a query to one nameserver ended with (`nameserver.query` raising or returning)
enum Outcome {
    Response(Message),
    /// an exception of the query: (text, kind)
    Failed(String, Fail),
}

#[derive(PartialEq)]
enum Fail {
    /// FormError, EOFError, OSError, NotImplementedError: the nameserver is dropped
    Drop,
    Truncated,
    /// dns.exception.Timeout and others: retried
    Keep,
}

struct Query {
    wire: Vec<u8>,
    msg: Message,
}

fn make_query(qname: &Name, rdtype: u16, c: &Conf) -> R<Query> {
    use rand::Rng;
    let id: u16 = rand::thread_rng().gen();
    let flags: u16 = match &c.flags {
        V::None => 0x0100,
        V::Int(f) => *f as u16,
        _ => return Err(Exc::type_error("py2axum: Resolver.flags must be an int")),
    };
    let edns = match &c.edns {
        V::Bool(b) => if *b { 0 } else { -1 },
        V::None => -1,
        V::Int(e) => *e,
        _ => -1,
    };
    let mut w = Vec::new();
    w.extend_from_slice(&id.to_be_bytes());
    w.extend_from_slice(&flags.to_be_bytes());
    w.extend_from_slice(&1u16.to_be_bytes());
    w.extend_from_slice(&[0, 0, 0, 0]);
    w.extend_from_slice(&(if edns >= 0 { 1u16 } else { 0 }).to_be_bytes());
    qname.to_wire(&mut w);
    w.extend_from_slice(&rdtype.to_be_bytes());
    w.extend_from_slice(&1u16.to_be_bytes());
    if edns >= 0 {
        let payload = match &c.payload {
            V::Int(p) if *p > 0 => *p as u16,
            _ => 1232,
        };
        let ednsflags = match &c.ednsflags {
            V::Int(f) => *f as u32,
            _ => 0,
        };
        w.push(0);
        w.extend_from_slice(&41u16.to_be_bytes());
        w.extend_from_slice(&payload.to_be_bytes());
        w.extend_from_slice(&(((edns as u32 & 0xff) << 16) | (ednsflags & 0xffff)).to_be_bytes());
        w.extend_from_slice(&0u16.to_be_bytes());
    }
    Ok(Query { wire: w, msg: Message { id, flags, rcode: 0, question: vec![(qname.clone(), rdtype, 1)], answer: vec![] } })
}

fn timeout_text() -> String {
    "The DNS operation timed out.".into()
}

/// `dns.query.udp(..., raise_on_truncation=True, ignore_errors=True, ignore_unexpected=True)`
async fn query_udp(q: &Query, ns: &Ns, timeout: f64) -> Outcome {
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    let bind: SocketAddr = if ns.addr.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let sock = match tokio::net::UdpSocket::bind(bind).await {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(os_text(&e), Fail::Drop),
    };
    let dest = SocketAddr::new(ns.addr, ns.port);
    if let Err(e) = sock.send_to(&q.wire, dest).await {
        return Outcome::Failed(os_text(&e), Fail::Drop);
    }
    let mut buf = vec![0u8; 65535];
    loop {
        let r = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await;
        let (n, from) = match r {
            Err(_) => return Outcome::Failed(timeout_text(), Fail::Keep),
            Ok(Err(e)) => return Outcome::Failed(os_text(&e), Fail::Drop),
            Ok(Ok(x)) => x,
        };
        if from.ip() != dest.ip() || from.port() != dest.port() {
            continue;
        }
        match from_wire(&buf[..n], true) {
            Parsed::Ok(m) => {
                if !is_response(&q.msg, &m) {
                    continue;
                }
                return Outcome::Response(m);
            }
            Parsed::Truncated(m) => {
                if !is_response(&q.msg, &m) {
                    continue;
                }
                // str(Truncated(message=m)): '' (a parametrized DNSException without fmt)
                return Outcome::Failed(String::new(), Fail::Truncated);
            }
            Parsed::Err(_) => continue,
        }
    }
}

/// `dns.query.tcp(...)`
async fn query_tcp(q: &Query, ns: &Ns, timeout: f64) -> Outcome {
    use tokio::io::AsyncWriteExt;
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    let dest = SocketAddr::new(ns.addr, ns.port);
    let mut s = match tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(dest)).await {
        Err(_) => return Outcome::Failed(timeout_text(), Fail::Keep),
        Ok(Err(e)) => return Outcome::Failed(os_text(&e), Fail::Drop),
        Ok(Ok(s)) => s,
    };
    let mut msg = (q.wire.len() as u16).to_be_bytes().to_vec();
    msg.extend_from_slice(&q.wire);
    match tokio::time::timeout_at(deadline, s.write_all(&msg)).await {
        Err(_) => return Outcome::Failed(timeout_text(), Fail::Keep),
        Ok(Err(e)) => return Outcome::Failed(os_text(&e), Fail::Drop),
        Ok(Ok(())) => {}
    }
    let l = match read_exact(&mut s, 2, deadline).await {
        Ok(b) => u16::from_be_bytes([b[0], b[1]]) as usize,
        Err((t, f)) => return Outcome::Failed(t, f),
    };
    let wire = match read_exact(&mut s, l, deadline).await {
        Ok(b) => b,
        Err((t, f)) => return Outcome::Failed(t, f),
    };
    match from_wire(&wire, false) {
        Parsed::Ok(m) | Parsed::Truncated(m) => {
            if !is_response(&q.msg, &m) {
                return Outcome::Failed("A DNS query response does not respond to the question asked.".into(), Fail::Drop);
            }
            Outcome::Response(m)
        }
        Parsed::Err(WireErr::Form(_, text)) => Outcome::Failed(text.into(), Fail::Drop),
    }
}

async fn read_exact(s: &mut tokio::net::TcpStream, n: usize, deadline: tokio::time::Instant) -> Result<Vec<u8>, (String, Fail)> {
    use tokio::io::AsyncReadExt;
    let mut b = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match tokio::time::timeout_at(deadline, s.read(&mut b[got..])).await {
            Err(_) => return Err((timeout_text(), Fail::Keep)),
            Ok(Err(e)) => return Err((os_text(&e), Fail::Drop)),
            Ok(Ok(0)) => return Err(("EOF".to_string(), Fail::Drop)),
            Ok(Ok(k)) => got += k,
        }
    }
    Ok(b)
}

fn os_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => format!("[Errno {n}] {}", super::net::strerror(n)),
        None => e.to_string(),
    }
}

/// the answer of `resolve()` (`dns.resolver.Answer`)
pub struct Answer {
    pub qname: Name,
    pub canonical_name: Name,
    pub rrset: Option<Vec<Rdata>>,
}

/// `response.resolve_chaining()`: the answer RRset after the CNAME chain, and the canonical name
/// (canonical name, answer); an answer of records not ported: `Err(type)`
fn resolve_chaining(m: &Message) -> Result<(Name, Option<Result<Vec<Rdata>, u16>>), String> {
    if m.flags & QR == 0 {
        return Err("Message is not a response to a query.".into());
    }
    if m.question.len() != 1 {
        return Err("DNS message is malformed.".into());
    }
    let (mut qname, qtype, qclass) = m.question[0].clone();
    let mut answer = None;
    let mut count = 0;
    while count < 16 {
        if let Some(s) = m.answer.iter().find(|s| s.name == qname && s.rdclass == qclass && s.rdtype == qtype) {
            if s.opaque {
                return Ok((qname, Some(Err(qtype))));
            }
            answer = Some(s.rdatas.clone());
            break;
        }
        if qtype == 5 {
            break;
        }
        match m.answer.iter().find(|s| s.name == qname && s.rdclass == qclass && s.rdtype == 5) {
            Some(c) => {
                if let Some(Rdata::Target(_, t)) = c.rdatas.first() {
                    qname = t.clone();
                }
                count += 1;
            }
            None => break,
        }
    }
    if count >= 16 {
        return Err("The CNAME chain is too long.".into());
    }
    if m.rcode == 3 && answer.is_some() {
        return Err("The rcode is NXDOMAIN but an answer was found.".into());
    }
    Ok((qname, answer.map(Ok)))
}

fn question_text(q: &(Name, u16, u16)) -> String {
    format!("{} IN {}", q.0.to_text(false), type_text(q.1))
}

/// `Resolver.resolve(qname, rdtype='A', rdclass='IN', tcp=False, source=None, raise_on_no_answer=True,
/// source_port=0, lifetime=None, search=None)`
pub async fn resolve(r: &Arc<Resolver>, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    const PARAMS: [&str; 9] = ["qname", "rdtype", "rdclass", "tcp", "source", "raise_on_no_answer", "source_port", "lifetime", "search"];
    let mut vals: [Option<V>; 9] = Default::default();
    if args.len() > 9 {
        return Err(Exc::type_error(format!("Resolver.resolve() takes from 2 to 10 positional arguments but {} were given", args.len() + 1)));
    }
    for (i, a) in args.into_iter().enumerate() {
        vals[i] = Some(a);
    }
    for (k, v) in kwargs {
        match PARAMS.iter().position(|p| *p == k) {
            Some(i) if vals[i].is_none() => vals[i] = Some(v),
            Some(_) => return Err(Exc::type_error(format!("Resolver.resolve() got multiple values for argument '{k}'"))),
            None => return Err(Exc::type_error(format!("Resolver.resolve() got an unexpected keyword argument '{k}'"))),
        }
    }
    let qname = vals[0].take().ok_or_else(|| Exc::type_error("Resolver.resolve() missing 1 required positional argument: 'qname'"))?;
    let tcp = match vals[3].take() {
        Some(v) => ops::truthy(&v)?,
        None => false,
    };
    if vals[4].as_ref().is_some_and(|v| !v.is_none()) {
        return Err(Exc::type_error("py2axum: Resolver.resolve(source=) is not supported"));
    }
    if vals[6].as_ref().is_some_and(|v| !matches!(v, V::Int(0))) {
        return Err(Exc::type_error("py2axum: Resolver.resolve(source_port=) is not supported"));
    }
    let raise_on_no_answer = match vals[5].take() {
        Some(v) => ops::truthy(&v)?,
        None => true,
    };
    // _Resolution.__init__: the name, the type, the class
    let qname = match &qname {
        V::Str(s) => Name::from_text(s, None)?,
        V::Native(n) if matches!(&**n, Native::DnsName(_)) => match &**n {
            Native::DnsName(x) => (**x).clone(),
            _ => unreachable!(),
        },
        other => return Err(Exc::type_error(format!("py2axum: a DNS name given as a {} is not supported (str)", other.type_name()))),
    };
    let rdtype = make_type(vals[1].as_ref().unwrap_or(&V::str("A")))?;
    if is_metatype(rdtype) {
        return Err(dexc(&DNS_NO_METAQUERIES, "DNS metaqueries are not allowed."));
    }
    match vals[2].as_ref() {
        None => {}
        Some(V::Str(s)) if s.eq_ignore_ascii_case("IN") => {}
        Some(V::Int(1)) => {}
        Some(V::Str(s)) if s.eq_ignore_ascii_case("ANY") || s.eq_ignore_ascii_case("NONE") => {
            return Err(dexc(&DNS_NO_METAQUERIES, "DNS metaqueries are not allowed."))
        }
        Some(_) => return Err(Exc::type_error("py2axum: Resolver.resolve(rdclass=) supports IN only")),
    }
    let lifetime_arg = vals[7].take().filter(|v| !v.is_none());
    let search = vals[8].take().filter(|v| !v.is_none());
    let (qnames_to_try, lifetime) = {
        let c = r.c.lock();
        let search = match &search {
            Some(v) => ops::truthy(v)?,
            None => ops::truthy(&c.use_search_by_default)?,
        };
        let mut qnames = Vec::new();
        if qname.is_absolute() {
            qnames.push(qname.clone());
        } else {
            let abs = qname.absolute()?;
            if search {
                let list: Vec<Name> = if !c.search.is_empty() {
                    c.search.clone()
                } else if c.domain != Name::root() {
                    vec![c.domain.clone()]
                } else {
                    vec![]
                };
                let ndots = match &c.ndots {
                    V::None => 1,
                    V::Int(n) => *n,
                    _ => 1,
                };
                for suffix in list {
                    let mut l = qname.labels.clone();
                    l.extend(suffix.labels.iter().cloned());
                    qnames.push(Name::new(l)?);
                }
                if qname.labels.len() as i64 > ndots {
                    qnames.insert(0, abs);
                } else {
                    qnames.push(abs);
                }
            } else {
                qnames.push(abs);
            }
        }
        let lifetime = match &lifetime_arg {
            Some(v) => num(v, "lifetime")?,
            None => num(&c.lifetime, "lifetime")?,
        };
        (qnames, lifetime)
    };
    let start = now();
    let mut pending = qnames_to_try.clone();
    while !pending.is_empty() {
        let qn = pending.remove(0);
        // next_request: a query and the nameserver list, all retried in turn with backoff
        let (query, mut nameservers, timeout_setting, retry_servfail, rotate) = {
            let c = r.c.lock();
            let q = make_query(&qn, rdtype, &c)?;
            let ns = enrich(&c.nameservers, &c.nameserver_ports, &c.port)?;
            (q, ns, num(&c.timeout, "timeout")?, ops::truthy(&c.retry_servfail)?, ops::truthy(&c.rotate)?)
        };
        if rotate {
            use rand::seq::SliceRandom;
            nameservers.shuffle(&mut rand::thread_rng());
        }
        let mut current: Vec<Ns> = nameservers.clone();
        let mut errors: Vec<ErrorTuple> = Vec::new();
        let mut backoff_next = 0.10f64;
        let mut retry_with_tcp: Option<Ns> = None;
        'servers: loop {
            // next_nameserver
            let (ns, use_tcp, backoff) = if let Some(ns) = retry_with_tcp.take() {
                (ns, true, 0.0)
            } else {
                let mut backoff = 0.0;
                if current.is_empty() {
                    if nameservers.is_empty() {
                        return Err(dexc(&DNS_NO_NAMESERVERS, &format!(
                            "All nameservers failed to answer the query {}: {}", question_text(&query.msg.question[0]), errors_text(&errors))));
                    }
                    current = nameservers.clone();
                    backoff = backoff_next;
                    backoff_next = (backoff_next * 2.0).min(2.0);
                }
                (current.remove(0), tcp, backoff)
            };
            if backoff > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(backoff)).await;
            }
            // _compute_timeout
            let duration = now() - start;
            let duration = if duration < 0.0 {
                if duration < -1.0 {
                    return Err(lifetime_timeout(duration, &errors));
                }
                0.0
            } else {
                duration
            };
            if duration >= lifetime {
                return Err(lifetime_timeout(duration, &errors));
            }
            let timeout = (lifetime - duration).min(timeout_setting);
            let outcome = if use_tcp { query_tcp(&query, &ns, timeout).await } else { query_udp(&query, &ns, timeout).await };
            let remove = |list: &mut Vec<Ns>, ns: &Ns| {
                if let Some(i) = list.iter().position(|x| x.addr == ns.addr && x.port == ns.port && x.text == ns.text) {
                    list.remove(i);
                }
            };
            match outcome {
                Outcome::Failed(text, kind) => {
                    errors.push(ErrorTuple { server: ns.label(), text });
                    match kind {
                        Fail::Drop => remove(&mut nameservers, &ns),
                        Fail::Truncated => {
                            if use_tcp {
                                remove(&mut nameservers, &ns);
                            } else {
                                retry_with_tcp = Some(ns.clone());
                            }
                        }
                        Fail::Keep => {}
                    }
                    continue 'servers;
                }
                Outcome::Response(m) => {
                    // query_result
                    match m.rcode {
                        0 => match resolve_chaining(&m) {
                            Err(text) => {
                                errors.push(ErrorTuple { server: ns.label(), text });
                                remove(&mut nameservers, &ns);
                                continue 'servers;
                            }
                            Ok((_, Some(Err(t)))) => {
                                return Err(Exc::type_error(format!(
                                    "py2axum: DNS records of type {} are not supported (A, AAAA, CNAME, MX, NS, TXT)", type_text(t))));
                            }
                            Ok((canonical, rrset)) => {
                                let rrset = rrset.map(|r| r.unwrap_or_default());
                                if rrset.is_none() && raise_on_no_answer {
                                    return Err(dexc(&DNS_NO_ANSWER, &format!(
                                        "The DNS response does not contain an answer to the question: {}",
                                        m.question.first().map(question_text).unwrap_or_default())));
                                }
                                return Ok(V::native(Native::DnsAnswer(Arc::new(Answer { qname: qn, canonical_name: canonical, rrset }))));
                            }
                        },
                        3 => match resolve_chaining(&m) {
                            Err(text) => {
                                errors.push(ErrorTuple { server: ns.label(), text });
                                remove(&mut nameservers, &ns);
                                continue 'servers;
                            }
                            Ok(_) => break 'servers,
                        },
                        6 => {
                            return Err(dexc(&DNS_YXDOMAIN, "The DNS query name is too long after DNAME substitution."));
                        }
                        rc => {
                            if rc != 2 || !retry_servfail {
                                remove(&mut nameservers, &ns);
                            }
                            errors.push(ErrorTuple { server: ns.label(), text: rcode_text(rc) });
                            continue 'servers;
                        }
                    }
                }
            }
        }
    }
    let names: Vec<String> = qnames_to_try.iter().map(|n| n.to_text(false)).collect();
    Err(dexc(&DNS_NXDOMAIN, &format!(
        "{}: {}", if names.len() > 1 { "None of DNS query names exist" } else { "The DNS query name does not exist" }, names.join(", "))))
}

// ---------------------------------------------------------------- Python values

fn name_v(n: &Name) -> V {
    V::native(Native::DnsName(Arc::new(n.clone())))
}

pub fn resolver_attr(r: &Resolver, name: &str) -> R {
    let c = r.c.lock();
    Ok(match name {
        "nameservers" => c.nameservers.clone(),
        "nameserver_ports" => c.nameserver_ports.clone(),
        "port" => c.port.clone(),
        "search" => V::list(c.search.iter().map(name_v).collect()),
        "domain" => name_v(&c.domain),
        "use_search_by_default" => c.use_search_by_default.clone(),
        "timeout" => c.timeout.clone(),
        "lifetime" => c.lifetime.clone(),
        "edns" => c.edns.clone(),
        "ednsflags" => c.ednsflags.clone(),
        "payload" => c.payload.clone(),
        "flags" => c.flags.clone(),
        "retry_servfail" => c.retry_servfail.clone(),
        "rotate" => c.rotate.clone(),
        "ndots" => c.ndots.clone(),
        "cache" | "keyring" | "keyname" | "ednsoptions" => V::None,
        _ => return Err(Exc::attr_error(format!("'Resolver' object has no attribute '{name}'"))),
    })
}

pub fn resolver_setattr(r: &Resolver, name: &str, val: V) -> R<()> {
    let mut c = r.c.lock();
    match name {
        "nameservers" => {
            enrich(&val, &c.nameserver_ports, &c.port)?;
            c.nameservers = val;
        }
        "port" => match val {
            V::Int(p) if (0..=65535).contains(&p) => c.port = val,
            _ => return Err(Exc::type_error("py2axum: Resolver.port must be an int in 0..65535")),
        },
        "timeout" | "lifetime" => {
            num(&val, name)?;
            if name == "timeout" {
                c.timeout = val
            } else {
                c.lifetime = val
            }
        }
        "use_search_by_default" => c.use_search_by_default = val,
        "retry_servfail" => c.retry_servfail = val,
        "rotate" => c.rotate = val,
        "ndots" => match val {
            V::None | V::Int(_) => c.ndots = val,
            _ => return Err(Exc::type_error("py2axum: Resolver.ndots must be an int or None")),
        },
        "search" => {
            let mut out = Vec::new();
            for x in ops::iter(&val)? {
                match &x {
                    V::Native(n) if matches!(&**n, Native::DnsName(_)) => {
                        if let Native::DnsName(nm) = &**n {
                            out.push((**nm).clone());
                        }
                    }
                    _ => return Err(Exc::type_error("py2axum: Resolver.search must be a list of dns.name.Name")),
                }
            }
            c.search = out;
        }
        "nameserver_ports" => match val {
            V::Dict(_) => c.nameserver_ports = val,
            _ => return Err(Exc::type_error("py2axum: Resolver.nameserver_ports must be a dict")),
        },
        "cache" if val.is_none() => {}
        _ => return Err(Exc::type_error(format!("py2axum: setting Resolver.{name} is not supported"))),
    }
    Ok(())
}

/// `resolver.use_edns(edns=0, ednsflags=0, payload=1232, options=None)`
fn use_edns(r: &Resolver, args: &[V], kwargs: &[(String, V)]) -> R {
    let names = ["edns", "ednsflags", "payload", "options"];
    let mut vals: [Option<V>; 4] = Default::default();
    for (i, a) in args.iter().enumerate().take(4) {
        vals[i] = Some(a.clone());
    }
    for (k, v) in kwargs {
        match names.iter().position(|n| n == k) {
            Some(i) => vals[i] = Some(v.clone()),
            None => return Err(Exc::type_error(format!("BaseResolver.use_edns() got an unexpected keyword argument '{k}'"))),
        }
    }
    if vals[3].as_ref().is_some_and(|v| !v.is_none()) {
        return Err(Exc::type_error("py2axum: use_edns(options=) is not supported"));
    }
    let mut edns = vals[0].clone().unwrap_or(V::Int(0));
    if edns.is_none() || matches!(edns, V::Bool(false)) {
        edns = V::Int(-1);
    } else if matches!(edns, V::Bool(true)) {
        edns = V::Int(0);
    }
    let mut c = r.c.lock();
    c.edns = edns;
    c.ednsflags = vals[1].clone().unwrap_or(V::Int(0));
    c.payload = vals[2].clone().unwrap_or(V::Int(1232));
    Ok(V::None)
}

pub async fn resolver_method(r: &Arc<Resolver>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    match name {
        "resolve" => resolve(r, args, kwargs).await,
        "use_edns" => use_edns(r, &args, &kwargs),
        _ => Err(Exc::type_error(format!("py2axum: Resolver.{name}() is not supported (resolve, use_edns)"))),
    }
}

pub fn answer_attr(a: &Answer, name: &str) -> R {
    match name {
        "qname" => Ok(name_v(&a.qname)),
        "canonical_name" => Ok(name_v(&a.canonical_name)),
        _ => Err(Exc::type_error(format!("py2axum: Answer.{name} is not supported (iteration, len, [i], qname, canonical_name)"))),
    }
}

pub fn answer_items(a: &Answer) -> Vec<V> {
    a.rrset.as_ref().map(|s| s.iter().map(|d| V::native(Native::DnsRdata(Arc::new(d.clone())))).collect()).unwrap_or_default()
}

pub fn rdata_attr(d: &Rdata, name: &str) -> R {
    Ok(match (d, name) {
        (Rdata::Mx { preference, .. }, "preference") => V::Int(*preference as i64),
        (Rdata::Mx { exchange, .. }, "exchange") => name_v(exchange),
        (Rdata::Txt { strings }, "strings") => V::tuple(strings.iter().map(|s| V::Bytes(Arc::from(s.as_slice()))).collect()),
        (Rdata::A(a) | Rdata::Aaaa(a), "address") => V::str(a),
        (Rdata::Target(_, t), "target") => name_v(t),
        (_, "rdtype") => V::Int(d.rdtype() as i64),
        (_, "rdclass") => V::Int(1),
        _ => return Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", rdata_class(d)))),
    })
}

pub fn rdata_class(d: &Rdata) -> &'static str {
    match d {
        Rdata::Mx { .. } => "MX",
        Rdata::Txt { .. } => "TXT",
        Rdata::A(_) => "A",
        Rdata::Aaaa(_) => "AAAA",
        Rdata::Target(5, _) => "CNAME",
        Rdata::Target(..) => "NS",
    }
}

pub fn rdata_method(d: &Rdata, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "to_text" if args.is_empty() && kwargs.is_empty() => Ok(V::str(d.to_text())),
        _ => Err(Exc::type_error(format!("py2axum: {}.{name}() is not supported (to_text())", rdata_class(d)))),
    }
}

pub fn rdata_repr(d: &Rdata) -> String {
    format!("<DNS IN {} rdata: {}>", type_text(d.rdtype()), d.to_text())
}

pub fn name_method(n: &Name, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match name {
        "to_text" => {
            let omit = match (args.first(), kwargs.iter().find(|(k, _)| k == "omit_final_dot")) {
                (Some(v), None) | (None, Some((_, v))) => ops::truthy(v)?,
                (None, None) => false,
                _ => return Err(Exc::type_error("Name.to_text() got multiple values for argument 'omit_final_dot'")),
            };
            Ok(V::str(n.to_text(omit)))
        }
        "is_absolute" if args.is_empty() && kwargs.is_empty() => Ok(V::Bool(n.is_absolute())),
        _ => Err(Exc::type_error(format!("py2axum: Name.{name}() is not supported (to_text, is_absolute)"))),
    }
}

/// `dns.name.from_text(text, origin=dns.name.root)`
pub fn name_from_text(args: &[V], kwargs: &[(String, V)]) -> R {
    if args.len() > 2 || kwargs.iter().any(|(k, _)| k != "origin" && k != "text") {
        return Err(Exc::type_error("py2axum: dns.name.from_text(text, origin=root) only (no idna_codec)"));
    }
    let text = args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "text").map(|(_, v)| v));
    let origin = args.get(1).or_else(|| kwargs.iter().find(|(k, _)| k == "origin").map(|(_, v)| v));
    let origin = match origin {
        None => Some(Name::root()),
        Some(V::None) => None,
        Some(V::Native(n)) if matches!(&**n, Native::DnsName(_)) => match &**n {
            Native::DnsName(x) => Some((**x).clone()),
            _ => unreachable!(),
        },
        Some(_) => return Err(Exc::value_error("origin must be a Name or None")),
    };
    match text {
        Some(V::Str(s)) => Ok(name_v(&Name::from_text(s, origin.as_ref())?)),
        Some(V::Bytes(_)) => Err(Exc::type_error("py2axum: dns.name.from_text(bytes) is not supported")),
        Some(_) => Err(Exc::value_error("input to from_text() must be a string")),
        None => Err(Exc::type_error("from_text() missing 1 required positional argument: 'text'")),
    }
}

pub fn root_name() -> V {
    name_v(&Name::root())
}

pub fn empty_name() -> V {
    name_v(&Name { labels: vec![] })
}
