//! qrcode 8.2 with its default image factory, Pillow's `PilImage` (black on white: a mode "1" image), saved as PNG
//! the way Pillow 12 writes it: `QRCode(...)`, `add_data`, `make`, `make_image`, `qrcode.make`, `image.save(stream,
//! format="PNG")`. The encoding is a port of qrcode's main.py, util.py and base.py (the data split into numeric,
//! alphanumeric and byte chunks, the version and mask chosen as qrcode chooses them, its exceptions); the image is
//! Pillow's: one bit per pixel, each row filtered as ZipEncode.c chooses (none, up, sub, paeth), compressed by the
//! zlib of Pillow's wheels (zlib 1.3.1 on Linux, zlib-ng on macOS: see `Zip`) one row per deflate call, Z_FILTERED,
//! memLevel 9, level 6, in output blocks of max(65536, 4 * width) bytes, each an IDAT chunk.
use std::sync::Arc;

use parking_lot::Mutex;

use super::ops;
use super::v::*;

const MODE_NUMBER: u8 = 1;
const MODE_ALPHA_NUM: u8 = 2;
const MODE_8BIT_BYTE: u8 = 4;
const ALPHA_NUM: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";

pub enum Obj {
    Code(Mutex<Code>),
    Img(Img),
}

struct Data {
    mode: u8,
    data: Vec<u8>,
}

pub struct Code {
    version: Option<i64>,
    ec: i64,
    box_size: i64,
    border: i64,
    mask: Option<i64>,
    data_list: Vec<Data>,
    modules: Vec<Vec<Option<bool>>>,
    count: usize,
    data_cache: Option<Vec<u8>>,
}

pub struct Img {
    border: i64,
    width: i64,
    box_size: i64,
    modules: Vec<Vec<bool>>,
}

pub fn type_name(o: &Obj) -> &'static str {
    match o {
        Obj::Code(_) => "QRCode",
        Obj::Img(_) => "PilImage",
    }
}

fn unported(what: &str) -> Exc {
    Exc::type_error(format!("py2axum: {what} is not supported (qrcode: QRCode(version, error_correction, box_size, border, \
                             mask_pattern), add_data, make, make_image() in black on white, image.save(<io.BytesIO>, \"PNG\"))"))
}

fn arg<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v))
}

fn check_args(what: &str, args: &[V], kwargs: &[(String, V)], params: &[&str]) -> R<()> {
    if args.len() > params.len() {
        return Err(Exc::type_error(format!("{what}() takes from 1 to {} positional arguments but {} were given",
                                           params.len() + 1, args.len() + 1)));
    }
    for (k, _) in kwargs {
        match params.iter().position(|p| p == k) {
            None => return Err(Exc::type_error(format!("{what}() got an unexpected keyword argument '{k}'"))),
            Some(i) if i < args.len() => return Err(Exc::type_error(format!("{what}() got multiple values for argument '{k}'"))),
            _ => {}
        }
    }
    Ok(())
}

fn int_of(v: &V) -> R<i64> {
    match super::methods::b_int(std::slice::from_ref(v))? {
        V::Int(i) => Ok(i),
        _ => Err(Exc::msg(&OVERFLOW_ERROR, "py2axum: integer beyond 64 bits")),
    }
}

fn check_version(v: i64) -> R<()> {
    if !(1..=40).contains(&v) {
        return Err(Exc::value_error(format!("Invalid version (was {v}, expected 1 to 40)")));
    }
    Ok(())
}

/// `qrcode.QRCode(version=None, error_correction=ERROR_CORRECT_M, box_size=10, border=4, image_factory=None,
/// mask_pattern=None)` (image_factory is refused at transpile time)
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    const P: &[&str] = &["version", "error_correction", "box_size", "border", "image_factory", "mask_pattern"];
    check_args("QRCode.__init__", args, kwargs, P)?;
    let get = |i: usize| arg(args, kwargs, i, P[i]);
    let box_size = get(2).cloned().unwrap_or(V::Int(10));
    if int_of(&box_size)? <= 0 {
        return Err(Exc::value_error(format!("Invalid box size (was {}, expected larger than 0)", ops::str_(&box_size)?)));
    }
    let border = get(3).cloned().unwrap_or(V::Int(4));
    if int_of(&border)? < 0 {
        return Err(Exc::value_error(format!("Invalid border value (was {}, expected 0 or larger than that)",
                                            ops::str_(&border)?)));
    }
    let version = match get(0) {
        None | Some(V::None) => None,
        Some(v) => {
            let v = int_of(v)?;
            check_version(v)?;
            Some(v)
        }
    };
    let ec = match get(1) {
        None => 0,
        Some(v) => int_of(v)?,
    };
    if !matches!(get(4), None | Some(V::None)) {
        return Err(unported("QRCode(image_factory=)"));
    }
    let mask = mask_of(get(5))?;
    if ec < 0 {
        // qrcode loops forever on a negative level (BCH_digit of a negative number)
        return Err(unported("a negative error_correction"));
    }
    let code = Code {
        version, ec, box_size: int_of(&box_size)?, border: int_of(&border)?, mask, data_list: vec![], modules: vec![vec![]],
        count: 0, data_cache: None,
    };
    Ok(V::native(Native::Qr(Arc::new(Obj::Code(Mutex::new(code))))))
}

fn mask_of(v: Option<&V>) -> R<Option<i64>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(V::Int(i)) => {
            if !(0..=7).contains(i) {
                return Err(Exc::value_error(format!("Mask pattern should be in range(8) (got {i})")));
            }
            Ok(Some(*i))
        }
        Some(V::Bool(b)) => Ok(Some(*b as i64)),
        Some(other) => Err(Exc::type_error(format!("Invalid mask pattern (was <class '{}'>, expected int)", other.type_name()))),
    }
}

/// `qrcode.make(data=None, **kwargs)`: QRCode(**kwargs), add_data(data), make_image()
pub fn make(args: &[V], kwargs: &[(String, V)]) -> R {
    if args.len() > 1 {
        return Err(Exc::type_error(format!("make() takes from 0 to 1 positional arguments but {} were given", args.len())));
    }
    let mut data = args.first().cloned();
    let mut rest = vec![];
    for (k, v) in kwargs {
        if k == "data" {
            if data.is_some() {
                return Err(Exc::type_error("make() got multiple values for argument 'data'"));
            }
            data = Some(v.clone());
        } else {
            rest.push((k.clone(), v.clone()));
        }
    }
    let q = new(&[], &rest)?;
    let V::Native(n) = &q else { unreachable!() };
    let Native::Qr(o) = &**n else { unreachable!() };
    let Obj::Code(c) = &**o else { unreachable!() };
    let mut c = c.lock();
    c.add_data(&data.unwrap_or(V::None), 20)?;
    c.make_image(&[], &[])
}

