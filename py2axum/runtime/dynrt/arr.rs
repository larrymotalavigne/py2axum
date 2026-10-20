//! `array.array`: typed arrays of numbers (typecodes b B h H i I l L q Q f d, LP64 sizes), CPython's checks
//! and messages on every store (each typecode parses as CPython's `*_setitem` does: 'b' as a short first, 'H'
//! as an int first...). The 'u' and 'w' character arrays are refused.
use std::sync::Arc;

use parking_lot::Mutex;

use super::ops;
use super::v::*;

pub struct Arr {
    pub tc: char,
    pub items: Mutex<Vec<V>>,
}

const CODES: &str = "bBhHiIlLqQfd";

pub fn itemsize(tc: char) -> usize {
    match tc {
        'b' | 'B' => 1,
        'h' | 'H' => 2,
        'i' | 'I' | 'f' => 4,
        _ => 8,
    }
}

fn overflow(msg: &str) -> Exc {
    Exc::new(&OVERFLOW_ERROR, vec![V::str(msg)])
}

/// the integer an item is given as (an int or a bool; anything else as CPython's `__index__` refusal)
fn int_of(v: &V) -> R<i64> {
    match v {
        V::Int(i) => Ok(*i),
        V::Bool(b) => Ok(*b as i64),
        other => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", other.type_name()))),
    }
}

fn range(x: i64, lo: i64, hi: i64, what: &str) -> R<()> {
    if x < lo {
        return Err(overflow(&format!("{what} is less than minimum")));
    }
    if x > hi {
        return Err(overflow(&format!("{what} is greater than maximum")));
    }
    Ok(())
}

/// `array[i] = v` for a typecode: the stored value, or CPython's error
fn check(tc: char, v: &V) -> R<V> {
    if tc == 'f' || tc == 'd' {
        let f = match v {
            V::Float(f) => *f,
            V::Int(i) => *i as f64,
            V::Bool(b) => *b as i64 as f64,
            other => return Err(Exc::type_error(format!("must be real number, not {}", other.type_name()))),
        };
        return Ok(V::Float(if tc == 'f' { f as f32 as f64 } else { f }));
    }
    let x = int_of(v)?;
    match tc {
        'b' => {
            range(x, i16::MIN as i64, i16::MAX as i64, "signed short integer")?;
            range(x, i8::MIN as i64, i8::MAX as i64, "signed char")?;
        }
        'B' => range(x, 0, u8::MAX as i64, "unsigned byte integer")?,
        'h' => range(x, i16::MIN as i64, i16::MAX as i64, "signed short integer")?,
        'H' => {
            range(x, i32::MIN as i64, i32::MAX as i64, "signed integer")?;
            range(x, 0, u16::MAX as i64, "unsigned short")?;
        }
        'i' => range(x, i32::MIN as i64, i32::MAX as i64, "signed integer")?,
        'I' => {
            if x < 0 {
                return Err(overflow("can't convert negative value to unsigned int"));
            }
            if x > u32::MAX as i64 {
                return Err(overflow("unsigned int is greater than maximum"));
            }
        }
        'L' if x < 0 => return Err(overflow("can't convert negative value to unsigned int")),
        'Q' if x < 0 => return Err(overflow("can't convert negative int to unsigned")),
        _ => {} // 'l', 'q' and the other 64-bit codes: every i64
    }
    Ok(V::Int(x))
}

fn typecode(v: &V) -> R<char> {
    let py315 = super::python() >= (3, 15);
    let s = match v {
        V::Str(s) => s.clone(),
        // 3.15 reads the typecode as a str, whatever the type of the argument
        other if py315 => return Err(Exc::type_error(format!("array() argument 1 must be str, not {}", other.type_name()))),
        other => return Err(Exc::type_error(format!("array() argument 1 must be a unicode character, not {}", other.type_name()))),
    };
    let mut cs = s.chars();
    let (Some(c), None) = (cs.next(), cs.next()) else {
        // 3.15: a string of any other length is a bad typecode (ValueError); before, a TypeError
        if py315 {
            if &*s == "Zf" || &*s == "Zd" {
                return Err(unsupported_315());
            }
            return Err(Exc::value_error(BAD_315));
        }
        return Err(Exc::type_error(if super::python() >= (3, 14) {
            format!("array() argument 1 must be a unicode character, not a string of length {}", s.chars().count())
        } else {
            "array() argument 1 must be a unicode character, not str".to_string()
        }));
    };
    if py315 && c == 'e' {
        return Err(unsupported_315());
    }
    if c == 'u' || c == 'w' {
        return Err(Exc::type_error("py2axum: array.array('u'/'w') (character arrays) is not supported"));
    }
    if !CODES.contains(c) {
        return Err(Exc::value_error(if py315 {
            BAD_315
        } else if super::python() >= (3, 13) {
            "bad typecode (must be b, B, u, w, h, H, i, I, l, L, q, Q, f or d)"
        } else {
            "bad typecode (must be b, B, u, h, H, i, I, l, L, q, Q, f or d)"
        }));
    }
    Ok(c)
}

