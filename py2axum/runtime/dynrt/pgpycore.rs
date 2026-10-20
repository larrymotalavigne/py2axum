//! pgpy 0.6.0's `PGPKey.from_blob()` (`pgpy/types.py`, `pgpy/pgp.py`, `pgpy/packet/`), ported observable for
//! observable: the ASCII armor regex (search semantics), binascii's non-strict base64, CRC-24 and its warning,
//! the packet stream as one Python `bytearray` (slices that never raise, `del`, negative bounds, `packet[i]` as the
//! only `IndexError`), every packet and subpacket class with the reads its `parse` makes (bounded or not), the
//! `MetaDispatchable` wrapping of errors in `PGPError(str(ex))`, the `itertools.groupby` of `PGPKey.parse` (the
//! packet read to close an orphaned group is lost), the `SorteDeque` insertion of signatures and user ids
//! (`bisect_left` with `PGPSignature.__lt__` / `PGPUID.__lt__`, `selfsig`), the fingerprint (SHA-1 over the
//! re-encoded key material). Nothing here depends on the rest of the runtime: `pgpy.rs` turns the result into
//! Python values. Not reproduced: bzip2 blocks inside a CompressedData packet (refused at run time, see
//! docs/supported.md), the address reuse of CPython's allocator in the group keys of `PGPKey.parse`.
use indexmap::IndexMap;
use num_bigint::BigUint;
use sha1::{Digest, Sha1};

// ---------------------------------------------------------------- errors

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ECls {
    Index,
    Value,
    Pgp,
    Type,
    Attribute,
    Stop,
    UEncode,
    UDecode,
    NotImpl,
    Assertion,
    Struct,
    Zlib,
    OS,
    Asn1,
    Underrun,
    Eos,
    EcFmt,
    Binascii,
    /// a path of pgpy the port does not follow (bzip2 data): never wrapped, raised as is
    Unsupported,
}

impl ECls {
    pub fn name(self) -> &'static str {
        match self {
            ECls::Index => "IndexError",
            ECls::Value => "ValueError",
            ECls::Pgp => "PGPError",
            ECls::Type => "TypeError",
            ECls::Attribute => "AttributeError",
            ECls::Stop => "StopIteration",
            ECls::UEncode => "UnicodeEncodeError",
            ECls::UDecode => "UnicodeDecodeError",
            ECls::NotImpl => "NotImplementedError",
            ECls::Assertion => "AssertionError",
            ECls::Struct => "error",
            ECls::Zlib => "error",
            ECls::OS => "OSError",
            ECls::Asn1 => "PyAsn1Error",
            ECls::Underrun => "SubstrateUnderrunError",
            ECls::Eos => "EndOfStreamError",
            ECls::EcFmt => "PGPIncompatibleECPointFormatError",
            ECls::Binascii => "Error",
            ECls::Unsupported => "NotImplementedError",
        }
    }
}

#[derive(Debug)]
pub struct PErr {
    pub cls: ECls,
    pub msg: String,
    pub cause: Option<Box<PErr>>,
}

type PR<T> = Result<T, PErr>;

fn err(cls: ECls, msg: impl Into<String>) -> PErr {
    PErr { cls, msg: msg.into(), cause: None }
}

fn idx() -> PErr {
    err(ECls::Index, "bytearray index out of range")
}

/// `raise PGPError(str(ex)) from ex` (`MetaDispatchable.__call__`)
fn wrap(e: PErr) -> PErr {
    if e.cls == ECls::Unsupported {
        return e;
    }
    PErr { cls: ECls::Pgp, msg: e.msg.clone(), cause: Some(Box::new(e)) }
}

fn enum_err(v: impl std::fmt::Display, name: &str) -> PErr {
    err(ECls::Value, format!("{v} is not a valid {name}"))
}

const PUBKEY_ALGS: &[u8] = &[0, 1, 2, 3, 16, 17, 18, 19, 20, 21, 22];
const SIG_TYPES: &[u8] = &[0x00, 0x01, 0x02, 0x10, 0x11, 0x12, 0x13, 0x16, 0x18, 0x19, 0x1F, 0x20, 0x28, 0x30, 0x40, 0x50];
const SYM_ALGS: &[u8] = &[0, 1, 2, 3, 4, 7, 8, 9, 10, 11, 12, 13];
const REVOCATION_REASONS: &[u8] = &[0, 1, 2, 3, 0x20];

fn check(v: i64, ok: &[u8], name: &str) -> PR<u8> {
    if (0..=255).contains(&v) && ok.contains(&(v as u8)) {
        Ok(v as u8)
    } else {
        Err(enum_err(v, name))
    }
}

fn pubkey_alg(v: i64) -> PR<u8> {
    check(v, PUBKEY_ALGS, "PubKeyAlgorithm")
}

fn hash_alg(v: i64) -> PR<u8> {
    if (0..=11).contains(&v) { Ok(v as u8) } else { Err(enum_err(v, "HashAlgorithm")) }
}

fn sym_alg(v: i64) -> PR<u8> {
    check(v, SYM_ALGS, "SymmetricKeyAlgorithm")
}

// ---------------------------------------------------------------- the bytearray

/// a Python `bytearray` read from the front: `b[:n]`, `del b[:n]` (negative `n` from the end, clamped), `b[i]`
pub struct Buf {
    v: Vec<u8>,
    p: usize,
}

impl Buf {
    pub fn new(v: Vec<u8>) -> Buf {
        Buf { v, p: 0 }
    }
    pub fn len(&self) -> usize {
        self.v.len() - self.p
    }
    fn rest(&self) -> &[u8] {
        &self.v[self.p..]
    }
    fn at(&self, i: usize) -> PR<u8> {
        self.rest().get(i).copied().ok_or_else(idx)
    }
    fn end(&self, n: i64) -> usize {
        let l = self.len() as i64;
        (if n < 0 { (l + n).max(0) } else { n.min(l) }) as usize
    }
    fn peek(&self, n: i64) -> &[u8] {
        &self.rest()[..self.end(n)]
    }
    fn take(&self, n: i64) -> Vec<u8> {
        self.peek(n).to_vec()
    }
    fn del(&mut self, n: i64) {
        self.p += self.end(n);
        if self.p > 4096 && self.p * 2 > self.v.len() {
            self.v.drain(..self.p);
            self.p = 0;
        }
    }
    /// `x = packet[0]; del packet[0]`
    fn pop0(&mut self) -> PR<u8> {
        let b = self.at(0)?;
        self.p += 1;
        Ok(b)
    }
    /// `del b[a:b]` (a, b >= 0)
    fn del_range(&mut self, a: usize, b: usize) {
        let l = self.len();
        let (a, b) = (a.min(l), b.min(l));
        if a < b {
            self.v.drain(self.p + a..self.p + b);
        }
    }
}

/// `int.from_bytes(b, 'big')` of a short field
fn int_be(b: &[u8]) -> i64 {
    b.iter().fold(0i64, |a, &x| a.wrapping_shl(8) | x as i64)
}

/// `PGPObject.int_to_bytes(i, minlen)`: big-endian, at least `minlen` bytes and at least one
fn itb_u64(i: u64, minlen: usize) -> Vec<u8> {
    let n = ((64 - i.leading_zeros() as usize) + 7) / 8;
    let blen = minlen.max(n).max(1);
    let mut out = vec![0u8; blen];
    for (k, b) in out.iter_mut().rev().enumerate().take(8) {
        *b = (i >> (8 * k)) as u8;
    }
    out
}

