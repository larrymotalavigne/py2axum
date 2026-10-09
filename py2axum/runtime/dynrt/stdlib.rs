//! Python standard library pieces: `re`, `io.StringIO`, `csv`, `math`, `time`, `os.path`, `hmac`.
use std::sync::Arc;

use parking_lot::Mutex;

use super::ops;
use super::v::*;

fn kw<'a>(kwargs: &'a [(String, V)], name: &str) -> Option<&'a V> {
    kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn s_arg(v: Option<&V>, what: &str) -> R<String> {
    match v {
        Some(V::Str(s)) => Ok(s.to_string()),
        Some(o) => Err(Exc::type_error(format!("expected string or bytes-like object, got '{}'", o.type_name()))),
        None => Err(Exc::type_error(format!("missing required argument '{what}'"))),
    }
}

// ---------------------------------------------------------------- re

pub const I: i64 = 2;
pub const M: i64 = 8;
pub const S: i64 = 16;
pub const X: i64 = 64;

pub struct Pattern {
    pub src: String,
    pub flags: i64,
    re: fancy_regex::Regex,
    names: Vec<Option<String>>,
}

pub struct Match {
    string: Arc<str>,
    /// byte spans per group (0 = whole match)
    spans: Vec<Option<(usize, usize)>>,
    names: Vec<Option<String>>,
}

/// Python pattern syntax -> fancy-regex: (?P=name) backrefs, \Z, `$` (end or before a final newline).
fn translate(p: &str, flags: i64) -> String {
    let mut out = String::new();
    let chars: Vec<char> = p.chars().collect();
    let (mut i, mut in_class) = (0, false);
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            if !in_class && n == 'Z' {
                out += "\\z";
            } else {
                out.push(c);
                out.push(n);
            }
            i += 2;
            continue;
        }
        if in_class {
            if c == ']' {
                in_class = false;
            }
            out.push(c);
        } else if c == '[' {
            in_class = true;
            out.push(c);
            // a leading ']' or '^]' is literal
            if chars.get(i + 1) == Some(&'^') {
                out.push('^');
                i += 1;
            }
            if chars.get(i + 1) == Some(&']') {
                out += "\\]";
                i += 1;
            }
        } else if c == '(' && p[p.char_indices().nth(i).unwrap().0..].starts_with("(?P=") {
            let rest: String = chars[i + 4..].iter().collect();
            let end = rest.find(')').unwrap_or(rest.len());
            out += &format!("\\k<{}>", &rest[..end]);
            i += 4 + end + 1;
            continue;
        } else if c == '$' && flags & M == 0 {
            out += "(?=\\n?\\z)";
        } else {
            out.push(c);
        }
        i += 1;
    }
    let mut prefix = String::new();
    if flags & I != 0 {
        prefix.push('i');
    }
    if flags & M != 0 {
        prefix.push('m');
    }
    if flags & S != 0 {
        prefix.push('s');
    }
    if flags & X != 0 {
        prefix.push('x');
    }
    if prefix.is_empty() { out } else { format!("(?{prefix}){out}") }
}

pub fn compile(pattern: &V, flags: Option<&V>) -> R {
    if let V::Native(n) = pattern {
        if let Native::Pattern(_) = &**n {
            return Ok(pattern.clone());
        }
    }
    let src = s_arg(Some(pattern), "pattern")?;
    let flags = match flags {
        None | Some(V::None) => 0,
        Some(V::Int(f)) => *f,
        Some(o) => return Err(Exc::type_error(format!("flags must be int, not {}", o.type_name()))),
    };
    let re = fancy_regex::Regex::new(&translate(&src, flags)).map_err(|e| Exc::msg(&RE_ERROR, format!("{e}")))?;
    let names = re.capture_names().map(|n| n.map(str::to_string)).collect();
    Ok(V::native(Native::Pattern(Arc::new(Pattern { src, flags, re, names }))))
}

fn pattern_of(v: &V) -> R<Arc<Pattern>> {
    match v {
        V::Native(n) => match &**n {
            Native::Pattern(p) => Ok(p.clone()),
            _ => Err(Exc::type_error("first argument must be a string or compiled pattern")),
        },
        _ => Err(Exc::type_error("first argument must be a string or compiled pattern")),
    }
}

fn captures_at(p: &Pattern, s: &Arc<str>, start: usize) -> R<Option<Match>> {
    let caps = p.re.captures_from_pos(s, start).map_err(|e| Exc::runtime(format!("re: {e}")))?;
    Ok(caps.map(|c| Match {
        string: s.clone(),
        spans: (0..c.len()).map(|i| c.get(i).map(|m| (m.start(), m.end()))).collect(),
        names: p.names.clone(),
    }))
}

fn all_matches(p: &Pattern, s: &Arc<str>) -> R<Vec<Match>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= s.len() {
        let Some(m) = captures_at(p, s, pos)? else { break };
        let (a, b) = m.spans[0].unwrap();
        pos = if b == a { b + s[b..].chars().next().map(char::len_utf8).unwrap_or(1) } else { b };
        out.push(m);
    }
    Ok(out)
}

fn char_index(s: &str, byte: usize) -> i64 {
    s[..byte].chars().count() as i64
}

fn group_value(m: &Match, i: usize) -> V {
    match m.spans.get(i).copied().flatten() {
        Some((a, b)) => V::str(&m.string[a..b]),
        None => V::None,
    }
}

fn group_index(m: &Match, g: &V) -> R<usize> {
    match g {
        V::Int(i) if (*i as usize) < m.spans.len() && *i >= 0 => Ok(*i as usize),
        V::Str(n) => m.names.iter().position(|x| x.as_deref() == Some(&**n)).ok_or_else(|| Exc::msg(&INDEX_ERROR, "no such group")),
        _ => Err(Exc::msg(&INDEX_ERROR, "no such group")),
    }
}

pub fn match_method(m: &Match, name: &str, args: &[V]) -> R {
    match name {
        "group" => {
            if args.len() <= 1 {
                let i = args.first().map(|g| group_index(m, g)).transpose()?.unwrap_or(0);
                Ok(group_value(m, i))
            } else {
                Ok(V::tuple(args.iter().map(|g| group_index(m, g).map(|i| group_value(m, i))).collect::<R<_>>()?))
            }
        }
        "groups" => {
            let d = args.first().cloned().unwrap_or(V::None);
            Ok(V::tuple((1..m.spans.len()).map(|i| if m.spans[i].is_some() { group_value(m, i) } else { d.clone() }).collect()))
        }
        "groupdict" => {
            let d = args.first().cloned().unwrap_or(V::None);
            V::dict_from(
                m.names
                    .iter()
                    .enumerate()
                    .filter_map(|(i, n)| n.as_ref().map(|n| (V::str(n), if m.spans[i].is_some() { group_value(m, i) } else { d.clone() })))
                    .collect(),
            )
        }
        "start" | "end" | "span" => {
            let i = args.first().map(|g| group_index(m, g)).transpose()?.unwrap_or(0);
            let (a, b) = match m.spans[i] {
                Some((a, b)) => (char_index(&m.string, a), char_index(&m.string, b)),
                None => (-1, -1),
            };
            Ok(match name {
                "start" => V::Int(a),
                "end" => V::Int(b),
                _ => V::tuple(vec![V::Int(a), V::Int(b)]),
            })
        }
        _ => Err(Exc::attr_error(format!("'re.Match' object has no attribute '{name}'"))),
    }
}

pub fn match_attr(m: &Match, name: &str) -> R {
    match name {
        "string" => Ok(V::str(&*m.string)),
        "lastindex" => Ok(m.spans.iter().enumerate().skip(1).filter(|(_, s)| s.is_some()).map(|(i, _)| V::Int(i as i64)).last().unwrap_or(V::None)),
        _ => Err(Exc::attr_error(format!("'re.Match' object has no attribute '{name}'"))),
    }
}

enum Piece {
    Lit(String),
    Group(usize),
}

/// The replacement template (\1, \g<1>, \g<name>, \n...), checked like `re._compile_template`.
fn parse_repl(p: &Pattern, repl: &str) -> R<Vec<Piece>> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let chars: Vec<char> = repl.chars().collect();
    let mut i = 0;
    let bad = |m: String| Exc::msg(&RE_ERROR, m);
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' || i + 1 >= chars.len() {
            lit.push(c);
            i += 1;
            continue;
        }
        let n = chars[i + 1];
        i += 2;
        let push_group = |g: usize, lit: &mut String, out: &mut Vec<Piece>| {
            if !lit.is_empty() {
                out.push(Piece::Lit(std::mem::take(lit)));
            }
            out.push(Piece::Group(g));
        };
        match n {
            'g' => {
                let rest: String = chars[i..].iter().collect();
                if !rest.starts_with('<') || !rest.contains('>') {
                    return Err(bad("missing <".into()));
                }
                let name = &rest[1..rest.find('>').unwrap()];
                i += name.chars().count() + 2;
                let g = match name.parse::<usize>() {
                    Ok(g) => g,
                    Err(_) => p.names.iter().position(|x| x.as_deref() == Some(name)).ok_or_else(|| Exc::msg(&INDEX_ERROR, format!("unknown group name '{name}'")))?,
                };
                if g >= p.names.len() {
                    return Err(bad(format!("invalid group reference {g}")));
                }
                push_group(g, &mut lit, &mut out);
            }
            '0'..='9' => {
                let mut num = n.to_digit(10).unwrap() as usize;
                if let Some(d) = chars.get(i).and_then(|c| c.to_digit(10)) {
                    num = num * 10 + d as usize;
                    i += 1;
                }
                if num >= p.names.len() {
                    return Err(bad(format!("invalid group reference {num}")));
                }
                push_group(num, &mut lit, &mut out);
            }
            'n' => lit.push('\n'),
            't' => lit.push('\t'),
            'r' => lit.push('\r'),
            '\\' => lit.push('\\'),
            'a' => lit.push('\u{7}'),
            'f' => lit.push('\u{c}'),
            'v' => lit.push('\u{b}'),
            c if c.is_ascii_alphabetic() => return Err(bad(format!("bad escape \\{c}"))),
            c => {
                lit.push('\\');
                lit.push(c);
            }
        }
    }
    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }
    Ok(out)
}