pub fn attr(o: &Obj, name: &str) -> R {
    match o {
        Obj::Code(c) => {
            let mut c = c.lock();
            match name {
                "version" => Ok(V::Int(c.version()?)),
                "error_correction" => Ok(V::Int(c.ec)),
                "box_size" => Ok(V::Int(c.box_size)),
                "border" => Ok(V::Int(c.border)),
                "mask_pattern" => Ok(c.mask.map(V::Int).unwrap_or(V::None)),
                "modules_count" => Ok(V::Int(c.count as i64)),
                n => Err(Exc::attr_error(format!("py2axum: QRCode.{n} is not supported (qrcode: version, error_correction, \
                                                  box_size, border, mask_pattern, modules_count)"))),
            }
        }
        Obj::Img(i) => {
            let ps = i.pixel_size();
            match name {
                "border" => Ok(V::Int(i.border)),
                "width" => Ok(V::Int(i.width)),
                "box_size" => Ok(V::Int(i.box_size)),
                "pixel_size" => Ok(V::Int(ps)),
                "kind" => Ok(V::str("PNG")),
                "size" => Ok(V::tuple(vec![V::Int(ps), V::Int(ps)])),
                "mode" => Ok(V::str("1")),
                n => Err(Exc::attr_error(format!("py2axum: PilImage.{n} is not supported (qrcode: border, width, box_size, \
                                                  pixel_size, kind, size, mode, save())"))),
            }
        }
    }
}

pub fn method(o: &Obj, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match (o, name) {
        (Obj::Code(c), "add_data") => {
            check_args("QRCode.add_data", args, kwargs, &["data", "optimize"])?;
            let data = arg(args, kwargs, 0, "data")
                .ok_or_else(|| Exc::type_error("QRCode.add_data() missing 1 required positional argument: 'data'"))?;
            let optimize = match arg(args, kwargs, 1, "optimize") {
                None => 20,
                Some(V::Int(i)) if *i >= 0 => *i,
                Some(_) => return Err(unported("add_data(optimize=) other than an int >= 0")),
            };
            c.lock().add_data(data, optimize)?;
            Ok(V::None)
        }
        (Obj::Code(c), "make") => {
            check_args("QRCode.make", args, kwargs, &["fit"])?;
            let fit = match arg(args, kwargs, 0, "fit") {
                None => true,
                Some(v) => ops::truthy(v)?,
            };
            c.lock().make(fit)?;
            Ok(V::None)
        }
        (Obj::Code(c), "make_image") => c.lock().make_image(args, kwargs),
        (Obj::Code(c), "clear") => {
            check_args("QRCode.clear", args, kwargs, &[])?;
            c.lock().clear();
            Ok(V::None)
        }
        (Obj::Img(i), "save") => i.save(args, kwargs),
        (Obj::Code(_), n) => Err(Exc::attr_error(format!("py2axum: QRCode.{n}() is not supported (qrcode: add_data, make, \
                                                          make_image, clear)"))),
        (Obj::Img(_), n) => Err(Exc::attr_error(format!("py2axum: PilImage.{n}() is not supported (qrcode: save)"))),
    }
}

// ---------------------------------------------------------------- data (util.py)

struct BitBuffer {
    buffer: Vec<u8>,
    length: usize,
}

impl BitBuffer {
    fn new() -> Self {
        BitBuffer { buffer: vec![], length: 0 }
    }
    fn put(&mut self, num: u64, length: u32) {
        for i in 0..length {
            let sh = length - i - 1;
            self.put_bit(sh < 64 && (num >> sh) & 1 == 1);
        }
    }
    fn put_bit(&mut self, bit: bool) {
        let i = self.length / 8;
        if self.buffer.len() <= i {
            self.buffer.push(0);
        }
        if bit {
            self.buffer[i] |= 0x80 >> (self.length % 8);
        }
        self.length += 1;
    }
}

fn is_alnum(b: u8) -> bool {
    ALPHA_NUM.contains(&b)
}

/// `re.search` of `^X+$` (whole: the data no longer than `minimum`) or of `X{minimum,}`: (start, end)
fn search(data: &[u8], class: fn(u8) -> bool, whole: bool, minimum: usize) -> Option<(usize, usize)> {
    if whole {
        let k = data.iter().take_while(|&&b| class(b)).count();
        // `$`: at the end, or before a newline that ends the data
        return (k > 0 && (k == data.len() || (k == data.len() - 1 && data[k] == b'\n'))).then_some((0, k));
    }
    let mut i = 0;
    while i < data.len() {
        if class(data[i]) {
            let k = data[i..].iter().take_while(|&&b| class(b)).count();
            if k >= minimum {
                return Some((i, i + k));
            }
            i += k;
        } else {
            i += 1;
        }
    }
    None
}

fn optimal_split(mut data: &[u8], class: fn(u8) -> bool, whole: bool, minimum: usize) -> Vec<(bool, &[u8])> {
    let mut out = vec![];
    while !data.is_empty() {
        let Some((start, end)) = search(data, class, whole, minimum) else { break };
        if start > 0 {
            out.push((false, &data[..start]));
        }
        out.push((true, &data[start..end]));
        data = &data[end..];
    }
    if !data.is_empty() {
        out.push((false, data));
    }
    out
}

fn optimal_mode(data: &[u8]) -> u8 {
    if !data.is_empty() && data.iter().all(u8::is_ascii_digit) {
        MODE_NUMBER
    } else if data.iter().all(|&b| is_alnum(b)) {
        MODE_ALPHA_NUM
    } else {
        MODE_8BIT_BYTE
    }
}

impl Data {
    fn write(&self, buf: &mut BitBuffer) {
        match self.mode {
            MODE_NUMBER => {
                for chars in self.data.chunks(3) {
                    let n = chars.iter().fold(0u64, |a, &b| a * 10 + (b - b'0') as u64);
                    buf.put(n, [0, 4, 7, 10][chars.len()]);
                }
            }
            MODE_ALPHA_NUM => {
                // bytes.find: -1 for a byte outside the table (impossible here: the chunk matched it)
                let find = |b: u8| ALPHA_NUM.iter().position(|&x| x == b).map(|p| p as i64).unwrap_or(-1);
                for chars in self.data.chunks(2) {
                    if chars.len() > 1 {
                        buf.put((find(chars[0]) * 45 + find(chars[1])) as u64, 11);
                    } else {
                        buf.put(find(chars[0]) as u64, 6);
                    }
                }
            }
            _ => {
                for &c in &self.data {
                    buf.put(c as u64, 8);
                }
            }
        }
    }
}

/// MODE_SIZE_SMALL / MEDIUM / LARGE
fn mode_sizes(version: i64) -> usize {
    if version < 10 { 0 } else if version < 27 { 1 } else { 2 }
}

fn length_in_bits(mode: u8, sizes: usize) -> u32 {
    let row: [u32; 3] = match mode {
        MODE_NUMBER => [10, 12, 14],
        MODE_ALPHA_NUM => [9, 11, 13],
        _ => [8, 16, 16],
    };
    row[sizes]
}