/// the same for a big integer held as its normalized big-endian bytes
fn itb(v: &[u8], minlen: usize) -> Vec<u8> {
    let blen = minlen.max(v.len()).max(1);
    let mut out = vec![0u8; blen - v.len()];
    out.extend_from_slice(v);
    out
}

fn normalize(b: &[u8]) -> Vec<u8> {
    let k = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    b[k..].to_vec()
}

// ---------------------------------------------------------------- MPIs, EC points, OIDs

/// `MPI`: a non-negative integer, normalized big-endian bytes (empty = 0)
struct Mpi(Vec<u8>);

impl Mpi {
    fn parse(b: &mut Buf) -> Mpi {
        let fl = (int_be(b.peek(2)) + 7) / 8;
        b.del(2);
        let v = normalize(b.peek(fl));
        b.del(fl);
        Mpi(v)
    }
    fn bit_length(&self) -> u64 {
        match self.0.first() {
            None => 0,
            Some(&f) => (self.0.len() as u64 - 1) * 8 + (8 - f.leading_zeros() as u64),
        }
    }
    /// `len(mpi)`
    fn len(&self) -> usize {
        self.0.len() + 2
    }
    fn to_mpibytes(&self) -> Vec<u8> {
        let mut out = itb_u64(self.bit_length(), 2);
        out.extend(itb(&self.0, self.0.len()));
        out
    }
}

#[derive(PartialEq, Clone, Copy)]
enum PtFmt {
    Standard,
    Native,
}

struct Point {
    fmt: PtFmt,
    bytelen: usize,
    x: Vec<u8>,
    y: Vec<u8>,
}

impl Point {
    /// `ECPoint(packet)`
    fn parse(b: &mut Buf) -> PR<Point> {
        let mb = Mpi::parse(b).to_mpibytes();
        let xy = &mb[2..];
        let fmt = match xy[0] {
            0x04 => PtFmt::Standard,
            0x40 => PtFmt::Native,
            0x41 | 0x42 => return Err(err(ECls::NotImpl, "No curve is supposed to use only X or Y coordinates")),
            v => return Err(enum_err(v, "ECPointFormat")),
        };
        let rest = &xy[1..];
        if fmt == PtFmt::Standard {
            if rest.len() % 2 != 0 {
                return Err(err(ECls::Pgp, "malformed EC point"));
            }
            let bl = rest.len() / 2;
            Ok(Point { fmt, bytelen: bl, x: normalize(&rest[..bl]), y: normalize(&rest[bl..]) })
        } else {
            Ok(Point { fmt, bytelen: 0, x: rest.to_vec(), y: vec![] })
        }
    }
    fn len(&self) -> usize {
        match self.fmt {
            PtFmt::Standard => 2 * self.bytelen + 3,
            PtFmt::Native => self.x.len() + 3,
        }
    }
    fn to_mpibytes(&self) -> Vec<u8> {
        let mut b = vec![if self.fmt == PtFmt::Standard { 0x04 } else { 0x40 }];
        if self.fmt == PtFmt::Standard {
            b.extend(itb(&self.x, self.bytelen));
            b.extend(itb(&self.y, self.bytelen));
        } else {
            b.extend_from_slice(&self.x);
        }
        Mpi(normalize(&b)).to_mpibytes()
    }
}

/// `EllipticCurveOID` (pgpy/constants.py): the dotted value of each member that a decoded OID can equal
const CURVES: &[&str] = &[
    "1.3.6.1.4.1.3029.1.5.1",
    "1.3.6.1.4.1.11591.15.1",
    "1.2.840.10045.3.1.7",
    "1.3.132.0.34",
    "1.3.132.0.35",
    "1.3.36.3.3.2.8.1.1.7",
    "1.3.36.3.3.2.8.1.1.11",
    "1.3.36.3.3.2.8.1.1.13",
    "1.3.132.0.10",
];
const CURVE25519: usize = 0;

/// `encoder.encode(oid)` (DER) of a known curve
fn der_oid(dotted: &str) -> Vec<u8> {
    let arcs: Vec<u64> = dotted.split('.').map(|a| a.parse().unwrap_or(0)).collect();
    let mut content: Vec<u8> = vec![];
    let mut push = |v: u64| {
        let mut tmp = vec![(v & 0x7f) as u8];
        let mut v = v >> 7;
        while v > 0 {
            tmp.push(0x80 | (v & 0x7f) as u8);
            v >>= 7;
        }
        content.extend(tmp.iter().rev());
    };
    push(arcs[0] * 40 + arcs[1]);
    for &a in &arcs[2..] {
        push(a);
    }
    let mut out = vec![0x06, content.len() as u8];
    out.extend(content);
    out
}

fn py_tuple(arcs: &[BigUint]) -> String {
    match arcs.len() {
        0 => "()".into(),
        1 => format!("({},)", arcs[0]),
        _ => format!("({})", arcs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")),
    }
}

/// `decoder.decode(b'\x06' + bytes([oidlen]) + packet[:oidlen])` of pyasn1 0.6 (BER), then the two leading arcs
fn ber_oid(l: u8, s: &[u8]) -> PR<Vec<BigUint>> {
    let (length, vs) = if l < 0x80 {
        (l as usize, 0)
    } else if l == 0x80 {
        return Err(err(ECls::Asn1, "Indefinite length mode decoder not implemented for <TagSet object, tags 0:0:6>"));
    } else {
        let size = (l & 0x7f) as usize;
        if size > 8 {
            return Err(err(ECls::Asn1, format!("BER length field size {size} exceeds limit (8)")));
        }
        if s.is_empty() {
            return Err(err(ECls::Eos, ""));
        }
        if s.len() < size {
            return Err(err(ECls::Underrun, "Short substrate on input"));
        }
        let n = s[..size].iter().fold(0u128, |a, &x| (a << 8) | x as u128);
        (n.min(usize::MAX as u128) as usize, size)
    };
    if length == 0 {
        return Err(err(ECls::Asn1, "Empty substrate"));
    }
    let avail = s.len() - vs;
    if avail == 0 {
        return Err(err(ECls::Eos, ""));
    }
    if avail < length {
        return Err(err(ECls::Underrun, "Short substrate on input"));
    }
    let c = &s[vs..vs + length];
    let mut oid: Vec<BigUint> = vec![];
    let mut i = 0;
    while i < c.len() {
        let sub = c[i];
        i += 1;
        if sub < 128 {
            oid.push(BigUint::from(sub));
        } else if sub > 128 {
            let mut next = sub;
            let mut acc = BigUint::from(0u8);
            let mut count = 0;
            while next >= 128 {
                count += 1;
                if count > 20 {
                    return Err(err(ECls::Asn1, format!(
                        "OID arc exceeds maximum continuation octets limit (20) at position {i}")));
                }
                acc = (acc << 7u32) + BigUint::from(next & 0x7f);
                if i >= c.len() {
                    return Err(err(ECls::Underrun, format!("Short substrate for sub-OID past {}", py_tuple(&oid))));
                }
                next = c[i];
                i += 1;
            }
            oid.push((acc << 7u32) + BigUint::from(next));
        } else {
            return Err(err(ECls::Asn1, "Invalid octet 0x80 in OID encoding"));
        }
    }
    let first = oid[0].clone();
    if first <= BigUint::from(39u8) {
        oid.insert(0, BigUint::from(0u8));
    } else if first <= BigUint::from(79u8) {
        oid[0] = first - BigUint::from(40u8);
        oid.insert(0, BigUint::from(1u8));
    } else {
        oid[0] = first - BigUint::from(80u8);
        oid.insert(0, BigUint::from(2u8));
    }
    Ok(oid)
}

/// `oidlen = packet[0]; ...; self.oid = EllipticCurveOID(oid); del packet[:oidlen]`: the curve's index
fn parse_oid(b: &mut Buf) -> PR<usize> {
    let l = b.pop0()?;
    let arcs = ber_oid(l, b.peek(l as i64))?;
    let dotted = arcs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(".");
    let Some(k) = CURVES.iter().position(|c| *c == dotted) else {
        let shown = if dotted.chars().count() > 32 {
            let c: Vec<char> = dotted.chars().collect();
            format!("{}...{}", c[..16].iter().collect::<String>(), c[c.len() - 16..].iter().collect::<String>())
        } else {
            dotted
        };
        return Err(err(ECls::Value, format!(
            "<ObjectIdentifier value object, tagSet <TagSet object, tags 0:0:6>, payload [{shown}]> is not a valid EllipticCurveOID")));
    };
    b.del(l as i64);
    Ok(k)
}

// ---------------------------------------------------------------- warnings

pub struct Warn {
    pub category: &'static str,
    pub text: String,
    /// relative to site-packages
    pub file: &'static str,
    pub line: u32,
    pub src: &'static str,
}

pub struct Ctx {
    pub warnings: Vec<Warn>,
    pub b64: fn(&[u8]) -> Result<Vec<u8>, String>,
    next_id: u64,
    depth: usize,
    /// `from_blob(bytes)`: its warnings point at the other line of `from_blob`
    bytes: bool,
}

impl Ctx {
    pub fn new(b64: fn(&[u8]) -> Result<Vec<u8>, String>) -> Ctx {
        Ctx { warnings: vec![], b64, next_id: 0, depth: 0, bytes: false }
    }
    /// a packet's `id()`: unique in the process (the orphan warning names it, so each one is shown)
    fn id(&mut self) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.next_id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.next_id
    }
    fn user_warning(&mut self, text: String) {
        let (line, src) = if self.bytes { (198, "po = obj.parse(bytearray(blob))") } else { (195, "po = obj.parse(bytearray(blob, 'latin-1'))") };
        self.warnings.push(Warn { category: "UserWarning", text, file: "pgpy/types.py", line, src });
    }
}