async fn sub(cx: &super::Cx, p: &Pattern, repl: &V, string: &V, count: i64, with_n: bool) -> R {
    let s: Arc<str> = Arc::from(s_arg(Some(string), "string")?.as_str());
    let pieces = match repl {
        V::Str(r) => Some(parse_repl(p, r)?),
        _ => None,
    };
    let mut out = String::new();
    let mut last = 0;
    let mut n = 0;
    for m in all_matches(p, &s)? {
        if count > 0 && n >= count {
            break;
        }
        let (a, b) = m.spans[0].unwrap();
        out += &s[last..a];
        match &pieces {
            Some(ps) => {
                for piece in ps {
                    match piece {
                        Piece::Lit(l) => out += l,
                        Piece::Group(g) => {
                            if let Some((x, y)) = m.spans[*g] {
                                out += &s[x..y];
                            }
                        }
                    }
                }
            }
            None => {
                let r = super::methods::call_value(cx, repl, vec![V::native(Native::Match(Arc::new(m)))], vec![]).await?;
                out += &s_arg(Some(&r), "repl result")?;
            }
        }
        last = b;
        n += 1;
    }
    out += &s[last..];
    Ok(if with_n { V::tuple(vec![V::str(out), V::Int(n)]) } else { V::str(out) })
}

fn findall(p: &Pattern, s: &V) -> R {
    let s: Arc<str> = Arc::from(s_arg(Some(s), "string")?.as_str());
    let groups = p.names.len() - 1;
    Ok(V::list(
        all_matches(p, &s)?
            .into_iter()
            .map(|m| {
                let g = |i: usize| match m.spans[i] {
                    Some((a, b)) => V::str(&s[a..b]),
                    None => V::str(""),
                };
                match groups {
                    0 => g(0),
                    1 => g(1),
                    k => V::tuple((1..=k).map(g).collect()),
                }
            })
            .collect(),
    ))
}

fn split(p: &Pattern, s: &V, maxsplit: i64) -> R {
    let s: Arc<str> = Arc::from(s_arg(Some(s), "string")?.as_str());
    let mut out = Vec::new();
    let mut last = 0;
    for (n, m) in all_matches(p, &s)?.into_iter().enumerate() {
        if maxsplit > 0 && n as i64 >= maxsplit {
            break;
        }
        let (a, b) = m.spans[0].unwrap();
        out.push(V::str(&s[last..a]));
        for i in 1..m.spans.len() {
            out.push(group_value(&m, i));
        }
        last = b;
    }
    out.push(V::str(&s[last..]));
    Ok(V::list(out))
}

/// `Pattern.search(string, pos=0, endpos=len)`: the search starts at `pos` (`^` and lookbehinds still see the whole
/// string, as in CPython) in the string cut at `endpos`; spans count from the start of the string
fn search_from(p: &Pattern, s: &V, pos: Option<V>, endpos: Option<V>) -> R {
    let full: Arc<str> = Arc::from(s_arg(Some(s), "string")?.as_str());
    let n = full.chars().count() as i64;
    let idx = |v: Option<V>, d: i64| -> R<i64> {
        match v {
            None => Ok(d),
            Some(V::Int(i)) => Ok(i.clamp(0, n)),
            Some(V::Bool(b)) => Ok((b as i64).min(n)),
            Some(o) => Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
        }
    };
    let (pos, end) = (idx(pos, 0)?, idx(endpos, n)?);
    if end < pos {
        return Ok(V::None);
    }
    let byte = |c: i64| full.char_indices().nth(c as usize).map(|(b, _)| b).unwrap_or(full.len());
    let cut: Arc<str> = Arc::from(&full[..byte(end)]);
    Ok(match captures_at(p, &cut, byte(pos))? {
        Some(mut m) => {
            m.string = full;
            V::native(Native::Match(Arc::new(m)))
        }
        None => V::None,
    })
}

fn search_like(p: &Pattern, s: &V, mode: &str) -> R {
    let s: Arc<str> = Arc::from(s_arg(Some(s), "string")?.as_str());
    let m = captures_at(p, &s, 0)?;
    // match = anchored at the start, fullmatch = the whole string
    let ok = |m: &Match| match mode {
        "match" => m.spans[0].unwrap().0 == 0,
        "fullmatch" => m.spans[0].unwrap() == (0, s.len()),
        _ => true,
    };
    if mode == "search" {
        return Ok(m.map(|m| V::native(Native::Match(Arc::new(m)))).unwrap_or(V::None));
    }
    // anchored forms: compile an anchored variant to keep backtracking semantics
    let anchored = fancy_regex::Regex::new(&format!(
        "\\A(?:{}){}",
        translate(&p.src, p.flags),
        if mode == "fullmatch" { "\\z" } else { "" }
    ))
    .map_err(|e| Exc::msg(&RE_ERROR, format!("{e}")))?;
    let _ = (m, ok);
    let c = anchored.captures(&s).map_err(|e| Exc::runtime(format!("re: {e}")))?;
    Ok(match c {
        None => V::None,
        Some(c) => V::native(Native::Match(Arc::new(Match {
            string: s.clone(),
            spans: (0..c.len()).map(|i| c.get(i).map(|m| (m.start(), m.end()))).collect(),
            names: p.names.clone(),
        }))),
    })
}

pub async fn pattern_method(cx: &super::Cx, p: &Arc<Pattern>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let a = |i: usize, k: &str| args.get(i).or_else(|| kw(kwargs, k)).cloned();
    // search(string, pos, endpos); the other methods' pos/endpos: refused, never ignored
    if name == "search" && (args.len() > 1 || kwargs.iter().any(|(k, _)| k != "string")) {
        if args.len() > 3 || kwargs.iter().any(|(k, _)| !matches!(k.as_str(), "string" | "pos" | "endpos")) {
            return Err(Exc::type_error("search() takes at most 3 arguments"));
        }
        return search_from(p, &a(0, "string").unwrap_or(V::None), a(1, "pos"), a(2, "endpos"));
    }
    if matches!(name, "match" | "fullmatch" | "findall" | "finditer") && (args.len() > 1 || kwargs.iter().any(|(k, _)| k != "string")) {
        return Err(Exc::type_error(format!("py2axum: Pattern.{name}() with pos/endpos is not supported")));
    }
    match name {
        "search" | "match" | "fullmatch" => search_like(p, &a(0, "string").unwrap_or(V::None), name),
        "findall" => findall(p, &a(0, "string").unwrap_or(V::None)),
        "finditer" => {
            let s: Arc<str> = Arc::from(s_arg(a(0, "string").as_ref(), "string")?.as_str());
            Ok(V::list(all_matches(p, &s)?.into_iter().map(|m| V::native(Native::Match(Arc::new(m)))).collect()))
        }
        "sub" | "subn" => {
            let count = match a(2, "count") {
                Some(V::Int(c)) => c,
                _ => 0,
            };
            sub(cx, p, &a(0, "repl").unwrap_or(V::None), &a(1, "string").unwrap_or(V::None), count, name == "subn").await
        }
        "split" => {
            let maxsplit = match a(1, "maxsplit") {
                Some(V::Int(c)) => c,
                _ => 0,
            };
            split(p, &a(0, "string").unwrap_or(V::None), maxsplit)
        }
        _ => Err(Exc::attr_error(format!("'re.Pattern' object has no attribute '{name}'"))),
    }
}

pub fn pattern_attr(p: &Pattern, name: &str) -> R {
    match name {
        "pattern" => Ok(V::str(&p.src)),
        "flags" => Ok(V::Int(p.flags | 32)),
        "groups" => Ok(V::Int(p.names.len() as i64 - 1)),
        _ => Err(Exc::attr_error(format!("'re.Pattern' object has no attribute '{name}'"))),
    }
}

/// module-level `re.search(pattern, string, flags=0)` and friends
pub async fn re_call(cx: &super::Cx, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let flags_pos = match name {
        "sub" | "subn" => 4,
        "split" => 3,
        _ => 2,
    };
    let flags = args.get(flags_pos).or_else(|| kw(&kwargs, "flags")).cloned();
    let pat = compile(args.first().or_else(|| kw(&kwargs, "pattern")).ok_or_else(|| Exc::type_error("missing 'pattern'"))?, flags.as_ref())?;
    let p = pattern_of(&pat)?;
    let rest: Vec<V> = args.iter().skip(1).take(flags_pos - 1).cloned().collect();
    let kw2: Vec<(String, V)> = kwargs.into_iter().filter(|(k, _)| k != "flags" && k != "pattern").collect();
    pattern_method(cx, &p, name, &rest, &kw2).await
}