fn unsupported_315() -> Exc {
    Exc::type_error("py2axum: array.array('e'/'Zf'/'Zd') (half floats, complex) is not supported")
}

const BAD_315: &str = "bad typecode (must be b, B, u, w, h, H, i, I, l, L, q, Q, e, f, d, Zf or Zd)";

/// `array.array(typecode[, initializer])`: a list, tuple, iterable or bytes-like initializer
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    if !kwargs.is_empty() {
        return Err(Exc::type_error("array.array() takes no keyword arguments"));
    }
    let (tc, init) = match args {
        [t] => (typecode(t)?, None),
        [t, i] => (typecode(t)?, Some(i)),
        [] => return Err(Exc::type_error("array() takes at least 1 argument (0 given)")),
        _ => return Err(Exc::type_error(format!("array() takes at most 2 arguments ({} given)", args.len()))),
    };
    let a = Arr { tc, items: Mutex::new(Vec::new()) };
    match init {
        None => {}
        Some(V::Str(_)) => {
            return Err(Exc::type_error(format!("cannot use a str to initialize an array with typecode '{tc}'")));
        }
        Some(V::Bytes(b)) => frombytes(&a, b)?,
        Some(it) => {
            let items = ops::iter(it)?.iter().map(|x| check(tc, x)).collect::<R<Vec<_>>>()?;
            *a.items.lock() = items;
        }
    }
    Ok(V::native(Native::Array(Arc::new(a))))
}

fn frombytes(a: &Arr, b: &[u8]) -> R<()> {
    let n = itemsize(a.tc);
    if b.len() % n != 0 {
        return Err(Exc::value_error("bytes length not a multiple of item size"));
    }
    let mut out = Vec::with_capacity(b.len() / n);
    for c in b.chunks(n) {
        let mut buf = [0u8; 8];
        buf[..n].copy_from_slice(c);
        out.push(match a.tc {
            'b' => V::Int(c[0] as i8 as i64),
            'B' => V::Int(c[0] as i64),
            'h' => V::Int(i16::from_ne_bytes([c[0], c[1]]) as i64),
            'H' => V::Int(u16::from_ne_bytes([c[0], c[1]]) as i64),
            'i' => V::Int(i32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as i64),
            'I' => V::Int(u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as i64),
            'f' => V::Float(f32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as f64),
            'd' => V::Float(f64::from_ne_bytes(buf)),
            'L' | 'Q' => {
                let u = u64::from_ne_bytes(buf);
                if u > i64::MAX as u64 {
                    return Err(Exc::type_error("py2axum: an array item beyond 2**63 - 1 is not supported"));
                }
                V::Int(u as i64)
            }
            _ => V::Int(i64::from_ne_bytes(buf)),
        });
    }
    a.items.lock().extend(out);
    Ok(())
}

fn tobytes(a: &Arr) -> Vec<u8> {
    let mut out = Vec::new();
    for x in a.items.lock().iter() {
        match (a.tc, x) {
            ('f', V::Float(f)) => out.extend((*f as f32).to_ne_bytes()),
            ('d', V::Float(f)) => out.extend(f.to_ne_bytes()),
            (tc, V::Int(i)) => out.extend(&i.to_ne_bytes()[..itemsize(tc)]),
            _ => {}
        }
    }
    out
}

fn index(a: &Arr, k: &V) -> R<usize> {
    let n = a.items.lock().len() as i64;
    let i = match k {
        V::Int(i) => *i,
        V::Bool(b) => *b as i64,
        _ => return Err(Exc::type_error("array indices must be integers")),
    };
    let j = if i < 0 { i + n } else { i };
    if j < 0 || j >= n {
        return Err(Exc::new(&INDEX_ERROR, vec![V::str("array index out of range")]));
    }
    Ok(j as usize)
}