/// `SymmetricKeyAlgorithm.block_size` (`.cipher`): the dict it builds touches cryptography's deprecated ciphers
fn block_size(alg: u8, ctx: &mut Ctx) -> PR<usize> {
    const TDES: &str = "TripleDES has been moved to cryptography.hazmat.decrepit.ciphers.algorithms.TripleDES and will be removed from cryptography.hazmat.primitives.ciphers.algorithms in 48.0.0.";
    const CAM: &str = "Camellia has been moved to cryptography.hazmat.decrepit.ciphers.algorithms.Camellia and will be removed from cryptography.hazmat.primitives.ciphers.algorithms in 49.0.0.";
    for (line, text, src) in [
        (193, TDES, "SymmetricKeyAlgorithm.TripleDES: algorithms.TripleDES,"),
        (200, CAM, "SymmetricKeyAlgorithm.Camellia128: algorithms.Camellia,"),
        (201, CAM, "SymmetricKeyAlgorithm.Camellia192: algorithms.Camellia,"),
        (202, CAM, "SymmetricKeyAlgorithm.Camellia256: algorithms.Camellia}"),
    ] {
        ctx.warnings.push(Warn { category: "CryptographyDeprecationWarning", text: text.into(), file: "pgpy/constants.py", line, src });
    }
    match alg {
        1..=4 => Ok(64),
        7..=13 => Ok(128),
        _ => Err(err(ECls::NotImpl, "<SymmetricKeyAlgorithm.Plaintext: 0>")),
    }
}

// ---------------------------------------------------------------- headers

#[derive(Clone)]
struct Hdr {
    lenfmt: u8,
    tag: i64,
    llen: u8,
    len: i64,
}

/// `_parse_len(a, offset)` of `Header.length_bin`
fn parse_len(b: &Buf, off: usize) -> PR<(i64, usize, bool)> {
    let fo = b.at(off)?;
    let r = b.rest();
    let sl = |a: usize, z: usize| int_be(&r[a.min(r.len())..z.min(r.len())]);
    Ok(if fo < 192 {
        (fo as i64, 1, false)
    } else if fo < 224 {
        let dlen = sl(off, off + 2);
        (((dlen - (192 << 8)) & 0xFF00) + ((dlen & 0xFF) + 192), 2, false)
    } else if fo < 255 {
        (1i64 << (fo & 0x1f), 1, true)
    } else {
        (sl(off + 1, off + 5), 5, false)
    })
}

/// a new-format length (packets and subpackets), partial lengths removed from the stream in place
fn new_len(b: &mut Buf) -> PR<i64> {
    let (part, size, partial) = parse_len(b, 0)?;
    b.del(size as i64);
    if !partial {
        return Ok(part);
    }
    let mut total = part;
    let mut partial = true;
    while partial {
        let (p, size, more) = parse_len(b, total as usize)?;
        b.del_range(total as usize, total as usize + size);
        total += p;
        partial = more;
    }
    Ok(total)
}

/// `Header.parse` of a packet (also the reparse of an Opaque packet of tag 0)
fn parse_hdr(b: &mut Buf, h: &mut Hdr) -> PR<()> {
    let b0 = b.at(0)?;
    h.lenfmt = (b0 & 0x40) >> 6;
    h.tag = if h.lenfmt == 1 { (b0 & 0x3F) as i64 } else { ((b0 & 0x3C) >> 2) as i64 };
    if h.lenfmt == 0 {
        h.llen = [1, 2, 4, 0][(b0 & 3) as usize];
    }
    b.del(1);
    if h.lenfmt == 1 {
        h.len = new_len(b)?;
    } else if h.llen > 0 {
        h.len = int_be(b.peek(h.llen as i64));
        b.del(h.llen as i64);
    } else {
        h.len = b.len() as i64;
    }
    Ok(())
}

/// a subpacket header: (length, typeid)
fn sub_hdr(b: &mut Buf) -> PR<(i64, u8)> {
    let len = new_len(b)?;
    let t = int_be(b.peek(1)) as u8;
    b.del(1);
    Ok((len, t & 0x7f))
}

// ---------------------------------------------------------------- signatures

/// what `PGPKey.parse` and the sorts read of a SignatureV4 (or an EmbeddedSignature)
#[derive(Clone, Default)]
pub struct Sig {
    pub sigtype: u8,
    /// the last hashed CreationTime
    pub created: Option<i64>,
    /// the first hashed PrimaryUserID
    pub primary_uid: Option<bool>,
    /// the first IssuerFingerprint (hashed, then unhashed)
    pub issuer_fp: Option<String>,
    /// the last Issuer (hashed, then unhashed)
    pub issuer: Option<String>,
    embedded: Vec<Sig>,
}

impl Sig {
    fn created(&self) -> PR<i64> {
        self.created.ok_or_else(|| err(ECls::Index, "list index out of range"))
    }
}