/// `re.escape(s)`
pub fn escape(s: &V) -> R {
    let s = s_arg(Some(s), "pattern")?;
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' || !c.is_ascii() {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    Ok(V::str(out))
}

// ---------------------------------------------------------------- io.StringIO

pub struct StringIO {
    pub buf: Mutex<Vec<char>>,
    pub pos: Mutex<usize>,
}

pub fn stringio_new(init: Option<&V>) -> R {
    let init: Vec<char> = match init {
        None | Some(V::None) => vec![],
        Some(V::Str(s)) => s.chars().collect(),
        Some(o) => return Err(Exc::type_error(format!("initial_value must be str or None, not {}", o.type_name()))),
    };
    Ok(V::native(Native::StringIO(StringIO { buf: Mutex::new(init), pos: Mutex::new(0) })))
}

fn sio_write(s: &StringIO, text: &str) -> usize {
    let mut buf = s.buf.lock();
    let mut pos = s.pos.lock();
    let new: Vec<char> = text.chars().collect();
    if *pos > buf.len() {
        buf.resize(*pos, '\0');
    }
    let end = *pos + new.len();
    if end > buf.len() {
        buf.resize(end, '\0');
    }
    buf[*pos..end].copy_from_slice(&new);
    *pos = end;
    new.len()
}