/// RS_BLOCK_OFFSET: L=1 -> 0, M=0 -> 1, Q=3 -> 2, H=2 -> 3
fn rs_offset(ec: i64) -> Option<usize> {
    match ec {
        1 => Some(0),
        0 => Some(1),
        3 => Some(2),
        2 => Some(3),
        _ => None,
    }
}

fn rs_blocks(version: i64, ec: i64) -> R<Vec<(usize, usize)>> {
    let Some(off) = rs_offset(ec) else {
        return Err(Exc::msg(&EXCEPTION, format!("bad rs block @ version: {version} / error_correction: {ec}")));
    };
    let row = RS_BLOCK_TABLE[(version as usize - 1) * 4 + off];
    let mut blocks = vec![];
    for t in row.chunks(3) {
        for _ in 0..t[0] {
            blocks.push((t[1] as usize, t[2] as usize));
        }
    }
    Ok(blocks)
}

/// BIT_LIMIT_TABLE[ec] (a list index: -4..=3)
fn bit_limits(ec: i64) -> R<Vec<usize>> {
    let ec = if ec < 0 { ec + 4 } else { ec };
    if !(0..4).contains(&ec) {
        return Err(Exc::msg(&INDEX_ERROR, "list index out of range"));
    }
    let mut t = vec![0];
    for v in 1..=40 {
        t.push(8 * rs_blocks(v, ec)?.iter().map(|b| b.1).sum::<usize>());
    }
    Ok(t)
}

// GF(256) of base.py
struct Gf {
    exp: [u8; 256],
    log: [u8; 256],
}

static GF: std::sync::LazyLock<Gf> = std::sync::LazyLock::new(|| {
    let mut exp = [0u16; 256];
    for i in 0..8 {
        exp[i] = 1 << i;
    }
    for i in 8..256 {
        exp[i] = exp[i - 4] ^ exp[i - 5] ^ exp[i - 6] ^ exp[i - 8];
    }
    let mut log: [u8; 256] = std::array::from_fn(|i| i as u8);
    for i in 0..255 {
        log[exp[i] as usize] = i as u8;
    }
    Gf { exp: exp.map(|x| x as u8), log }
});

fn glog(n: u8) -> R<usize> {
    if n < 1 {
        return Err(Exc::value_error(format!("glog({n})")));
    }
    Ok(GF.log[n as usize] as usize)
}

fn gexp(n: usize) -> u8 {
    GF.exp[n % 255]
}

/// base.Polynomial(num, shift): the leading zeros dropped (all but the last one of an all-zero list)
fn poly(num: &[u8], shift: usize) -> Vec<u8> {
    let mut off = 0;
    for (i, &x) in num.iter().enumerate() {
        off = i;
        if x != 0 {
            break;
        }
    }
    let mut v = num[off..].to_vec();
    v.extend(std::iter::repeat_n(0, shift));
    v
}

fn poly_mul(a: &[u8], b: &[u8]) -> R<Vec<u8>> {
    let mut num = vec![0u8; a.len() + b.len() - 1];
    for (i, &x) in a.iter().enumerate() {
        for (j, &y) in b.iter().enumerate() {
            num[i + j] ^= gexp(glog(x)? + glog(y)?);
        }
    }
    Ok(poly(&num, 0))
}

fn poly_mod(a: &[u8], b: &[u8]) -> R<Vec<u8>> {
    let mut a = a.to_vec();
    loop {
        if a.len() < b.len() {
            return Ok(a);
        }
        let difference = a.len() - b.len();
        // ratio = glog(self[0]) - glog(other[0]): may be negative, gexp takes it modulo 255 as Python's %
        let ratio = glog(a[0])? as i64 - glog(b[0])? as i64;
        let mut num = Vec::with_capacity(a.len());
        for (x, &y) in a.iter().zip(b) {
            num.push(x ^ gexp((glog(y)? as i64 + ratio).rem_euclid(255) as usize));
        }
        if difference > 0 {
            num.extend_from_slice(&a[a.len() - difference..]);
        }
        a = poly(&num, 0);
    }
}

fn create_bytes(buffer: &BitBuffer, blocks: &[(usize, usize)]) -> R<Vec<u8>> {
    let mut offset = 0;
    let (mut max_dc, mut max_ec) = (0, 0);
    let mut dcdata = vec![];
    let mut ecdata = vec![];
    for &(total, dc_count) in blocks {
        let ec_count = total - dc_count;
        max_dc = max_dc.max(dc_count);
        max_ec = max_ec.max(ec_count);
        let current_dc: Vec<u8> = buffer.buffer[offset..offset + dc_count].to_vec();
        offset += dc_count;
        let mut rs_poly = vec![1u8];
        for i in 0..ec_count {
            rs_poly = poly_mul(&rs_poly, &[1, gexp(i)])?;
        }
        let raw = poly(&current_dc, rs_poly.len() - 1);
        let modp = poly_mod(&raw, &rs_poly)?;
        let mod_offset = modp.len() as i64 - ec_count as i64;
        let current_ec: Vec<u8> = (0..ec_count as i64)
            .map(|i| if i + mod_offset >= 0 { modp[(i + mod_offset) as usize] } else { 0 })
            .collect();
        dcdata.push(current_dc);
        ecdata.push(current_ec);
    }
    let mut data = vec![];
    for i in 0..max_dc {
        data.extend(dcdata.iter().filter_map(|d| d.get(i)));
    }
    for i in 0..max_ec {
        data.extend(ecdata.iter().filter_map(|d| d.get(i)));
    }
    Ok(data)
}

fn create_data(version: i64, ec: i64, data_list: &[Data]) -> R<Vec<u8>> {
    let mut buffer = BitBuffer::new();
    for d in data_list {
        buffer.put(d.mode as u64, 4);
        check_version(version)?;
        buffer.put(d.data.len() as u64, length_in_bits(d.mode, mode_sizes(version)));
        d.write(&mut buffer);
    }
    let blocks = rs_blocks(version, ec)?;
    let bit_limit: usize = blocks.iter().map(|b| b.1 * 8).sum();
    if buffer.length > bit_limit {
        return Err(Exc::msg(&QRCODE_DATA_OVERFLOW_ERROR,
                            format!("Code length overflow. Data size ({}) > size available ({bit_limit})", buffer.length)));
    }
    for _ in 0..(bit_limit - buffer.length).min(4) {
        buffer.put_bit(false);
    }
    let delimit = buffer.length % 8;
    if delimit != 0 {
        for _ in 0..8 - delimit {
            buffer.put_bit(false);
        }
    }
    let fill = (bit_limit - buffer.length) / 8;
    for i in 0..fill {
        buffer.put(if i % 2 == 0 { 0xEC } else { 0x11 }, 8);
    }
    create_bytes(&buffer, &blocks)
}

fn bch_digit(mut data: u32) -> i32 {
    let mut digit = 0;
    while data != 0 {
        digit += 1;
        data >>= 1;
    }
    digit
}