fn hex_upper(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// `Fingerprint(''.join('{:02x}'.format(c) for c in val).upper())`
fn fingerprint_of(b: &[u8]) -> PR<String> {
    if b.is_empty() {
        return Err(err(ECls::Value, "Fingerprint must be a string of 40 hex digits"));
    }
    Ok(hex_upper(b))
}

/// one signature subpacket (`SignatureSP(packet)`), recorded in `s`
fn sig_sub(b: &mut Buf, s: &mut Sig, hashed: bool, ctx: &mut Ctx) -> PR<()> {
    let (len, typeid) = sub_hdr(b)?;
    let body = |b: &mut Buf, s: &mut Sig, ctx: &mut Ctx| -> PR<()> {
        match typeid {
            2 => {
                let v = int_be(b.peek(4));
                b.del(4);
                if hashed {
                    s.created = Some(v);
                }
            }
            3 | 9 => b.del(4),
            4 | 7 => b.del(1),
            5 => {
                b.del(1);
                b.del(1);
            }
            6 | 24 | 26 | 28 | 37 => b.del(len - 1),
            11 | 21 | 22 => {
                for _ in 0..(len - 1).max(0) {
                    if b.len() == 0 {
                        break; // the rest of the loop reads 0, a valid member of the three enums
                    }
                    let v = int_be(b.peek(1));
                    b.del(1);
                    match typeid {
                        11 => {
                            sym_alg(v)?;
                        }
                        21 => {
                            hash_alg(v)?;
                        }
                        _ => {
                            if v > 3 {
                                return Err(enum_err(v, "CompressionAlgorithm"));
                            }
                        }
                    }
                }
            }
            23 | 27 | 30 => b.del((len - 1).max(0)),
            12 => {
                b.del(1);
                let a = int_be(b.peek(1));
                pubkey_alg(a)?;
                b.del(1);
                fingerprint_of(b.peek(20))?;
                b.del(20);
            }
            16 => {
                let v = hex_upper(b.peek(8));
                b.del(8);
                s.issuer = Some(v);
            }
            20 => {
                b.del(4);
                let nlen = int_be(b.peek(2));
                b.del(2);
                let vlen = int_be(b.peek(2));
                b.del(2);
                b.del(nlen);
                b.del(vlen);
            }
            25 => {
                let v = int_be(b.peek(1)) != 0;
                b.del(1);
                if hashed && s.primary_uid.is_none() {
                    s.primary_uid = Some(v);
                }
            }
            29 => {
                let c = int_be(b.peek(1));
                check(c, REVOCATION_REASONS, "RevocationReason")?;
                b.del(1);
                b.del(len - 2);
            }
            32 => {
                // EmbeddedSignatureHeader.parse: the version byte (any version), then a v4 body
                b.pop0()?;
                let e = sig_body(b, ctx)?;
                s.embedded.push(e);
            }
            33 | 35 => {
                let v = int_be(b.peek(1));
                b.del(1);
                let n = match v {
                    4 => 20,
                    5 => 32,
                    _ => len - 1,
                };
                let fp = fingerprint_of(b.peek(n))?;
                b.del(n);
                if typeid == 33 && s.issuer_fp.is_none() {
                    s.issuer_fp = Some(fp);
                }
            }
            _ => b.del(len - 1),
        }
        Ok(())
    };
    body(b, s, ctx).map_err(wrap)
}

/// `SignatureV4.parse` after the version: never bounded by the packet length
fn sig_body(b: &mut Buf, ctx: &mut Ctx) -> PR<Sig> {
    let mut s = Sig::default();
    let t = b.at(0)?;
    s.sigtype = check(t as i64, SIG_TYPES, "SignatureType")?;
    b.del(1);
    let pubalg = pubkey_alg(b.at(0)? as i64)?;
    b.del(1);
    b.pop0()?; // halg: an unknown value is kept as an int
    for hashed in [true, false] {
        let hl = int_be(b.peek(2));
        b.del(2);
        let plen = b.len() as i64;
        while plen - (b.len() as i64) < hl {
            ctx.depth += 1;
            if ctx.depth > 200 {
                return Err(err(ECls::Unsupported, "py2axum: signatures nested more than 200 deep are not supported"));
            }
            let r = sig_sub(b, &mut s, hashed, ctx);
            ctx.depth -= 1;
            r?;
        }
    }
    b.del(2); // hash2
    match pubalg {
        1..=3 => {
            Mpi::parse(b);
        }
        17 | 19 | 22 => {
            Mpi::parse(b);
            Mpi::parse(b);
        }
        _ => {} // OpaqueSignature: reads nothing
    }
    Ok(s)
}

// ---------------------------------------------------------------- keys

#[derive(Clone)]
pub struct KeyPkt {
    /// 5 SecretKey, 6 PublicKey, 7 SecretSubKey, 14 PublicSubKey
    pub tag: u8,
    pub pkalg: u8,
    pub created: i64,
    /// 40 upper-case hex digits
    pub fp: String,
}

impl KeyPkt {
    pub fn is_public(&self) -> bool {
        matches!(self.tag, 6 | 14)
    }
    pub fn is_primary(&self) -> bool {
        matches!(self.tag, 5 | 6)
    }
    pub fn class(&self) -> &'static str {
        match self.tag {
            5 => "PrivKeyV4",
            6 => "PubKeyV4",
            7 => "PrivSubKeyV4",
            _ => "PubSubKeyV4",
        }
    }
    pub fn keyid(&self) -> &str {
        &self.fp[24..]
    }
}

/// `String2Key.parse(packet, iv)`: its length (`len(s2k)`); `usage` already read for a SKESK
fn parse_s2k(b: &mut Buf, usage: Option<u8>, iv: bool, ctx: &mut Ctx) -> PR<i64> {
    let usage = match usage {
        Some(u) => u,
        None => b.pop0()?,
    };
    let mut len = 1;
    if usage != 254 && usage != 255 {
        return Ok(len);
    }
    let enc = sym_alg(b.at(0)? as i64)?;
    b.del(1);
    let spec = b.at(0)?;
    if !matches!(spec, 0..=3 | 101) {
        return Err(enum_err(spec, "String2KeyType"));
    }
    b.del(1);
    len += 2;
    if spec == 101 {
        if b.peek(4) != b"\x00GNU" {
            return Err(err(ECls::Pgp, "Invalid S2K GNU extension magic value"));
        }
        b.del(4);
        let g = b.at(0)?;
        if !matches!(g, 1 | 2) {
            return Err(enum_err(g, "S2KGNUExtension"));
        }
        b.del(1);
        len += 5;
        if g == 2 {
            let slen = b.at(0)?.min(16) as i64;
            b.del(1);
            let sc = b.take(slen);
            b.del(slen);
            if !sc.is_empty() {
                len += 1 + sc.len() as i64;
            }
        }
        return Ok(len);
    }
    hash_alg(b.at(0)? as i64)?;
    b.del(1);
    len += 1;
    if spec >= 1 {
        len += b.peek(8).len() as i64;
        b.del(8);
    }
    if spec == 3 {
        b.pop0()?;
        len += 1;
    }
    if iv {
        // `packet[:(self.encalg.block_size // 8)]`, then the same expression again in the `del`
        let n = (block_size(enc, ctx)? / 8) as i64;
        len += b.peek(n).len() as i64;
        block_size(enc, ctx)?;
        b.del(n);
    }
    Ok(len)
}