/// lines from the current position (universal newlines are not translated: StringIO keeps \r\n)
fn sio_lines(s: &StringIO) -> Vec<String> {
    let buf = s.buf.lock();
    let mut pos = s.pos.lock();
    let rest: String = buf[(*pos).min(buf.len())..].iter().collect();
    *pos = buf.len();
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in rest.chars() {
        cur.push(c);
        if c == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn stringio_method(s: &StringIO, name: &str, args: &[V]) -> R {
    match name {
        "write" => match args.first() {
            Some(V::Str(t)) => Ok(V::Int(sio_write(s, t) as i64)),
            Some(o) => Err(Exc::type_error(format!("string argument expected, got '{}'", o.type_name()))),
            None => Err(Exc::type_error("write() takes exactly one argument (0 given)")),
        },
        "getvalue" => Ok(V::str(s.buf.lock().iter().collect::<String>())),
        "read" => {
            let buf = s.buf.lock();
            let mut pos = s.pos.lock();
            let start = (*pos).min(buf.len());
            let end = match args.first() {
                Some(V::Int(n)) if *n >= 0 => (start + *n as usize).min(buf.len()),
                _ => buf.len(),
            };
            *pos = end;
            Ok(V::str(buf[start..end].iter().collect::<String>()))
        }
        "readlines" => Ok(V::list(sio_lines(s).into_iter().map(V::str).collect())),
        "seek" => {
            let off = match args.first() {
                Some(V::Int(i)) if *i >= 0 => *i as usize,
                Some(V::Int(i)) => return Err(Exc::value_error(format!("Negative seek position {i}"))),
                _ => return Err(Exc::type_error("seek() needs an integer")),
            };
            *s.pos.lock() = off;
            Ok(V::Int(off as i64))
        }
        "tell" => Ok(V::Int(*s.pos.lock() as i64)),
        "truncate" => {
            let size = match args.first() {
                Some(V::Int(i)) => *i as usize,
                _ => *s.pos.lock(),
            };
            s.buf.lock().truncate(size);
            Ok(V::Int(size as i64))
        }
        "close" | "flush" => Ok(V::None),
        _ => Err(Exc::attr_error(format!("'_io.StringIO' object has no attribute '{name}'"))),
    }
}

/// `for line in f` over a StringIO / text file: its lines
pub fn iter_lines(v: &V) -> Option<Vec<V>> {
    if let V::Native(n) = v {
        if let Native::StringIO(s) = &**n {
            return Some(sio_lines(s).into_iter().map(V::str).collect());
        }
    }
    None
}

// ---------------------------------------------------------------- csv

pub struct Dialect {
    delimiter: char,
    quotechar: char,
    lineterminator: String,
    quoting: i64,
}

impl Default for Dialect {
    fn default() -> Self {
        Dialect { delimiter: ',', quotechar: '"', lineterminator: "\r\n".into(), quoting: 0 }
    }
}

fn dialect(kwargs: &[(String, V)]) -> R<Dialect> {
    let mut d = Dialect::default();
    for (k, v) in kwargs {
        match k.as_str() {
            "delimiter" => d.delimiter = ops::str_(v)?.chars().next().ok_or_else(|| Exc::type_error("\"delimiter\" must be a 1-character string"))?,
            "quotechar" => d.quotechar = ops::str_(v)?.chars().next().unwrap_or('"'),
            "lineterminator" => d.lineterminator = ops::str_(v)?,
            "quoting" => d.quoting = match v { V::Int(q) => *q, _ => 0 },
            "dialect" if matches!(v, V::Str(s) if &**s == "excel") || v.is_none() => {}
            "dialect" => {
                if let V::Native(n) = v {
                    if let Native::Sniffed(c) = &**n {
                        d.delimiter = *c;
                        continue;
                    }
                }
                return Err(Exc::type_error("py2axum: csv dialect= other than 'excel' or a sniffed one is not supported"));
            }
            "fieldnames" | "extrasaction" | "restval" | "restkey" => {}
            other => return Err(Exc::type_error(format!("py2axum: csv {other}= is not supported"))),
        }
    }
    Ok(d)
}

fn csv_field(d: &Dialect, v: &V) -> R<String> {
    let s = match v {
        V::None => return Ok(String::new()),
        V::Float(f) => ops::float_repr(*f),
        other => ops::str_(other)?,
    };
    let needs = d.quoting == 1
        || (d.quoting == 2 && !matches!(v, V::Int(_) | V::Float(_) | V::Bool(_)))
        || s.contains(d.delimiter)
        || s.contains(d.quotechar)
        || s.contains('\n')
        || s.contains('\r')
        || s.starts_with(' ') && false;
    Ok(if needs { format!("{q}{}{q}", s.replace(d.quotechar, &format!("{0}{0}", d.quotechar)), q = d.quotechar) } else { s })
}

pub struct CsvWriter {
    target: V,
    d: Dialect,
    /// DictWriter: (fieldnames, extrasaction == "raise", restval)
    dict: Option<(Vec<V>, bool, V)>,
}

pub fn writer_new(args: &[V], kwargs: &[(String, V)], dict: bool) -> R {
    // a positional dialect (writer) or restval/extrasaction (DictWriter): refused, never ignored
    if args.len() > if dict { 2 } else { 1 } {
        return Err(Exc::type_error(format!("py2axum: csv.{}() takes its options as keywords here", if dict { "DictWriter" } else { "writer" })));
    }
    if !dict && kwargs.iter().any(|(k, _)| matches!(k.as_str(), "fieldnames" | "extrasaction" | "restval" | "restkey")) {
        return Err(Exc::type_error("py2axum: csv.writer() takes no fieldnames/extrasaction/restval/restkey"));
    }
    if dict && kw(kwargs, "restkey").is_some() {
        return Err(Exc::type_error("DictWriter.__init__() got an unexpected keyword argument 'restkey'"));
    }
    let target = args.first().cloned().ok_or_else(|| Exc::type_error("writer() missing 'csvfile'"))?;
    let d = dialect(kwargs)?;
    let dict = if dict {
        let names = args.get(1).or_else(|| kw(kwargs, "fieldnames")).ok_or_else(|| Exc::type_error("DictWriter() missing 'fieldnames'"))?;
        let raise = !matches!(kw(kwargs, "extrasaction"), Some(V::Str(s)) if &**s == "ignore");
        Some((ops::iter(names)?, raise, kw(kwargs, "restval").cloned().unwrap_or(V::str(""))))
    } else {
        None
    };
    Ok(V::native(Native::CsvWriter(CsvWriter { target, d, dict })))
}

fn write_target(t: &V, text: &str) -> R {
    match t {
        V::Native(n) => match &**n {
            Native::StringIO(s) => stringio_method(s, "write", &[V::str(text)]),
            Native::File(f) => super::pathio::file_method(f, "write", &[V::str(text)]),
            _ => Err(Exc::type_error("py2axum: csv writes to a StringIO or a text file only")),
        },
        _ => Err(Exc::type_error("argument 1 must have a \"write\" method")),
    }
}

fn write_row(w: &CsvWriter, row: &V) -> R {
    let fields: Vec<V> = match (&w.dict, row) {
        (Some((names, raise, restval)), V::Dict(d)) => {
            let d = d.lock();
            if *raise {
                let wrong: Vec<String> = d.values().filter(|(k, _)| !names.iter().any(|n| ops::eq_bool(n, k))).map(|(k, _)| ops::repr(k).unwrap_or_default()).collect();
                if !wrong.is_empty() {
                    return Err(Exc::value_error(format!("dict contains fields not in fieldnames: {}", wrong.join(", "))));
                }
            }
            names.iter().map(|n| Key::of(n).ok().and_then(|k| d.get(&k).map(|(_, v)| v.clone())).unwrap_or_else(|| restval.clone())).collect()
        }
        (Some(_), o) => return Err(Exc::attr_error(format!("'{}' object has no attribute 'keys'", o.type_name()))),
        (None, r) => ops::iter(r)?,
    };
    let line = fields.iter().map(|f| csv_field(&w.d, f)).collect::<R<Vec<_>>>()?.join(&w.d.delimiter.to_string()) + &w.d.lineterminator;
    write_target(&w.target, &line)
}

pub fn writer_method(w: &CsvWriter, name: &str, args: &[V]) -> R {
    match name {
        "writerow" => write_row(w, args.first().ok_or_else(|| Exc::type_error("writerow() takes exactly one argument"))?),
        "writerows" => {
            for r in ops::iter(args.first().ok_or_else(|| Exc::type_error("writerows() takes exactly one argument"))?)? {
                write_row(w, &r)?;
            }
            Ok(V::None)
        }
        "writeheader" => {
            let (names, _, _) = w.dict.as_ref().ok_or_else(|| Exc::attr_error("'_csv.writer' object has no attribute 'writeheader'"))?;
            let line = names.iter().map(|f| csv_field(&w.d, f)).collect::<R<Vec<_>>>()?.join(&w.d.delimiter.to_string()) + &w.d.lineterminator;
            write_target(&w.target, &line)
        }
        _ => Err(Exc::attr_error(format!("'_csv.writer' object has no attribute '{name}'"))),
    }
}

/// csv parsing of an iterable of lines (a field may span lines inside quotes)
fn parse_rows(lines: Vec<String>, d: &Dialect) -> R<Vec<Vec<String>>> {
    let text: String = lines.concat();
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let (mut quoted, mut in_quotes, mut any) = (false, false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == d.quotechar {
                if chars.peek() == Some(&d.quotechar) {
                    field.push(c);
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            c if c == d.quotechar && field.is_empty() && !quoted => {
                in_quotes = true;
                quoted = true;
                any = true;
            }
            c if c == d.delimiter => {
                row.push(std::mem::take(&mut field));
                quoted = false;
                any = true;
            }
            '\r' => {}
            '\n' => {
                if any || !field.is_empty() || !row.is_empty() {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                } else {
                    rows.push(Vec::new());
                }
                quoted = false;
                any = false;
            }
            c => {
                field.push(c);
                any = true;
            }
        }
    }
    if in_quotes {
        return Err(Exc::msg(&CSV_ERROR, "unexpected end of data"));
    }
    if any || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

/// The rows of a csv source. An iterable of strings that is not a file (`text.splitlines()`): CPython ends
/// the record at the end of each item, newline or not, unless a quoted field is still open (the next item
/// then continues it, nothing inserted); an empty item is an empty row.
fn source_rows(src: &V, d: &Dialect) -> R<Vec<Vec<String>>> {
    // (reading a StringIO's lines consumes it: once)
    if let Some(l) = iter_lines(src) {
        return parse_rows(l.iter().map(ops::str_).collect::<R<_>>()?, d);
    }
    let mut rows = Vec::new();
    let mut buf = String::new();
    for item in ops::iter(src)? {
        buf.push_str(&ops::str_(&item)?);
        if buf.chars().filter(|c| *c == d.quotechar).count() % 2 == 1 {
            continue;
        }
        if buf.is_empty() {
            rows.push(Vec::new());
        } else {
            if !buf.ends_with('\n') && !buf.ends_with('\r') {
                buf.push('\n');
            }
            rows.extend(parse_rows(vec![std::mem::take(&mut buf)], d)?);
        }
    }
    if !buf.is_empty() {
        rows.extend(parse_rows(vec![buf], d)?);
    }
    Ok(rows)
}

/// `csv.reader(lines)`: the rows (lists of str)
pub fn reader(args: &[V], kwargs: &[(String, V)]) -> R {
    if args.len() > 1 || kwargs.iter().any(|(k, _)| matches!(k.as_str(), "fieldnames" | "extrasaction" | "restval" | "restkey")) {
        return Err(Exc::type_error("py2axum: csv.reader(f, **fmtparams) only (a positional dialect is not supported)"));
    }
    let d = dialect(kwargs)?;
    let rows = source_rows(args.first().ok_or_else(|| Exc::type_error("reader() missing 'csvfile'"))?, &d)?;
    Ok(V::native(Native::Iter(Mutex::new(rows.into_iter().map(|r| V::list(r.into_iter().map(V::str).collect())).collect()))))
}

/// `csv.DictReader(f, fieldnames=None, restkey=None, restval=None)`: the rows as dicts, blank rows skipped
pub fn dict_reader(args: &[V], kwargs: &[(String, V)]) -> R {
    if args.len() > 2 || kw(kwargs, "extrasaction").is_some() {
        return Err(Exc::type_error("py2axum: csv.DictReader(f, fieldnames, restkey=, restval=, **fmtparams) only"));
    }
    let d = dialect(kwargs)?;
    // short rows: the missing fields get restval; long rows: the extra values under restkey
    let restval = kw(kwargs, "restval").cloned().unwrap_or(V::None);
    let restkey = kw(kwargs, "restkey").cloned().unwrap_or(V::None);
    let mut rows = source_rows(args.first().ok_or_else(|| Exc::type_error("DictReader() missing 'f'"))?, &d)?.into_iter();
    let names: Vec<String> = match args.get(1).or_else(|| kw(kwargs, "fieldnames")).filter(|v| !v.is_none()) {
        Some(n) => ops::iter(n)?.iter().map(ops::str_).collect::<R<_>>()?,
        None => loop {
            match rows.next() {
                Some(r) if r.is_empty() => continue,
                Some(r) => break r,
                None => break Vec::new(),
            }
        },
    };
    let mut out = Vec::new();
    for r in rows {
        if r.is_empty() {
            continue;
        }
        let mut m: Vec<(V, V)> = names.iter().zip(r.iter().map(V::str).chain(std::iter::repeat(restval.clone()))).map(|(k, v)| (V::str(k), v)).collect();
        if r.len() > names.len() {
            m.push((restkey.clone(), V::list(r[names.len()..].iter().map(V::str).collect())));
        }
        out.push(V::dict_from(m)?);
    }
    let fieldnames = V::list(names.into_iter().map(V::str).collect());
    Ok(V::native(Native::CsvRows(fieldnames, V::list(out))))
}

/// `csv.Sniffer().sniff(sample, delimiters=None)`: the delimiter that splits the most lines consistently
pub fn sniff(args: &[V], kwargs: &[(String, V)]) -> R {
    let sample = ops::str_(args.first().ok_or_else(|| Exc::type_error("sniff() missing 'sample'"))?)?;
    let cands: Vec<char> = match args.get(1).or_else(|| kw(kwargs, "delimiters")) {
        Some(V::Str(s)) => s.chars().collect(),
        _ => vec![',', ';', '\t', '|', ':'],
    };
    let lines: Vec<&str> = sample.lines().filter(|l| !l.is_empty()).collect();
    let mut best: Option<(char, usize)> = None;
    for c in cands {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(c).count()).collect();
        let Some(&first) = counts.first() else { continue };
        if first == 0 {
            continue;
        }
        let consistent = counts.iter().filter(|n| **n == first).count();
        if best.map(|(_, b)| consistent > b).unwrap_or(true) {
            best = Some((c, consistent));
        }
    }
    match best {
        Some((c, _)) => Ok(V::native(Native::Sniffed(c))),
        None => Err(Exc::msg(&CSV_ERROR, "Could not determine delimiter")),
    }
}

// ---------------------------------------------------------------- math, time, os.path, hmac

fn num(v: &V) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Float(f) => Ok(*f),
        V::Bool(b) => Ok(*b as i64 as f64),
        o => Err(Exc::type_error(format!("must be real number, not {}", o.type_name()))),
    }
}

pub fn math(name: &str, args: &[V]) -> R {
    let a = |i: usize| args.get(i).ok_or_else(|| Exc::type_error(format!("math.{name}() missing argument"))).and_then(num);
    let domain = || Exc::value_error("math domain error");
    // Python 3.14 says which input was expected (pow and fmod keep "math domain error")
    let expected = |what: &str, x: Option<f64>| {
        if super::python() < (3, 14) {
            return domain();
        }
        let got = x.map(|x| format!(", got {}", super::ops::repr(&V::Float(x)).unwrap_or_default())).unwrap_or_default();
        Exc::value_error(format!("expected {what}{got}"))
    };
    Ok(match name {
        "ceil" | "floor" | "trunc" => {
            if let Some(V::Int(i)) = args.first() {
                return Ok(V::Int(*i));
            }
            let x = a(0)?;
            if !x.is_finite() {
                return Err(if x.is_nan() { Exc::value_error("cannot convert float NaN to integer") } else { Exc::msg(&OVERFLOW_ERROR, "cannot convert float infinity to integer") });
            }
            V::Int(match name { "ceil" => x.ceil(), "floor" => x.floor(), _ => x.trunc() } as i64)
        }
        "pow" => {
            let (x, y) = (a(0)?, a(1)?);
            let r = x.powf(y);
            // 0 ** negative: C sets EDOM (Rust returns inf)
            if (r.is_nan() && !x.is_nan() && !y.is_nan()) || (x == 0.0 && y < 0.0 && y.is_finite()) {
                return Err(domain());
            }
            V::Float(r)
        }
        "sqrt" => {
            let x = a(0)?;
            if x < 0.0 {
                return Err(expected("a nonnegative input", Some(x)));
            }
            V::Float(x.sqrt())
        }
        "log" => {
            let x = a(0)?;
            if x <= 0.0 {
                return Err(expected("a positive input", None));
            }
            match args.get(1) {
                Some(b) => V::Float(x.ln() / num(b)?.ln()),
                None => V::Float(x.ln()),
            }
        }
        "log10" => {
            let x = a(0)?;
            if x <= 0.0 {
                return Err(expected("a positive input", None));
            }
            V::Float(x.log10())
        }
        "exp" => V::Float(a(0)?.exp()),
        "fabs" => V::Float(a(0)?.abs()),
        "isclose" => {
            let (x, y) = (a(0)?, a(1)?);
            V::Bool(x == y || (x - y).abs() <= (1e-9 * x.abs().max(y.abs())).max(0.0))
        }
        "isnan" => V::Bool(a(0)?.is_nan()),
        "isinf" => V::Bool(a(0)?.is_infinite()),
        "isfinite" => V::Bool(a(0)?.is_finite()),
        "radians" => V::Float(a(0)? * (std::f64::consts::PI / 180.0)),
        "degrees" => V::Float(a(0)? * (180.0 / std::f64::consts::PI)),
        // libm, like CPython; ValueError where C sets EDOM (NaN out of a non-NaN input)
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "log2" => {
            let x = a(0)?;
            let r = match name {
                "sin" => x.sin(),
                "cos" => x.cos(),
                "tan" => x.tan(),
                "asin" => x.asin(),
                "acos" => x.acos(),
                "atan" => x.atan(),
                _ => {
                    if x <= 0.0 {
                        return Err(expected("a positive input", None));
                    }
                    x.log2()
                }
            };
            if r.is_nan() && !x.is_nan() {
                return Err(match name {
                    "asin" | "acos" => expected("a number in range from -1 up to 1", Some(x)),
                    _ => expected("a finite input", Some(x)),
                });
            }
            V::Float(r)
        }
        "atan2" => V::Float(a(0)?.atan2(a(1)?)),
        "hypot" => V::Float(args.iter().map(num).collect::<R<Vec<f64>>>()?.iter().fold(0.0f64, |h, x| h.hypot(*x))),
        "copysign" => V::Float(a(0)?.copysign(a(1)?)),
        "fmod" => {
            let (x, y) = (a(0)?, a(1)?);
            if y == 0.0 || x.is_infinite() {
                return Err(domain());
            }
            V::Float(x % y)
        }
        _ => return Err(Exc::attr_error(format!("module 'math' has no attribute '{name}'"))),
    })
}

pub fn time_now(name: &str) -> R {
    Ok(V::Float(match name {
        "time" => {
            let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            d.as_secs_f64()
        }
        _ => {
            static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
        }
    }))
}

pub fn os_path(name: &str, args: &[V]) -> R {
    let p = |i: usize| args.get(i).ok_or_else(|| Exc::type_error(format!("{name}() missing argument"))).and_then(super::pathio::fspath);
    Ok(match name {
        "exists" => V::Bool(std::fs::metadata(p(0)?).is_ok()),
        "isfile" => V::Bool(std::fs::metadata(p(0)?).map(|m| m.is_file()).unwrap_or(false)),
        "isdir" => V::Bool(std::fs::metadata(p(0)?).map(|m| m.is_dir()).unwrap_or(false)),
        "join" => {
            let mut out = p(0)?;
            for i in 1..args.len() {
                let b = p(i)?;
                if b.starts_with('/') {
                    out = b;
                } else if out.is_empty() || out.ends_with('/') {
                    out += &b;
                } else {
                    out = format!("{out}/{b}");
                }
            }
            V::str(out)
        }
        "basename" => V::str(p(0)?.rsplit('/').next().unwrap_or("")),
        "normpath" => V::str(normpath(&p(0)?)),
        "abspath" => V::str(abspath(&p(0)?)),
        "realpath" => V::str(realpath(&p(0)?)),
        "dirname" => {
            let s = p(0)?;
            V::str(match s.rfind('/') {
                Some(0) => "/".to_string(),
                Some(i) => s[..i].trim_end_matches('/').to_string().chars().collect::<String>(),
                None => String::new(),
            })
        }
        "splitext" => {
            let s = p(0)?;
            let base_start = s.rfind('/').map(|i| i + 1).unwrap_or(0);
            let base = &s[base_start..];
            let dot = base.trim_start_matches('.').rfind('.').map(|i| i + (base.len() - base.trim_start_matches('.').len()));
            match dot {
                Some(i) => V::tuple(vec![V::str(&s[..base_start + i]), V::str(&base[i..])]),
                None => V::tuple(vec![V::str(&s), V::str("")]),
            }
        }
        _ => return Err(Exc::attr_error(format!("module 'posixpath' has no attribute '{name}'"))),
    })
}

/// `hmac.new(key, msg=None, digestmod=...)`
pub fn hmac_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let bytes = |v: &V| -> R<Vec<u8>> {
        match v {
            V::Bytes(b) => Ok(b.to_vec()),
            o => Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
        }
    };
    let key = bytes(args.first().or_else(|| kw(kwargs, "key")).ok_or_else(|| Exc::type_error("missing 'key'"))?)?;
    let msg = args.get(1).or_else(|| kw(kwargs, "msg")).filter(|v| !v.is_none()).map(bytes).transpose()?.unwrap_or_default();
    let algo = match args.get(2).or_else(|| kw(kwargs, "digestmod")) {
        Some(V::Str(s)) => s.to_ascii_lowercase(),
        Some(V::Native(n)) => match &**n {
            Native::HashCtor(a) => a.to_string(),
            _ => return Err(Exc::type_error("py2axum: hmac digestmod must be a hashlib constructor or its name")),
        },
        _ => return Err(Exc::type_error("Missing required argument 'digestmod'.")),
    };
    Ok(V::native(Native::Hmac(Mutex::new(Hmac { algo, key, msg }))))
}