const G15: u32 = (1 << 10) | (1 << 8) | (1 << 5) | (1 << 4) | (1 << 2) | (1 << 1) | 1;
const G18: u32 = (1 << 12) | (1 << 11) | (1 << 10) | (1 << 9) | (1 << 8) | (1 << 5) | (1 << 2) | 1;
const G15_MASK: u32 = (1 << 14) | (1 << 12) | (1 << 10) | (1 << 4) | (1 << 1);

fn bch_type_info(data: u32) -> u32 {
    let mut d = data << 10;
    while bch_digit(d) - bch_digit(G15) >= 0 {
        d ^= G15 << (bch_digit(d) - bch_digit(G15));
    }
    ((data << 10) | d) ^ G15_MASK
}

fn bch_type_number(data: u32) -> u32 {
    let mut d = data << 12;
    while bch_digit(d) - bch_digit(G18) >= 0 {
        d ^= G18 << (bch_digit(d) - bch_digit(G18));
    }
    (data << 12) | d
}

fn mask_func(pattern: i64, i: usize, j: usize) -> bool {
    match pattern {
        0 => (i + j) % 2 == 0,
        1 => i % 2 == 0,
        2 => j % 3 == 0,
        3 => (i + j) % 3 == 0,
        4 => (i / 2 + j / 3) % 2 == 0,
        5 => (i * j) % 2 + (i * j) % 3 == 0,
        6 => ((i * j) % 2 + (i * j) % 3) % 2 == 0,
        _ => ((i * j) % 3 + (i + j) % 2) % 2 == 0,
    }
}

// ---------------------------------------------------------------- lost points (util.py)

fn lost_point(m: &[Vec<Option<bool>>]) -> i64 {
    let n = m.len();
    let at = |r: usize, c: usize| m[r][c].unwrap_or(false);
    // level 1
    let mut container = vec![0i64; n + 1];
    for row in 0..n {
        let mut prev = at(row, 0);
        let mut length = 0;
        for col in 0..n {
            if at(row, col) == prev {
                length += 1;
            } else {
                if length >= 5 {
                    container[length] += 1;
                }
                length = 1;
                prev = at(row, col);
            }
        }
        if length >= 5 {
            container[length] += 1;
        }
    }
    for col in 0..n {
        let mut prev = at(0, col);
        let mut length = 0;
        for row in 0..n {
            if at(row, col) == prev {
                length += 1;
            } else {
                if length >= 5 {
                    container[length] += 1;
                }
                length = 1;
                prev = at(row, col);
            }
        }
        if length >= 5 {
            container[length] += 1;
        }
    }
    let mut lost: i64 = (5..=n).map(|l| container[l] * (l as i64 - 2)).sum();
    // level 2 (`next(it)` skips the following column)
    for row in 0..n - 1 {
        let mut col = 0;
        while col < n - 1 {
            let top_right = at(row, col + 1);
            if top_right != at(row + 1, col + 1) {
                col += 2;
                continue;
            } else if top_right != at(row, col) || top_right != at(row + 1, col) {
            } else {
                lost += 3;
            }
            col += 1;
        }
    }
    // level 3
    let finder = |x: &dyn Fn(usize) -> bool| {
        !x(1) && x(4) && !x(5) && x(6) && !x(9)
            && (x(0) && x(2) && x(3) && !x(7) && !x(8) && !x(10) || !x(0) && !x(2) && !x(3) && x(7) && x(8) && x(10))
    };
    let short = n.saturating_sub(10);
    for row in 0..n {
        let mut col = 0;
        while col < short {
            if finder(&|k| at(row, col + k)) {
                lost += 40;
            }
            col += if at(row, col + 10) { 2 } else { 1 };
        }
    }
    for col in 0..n {
        let mut row = 0;
        while row < short {
            if finder(&|k| at(row + k, col)) {
                lost += 40;
            }
            row += if at(row + 10, col) { 2 } else { 1 };
        }
    }
    // level 4
    let dark: usize = m.iter().map(|r| r.iter().filter(|x| x.unwrap_or(false)).count()).sum();
    let percent = dark as f64 / (n * n) as f64;
    let rating = ((percent * 100.0 - 50.0).abs() / 5.0).trunc() as i64;
    lost + rating * 10
}

// ---------------------------------------------------------------- QRCode (main.py)

impl Code {
    /// the `version` property: best_fit() when unset
    fn version(&mut self) -> R<i64> {
        if self.version.is_none() {
            self.best_fit(None)?;
        }
        Ok(self.version.expect("set by best_fit"))
    }

    fn clear(&mut self) {
        self.modules = vec![vec![]];
        self.count = 0;
        self.data_cache = None;
        self.data_list.clear();
    }

    fn add_data(&mut self, data: &V, optimize: i64) -> R<()> {
        let data: Vec<u8> = match data {
            V::Bytes(b) => b.to_vec(),
            V::Native(n) if matches!(&**n, Native::Qr(_)) => return Err(unported("add_data(<qrcode object>)")),
            other => ops::str_(other)?.into_bytes(),
        };
        if optimize != 0 {
            let minimum = optimize as usize;
            let whole = data.len() <= minimum;
            for (is_num, chunk) in optimal_split(&data, |b| b.is_ascii_digit(), whole, minimum) {
                if is_num {
                    self.data_list.push(Data { mode: MODE_NUMBER, data: chunk.to_vec() });
                } else {
                    for (is_alpha, sub) in optimal_split(chunk, is_alnum, whole, minimum) {
                        let mode = if is_alpha { MODE_ALPHA_NUM } else { MODE_8BIT_BYTE };
                        self.data_list.push(Data { mode, data: sub.to_vec() });
                    }
                }
            }
        } else {
            self.data_list.push(Data { mode: optimal_mode(&data), data });
        }
        self.data_cache = None;
        Ok(())
    }

    fn make(&mut self, fit: bool) -> R<()> {
        if fit {
            let start = self.version()?;
            self.best_fit(Some(start))?;
        } else {
            self.version()?;
        }
        let mask = match self.mask {
            Some(m) => m,
            None => self.best_mask_pattern()?,
        };
        self.make_impl(false, mask)
    }

    fn best_fit(&mut self, start: Option<i64>) -> R<i64> {
        let start = start.unwrap_or(1);
        check_version(start)?;
        let sizes = mode_sizes(start);
        let mut buffer = BitBuffer::new();
        for d in &self.data_list {
            buffer.put(d.mode as u64, 4);
            buffer.put(d.data.len() as u64, length_in_bits(d.mode, sizes));
            d.write(&mut buffer);
        }
        let needed = buffer.length;
        let table = bit_limits(self.ec)?;
        // bisect_left(table, needed, lo=start)
        let mut v = start as usize;
        while v < table.len() && table[v] < needed {
            v += 1;
        }
        // the setter checks the version before the DataOverflowError test can see 41
        check_version(v as i64)?;
        self.version = Some(v as i64);
        if sizes != mode_sizes(v as i64) {
            self.best_fit(Some(v as i64))?;
        }
        Ok(self.version.expect("set"))
    }