/// the key material of a v4 key packet: (`__bytearray__()` of its public part, `publen()`)
fn key_material(km: &mut Buf, public: bool, alg: u8, ctx: &mut Ctx) -> PR<(Vec<u8>, usize)> {
    let mpis = |km: &mut Buf, n: usize| {
        let mut out = vec![];
        let mut len = 0;
        for _ in 0..n {
            let m = Mpi::parse(km);
            len += m.len();
            out.extend(m.to_mpibytes());
        }
        (out, len)
    };
    let (bytes, plen) = match alg {
        1..=3 => mpis(km, 2),
        17 => mpis(km, 4),
        16 | 20 => mpis(km, 3),
        18 | 19 | 22 => {
            let curve = parse_oid(km)?;
            let pt = Point::parse(km)?;
            match alg {
                19 if pt.fmt != PtFmt::Standard => return Err(err(ECls::EcFmt, "Only Standard format is valid for ECDSA")),
                22 if pt.fmt != PtFmt::Native => return Err(err(ECls::EcFmt, "Only Native format is valid for EdDSA")),
                18 if curve == CURVE25519 && pt.fmt != PtFmt::Native => {
                    return Err(err(ECls::EcFmt, "Only Native format is valid for Curve25519"))
                }
                18 if curve != CURVE25519 && pt.fmt != PtFmt::Standard => {
                    return Err(err(ECls::EcFmt, "Only Standard format is valid for this curve"))
                }
                _ => {}
            }
            let der = der_oid(CURVES[curve]);
            let mut bytes = der[1..].to_vec();
            bytes.extend(pt.to_mpibytes());
            let mut len = pt.len() + der.len() - 1;
            if alg == 18 {
                if km.peek(2) != b"\x03\x01" {
                    return Err(err(ECls::Assertion, ""));
                }
                km.del(2);
                let h = hash_alg(km.at(0)? as i64)?;
                km.del(1);
                let e = sym_alg(km.at(0)? as i64)?;
                km.del(1);
                bytes.extend([3, 1, h, e]);
                len += 4;
            }
            (bytes, len)
        }
        _ => return Ok((vec![], 0)), // OpaquePubKey / OpaquePrivKey: the rest, no S2K
    };
    if !public {
        // the secret part: only the S2K can raise (MPIs and checksums are slices)
        parse_s2k(km, None, true, ctx)?;
    }
    Ok((bytes, plen))
}

fn fingerprint(created: i64, pkalg: u8, bytes: &[u8], plen: usize) -> String {
    let bc = itb_u64(6 + plen as u64, 2);
    let mut h = Sha1::new();
    h.update([0x99, bc[0], bc[bc.len() - 1], 0x04]);
    h.update(itb_u64(created as u64, 4));
    h.update([pkalg]);
    h.update(&bytes[..plen.min(bytes.len())]);
    hex_upper(&h.finalize())
}

fn parse_key(b: &mut Buf, h: &Hdr, ctx: &mut Ctx) -> PR<KeyPkt> {
    let created = int_be(b.peek(4));
    b.del(4);
    let pkalg = pubkey_alg(b.at(0)? as i64)?;
    b.del(1);
    let pend = h.len - 6;
    let mut km = Buf::new(b.take(pend));
    let tag = h.tag as u8;
    let (bytes, plen) = key_material(&mut km, matches!(tag, 6 | 14), pkalg, ctx)?;
    b.del(pend);
    Ok(KeyPkt { tag, pkalg, created, fp: fingerprint(created, pkalg, &bytes, plen) })
}

// ---------------------------------------------------------------- decompression

fn inflate(data: &[u8], zlib_header: bool) -> PR<Vec<u8>> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut d = Decompress::new(zlib_header);
    let mut out: Vec<u8> = Vec::with_capacity((data.len() * 4).max(256));
    loop {
        let at = (d.total_in() as usize).min(data.len());
        let (tin, tout) = (d.total_in(), d.total_out());
        let r = d.decompress_vec(&data[at..], &mut out, FlushDecompress::Finish);
        match r {
            Ok(Status::StreamEnd) => return Ok(out),
            Ok(_) => {
                let full = out.len() == out.capacity();
                let stuck = d.total_in() == tin && d.total_out() == tout;
                if !full && (d.total_in() as usize >= data.len() || stuck) {
                    return Err(err(ECls::Zlib, "Error -5 while decompressing data: incomplete or truncated stream"));
                }
                if out.len() > 1 << 28 {
                    return Err(err(ECls::Unsupported, "py2axum: compressed data above 256 MiB is not supported"));
                }
                if full {
                    out.reserve(out.len().max(256));
                }
            }
            Err(e) => {
                if e.needs_dictionary().is_some() {
                    return Err(err(ECls::Zlib, "Error 2 while decompressing data"));
                }
                return Err(err(ECls::Zlib, format!("Error -3 while decompressing data: {}", e.message().unwrap_or("invalid input data"))));
            }
        }
    }
}

/// `bz2.decompress`: the stream headers and empty streams are followed; a stream with a block is refused
fn bunzip(mut data: &[u8]) -> PR<Vec<u8>> {
    let mut streams = 0;
    let invalid = || err(ECls::OS, "Invalid data stream");
    let ended = || err(ECls::Value, "Compressed data ended before the end-of-stream marker was reached");
    while !data.is_empty() {
        let mut ok = true;
        for (i, want) in [b'B', b'Z', b'h'].iter().enumerate() {
            match data.get(i) {
                Some(c) if c == want => {}
                Some(_) => ok = false,
                None => {}
            }
            if !ok {
                break;
            }
        }
        if ok {
            if let Some(&lv) = data.get(3) {
                ok = (b'1'..=b'9').contains(&lv);
            }
        }
        if ok && data.len() > 4 {
            let m = &data[4..data.len().min(10)];
            const BLOCK: [u8; 6] = [0x31, 0x41, 0x59, 0x26, 0x53, 0x59];
            const EOS: [u8; 6] = [0x17, 0x72, 0x45, 0x38, 0x50, 0x90];
            if BLOCK.starts_with(m) {
                if m.len() == 6 {
                    return Err(err(ECls::Unsupported, "py2axum: bzip2-compressed data is not supported (pgpy CompressedData packet)"));
                }
            } else if !EOS.starts_with(m) {
                ok = false;
            } else if m.len() == 6 {
                let crc = &data[10..data.len().min(14)];
                if crc.len() == 4 {
                    if crc != [0, 0, 0, 0] {
                        ok = false;
                    } else {
                        data = &data[14..];
                        streams += 1;
                        continue;
                    }
                }
            }
        }
        if !ok {
            if streams > 0 {
                break;
            }
            return Err(invalid());
        }
        return Err(ended());
    }
    Ok(vec![])
}

// ---------------------------------------------------------------- packets

pub enum PK {
    Key(KeyPkt),
    Sig(Sig),
    Uid(String),
    UAttr,
    /// a packet that cannot head a group (LiteralData, Marker...)
    Other,
    Opaque,
}

pub struct Pkt {
    tag: i64,
    cls: &'static str,
    ver: Option<u8>,
    kind: PK,
    id: u64,
}

impl Pkt {
    fn repr(&self) -> String {
        let addr = 0x1_0a00_0000u64 + self.id * 0x70;
        match self.ver {
            Some(v) => format!("<{} [tag {:02}][v{}] at 0x{:x}>", self.cls, self.tag, v, addr),
            None => format!("<{} [tag {:02}] at 0x{:x}>", self.cls, self.tag, addr),
        }
    }
}

/// `bytes.decode('utf-8')`'s UnicodeDecodeError message, or the text
pub fn utf8(b: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(b) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => {
            let start = e.valid_up_to();
            let (end, reason) = match e.error_len() {
                None => (b.len(), "unexpected end of data"),
                Some(n) => (start + n, if matches!(b[start], 0x80..=0xC1 | 0xF5..=0xFF) { "invalid start byte" } else { "invalid continuation byte" }),
            };
            Err(if end - start == 1 {
                format!("'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}", b[start])
            } else {
                format!("'utf-8' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
            })
        }
    }
}