pub struct Hmac {
    algo: String,
    key: Vec<u8>,
    msg: Vec<u8>,
}

pub fn hmac_method(h: &Mutex<Hmac>, name: &str, args: &[V]) -> R {
    use hmac::Mac;
    match name {
        "update" => {
            match args.first() {
                Some(V::Bytes(b)) => h.lock().msg.extend_from_slice(b),
                _ => return Err(Exc::type_error("a bytes-like object is required")),
            }
            Ok(V::None)
        }
        "digest" | "hexdigest" => {
            let g = h.lock();
            macro_rules! mac {
                ($t:ty) => {{
                    let mut m = <hmac::Hmac<$t> as Mac>::new_from_slice(&g.key).unwrap();
                    m.update(&g.msg);
                    m.finalize().into_bytes().to_vec()
                }};
            }
            let out = match g.algo.as_str() {
                "sha256" => mac!(sha2::Sha256),
                "sha1" => mac!(sha1::Sha1),
                "sha512" => mac!(sha2::Sha512),
                "md5" => mac!(md5::Md5),
                a => return Err(Exc::value_error(format!("py2axum: hmac with {a} is not supported"))),
            };
            Ok(if name == "digest" { V::Bytes(Arc::from(out)) } else { V::str(hex::encode(out)) })
        }
        _ => Err(Exc::attr_error(format!("'HMAC' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- urllib.parse

fn up_arg<'a>(f: &str, args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str, names: &[&str]) -> R<Option<&'a V>> {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !names.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("{f}() got an unexpected keyword argument '{k}'")));
    }
    Ok(args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)))
}

fn quote_bytes(b: &[u8], safe: &str) -> String {
    let mut out = String::new();
    for &c in b {
        if c.is_ascii_alphanumeric() || b"_.-~".contains(&c) || (c < 128 && safe.contains(c as char)) {
            out.push(c as char);
        } else {
            out += &format!("%{c:02X}");
        }
    }
    out
}

fn quote_value(v: &V, safe: &str) -> R<String> {
    match v {
        V::Str(s) => Ok(quote_bytes(s.as_bytes(), safe)),
        V::Bytes(b) => Ok(quote_bytes(b, safe)),
        o => Err(Exc::type_error(format!("quote() doesn't support '{}' objects", o.type_name()))),
    }
}

fn plus(v: &V, safe: &str) -> R<String> {
    // quote_plus: spaces become '+'
    let s = quote_value(v, &format!("{safe} "))?;
    Ok(s.replace(' ', "+"))
}