    fn best_mask_pattern(&mut self) -> R<i64> {
        let mut min_lost = 0;
        let mut pattern = 0;
        for i in 0..8 {
            self.make_impl(true, i)?;
            let lost = lost_point(&self.modules);
            if i == 0 || min_lost > lost {
                min_lost = lost;
                pattern = i;
            }
        }
        Ok(pattern)
    }

    fn make_impl(&mut self, test: bool, mask: i64) -> R<()> {
        let version = self.version()?;
        self.count = (version * 4 + 17) as usize;
        let n = self.count;
        self.modules = vec![vec![None; n]; n];
        self.probe(0, 0);
        self.probe(n - 7, 0);
        self.probe(0, n - 7);
        self.adjust(version);
        self.timing();
        self.type_info(test, mask);
        if version >= 7 {
            self.type_number(test, version);
        }
        if self.data_cache.is_none() {
            self.data_cache = Some(create_data(version, self.ec, &self.data_list)?);
        }
        let data = self.data_cache.take().expect("set");
        self.map_data(&data, mask);
        self.data_cache = Some(data);
        Ok(())
    }

    fn probe(&mut self, row: usize, col: usize) {
        let n = self.count as i64;
        for r in -1i64..8 {
            if row as i64 + r <= -1 || n <= row as i64 + r {
                continue;
            }
            for c in -1i64..8 {
                if col as i64 + c <= -1 || n <= col as i64 + c {
                    continue;
                }
                let on = ((0..=6).contains(&r) && (c == 0 || c == 6))
                    || ((0..=6).contains(&c) && (r == 0 || r == 6))
                    || ((2..=4).contains(&r) && (2..=4).contains(&c));
                self.modules[(row as i64 + r) as usize][(col as i64 + c) as usize] = Some(on);
            }
        }
    }

    fn timing(&mut self) {
        let n = self.count;
        for r in 8..n - 8 {
            if self.modules[r][6].is_none() {
                self.modules[r][6] = Some(r % 2 == 0);
            }
        }
        for c in 8..n - 8 {
            if self.modules[6][c].is_none() {
                self.modules[6][c] = Some(c % 2 == 0);
            }
        }
    }

    fn adjust(&mut self, version: i64) {
        let pos = PATTERN_POSITION_TABLE[version as usize - 1];
        for &row in pos {
            for &col in pos {
                let (row, col) = (row as usize, col as usize);
                if self.modules[row][col].is_some() {
                    continue;
                }
                for r in -2i64..=2 {
                    for c in -2i64..=2 {
                        let on = r == -2 || r == 2 || c == -2 || c == 2 || (r == 0 && c == 0);
                        self.modules[(row as i64 + r) as usize][(col as i64 + c) as usize] = Some(on);
                    }
                }
            }
        }
    }

    fn type_number(&mut self, test: bool, version: i64) {
        let bits = bch_type_number(version as u32);
        let n = self.count;
        for i in 0..18 {
            self.modules[i / 3][i % 3 + n - 8 - 3] = Some(!test && (bits >> i) & 1 == 1);
        }
        for i in 0..18 {
            self.modules[i % 3 + n - 8 - 3][i / 3] = Some(!test && (bits >> i) & 1 == 1);
        }
    }

    fn type_info(&mut self, test: bool, mask: i64) {
        let data = ((self.ec << 3) | mask) as u32;
        let bits = bch_type_info(data);
        let n = self.count;
        for i in 0..15 {
            let m = Some(!test && (bits >> i) & 1 == 1);
            if i < 6 {
                self.modules[i][8] = m;
            } else if i < 8 {
                self.modules[i + 1][8] = m;
            } else {
                self.modules[n - 15 + i][8] = m;
            }
        }
        for i in 0..15 {
            let m = Some(!test && (bits >> i) & 1 == 1);
            if i < 8 {
                self.modules[8][n - i - 1] = m;
            } else if i < 9 {
                self.modules[8][15 - i - 1 + 1] = m;
            } else {
                self.modules[8][15 - i - 1] = m;
            }
        }
        self.modules[n - 8][8] = Some(!test);
    }

    fn map_data(&mut self, data: &[u8], mask: i64) {
        let n = self.count as i64;
        let mut inc: i64 = -1;
        let mut row = n - 1;
        let mut bit_index: i32 = 7;
        let mut byte_index = 0usize;
        let mut col0 = n - 1;
        while col0 > 0 {
            let col = if col0 <= 6 { col0 - 1 } else { col0 };
            loop {
                for c in [col, col - 1] {
                    let cell = &mut self.modules[row as usize][c as usize];
                    if cell.is_none() {
                        let mut dark = byte_index < data.len() && (data[byte_index] >> bit_index) & 1 == 1;
                        if mask_func(mask, row as usize, c as usize) {
                            dark = !dark;
                        }
                        *cell = Some(dark);
                        bit_index -= 1;
                        if bit_index == -1 {
                            byte_index += 1;
                            bit_index = 7;
                        }
                    }
                }
                row += inc;
                if row < 0 || n <= row {
                    row -= inc;
                    inc = -inc;
                    break;
                }
            }
            col0 -= 2;
        }
    }

    /// `make_image(image_factory=None, **kwargs)`: PilImage(border, width, box_size, **kwargs), whose new_image
    /// reads back_color and fill_color and ignores the other keywords
    fn make_image(&mut self, args: &[V], kwargs: &[(String, V)]) -> R {
        if args.len() > 1 {
            return Err(Exc::type_error(format!("QRCode.make_image() takes from 1 to 2 positional arguments but {} were given",
                                               args.len() + 1)));
        }
        if !matches!(arg(args, kwargs, 0, "image_factory"), None | Some(V::None)) {
            return Err(unported("make_image(image_factory=)"));
        }
        let kw = |k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v);
        let mut embedded = false;
        for k in ["embedded_image_path", "embedded_image", "embeded_image_path", "embeded_image"] {
            if let Some(v) = kw(k) {
                embedded |= ops::truthy(v)?;
            }
        }
        if embedded && self.ec != 2 {
            return Err(Exc::value_error("Error correction level must be ERROR_CORRECT_H if an embedded image is provided"));
        }
        for (k, _) in kwargs {
            if !matches!(k.as_str(), "image_factory" | "fill_color" | "back_color") {
                return Err(unported(&format!("make_image({k}=)")));
            }
        }
        if self.box_size <= 0 {
            return Err(Exc::value_error(format!("Invalid box size (was {}, expected larger than 0)", self.box_size)));
        }
        if self.data_cache.is_none() {
            self.make(true)?;
        }
        let color = |k: &str, dflt: &str| -> R<String> {
            match kw(k) {
                None => Ok(dflt.into()),
                Some(V::Str(s)) => Ok(s.to_lowercase()),
                Some(_) => Err(unported(&format!("make_image({k}=) other than a str"))),
            }
        };
        let (fill, back) = (color("fill_color", "black")?, color("back_color", "white")?);
        if fill != "black" || back != "white" {
            return Err(unported(&format!("make_image(fill_color={fill:?}, back_color={back:?}): only black on white (a mode \"1\" image)")));
        }
        let img = Img {
            border: self.border,
            width: self.count as i64,
            box_size: self.box_size,
            modules: self.modules.iter().map(|r| r.iter().map(|x| x.unwrap_or(false)).collect()).collect(),
        };
        let ps = img.pixel_size();
        if ps > i32::MAX as i64 {
            return Err(Exc::msg(&MEMORY_ERROR, ""));
        }
        Ok(V::native(Native::Qr(Arc::new(Obj::Img(img)))))
    }
}