fn parse_body(b: &mut Buf, h: &Hdr, ctx: &mut Ctx) -> PR<PK> {
    Ok(match h.tag {
        1 => {
            b.del(8);
            let a = pubkey_alg(b.at(0)? as i64)?;
            b.del(1);
            match a {
                1 | 2 => {
                    Mpi::parse(b);
                }
                16 | 20 => {
                    Mpi::parse(b);
                    Mpi::parse(b);
                }
                18 => {
                    Point::parse(b)?;
                    let c = b.pop0()? as i64;
                    b.del(c);
                }
                _ => b.del(h.len - 18),
            }
            PK::Other
        }
        2 => PK::Sig(sig_body(b, ctx)?),
        3 => {
            let l = parse_s2k(b, Some(255), false, ctx)?;
            b.del(h.len - l);
            PK::Other
        }
        4 => {
            check(b.at(0)? as i64, SIG_TYPES, "SignatureType")?;
            b.del(1);
            b.pop0()?;
            pubkey_alg(b.at(0)? as i64)?;
            b.del(1);
            b.del(8);
            b.pop0()?;
            PK::Other
        }
        5 | 6 | 7 | 14 => PK::Key(parse_key(b, h, ctx)?),
        8 => {
            let c = b.at(0)?;
            if c > 3 {
                return Err(enum_err(c, "CompressionAlgorithm"));
            }
            b.del(1);
            let data = b.take(h.len - 1);
            let out = match c {
                0 => data,
                1 => inflate(&data, false)?,
                2 => inflate(&data, true)?,
                _ => bunzip(&data)?,
            };
            b.del(h.len - 1);
            let mut cd = Buf::new(out);
            ctx.depth += 1;
            if ctx.depth > 100 {
                return Err(err(ECls::Unsupported, "py2axum: compressed packets nested more than 100 deep are not supported"));
            }
            while cd.len() > 0 {
                parse_packet(&mut cd, ctx)?;
            }
            ctx.depth -= 1;
            PK::Other
        }
        9 | 10 => {
            b.del(h.len);
            PK::Other
        }
        11 => {
            b.pop0()?;
            let fnl = b.pop0()? as i64;
            utf8(b.peek(fnl)).map_err(|m| err(ECls::UDecode, m))?;
            b.del(fnl);
            b.del(4);
            b.del(h.len - (6 + fnl));
            PK::Other
        }
        12 => {
            let t = int_be(b.peek(2));
            b.del(2);
            if (t & 15) > 6 {
                return Err(enum_err(t & 15, "TrustLevel"));
            }
            PK::Other
        }
        13 => {
            let raw = b.take(h.len);
            b.del(h.len);
            PK::Uid(utf8(&raw).unwrap_or_else(|_| raw.iter().map(|&c| c as char).collect()))
        }
        17 => {
            let plen = b.len() as i64;
            while h.len > plen - b.len() as i64 {
                let (len, typeid) = sub_hdr(b)?;
                if typeid == 1 {
                    let image = |b: &mut Buf| -> PR<()> {
                        let n = b.len().min(16);
                        if n < 16 {
                            return Err(err(ECls::Struct, format!(
                                "unpack_from requires a buffer of at least 16 bytes for unpacking 16 bytes at offset 0 (actual buffer size is {n})")));
                        }
                        b.del(16);
                        b.del(len - 17);
                        Ok(())
                    };
                    image(b).map_err(wrap)?;
                } else {
                    b.del(len - 1);
                }
            }
            PK::UAttr
        }
        18 => {
            b.del(h.len - 1);
            PK::Other
        }
        19 => {
            b.del(20);
            PK::Other
        }
        _ => PK::Opaque,
    })
}

fn class_name(tag: i64, known: bool) -> &'static str {
    if !known {
        return "Opaque";
    }
    match tag {
        1 => "PKESessionKeyV3",
        2 => "SignatureV4",
        3 => "SKESessionKeyV4",
        4 => "OnePassSignatureV3",
        5 => "PrivKeyV4",
        6 => "PubKeyV4",
        7 => "PrivSubKeyV4",
        8 => "CompressedData",
        9 => "SKEData",
        10 => "Marker",
        11 => "LiteralData",
        12 => "Trust",
        13 => "UserID",
        14 => "PubSubKeyV4",
        17 => "UserAttribute",
        18 => "IntegrityProtectedSKEDataV1",
        _ => "MDC",
    }
}

/// `Packet(packet)` (`MetaDispatchable.__call__`)
fn parse_packet(b: &mut Buf, ctx: &mut Ctx) -> PR<Pkt> {
    let mut h = Hdr { lenfmt: 1, tag: 0, llen: 1, len: 1 };
    parse_hdr(b, &mut h)?;
    let (versioned, ver, known) = match h.tag {
        1 | 2 | 3 | 4 | 5 | 6 | 7 | 14 | 18 => {
            let v = b.pop0().map_err(wrap)?;
            let ok = matches!((h.tag, v), (1, 3) | (2, 4) | (3, 4) | (4, 3) | (5, 4) | (6, 4) | (7, 4) | (14, 4) | (18, 1));
            (true, Some(v), ok)
        }
        8..=13 | 17 | 19 => (false, None, true),
        _ => (false, None, false),
    };
    let kind = if known {
        parse_body(b, &h, ctx).map_err(wrap)?
    } else {
        let opaque = |b: &mut Buf, h: &mut Hdr| -> PR<()> {
            if h.tag == 0 {
                parse_hdr(b, h)?;
            }
            b.del(h.len - versioned as i64);
            Ok(())
        };
        opaque(b, &mut h).map_err(wrap)?;
        PK::Opaque
    };
    Ok(Pkt { tag: h.tag, cls: class_name(h.tag, known), ver: if known { ver } else { None }, kind, id: ctx.id() })
}

// ---------------------------------------------------------------- the armor

fn is_b64(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'+' || c == b'/'
}

/// `\r?\n` at `i`: the position after it
fn eol(t: &[u8], i: usize) -> Option<usize> {
    match t.get(i) {
        Some(b'\n') => Some(i + 1),
        Some(b'\r') if t.get(i + 1) == Some(&b'\n') => Some(i + 2),
        _ => None,
    }
}

struct Block {
    magic: String,
    body: (usize, usize),
    crc: (usize, usize),
}

/// the armor regex from its header line at `p` (no SIGNED MESSAGE prefix): its groups when it matches there
fn block_at(t: &[u8], p: usize) -> Option<Block> {
    const BEGIN: &[u8] = b"-----BEGIN PGP ";
    if !t[p..].starts_with(BEGIN) {
        return None;
    }
    let ms = p + BEGIN.len();
    let mut i = ms;
    while i < t.len() && (t[i].is_ascii_uppercase() || t[i].is_ascii_digit() || t[i] == b' ' || t[i] == b',') {
        i += 1;
    }
    if i == ms || !t[i..].starts_with(b"-----") {
        return None;
    }
    let magic = &t[ms..i];
    let mut i = eol(t, i + 5)?;
    // headers: `(^.+: .+(?:\r?\n))+`
    loop {
        let Some(nl) = t[i..].iter().position(|&c| c == b'\n') else { break };
        let line = &t[i..i + nl];
        if line.windows(2).enumerate().any(|(k, w)| w == b": " && k >= 1 && k + 2 < line.len()) {
            i += nl + 1;
        } else {
            break;
        }
    }
    if let Some(k) = eol(t, i) {
        i = k;
    }
    // body lines: `[A-Za-z0-9+/]{1,76}={,2}(?:\r?\n)`
    let bstart = i;
    loop {
        let mut k = i;
        while k < t.len() && is_b64(t[k]) {
            k += 1;
        }
        let n = k - i;
        if n == 0 || n > 76 {
            break;
        }
        let mut pads = 0;
        while pads < 2 && t.get(k) == Some(&b'=') {
            k += 1;
            pads += 1;
        }
        match eol(t, k) {
            Some(e) => i = e,
            None => break,
        }
    }
    if i == bstart {
        return None;
    }
    let body = (bstart, i);
    if t.get(i) != Some(&b'=') || i + 5 > t.len() || !t[i + 1..i + 5].iter().all(|&c| is_b64(c)) {
        return None;
    }
    let crc = (i + 1, i + 5);
    let i = eol(t, i + 5)?;
    let mut end = b"-----END PGP ".to_vec();
    end.extend_from_slice(magic);
    end.extend_from_slice(b"-----");
    if !t[i..].starts_with(&end) {
        return None;
    }
    Some(Block { magic: String::from_utf8_lossy(magic).into_owned(), body, crc })
}