fn unquote_str(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `urllib.parse.quote/quote_plus/unquote/unquote_plus/urlencode/urlparse/urlsplit`
pub fn urllib(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let safe_of = |v: Option<&V>, d: &str| -> R<String> { v.map(ops::str_).transpose().map(|s| s.unwrap_or_else(|| d.to_string())) };
    // encoding= other than utf-8 and errors= other than the default (strict to quote, replace to unquote): refused
    if matches!(name, "quote" | "quote_plus" | "unquote" | "unquote_plus") {
        let at = if name.starts_with('q') { 2 } else { 1 };
        let get = |i: usize, k: &str| args.get(i).or_else(|| kwargs.iter().find(|(x, _)| x == k).map(|(_, v)| v)).filter(|v| !v.is_none());
        if let Some(e) = get(at, "encoding") {
            if !matches!(ops::str_(e)?.to_ascii_lowercase().replace('_', "-").as_str(), "utf-8" | "utf8") {
                return Err(Exc::type_error(format!("py2axum: {name}(encoding=) other than utf-8 is not supported")));
            }
        }
        if let Some(e) = get(at + 1, "errors") {
            if ops::str_(e)? != if name.starts_with('q') { "strict" } else { "replace" } {
                return Err(Exc::type_error(format!("py2axum: {name}(errors=) other than the default is not supported")));
            }
        }
    }
    match name {
        "quote" => {
            let names = ["string", "safe", "encoding", "errors"];
            let s = up_arg(name, args, kwargs, 0, "string", &names)?.ok_or_else(|| Exc::type_error("quote() missing 1 required positional argument: 'string'"))?;
            Ok(V::str(quote_value(s, &safe_of(up_arg(name, args, kwargs, 1, "safe", &names)?, "/")?)?))
        }
        "quote_plus" => {
            let names = ["string", "safe", "encoding", "errors"];
            let s = up_arg(name, args, kwargs, 0, "string", &names)?.ok_or_else(|| Exc::type_error("quote_plus() missing 1 required positional argument: 'string'"))?;
            Ok(V::str(plus(s, &safe_of(up_arg(name, args, kwargs, 1, "safe", &names)?, "")?)?))
        }
        "unquote" | "unquote_plus" => {
            let s = ops::str_(up_arg(name, args, kwargs, 0, "string", &["string", "encoding", "errors"])?.ok_or_else(|| Exc::type_error("unquote() missing 'string'"))?)?;
            let s = if name == "unquote_plus" { s.replace('+', " ") } else { s };
            Ok(V::str(unquote_str(&s)))
        }
        "urlencode" => {
            let names = ["query", "doseq", "safe", "encoding", "errors", "quote_via"];
            let q = up_arg(name, args, kwargs, 0, "query", &names)?.ok_or_else(|| Exc::type_error("urlencode() missing 'query'"))?;
            let doseq = up_arg(name, args, kwargs, 1, "doseq", &names)?.map(ops::truthy).transpose()?.unwrap_or(false);
            let safe = safe_of(up_arg(name, args, kwargs, 2, "safe", &names)?, "")?;
            if up_arg(name, args, kwargs, 5, "quote_via", &names)?.is_some() {
                return Err(Exc::type_error("py2axum: urlencode(quote_via=) is not supported"));
            }
            let pairs: Vec<(V, V)> = match q {
                V::Dict(d) => d.lock().values().cloned().collect(),
                other => ops::iter(other)
                    .map_err(|_| Exc::type_error("not a valid non-string sequence or mapping object"))?
                    .into_iter()
                    .map(|p| match &p {
                        V::Tuple(t) if t.len() == 2 => Ok((t[0].clone(), t[1].clone())),
                        V::List(l) if l.lock().len() == 2 => {
                            let l = l.lock();
                            Ok((l[0].clone(), l[1].clone()))
                        }
                        _ => Err(Exc::type_error("not a valid non-string sequence or mapping object")),
                    })
                    .collect::<R<_>>()?,
            };
            let enc = |v: &V| -> R<String> {
                match v {
                    V::Str(_) | V::Bytes(_) => plus(v, &safe),
                    o => plus(&V::str(ops::str_(o)?), &safe),
                }
            };
            let mut out = Vec::new();
            for (k, v) in pairs {
                let k = enc(&k)?;
                if doseq && !matches!(v, V::Str(_) | V::Bytes(_)) {
                    match ops::iter(&v) {
                        Ok(items) => {
                            for x in items {
                                out.push(format!("{k}={}", enc(&x)?));
                            }
                        }
                        Err(_) => out.push(format!("{k}={}", enc(&v)?)),
                    }
                } else {
                    out.push(format!("{k}={}", enc(&v)?));
                }
            }
            Ok(V::str(out.join("&")))
        }
        "urlparse" | "urlsplit" => {
            let url = ops::str_(up_arg(name, args, kwargs, 0, "url", &["url", "scheme", "allow_fragments"])?.ok_or_else(|| Exc::type_error("missing 'url'"))?)?;
            Ok(V::native(Native::UrlParts(Arc::new(url_split(&url, name == "urlparse")))))
        }
        _ => Err(Exc::attr_error(format!("module 'urllib.parse' has no attribute '{name}'"))),
    }
}

/// `urlsplit` (+ `params` for `urlparse`), as CPython splits
pub struct UrlParts {
    pub parse: bool,
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub params: String,
    pub query: String,
    pub fragment: String,
}

fn url_split(url: &str, parse: bool) -> UrlParts {
    let url = url.trim_start_matches(|c: char| c <= ' ').trim_end_matches(|c: char| c <= ' ');
    let url: String = url.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut rest = url.as_str();
    let mut scheme = String::new();
    if let Some(i) = rest.find(':') {
        let cand = &rest[..i];
        if !cand.is_empty() && cand.chars().next().unwrap().is_ascii_alphabetic() && cand.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
            scheme = cand.to_ascii_lowercase();
            rest = &rest[i + 1..];
        }
    }
    let mut netloc = String::new();
    if let Some(r) = rest.strip_prefix("//") {
        let end = r.find(['/', '?', '#']).unwrap_or(r.len());
        netloc = r[..end].to_string();
        rest = &r[end..];
    }
    let (rest, fragment) = match rest.split_once('#') {
        Some((a, b)) => (a, b.to_string()),
        None => (rest, String::new()),
    };
    let (path, query) = match rest.split_once('?') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (rest.to_string(), String::new()),
    };
    let (path, params) = if parse && path.contains(';') {
        // params of the last segment only
        let seg = path.rfind('/').map(|i| i + 1).unwrap_or(0);
        match path[seg..].find(';') {
            Some(j) => (path[..seg + j].to_string(), path[seg + j + 1..].to_string()),
            None => (path.clone(), String::new()),
        }
    } else {
        (path, String::new())
    };
    UrlParts { parse, scheme, netloc, path, params, query, fragment }
}

pub fn url_parts_attr(u: &UrlParts, name: &str) -> R {
    let userinfo = u.netloc.rsplit_once('@').map(|(a, _)| a);
    let hostport = u.netloc.rsplit_once('@').map(|(_, b)| b).unwrap_or(&u.netloc);
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, p)) => (h.to_string(), p.strip_prefix(':').map(|s| s.to_string())),
            None => (rest.to_string(), None),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.to_string())),
            None => (hostport.to_string(), None),
        }
    };
    let opt = |s: Option<String>| s.map(V::str).unwrap_or(V::None);
    Ok(match name {
        "scheme" => V::str(&u.scheme),
        "netloc" => V::str(&u.netloc),
        "path" => V::str(&u.path),
        "params" if u.parse => V::str(&u.params),
        "query" => V::str(&u.query),
        "fragment" => V::str(&u.fragment),
        "hostname" => {
            if host.is_empty() {
                V::None
            } else {
                V::str(host.to_lowercase())
            }
        }
        "port" => match port.filter(|p| !p.is_empty()) {
            None => V::None,
            Some(p) => match p.parse::<u32>() {
                Ok(n) if n <= 65535 && p.chars().all(|c| c.is_ascii_digit()) => V::Int(n as i64),
                Ok(_) => return Err(Exc::value_error("Port out of range 0-65535")),
                Err(_) => return Err(Exc::value_error(format!("Port could not be cast to integer value as {}", ops::repr(&V::str(&p)).unwrap_or_default()))),
            },
        },
        "username" => opt(userinfo.map(|u| u.split_once(':').map(|(a, _)| a).unwrap_or(u).to_string())),
        "password" => opt(userinfo.and_then(|u| u.split_once(':').map(|(_, b)| b.to_string()))),
        _ => return Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", if u.parse { "ParseResult" } else { "SplitResult" }))),
    })
}

pub fn url_parts_tuple(u: &UrlParts) -> Vec<V> {
    let mut v = vec![V::str(&u.scheme), V::str(&u.netloc), V::str(&u.path)];
    if u.parse {
        v.push(V::str(&u.params));
    }
    v.push(V::str(&u.query));
    v.push(V::str(&u.fragment));
    v
}

pub fn url_parts_geturl(u: &UrlParts) -> String {
    let mut s = String::new();
    if !u.scheme.is_empty() {
        s += &u.scheme;
        s.push(':');
    }
    if !u.netloc.is_empty() || u.scheme == "http" || u.scheme == "https" || u.path.starts_with("//") {
        if !u.netloc.is_empty() || !u.path.is_empty() && u.path.starts_with("//") {
            s += "//";
            s += &u.netloc;
        }
    }
    s += &u.path;
    if u.parse && !u.params.is_empty() {
        s.push(';');
        s += &u.params;
    }
    if !u.query.is_empty() {
        s.push('?');
        s += &u.query;
    }
    if !u.fragment.is_empty() {
        s.push('#');
        s += &u.fragment;
    }
    s
}

// ---------------------------------------------------------------- base64

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `_bytes_from_decode_data`: bytes, or an ASCII-only str
fn decode_data(v: &V) -> R<Vec<u8>> {
    match v {
        V::Bytes(b) => Ok(b.to_vec()),
        V::Str(s) if s.is_ascii() => Ok(s.as_bytes().to_vec()),
        V::Str(_) => Err(Exc::value_error("string argument should contain only ASCII characters")),
        o => Err(Exc::type_error(format!("argument should be a bytes-like object or ASCII string, not '{}'", o.type_name()))),
    }
}

fn encode_data(v: &V) -> R<Vec<u8>> {
    match v {
        V::Bytes(b) => Ok(b.to_vec()),
        o => Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
    }
}

fn b64enc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63]);
        out.push(B64[(n >> 12) as usize & 63]);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] } else { b'=' });
    }
    out
}