// ---------------------------------------------------------------- the image and its PNG (Pillow 12)


/// Pillow's ZipEncode deflate stream (level 6, Z_FILTERED, memLevel 9, window 15). Pillow's wheels do not bundle
/// the same zlib everywhere: the manylinux ones have zlib 1.3.1, compressed here by the zlib libz-sys builds
/// (1.3.2: the same deflate output, only casts and bookkeeping changed); the macOS ones have zlib-ng (2.3.3 in 12.3.0),
/// whose output zlib-rs reproduces.
#[cfg(target_os = "linux")]
struct Zip(Box<libz_sys::z_stream>);

#[cfg(target_os = "linux")]
impl Zip {
    fn new() -> R<Zip> {
        unsafe extern "C" fn zalloc(_: libz_sys::voidpf, items: libz_sys::uInt, size: libz_sys::uInt) -> libz_sys::voidpf {
            unsafe { libc::calloc(items as usize, size as usize) }
        }
        unsafe extern "C" fn zfree(_: libz_sys::voidpf, p: libz_sys::voidpf) {
            unsafe { libc::free(p) }
        }
        let mut s = Box::new(libz_sys::z_stream {
            next_in: std::ptr::null_mut(),
            avail_in: 0,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc,
            zfree,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        });
        let rc = unsafe {
            libz_sys::deflateInit2_(&mut *s, libz_sys::Z_DEFAULT_COMPRESSION, libz_sys::Z_DEFLATED, 15, 9, libz_sys::Z_FILTERED,
                                    libz_sys::zlibVersion(), std::mem::size_of::<libz_sys::z_stream>() as i32)
        };
        if rc != libz_sys::Z_OK {
            return Err(Exc::msg(&OS_ERROR, format!("encoder error {rc}")));
        }
        Ok(Zip(s))
    }

    /// one deflate call: (input bytes used, output bytes written, stream ended)
    fn compress(&mut self, input: &[u8], out: &mut [u8], finish: bool) -> R<(usize, usize, bool)> {
        let s = &mut *self.0;
        s.next_in = input.as_ptr() as *mut u8;
        s.avail_in = input.len() as libz_sys::uInt;
        s.next_out = out.as_mut_ptr();
        s.avail_out = out.len() as libz_sys::uInt;
        let rc = unsafe { libz_sys::deflate(s, if finish { libz_sys::Z_FINISH } else { libz_sys::Z_NO_FLUSH }) };
        if rc != libz_sys::Z_OK && rc != libz_sys::Z_STREAM_END && rc != libz_sys::Z_BUF_ERROR {
            return Err(Exc::msg(&OS_ERROR, format!("encoder error {rc}")));
        }
        Ok((input.len() - s.avail_in as usize, out.len() - s.avail_out as usize, rc == libz_sys::Z_STREAM_END))
    }
}

#[cfg(target_os = "linux")]
impl Drop for Zip {
    fn drop(&mut self) {
        unsafe { libz_sys::deflateEnd(&mut *self.0) };
    }
}

#[cfg(not(target_os = "linux"))]
struct Zip(zlib_rs::Deflate);

#[cfg(not(target_os = "linux"))]
impl Zip {
    fn new() -> R<Zip> {
        let cfg = zlib_rs::DeflateConfig {
            level: -1,
            method: zlib_rs::Method::Deflated,
            window_bits: 15,
            mem_level: 9,
            strategy: zlib_rs::Strategy::Filtered,
        };
        Ok(Zip(zlib_rs::Deflate::new_with_config(cfg)))
    }

    fn compress(&mut self, input: &[u8], out: &mut [u8], finish: bool) -> R<(usize, usize, bool)> {
        let z = &mut self.0;
        let flush = if finish { zlib_rs::DeflateFlush::Finish } else { zlib_rs::DeflateFlush::NoFlush };
        let (bi, bo) = (z.total_in(), z.total_out());
        let st = z.compress(input, out, flush).map_err(|e| Exc::msg(&OS_ERROR, format!("encoder error {e:?}")))?;
        Ok(((z.total_in() - bi) as usize, (z.total_out() - bo) as usize, matches!(st, zlib_rs::Status::StreamEnd)))
    }
}

impl Img {
    fn pixel_size(&self) -> i64 {
        (self.width + self.border * 2) * self.box_size
    }