/// `Armorable.__armor_regex.search(text)`
fn search(t: &[u8]) -> Option<Block> {
    const SIGNED: &[u8] = b"-----BEGIN PGP SIGNED MESSAGE-----";
    let mut starts = vec![0];
    starts.extend(t.iter().enumerate().filter(|(_, &c)| c == b'\n').map(|(i, _)| i + 1));
    for (n, &p) in starts.iter().enumerate() {
        if p > t.len() {
            continue;
        }
        if t[p..].starts_with(SIGNED) {
            if let Some(c0) = eol(t, p + SIGNED.len()) {
                // the cleartext is greedy: the last block after the header line that matches
                if let Some(b) = starts[n + 1..].iter().rev().filter(|&&q| q > c0 && q < t.len()).find_map(|&q| block_at(t, q)) {
                    return Some(b);
                }
            }
        }
        if p < t.len() {
            if let Some(b) = block_at(t, p) {
                return Some(b);
            }
        }
    }
    None
}

fn crc24(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xB704CE;
    for &b in data {
        crc ^= (b as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x1000000 != 0 {
                crc ^= 0x1864CFB;
            }
        }
    }
    crc & 0xFFFFFF
}

/// `Armorable.ascii_unarmor(data)`: (magic, body)
fn unarmor(data: Vec<u8>, ctx: &mut Ctx) -> PR<(Option<String>, Vec<u8>)> {
    let ascii = data.iter().all(|&c| (0x20..=0x7e).contains(&c) || c == b'\r' || c == b'\n' || c == b'\t');
    if !ascii {
        return Ok((None, data));
    }
    let Some(m) = search(&data) else {
        return Err(err(ECls::Value, "Expected: ASCII-armored PGP data"));
    };
    let body = (ctx.b64)(&data[m.body.0..m.body.1])
        .map_err(|msg| PErr { cls: ECls::Pgp, msg: msg.clone(), cause: Some(Box::new(err(ECls::Binascii, msg))) })?;
    let crc = (ctx.b64)(&data[m.crc.0..m.crc.1]).unwrap_or_default();
    if crc24(&body) as i64 != int_be(&crc) {
        ctx.user_warning("Incorrect crc24".into());
    }
    Ok((Some(m.magic), body))
}

// ---------------------------------------------------------------- PGPKey.parse

pub enum UidKind {
    Uid(String),
    UAttr,
}

pub struct UidB {
    pub kind: UidKind,
    pub sigs: Vec<Sig>,
    pub parent_fp: Option<String>,
}

#[derive(Default)]
pub struct KeyB {
    pub key: Option<KeyPkt>,
    pub sigs: Vec<Sig>,
    pub uids: Vec<UidB>,
    /// subkeys: keyid -> arena index
    pub children: IndexMap<String, usize>,
}

impl KeyB {
    pub fn is_public(&self) -> bool {
        self.key.as_ref().is_some_and(|k| k.is_public())
    }
    pub fn is_primary(&self) -> bool {
        self.key.as_ref().is_some_and(|k| k.is_primary())
    }
}

pub struct Parsed {
    /// the PGPKey objects; 0 is the one `from_blob` returns first
    pub arena: Vec<KeyB>,
    /// the second element of the tuple: (keyid, is_public) -> arena index
    pub keys: IndexMap<(String, bool), usize>,
}

/// `SorteDeque.insort` of a signature (`PGPSignature.__lt__`: `self.created < other.created`)
fn insort_sig(v: &mut Vec<Sig>, x: Sig) -> PR<()> {
    let (mut lo, mut hi) = (0, v.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let a = v[mid].created()?;
        if a < x.created()? {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    v.insert(lo, x);
    Ok(())
}

/// `Fingerprint.__eq__(str)`
fn fp_eq_str(fp: &str, other: &str) -> bool {
    let o: String = other.chars().filter(|&c| c != ' ').collect();
    fp == o || fp[fp.len().saturating_sub(16)..] == o || fp[fp.len().saturating_sub(8)..] == o
}

impl UidB {
    pub fn is_uid(&self) -> bool {
        matches!(self.kind, UidKind::Uid(_))
    }
    /// `PGPUID.selfsig`: the most recent signature by the parent key
    pub fn selfsig(&self) -> PR<Option<&Sig>> {
        let Some(pfp) = &self.parent_fp else { return Ok(None) };
        for s in self.sigs.iter().rev() {
            if let Some(ifp) = &s.issuer_fp {
                if pfp == ifp {
                    return Ok(Some(s));
                }
            } else {
                let signer = s.issuer.as_ref().ok_or_else(|| err(ECls::Index, "list index out of range"))?;
                if !signer.is_empty() && fp_eq_str(pfp, signer) {
                    return Ok(Some(s));
                }
            }
        }
        Ok(None)
    }
    pub fn is_primary(&self) -> PR<bool> {
        Ok(match self.selfsig()? {
            Some(s) => s.primary_uid.unwrap_or(false),
            None => false,
        })
    }
}

/// `PGPUID.__lt__(a, b)`
fn uid_lt(a: &UidB, b: &UidB) -> PR<bool> {
    if a.is_uid() == b.is_uid() {
        let (pa, pb) = (a.is_primary()?, b.is_primary()?);
        if pa == pb {
            let ma = a.selfsig()?;
            let sa = b.selfsig()?;
            return match (ma, sa) {
                (None, s) => Ok(s.is_some()),
                (_, None) => Ok(false),
                // `ma > sa`: PGPSignature has only __lt__, so `sa.__lt__(ma)`
                (Some(m), Some(s)) => {
                    let sc = s.created()?;
                    Ok(sc < m.created()?)
                }
            };
        }
        return Ok(pa);
    }
    Ok(a.is_uid())
}

/// the `itertools.groupby` of `PGPKey.parse` over the packets (Trust packets filtered out)
struct Group<'a> {
    b: &'a mut Buf,
    done: bool,
    last: Option<String>,
    tgtkey: Option<Option<String>>,
    currkey: Option<Option<String>>,
    currvalue: Option<Pkt>,
}

impl Group<'_> {
    fn pull(&mut self, ctx: &mut Ctx) -> PR<Option<Pkt>> {
        loop {
            if self.done || self.b.len() == 0 {
                self.done = true;
                return Ok(None);
            }
            let p = parse_packet(self.b, ctx)?;
            if p.tag != 12 {
                return Ok(Some(p));
            }
        }
    }
    fn step(&mut self, ctx: &mut Ctx) -> PR<bool> {
        let Some(p) = self.pull(ctx)? else { return Ok(false) };
        if p.tag != 2 {
            self.last = Some(format!("{:02X}_{}", 0x1_0a00_0000u64 + p.id * 0x70, p.cls));
        }
        self.currkey = Some(self.last.clone());
        self.currvalue = Some(p);
        Ok(true)
    }
    /// `next(groupby)`: the key of the next group
    fn next_group(&mut self, ctx: &mut Ctx) -> PR<Option<Option<String>>> {
        loop {
            match (&self.currkey, &self.tgtkey) {
                (None, _) => {}
                (Some(_), None) => break,
                (Some(c), Some(t)) if c != t => break,
                _ => {}
            }
            if !self.step(ctx)? {
                return Ok(None);
            }
        }
        self.tgtkey = self.currkey.clone();
        Ok(self.currkey.clone())
    }
    /// `next(group)`
    fn grouper_next(&mut self, ctx: &mut Ctx) -> PR<Option<Pkt>> {
        if self.currvalue.is_none() && !self.step(ctx)? {
            return Ok(None);
        }
        if self.currkey != self.tgtkey {
            return Ok(None);
        }
        self.currkey = None;
        Ok(self.currvalue.take())
    }
}