/// `binascii.a2b_base64(s, strict_mode=validate)` as CPython 3.13 decodes
pub fn b64dec(s: &[u8], strict: bool) -> R<Vec<u8>> {
    let err = |m: String| Exc::msg(&BINASCII_ERROR, m);
    let val = |c: u8| B64.iter().position(|&x| x == c);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut quad, mut left, mut pads, mut count) = (0usize, 0u32, 0usize, 0usize);
    let mut padding_started = false;
    for (i, &c) in s.iter().enumerate() {
        if c == b'=' {
            padding_started = true;
            pads += 1;
            if quad >= 2 && quad + pads >= 4 {
                if strict && i + 1 < s.len() {
                    return Err(err("Excess data after padding".into()));
                }
                return Ok(out);
            }
            continue;
        }
        let Some(v) = val(c) else {
            if strict {
                return Err(err("Only base64 data is allowed".into()));
            }
            continue;
        };
        if strict && padding_started {
            return Err(err("Discontinuous padding not allowed".into()));
        }
        pads = 0;
        count += 1;
        let v = v as u32;
        match quad {
            0 => {
                quad = 1;
                left = v;
            }
            1 => {
                quad = 2;
                out.push(((left << 2) | (v >> 4)) as u8);
                left = v & 0xf;
            }
            2 => {
                quad = 3;
                out.push(((left << 4) | (v >> 2)) as u8);
                left = v & 0x3;
            }
            _ => {
                quad = 0;
                out.push(((left << 6) | v) as u8);
                left = 0;
            }
        }
    }
    if quad != 0 {
        if quad == 1 {
            return Err(err(format!("Invalid base64-encoded string: number of data characters ({count}) cannot be 1 more than a multiple of 4")));
        }
        return Err(err("Incorrect padding".into()));
    }
    Ok(out)
}

fn swap_chars(mut b: Vec<u8>, from: &[u8], to: &[u8]) -> Vec<u8> {
    for c in b.iter_mut() {
        if let Some(i) = from.iter().position(|x| x == c) {
            *c = to[i];
        }
    }
    b
}

/// `base64.b64encode/b64decode/urlsafe_*/standard_*/b16*/b32*`
pub fn base64(name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let arg = |i: usize, n: &str| args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == n).map(|(_, v)| v));
    let s = arg(0, "s").ok_or_else(|| Exc::type_error(format!("{name}() missing 1 required positional argument: 's'")))?;
    let bytes = |b: Vec<u8>| Ok(V::Bytes(Arc::from(b)));
    let alt = |n: usize| -> R<Option<Vec<u8>>> {
        match arg(n, "altchars") {
            None | Some(V::None) => Ok(None),
            Some(v) => {
                let a = decode_data(v)?;
                if a.len() != 2 {
                    return Err(Exc::msg(&ASSERTION_ERROR, format!("{} must be a bytes-like object of length 2", ops::repr(v).unwrap_or_default())));
                }
                Ok(Some(a))
            }
        }
    };
    match name {
        "b64encode" | "standard_b64encode" | "urlsafe_b64encode" => {
            let mut out = b64enc(&encode_data(s)?);
            if name == "urlsafe_b64encode" {
                out = swap_chars(out, b"+/", b"-_");
            } else if let Some(a) = alt(1)? {
                out = swap_chars(out, b"+/", &a);
            }
            bytes(out)
        }
        "b64decode" | "standard_b64decode" | "urlsafe_b64decode" => {
            let mut data = decode_data(s)?;
            let validate = if name == "b64decode" { arg(2, "validate").map(ops::truthy).transpose()?.unwrap_or(false) } else { false };
            if name == "urlsafe_b64decode" {
                data = swap_chars(data, b"-_", b"+/");
            } else if let Some(a) = alt(1)? {
                data = swap_chars(data, &a, b"+/");
            }
            bytes(b64dec(&data, validate)?)
        }
        "b16encode" => bytes(hex::encode_upper(encode_data(s)?).into_bytes()),
        "b16decode" => {
            let mut data = decode_data(s)?;
            if arg(1, "casefold").map(ops::truthy).transpose()?.unwrap_or(false) {
                data.make_ascii_uppercase();
            }
            if data.iter().any(|c| !(c.is_ascii_digit() || (b'A'..=b'F').contains(c))) {
                return Err(Exc::msg(&BINASCII_ERROR, "Non-base16 digit found"));
            }
            hex::decode(&data).map_err(|_| Exc::msg(&BINASCII_ERROR, "Odd-length string")).and_then(bytes)
        }
        "b32decode" => {
            let data = decode_data(s)?;
            let casefold = arg(1, "casefold").map(ops::truthy).transpose()?.unwrap_or(false);
            bytes(super::auth::b32decode(&String::from_utf8_lossy(&data), casefold)?)
        }
        "b32encode" => {
            let data = encode_data(s)?;
            const A: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
            let mut out = Vec::new();
            for c in data.chunks(5) {
                let mut buf = [0u8; 5];
                buf[..c.len()].copy_from_slice(c);
                let n = buf.iter().fold(0u64, |a, &b| a << 8 | b as u64);
                let chars = [0, 2, 4, 5, 7, 8][c.len()];
                for k in 0..8 {
                    out.push(if k < chars { A[((n >> (35 - 5 * k)) & 31) as usize] } else { b'=' });
                }
            }
            bytes(out)
        }
        _ => Err(Exc::attr_error(format!("module 'base64' has no attribute '{name}'"))),
    }
}

/// `posixpath.normpath`
pub fn normpath(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let initial = if p.starts_with("//") && !p.starts_with("///") { 2 } else if p.starts_with('/') { 1 } else { 0 };
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(c);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let s = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if s.is_empty() { ".".into() } else { s }
}

/// `posixpath.abspath`
pub fn abspath(p: &str) -> String {
    if p.starts_with('/') {
        normpath(p)
    } else {
        let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|_| ".".into());
        normpath(&format!("{cwd}/{p}"))
    }
}

/// `os.path.realpath(p)` (strict=False), port of posixpath's `_joinrealpath`: symlinks resolved one
/// component at a time, `..` applied to the resolved path, missing components kept as written, a symlink
/// loop left unresolved. `Path.resolve()` shares it: an upload guard
/// `(BASE / name).resolve().relative_to(BASE)` must see where a symlink inside BASE really leads.
pub fn realpath(p: &str) -> String {
    fn join(a: &str, b: &str) -> String {
        if b.starts_with('/') || a.is_empty() {
            b.to_string()
        } else if a.ends_with('/') {
            format!("{a}{b}")
        } else {
            format!("{a}/{b}")
        }
    }
    fn split(p: &str) -> (String, String) {
        let i = p.rfind('/').map(|i| i + 1).unwrap_or(0);
        let (head, tail) = (&p[..i], &p[i..]);
        let trimmed = head.trim_end_matches('/');
        (if trimmed.is_empty() { head.to_string() } else { trimmed.to_string() }, tail.to_string())
    }
    fn walk(mut path: String, rest: &str, seen: &mut std::collections::HashMap<String, Option<String>>) -> (String, bool) {
        let mut rest = rest.to_string();
        if rest.starts_with('/') {
            rest.remove(0);
            path = "/".into();
        }
        while !rest.is_empty() {
            let (name, tail) = match rest.find('/') {
                Some(i) => (rest[..i].to_string(), rest[i + 1..].to_string()),
                None => (rest.clone(), String::new()),
            };
            rest = tail;
            if name.is_empty() || name == "." {
                continue;
            }
            if name == ".." {
                if path.is_empty() {
                    path = "..".into();
                } else {
                    let (head, n) = split(&path);
                    path = if n == ".." { join(&join(&head, ".."), "..") } else { head };
                }
                continue;
            }
            let newpath = join(&path, &name);
            let is_link = std::fs::symlink_metadata(&newpath).map(|m| m.file_type().is_symlink()).unwrap_or(false);
            if !is_link {
                path = newpath;
                continue;
            }
            match seen.get(&newpath) {
                Some(Some(resolved)) => {
                    path = resolved.clone();
                    continue;
                }
                Some(None) => return (join(&newpath, &rest), false),
                None => {}
            }
            seen.insert(newpath.clone(), None);
            let target = std::fs::read_link(&newpath).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
            let (p2, ok) = walk(path, &target, seen);
            if !ok {
                return (join(&p2, &rest), false);
            }
            seen.insert(newpath, Some(p2.clone()));
            path = p2;
        }
        (path, true)
    }
    abspath(&walk(String::new(), p, &mut std::collections::HashMap::new()).0)
}

/// `html.escape(s, quote=True)`
pub fn html_escape(args: &[V], kwargs: &[(String, V)]) -> R {
    let s = args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "s").map(|(_, v)| v)).ok_or_else(|| Exc::type_error("escape() missing 1 required positional argument: 's'"))?;
    let quote = match args.get(1).or_else(|| kwargs.iter().find(|(k, _)| k == "quote").map(|(_, v)| v)) {
        Some(q) => super::ops::truthy(q)?,
        None => true,
    };
    let V::Str(s) = s else {
        return Err(Exc::attr_error(format!("'{}' object has no attribute 'replace'", s.type_name())));
    };
    let mut out = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    if quote {
        out = out.replace('"', "&quot;").replace('\'', "&#x27;");
    }
    Ok(V::str(out))
}

/// A code point the translating Python's `unicodedata` does not know (`UCD_UNASSIGNED` of the generated
/// crate, sorted ranges): CPython leaves it alone (no decomposition, class 0, composes with nothing), whatever
/// the newer tables of unicode-normalization say
fn ucd_unassigned(table: &[(u32, u32)], c: char) -> bool {
    let c = c as u32;
    table.binary_search_by(|&(lo, hi)| if hi < c { std::cmp::Ordering::Less } else if lo > c { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }).is_ok()
}