    /// one row of the mode "1" image, packed as Pillow's pack1 does (white = 1, padding bits 0)
    fn row(&self, y: i64, out: &mut [u8]) {
        out.fill(0);
        let mrow = y / self.box_size - self.border;
        for x in 0..self.pixel_size() {
            let mcol = x / self.box_size - self.border;
            let dark = (0..self.width).contains(&mrow) && (0..self.width).contains(&mcol)
                && self.modules[mrow as usize][mcol as usize];
            if !dark {
                out[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
    }

    pub fn png(&self) -> R<Vec<u8>> {
        let w = self.pixel_size();
        let bytes = ((w + 7) / 8) as usize;
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = vec![];
        ihdr.extend((w as u32).to_be_bytes());
        ihdr.extend((w as u32).to_be_bytes());
        ihdr.extend([1, 0, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        let mut z = Zip::new()?;
        let bufsize = (65536usize).max(w as usize * 4);
        let mut out = vec![0u8; bufsize];
        let mut pos = 0usize;
        let mut idat: Vec<Vec<u8>> = vec![];
        // one encoder call per row as ZipEncode.c (a full output buffer ends an IDAT chunk), then Z_FINISH
        let mut feed = |input: &[u8], finish: bool, out: &mut Vec<u8>, pos: &mut usize, idat: &mut Vec<Vec<u8>>| -> R<()> {
            let mut inp = input;
            loop {
                let (used, made, end) = z.compress(inp, &mut out[*pos..], finish)?;
                inp = &inp[used..];
                *pos += made;
                let ended = finish && end;
                if *pos == out.len() {
                    idat.push(std::mem::take(out));
                    *out = vec![0u8; bufsize];
                    *pos = 0;
                    if ended || (!finish && inp.is_empty()) {
                        return Ok(());
                    }
                    continue;
                }
                if ended || (!finish && inp.is_empty()) {
                    return Ok(());
                }
            }
        };
        let mut buffer = vec![0u8; bytes + 1];
        let mut previous = vec![0u8; bytes + 1];
        let mut up = vec![2u8; bytes + 1];
        let mut prior = vec![1u8; bytes + 1];
        let mut paeth = vec![4u8; bytes + 1];
        let dist = |v: u8| -> i64 { if v < 128 { v as i64 } else { 256 - v as i64 } };
        for y in 0..w {
            self.row(y, &mut buffer[1..]);
            buffer[0] = 0;
            let mut which = 0;
            let mut sum: i64 = buffer[1..].iter().map(|&v| dist(v)).sum();
            if sum > 0 {
                let mut s = 0;
                for i in 1..=bytes {
                    up[i] = buffer[i].wrapping_sub(previous[i]);
                    s += dist(up[i]);
                }
                if s < sum {
                    which = 2;
                    sum = s;
                }
            }
            if sum > 0 {
                // bpp = 1
                prior[1] = buffer[1];
                let mut s = dist(prior[1]);
                for i in 2..=bytes {
                    prior[i] = buffer[i].wrapping_sub(buffer[i - 1]);
                    s += dist(prior[i]);
                }
                if s < sum {
                    which = 1;
                    sum = s;
                }
            }
            if sum > 0 {
                paeth[1] = buffer[1].wrapping_sub(previous[1]);
                let mut s = dist(paeth[1]);
                for i in 2..=bytes {
                    let (a, b, c) = (buffer[i - 1] as i32, previous[i] as i32, previous[i - 1] as i32);
                    let (pa, pb, pc) = ((b - c).abs(), (a - c).abs(), (a + b - 2 * c).abs());
                    let p = if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c };
                    paeth[i] = buffer[i].wrapping_sub(p as u8);
                    s += dist(paeth[i]);
                }
                if s < sum {
                    which = 4;
                }
            }
            let line: &[u8] = match which {
                2 => &up,
                1 => &prior,
                4 => &paeth,
                _ => &buffer,
            };
            feed(line, false, &mut out, &mut pos, &mut idat)?;
            std::mem::swap(&mut buffer, &mut previous);
        }
        feed(&[], true, &mut out, &mut pos, &mut idat)?;
        out.truncate(pos);
        idat.push(out);
        for c in &idat {
            chunk(&mut png, b"IDAT", c);
        }
        chunk(&mut png, b"IEND", b"");
        Ok(png)
    }

    /// `PilImage.save(stream, format=None, **kwargs)`: `kind` defaults the format; Image.save(stream, format)
    fn save(&self, args: &[V], kwargs: &[(String, V)]) -> R {
        let mut rest: Vec<(String, V)> = vec![];
        let mut kind = V::str("PNG");
        for (k, v) in kwargs {
            if k == "kind" {
                kind = v.clone();
            } else {
                rest.push((k.clone(), v.clone()));
            }
        }
        check_args("PilImage.save", args, &rest, &["stream", "format"])?;
        let stream = arg(args, &rest, 0, "stream")
            .ok_or_else(|| Exc::type_error("PilImage.save() missing 1 required positional argument: 'stream'"))?;
        let format = match arg(args, &rest, 1, "format") {
            None | Some(V::None) => kind,
            Some(f) => f.clone(),
        };
        match &format {
            V::Str(s) if s.eq_ignore_ascii_case("png") => {}
            _ => return Err(unported(&format!("image.save(format={}) other than PNG", ops::str_(&format)?))),
        }
        if rest.iter().any(|(k, _)| k != "stream" && k != "format") {
            return Err(unported("image.save() with encoder options"));
        }
        let is_bio = matches!(stream, V::Native(n) if matches!(&**n, Native::BytesIO(_)));
        if !is_bio {
            return Err(unported("image.save() into something other than an io.BytesIO"));
        }
        let png = self.png()?;
        let V::Native(n) = stream else { unreachable!() };
        let Native::BytesIO(b) = &**n else { unreachable!() };
        super::files::bytesio_method(b, "write", &[V::Bytes(Arc::from(png))])?;
        Ok(V::None)
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(kind);
    out.extend(data);
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend(crc.sum().to_be_bytes());
}

// qrcode 8.2 base.RS_BLOCK_TABLE: per version (1..=40) and level offset (L, M, Q, H), (count, total, data) triples
static RS_BLOCK_TABLE: [&[u16]; 160] = [
    &[1, 26, 19],
    &[1, 26, 16],
    &[1, 26, 13],
    &[1, 26, 9],
    &[1, 44, 34],
    &[1, 44, 28],
    &[1, 44, 22],
    &[1, 44, 16],
    &[1, 70, 55],
    &[1, 70, 44],
    &[2, 35, 17],
    &[2, 35, 13],
    &[1, 100, 80],
    &[2, 50, 32],
    &[2, 50, 24],
    &[4, 25, 9],
    &[1, 134, 108],
    &[2, 67, 43],
    &[2, 33, 15, 2, 34, 16],
    &[2, 33, 11, 2, 34, 12],
    &[2, 86, 68],
    &[4, 43, 27],
    &[4, 43, 19],
    &[4, 43, 15],
    &[2, 98, 78],
    &[4, 49, 31],
    &[2, 32, 14, 4, 33, 15],
    &[4, 39, 13, 1, 40, 14],
    &[2, 121, 97],
    &[2, 60, 38, 2, 61, 39],
    &[4, 40, 18, 2, 41, 19],
    &[4, 40, 14, 2, 41, 15],
    &[2, 146, 116],
    &[3, 58, 36, 2, 59, 37],
    &[4, 36, 16, 4, 37, 17],
    &[4, 36, 12, 4, 37, 13],
    &[2, 86, 68, 2, 87, 69],
    &[4, 69, 43, 1, 70, 44],
    &[6, 43, 19, 2, 44, 20],
    &[6, 43, 15, 2, 44, 16],
    &[4, 101, 81],
    &[1, 80, 50, 4, 81, 51],
    &[4, 50, 22, 4, 51, 23],
    &[3, 36, 12, 8, 37, 13],
    &[2, 116, 92, 2, 117, 93],
    &[6, 58, 36, 2, 59, 37],
    &[4, 46, 20, 6, 47, 21],
    &[7, 42, 14, 4, 43, 15],
    &[4, 133, 107],
    &[8, 59, 37, 1, 60, 38],
    &[8, 44, 20, 4, 45, 21],
    &[12, 33, 11, 4, 34, 12],
    &[3, 145, 115, 1, 146, 116],
    &[4, 64, 40, 5, 65, 41],
    &[11, 36, 16, 5, 37, 17],
    &[11, 36, 12, 5, 37, 13],
    &[5, 109, 87, 1, 110, 88],
    &[5, 65, 41, 5, 66, 42],
    &[5, 54, 24, 7, 55, 25],
    &[11, 36, 12, 7, 37, 13],
    &[5, 122, 98, 1, 123, 99],
    &[7, 73, 45, 3, 74, 46],
    &[15, 43, 19, 2, 44, 20],
    &[3, 45, 15, 13, 46, 16],
    &[1, 135, 107, 5, 136, 108],
    &[10, 74, 46, 1, 75, 47],
    &[1, 50, 22, 15, 51, 23],
    &[2, 42, 14, 17, 43, 15],
    &[5, 150, 120, 1, 151, 121],
    &[9, 69, 43, 4, 70, 44],
    &[17, 50, 22, 1, 51, 23],
    &[2, 42, 14, 19, 43, 15],
    &[3, 141, 113, 4, 142, 114],
    &[3, 70, 44, 11, 71, 45],
    &[17, 47, 21, 4, 48, 22],
    &[9, 39, 13, 16, 40, 14],
    &[3, 135, 107, 5, 136, 108],
    &[3, 67, 41, 13, 68, 42],
    &[15, 54, 24, 5, 55, 25],
    &[15, 43, 15, 10, 44, 16],
    &[4, 144, 116, 4, 145, 117],
    &[17, 68, 42],
    &[17, 50, 22, 6, 51, 23],
    &[19, 46, 16, 6, 47, 17],
    &[2, 139, 111, 7, 140, 112],
    &[17, 74, 46],
    &[7, 54, 24, 16, 55, 25],
    &[34, 37, 13],
    &[4, 151, 121, 5, 152, 122],
    &[4, 75, 47, 14, 76, 48],
    &[11, 54, 24, 14, 55, 25],
    &[16, 45, 15, 14, 46, 16],
    &[6, 147, 117, 4, 148, 118],
    &[6, 73, 45, 14, 74, 46],
    &[11, 54, 24, 16, 55, 25],
    &[30, 46, 16, 2, 47, 17],
    &[8, 132, 106, 4, 133, 107],
    &[8, 75, 47, 13, 76, 48],
    &[7, 54, 24, 22, 55, 25],
    &[22, 45, 15, 13, 46, 16],
    &[10, 142, 114, 2, 143, 115],
    &[19, 74, 46, 4, 75, 47],
    &[28, 50, 22, 6, 51, 23],
    &[33, 46, 16, 4, 47, 17],
    &[8, 152, 122, 4, 153, 123],
    &[22, 73, 45, 3, 74, 46],
    &[8, 53, 23, 26, 54, 24],
    &[12, 45, 15, 28, 46, 16],
    &[3, 147, 117, 10, 148, 118],
    &[3, 73, 45, 23, 74, 46],
    &[4, 54, 24, 31, 55, 25],
    &[11, 45, 15, 31, 46, 16],
    &[7, 146, 116, 7, 147, 117],
    &[21, 73, 45, 7, 74, 46],
    &[1, 53, 23, 37, 54, 24],
    &[19, 45, 15, 26, 46, 16],
    &[5, 145, 115, 10, 146, 116],
    &[19, 75, 47, 10, 76, 48],
    &[15, 54, 24, 25, 55, 25],
    &[23, 45, 15, 25, 46, 16],
    &[13, 145, 115, 3, 146, 116],
    &[2, 74, 46, 29, 75, 47],
    &[42, 54, 24, 1, 55, 25],
    &[23, 45, 15, 28, 46, 16],
    &[17, 145, 115],
    &[10, 74, 46, 23, 75, 47],
    &[10, 54, 24, 35, 55, 25],
    &[19, 45, 15, 35, 46, 16],
    &[17, 145, 115, 1, 146, 116],
    &[14, 74, 46, 21, 75, 47],
    &[29, 54, 24, 19, 55, 25],
    &[11, 45, 15, 46, 46, 16],
    &[13, 145, 115, 6, 146, 116],
    &[14, 74, 46, 23, 75, 47],
    &[44, 54, 24, 7, 55, 25],
    &[59, 46, 16, 1, 47, 17],
    &[12, 151, 121, 7, 152, 122],
    &[12, 75, 47, 26, 76, 48],
    &[39, 54, 24, 14, 55, 25],
    &[22, 45, 15, 41, 46, 16],
    &[6, 151, 121, 14, 152, 122],
    &[6, 75, 47, 34, 76, 48],
    &[46, 54, 24, 10, 55, 25],
    &[2, 45, 15, 64, 46, 16],
    &[17, 152, 122, 4, 153, 123],
    &[29, 74, 46, 14, 75, 47],
    &[49, 54, 24, 10, 55, 25],
    &[24, 45, 15, 46, 46, 16],
    &[4, 152, 122, 18, 153, 123],
    &[13, 74, 46, 32, 75, 47],
    &[48, 54, 24, 14, 55, 25],
    &[42, 45, 15, 32, 46, 16],
    &[20, 147, 117, 4, 148, 118],
    &[40, 75, 47, 7, 76, 48],
    &[43, 54, 24, 22, 55, 25],
    &[10, 45, 15, 67, 46, 16],
    &[19, 148, 118, 6, 149, 119],
    &[18, 75, 47, 31, 76, 48],
    &[34, 54, 24, 34, 55, 25],
    &[20, 45, 15, 61, 46, 16],
];
// qrcode 8.2 util.PATTERN_POSITION_TABLE
static PATTERN_POSITION_TABLE: [&[u8]; 40] = [
    &[],
    &[6, 18],
    &[6, 22],
    &[6, 26],
    &[6, 30],
    &[6, 34],
    &[6, 22, 38],
    &[6, 24, 42],
    &[6, 26, 46],
    &[6, 28, 50],
    &[6, 30, 54],
    &[6, 32, 58],
    &[6, 34, 62],
    &[6, 26, 46, 66],
    &[6, 26, 48, 70],
    &[6, 26, 50, 74],
    &[6, 30, 54, 78],
    &[6, 30, 56, 82],
    &[6, 30, 58, 86],
    &[6, 34, 62, 90],
    &[6, 28, 50, 72, 94],
    &[6, 26, 50, 74, 98],
    &[6, 30, 54, 78, 102],
    &[6, 28, 54, 80, 106],
    &[6, 32, 58, 84, 110],
    &[6, 30, 58, 86, 114],
    &[6, 34, 62, 90, 118],
    &[6, 26, 50, 74, 98, 122],
    &[6, 30, 54, 78, 102, 126],
    &[6, 26, 52, 78, 104, 130],
    &[6, 30, 56, 82, 108, 134],
    &[6, 34, 60, 86, 112, 138],
    &[6, 30, 58, 86, 114, 142],
    &[6, 34, 62, 90, 118, 146],
    &[6, 30, 54, 78, 102, 126, 150],
    &[6, 24, 50, 76, 102, 128, 154],
    &[6, 28, 54, 80, 106, 132, 158],
    &[6, 32, 58, 84, 110, 136, 162],
    &[6, 26, 54, 82, 110, 138, 166],
    &[6, 30, 58, 86, 114, 142, 170],
];