pub fn getitem(a: &Arr, k: &V) -> R {
    let i = index(a, k)?;
    Ok(a.items.lock()[i].clone())
}

pub fn setitem(a: &Arr, k: &V, v: V) -> R<()> {
    let i = index(a, k).map_err(|e| {
        if e.isinstance(&INDEX_ERROR) { Exc::new(&INDEX_ERROR, vec![V::str("array assignment index out of range")]) } else { e }
    })?;
    let x = check(a.tc, &v)?;
    a.items.lock()[i] = x;
    Ok(())
}

pub fn items(a: &Arr) -> Vec<V> {
    a.items.lock().clone()
}

pub fn repr(a: &Arr) -> R<String> {
    let items = a.items.lock().clone();
    if items.is_empty() {
        return Ok(format!("array('{}')", a.tc));
    }
    let parts = items.iter().map(ops::repr).collect::<R<Vec<_>>>()?;
    Ok(format!("array('{}', [{}])", a.tc, parts.join(", ")))
}

/// `a == b` of two arrays: the same items (any typecodes)
pub fn eq(a: &Arr, b: &Arr) -> bool {
    let (x, y) = (a.items.lock().clone(), b.items.lock().clone());
    x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| ops::eq_bool(p, q))
}

pub fn method(a: &Arc<Arr>, name: &str, args: &[V]) -> R {
    let one = |what: &str| -> R<&V> {
        match args {
            [x] => Ok(x),
            _ => Err(Exc::type_error(format!("array.{what}() takes exactly one argument ({} given)", args.len()))),
        }
    };
    match name {
        "append" => {
            let x = check(a.tc, one("append")?)?;
            a.items.lock().push(x);
            Ok(V::None)
        }
        "extend" => {
            let src = one("extend")?;
            let xs = match src {
                V::Native(n) if matches!(&**n, Native::Array(_)) => {
                    let Native::Array(b) = &**n else { unreachable!() };
                    if b.tc != a.tc {
                        return Err(Exc::type_error("can only extend with array of same kind"));
                    }
                    items(b)
                }
                other => ops::iter(other)?.iter().map(|x| check(a.tc, x)).collect::<R<Vec<_>>>()?,
            };
            a.items.lock().extend(xs);
            Ok(V::None)
        }
        "tolist" if args.is_empty() => Ok(V::list(items(a))),
        "tobytes" if args.is_empty() => Ok(V::Bytes(Arc::from(tobytes(a)))),
        "frombytes" => match one("frombytes")? {
            V::Bytes(b) => frombytes(a, b).map(|_| V::None),
            other => Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", other.type_name()))),
        },
        "fromlist" => {
            let xs = match one("fromlist")? {
                V::List(l) => l.lock().iter().map(|x| check(a.tc, x)).collect::<R<Vec<_>>>()?,
                _ => return Err(Exc::type_error("arg must be list")),
            };
            a.items.lock().extend(xs);
            Ok(V::None)
        }
        "count" => {
            let x = one("count")?;
            Ok(V::Int(a.items.lock().iter().filter(|y| ops::eq_bool(y, x)).count() as i64))
        }
        "index" if args.len() == 1 => {
            let x = &args[0];
            match a.items.lock().iter().position(|y| ops::eq_bool(y, x)) {
                Some(i) => Ok(V::Int(i as i64)),
                None => Err(Exc::value_error("array.index(x): x not in array")),
            }
        }
        "reverse" if args.is_empty() => {
            a.items.lock().reverse();
            Ok(V::None)
        }
        "pop" => {
            let mut v = a.items.lock();
            if v.is_empty() {
                return Err(Exc::new(&INDEX_ERROR, vec![V::str("pop from empty array")]));
            }
            let n = v.len() as i64;
            let i = match args {
                [] => -1,
                [V::Int(i)] => *i,
                _ => return Err(Exc::type_error("py2axum: array.pop() takes an int index")),
            };
            let j = if i < 0 { i + n } else { i };
            if j < 0 || j >= n {
                return Err(Exc::new(&INDEX_ERROR, vec![V::str("pop index out of range")]));
            }
            Ok(v.remove(j as usize))
        }
        _ => Err(Exc::type_error(format!("py2axum: array.{name}() is not supported"))),
    }
}

pub fn attr(a: &Arr, name: &str) -> Option<V> {
    match name {
        "itemsize" => Some(V::Int(itemsize(a.tc) as i64)),
        "typecode" => Some(V::str(a.tc.to_string())),
        _ => None,
    }
}