/// The type in CPython's "argument must be X, not Y" messages (`_PyArg_BadArgument`: None, not NoneType)
fn arg_type(v: &V) -> &'static str {
    if v.is_none() { "None" } else { v.type_name() }
}

/// `unicodedata.normalize(form, unistr)`; an unassigned code point (`ucd_unassigned`) is a stable starter that
/// composes with nothing: the text around it normalizes independently
pub fn unicodedata_normalize(table: &[(u32, u32)], form: &V, s: &V) -> R {
    use unicode_normalization::UnicodeNormalization;
    let V::Str(form) = form else {
        return Err(Exc::type_error(format!("normalize() argument 1 must be str, not {}", arg_type(form))));
    };
    let V::Str(s) = s else {
        return Err(Exc::type_error(format!("normalize() argument 2 must be str, not {}", arg_type(s))));
    };
    if s.is_empty() {
        return Ok(V::Str(s.clone())); // CPython returns an empty input before looking at the form
    }
    let f: fn(&str) -> String = match form.as_ref() {
        "NFC" => |t| t.nfc().collect(),
        "NFD" => |t| t.nfd().collect(),
        "NFKC" => |t| t.nfkc().collect(),
        "NFKD" => |t| t.nfkd().collect(),
        _ => return Err(Exc::value_error("invalid normalization form")),
    };
    let mut out = String::with_capacity(s.len());
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if ucd_unassigned(table, c) {
            out.push_str(&f(&s[start..i]));
            out.push(c);
            start = i + c.len_utf8();
        }
    }
    out.push_str(&f(&s[start..]));
    Ok(V::str(out))
}

/// `unicodedata.combining(chr)`
pub fn unicodedata_combining(table: &[(u32, u32)], c: &V) -> R {
    let V::Str(s) = c else {
        return Err(Exc::type_error(format!("combining() argument must be a unicode character, not {}", arg_type(c))));
    };
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(ch), None) => Ok(V::Int(if ucd_unassigned(table, ch) {
            0
        } else {
            unicode_normalization::char::canonical_combining_class(ch) as i64
        })),
        // reworded in CPython 3.14
        _ if super::python() < (3, 14) => Err(Exc::type_error("combining() argument must be a unicode character, not str")),
        _ => Err(Exc::type_error(format!(
            "combining(): argument must be a unicode character, not a string of length {}",
            s.chars().count()
        ))),
    }
}

// ---------------------------------------------------------------- string.Template

/// `string.Template(template)`
pub fn template_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let t = args.first().or_else(|| kwargs.iter().find(|(k, _)| k == "template").map(|(_, v)| v));
    match t {
        Some(V::Str(s)) => Ok(V::native(Native::Template(s.to_string()))),
        Some(o) => Err(Exc::type_error(format!("py2axum: string.Template of a {}", o.type_name()))),
        None => Err(Exc::type_error("Template.__init__() missing 1 required positional argument: 'template'")),
    }
}

fn ident_len(s: &[u8]) -> usize {
    // idpattern (?a:[_a-z][_a-z0-9]*) with re.IGNORECASE
    match s.first() {
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => 1 + s[1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count(),
        _ => 0,
    }
}

/// `Template.substitute(mapping={}, /, **kws)` and `safe_substitute`
pub fn template_method(t: &str, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let safe = match name {
        "substitute" => false,
        "safe_substitute" => true,
        "template" => return Ok(V::str(t)),
        _ => return Err(Exc::attr_error(format!("'Template' object has no attribute '{name}'"))),
    };
    if args.len() > 1 {
        return Err(Exc::type_error("Too many positional arguments"));
    }
    let lookup = |k: &str| -> R<Option<V>> {
        if let Some((_, v)) = kwargs.iter().find(|(n, _)| n == k) {
            return Ok(Some(v.clone()));
        }
        match args.first() {
            Some(m) => match super::ops::getitem(m, &V::str(k)) {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.isinstance(&KEY_ERROR) => Ok(None),
                Err(e) => Err(e),
            },
            None => Ok(None),
        }
    };
    let b = t.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        out += &t[last..i];
        let rest = &b[i + 1..];
        let (name, len) = if rest.first() == Some(&b'$') {
            out.push('$');
            i += 2;
            last = i;
            continue;
        } else if rest.first() == Some(&b'{') && ident_len(&rest[1..]) > 0 && rest.get(1 + ident_len(&rest[1..])) == Some(&b'}') {
            let n = ident_len(&rest[1..]);
            (&t[i + 2..i + 2 + n], n + 3)
        } else if ident_len(rest) > 0 {
            let n = ident_len(rest);
            (&t[i + 1..i + 1 + n], n + 1)
        } else {
            // invalid placeholder
            if safe {
                out.push('$');
                i += 1;
                last = i;
                continue;
            }
            // string.Template._invalid: i = start of the (empty) `invalid` group, just after the `$`;
            // lines = text[:i].splitlines(keepends=True), counted in characters
            let before = &t[..i + 1];
            let (line, col) = if before.is_empty() {
                (1, 1)
            } else {
                let lines: Vec<&str> = before.split_inclusive('\n').collect();
                let prior: usize = lines[..lines.len() - 1].iter().map(|l| l.chars().count()).sum();
                (lines.len(), before.chars().count() - prior)
            };
            return Err(Exc::value_error(format!("Invalid placeholder in string: line {line}, col {col}")));
        };
        match lookup(name)? {
            Some(v) => out += &super::ops::str_(&v)?,
            None if safe => out += &t[i..i + len],
            None => return Err(Exc::new(&KEY_ERROR, vec![V::str(name)])),
        }
        i += len;
        last = i;
    }
    out += &t[last..];
    Ok(V::str(out))
}

/// `gzip.decompress(data)` (CPython 3.12+): every member, its header read like `_read_gzip_header`, the
/// deflate stream inflated raw, then CRC and length checked; NUL padding between members skipped.
pub fn gzip_decompress(args: &[V], kwargs: &[(String, V)]) -> R {
    let data = match (args, kwargs) {
        ([V::Bytes(b)], []) => b.clone(),
        ([], [(k, V::Bytes(b))]) if k == "data" => b.clone(),
        ([o], []) => return Err(Exc::type_error(format!("a bytes-like object is required, not '{}'", o.type_name()))),
        _ => return Err(Exc::type_error("py2axum: gzip.decompress(data) takes the data only")),
    };
    let eof = || Exc::new(&EOF_ERROR, vec![V::str("Compressed file ended before the end-of-stream marker was reached")]);
    let bad = |m: String| Exc::new(&BAD_GZIP_FILE, vec![V::str(&m)]);
    let mut out: Vec<u8> = Vec::new();
    let mut d: &[u8] = &data;
    loop {
        // header
        if d.is_empty() {
            return Ok(V::Bytes(Arc::from(out)));
        }
        let magic = &d[..d.len().min(2)];
        if magic != b"\x1f\x8b" {
            return Err(bad(format!("Not a gzipped file ({})", super::ops::repr(&V::Bytes(Arc::from(magic)))?)));
        }
        if d.len() < 10 {
            return Err(eof());
        }
        let (method, flag) = (d[2], d[3]);
        if method != 8 {
            return Err(bad("Unknown compression method".into()));
        }
        let mut p = 10;
        if flag & 4 != 0 {
            if d.len() < p + 2 {
                return Err(eof());
            }
            let n = u16::from_le_bytes([d[p], d[p + 1]]) as usize;
            p += 2;
            if d.len() < p + n {
                return Err(eof());
            }
            p += n;
        }
        for bit in [8u8, 16] {
            if flag & bit != 0 {
                while p < d.len() {
                    p += 1;
                    if d[p - 1] == 0 {
                        break;
                    }
                }
            }
        }
        if flag & 2 != 0 {
            if d.len() < p + 2 {
                return Err(eof());
            }
            p += 2;
        }
        // raw deflate
        let input = &d[p..];
        let mut z = flate2::Decompress::new(false);
        let mut member: Vec<u8> = Vec::with_capacity(input.len() * 3);
        let mut done = false;
        loop {
            if member.capacity() - member.len() < 32 * 1024 {
                member.reserve(64 * 1024);
            }
            let before = (z.total_in(), z.total_out());
            let st = z
                .decompress_vec(&input[z.total_in() as usize..], &mut member, flate2::FlushDecompress::None)
                .map_err(|e| Exc::new(&ZLIB_ERROR, vec![V::str(&format!("Error -3 while decompressing data: {}", e.message().unwrap_or("invalid data")))]))?;
            if st == flate2::Status::StreamEnd {
                done = true;
                break;
            }
            if (z.total_in(), z.total_out()) == before {
                break; // input exhausted before the end of the stream
            }
        }
        let unused = &input[z.total_in() as usize..];
        if !done || unused.len() < 8 {
            return Err(eof());
        }
        let crc = u32::from_le_bytes([unused[0], unused[1], unused[2], unused[3]]);
        let len = u32::from_le_bytes([unused[4], unused[5], unused[6], unused[7]]);
        let mut c = flate2::Crc::new();
        c.update(&member);
        if crc != c.sum() {
            return Err(bad("CRC check failed".into()));
        }
        if len != member.len() as u32 {
            return Err(bad("Incorrect length of data produced".into()));
        }
        out.extend_from_slice(&member);
        let rest = &unused[8..];
        d = &rest[rest.iter().position(|b| *b != 0).unwrap_or(rest.len())..];
    }
}