enum Obj {
    Key(usize),
    Uid(UidB),
}

/// `PGPKey.from_blob(text)` on a str (`Err` = what it raises); the warnings it emits are in `ctx`
pub fn from_text(text: &str, ctx: &mut Ctx) -> PR<Parsed> {
    let chars: Vec<char> = text.chars().collect();
    if let Some(start) = chars.iter().position(|&c| c as u32 > 0xFF) {
        let mut end = start;
        while end < chars.len() && chars[end] as u32 > 0xFF {
            end += 1;
        }
        let msg = if end - start == 1 {
            let c = chars[start] as u32;
            let esc = if c <= 0xFFFF { format!("\\u{c:04x}") } else { format!("\\U{c:08x}") };
            format!("'latin-1' codec can't encode character '{esc}' in position {start}: ordinal not in range(256)")
        } else {
            format!("'latin-1' codec can't encode characters in position {start}-{}: ordinal not in range(256)", end - 1)
        };
        return Err(err(ECls::UEncode, msg));
    }
    parse(chars.iter().map(|&c| c as u8).collect(), ctx)
}

/// `PGPKey.from_blob(bytes)`
pub fn from_bytes(data: Vec<u8>, ctx: &mut Ctx) -> PR<Parsed> {
    ctx.bytes = true;
    parse(data, ctx)
}

/// `PGPKey.parse(data)`
fn parse(data: Vec<u8>, ctx: &mut Ctx) -> PR<Parsed> {
    let (magic, body) = unarmor(data, ctx)?;
    if let Some(m) = &magic {
        if !m.contains("KEY") {
            return Err(err(ECls::Value, format!("Expected: KEY. Got: {m}")));
        }
    }
    let mut out = Parsed { arena: vec![KeyB::default()], keys: IndexMap::new() };
    let mut b = Buf::new(body);
    let mut g = Group { b: &mut b, done: false, last: None, tgtkey: None, currkey: None, currvalue: None };
    loop {
        // a new groupby (and a new PktGrouper) over the same packet iterator
        g.last = None;
        g.tgtkey = None;
        g.currkey = None;
        g.currvalue = None;
        let mut orphan = None;
        loop {
            let Some(key) = g.next_group(ctx)? else { break };
            let Some(k) = key else {
                return Err(err(ECls::Attribute, "'NoneType' object has no attribute 'endswith'"));
            };
            if k.ends_with("Opaque") {
                continue;
            }
            let pkt = g.grouper_next(ctx)?.expect("a group has its first packet");
            if !matches!(pkt.kind, PK::Key(_) | PK::Uid(_) | PK::UAttr) {
                orphan = Some(pkt);
                break;
            }
            let mut obj = match pkt.kind {
                PK::Key(kp) => {
                    let i = if out.arena[0].key.is_none() { 0 } else { out.arena.push(KeyB::default()); out.arena.len() - 1 };
                    out.arena[i].key = Some(kp);
                    Obj::Key(i)
                }
                PK::Uid(s) => Obj::Uid(UidB { kind: UidKind::Uid(s), sigs: vec![], parent_fp: None }),
                _ => Obj::Uid(UidB { kind: UidKind::UAttr, sigs: vec![], parent_fp: None }),
            };
            while let Some(s) = g.grouper_next(ctx)? {
                let PK::Sig(sig) = s.kind else { continue };
                match &mut obj {
                    Obj::Key(i) => {
                        let emb = if sig.sigtype == 0x18 { sig.embedded.clone() } else { vec![] };
                        insort_sig(&mut out.arena[*i].sigs, sig)?;
                        for e in emb {
                            insort_sig(&mut out.arena[*i].sigs, e)?;
                        }
                    }
                    Obj::Uid(u) => insort_sig(&mut u.sigs, sig)?,
                }
            }
            let last = || err(ECls::Stop, "");
            match obj {
                Obj::Key(i) => {
                    if out.arena[i].is_primary() {
                        let kp = out.arena[i].key.as_ref().unwrap();
                        out.keys.insert((kp.keyid().to_string(), kp.is_public()), i);
                    } else {
                        let &parent = out.keys.values().last().ok_or_else(last)?;
                        if out.arena[i].is_public() != out.arena[parent].is_public() {
                            return Err(err(ECls::Type, "unsupported operand type(s) for |: 'PGPKey' and 'PGPKey'"));
                        }
                        let keyid = out.arena[i].key.as_ref().unwrap().keyid().to_string();
                        out.arena[parent].children.insert(keyid, i);
                    }
                }
                Obj::Uid(mut u) => {
                    let &parent = out.keys.values().last().ok_or_else(last)?;
                    u.parent_fp = out.arena[parent].key.as_ref().map(|k| k.fp.clone());
                    let uids = &mut out.arena[parent].uids;
                    let (mut lo, mut hi) = (0, uids.len());
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        if uid_lt(&uids[mid], &u)? {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    uids.insert(lo, u);
                }
            }
        }
        let Some(pkt) = orphan else { break };
        ctx.user_warning(format!("Warning: Orphaned packet detected! {}", pkt.repr()));
        while g.grouper_next(ctx)?.is_some() {}
    }
    Ok(out)
}

// ---------------------------------------------------------------- PGPUID name / comment / email

/// `PGPUID._splitstring()` on a User ID: (name, comment, email); `None` when the regex does not match (an inner
/// newline: `None.groupdict()`)
pub fn split_uid(uid: &str) -> Option<(String, String, String)> {
    if uid.is_empty() {
        return Some((String::new(), String::new(), String::new()));
    }
    let c: Vec<char> = uid.chars().collect();
    // `$` (no MULTILINE): the end, or before a final newline; `.` never matches a newline
    let n = if c.last() == Some(&'\n') { c.len() - 1 } else { c.len() };
    if c[..n].contains(&'\n') {
        return None;
    }
    let s = &c[..n];
    let text = |a: usize, z: usize| s[a..z].iter().collect::<String>();
    let starts = |p: usize, pat: &str| {
        let pc: Vec<char> = pat.chars().collect();
        p + pc.len() <= n && s[p..p + pc.len()] == pc[..]
    };
    // `(\ <(?P<email>.+)>)?$` at p
    let email = |p: usize| -> Option<String> {
        if starts(p, " <") && n >= p + 4 && s[n - 1] == '>' {
            Some(text(p + 2, n - 1))
        } else {
            None
        }
    };
    for e in 1..=n {
        if starts(e, " (") {
            for q in e + 3..n {
                if s[q] != ')' || !(starts(q + 1, " <") || q + 1 == n) {
                    continue;
                }
                if let Some(m) = email(q + 1) {
                    return Some((text(0, e), text(e + 2, q), m));
                }
                if q + 1 == n {
                    return Some((text(0, e), text(e + 2, q), String::new()));
                }
            }
        }
        if let Some(m) = email(e) {
            return Some((text(0, e), String::new(), m));
        }
        if e == n {
            return Some((text(0, e), String::new(), String::new()));
        }
    }
    None
}
