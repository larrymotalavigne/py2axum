//! PyYAML 6.0's dumping side (`yaml.safe_dump`, `yaml.dump`): a port of representer.py (SafeRepresenter, and
//! Representer for the builtin types it adds: tuple, complex), serializer.py and emitter.py, writing the same
//! text byte for byte. The loader is in `pyyaml`.
//!
//! Not followed: the identity of dates (CPython writes an anchor for the same `date` object met twice: a value
//! here, never aliased), and `yaml.dump` of an object Representer writes as `!!python/object...` (a py2axum
//! TypeError). Both are documented in supported.md.
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use indexmap::IndexMap;

use super::ops;
use super::pyyaml::{implicit_tag, TAG_STR};
use super::v::*;

const BREAKS: &str = "\n\u{85}\u{2028}\u{2029}";
const SPACE_BREAKS: &str = "\0 \t\r\n\u{85}\u{2028}\u{2029}";

fn has(c: char, set: &str) -> bool {
    set.contains(c)
}

fn emitter_err(msg: impl AsRef<str>) -> Exc {
    Exc::msg(&YAML_EMITTER_ERROR, msg)
}

// ---------------------------------------------------------------- options

struct Opts {
    default_style: Option<String>,
    default_flow_style: Option<bool>,
    canonical: bool,
    indent: Option<i64>,
    width: Option<f64>,
    allow_unicode: bool,
    line_break: Option<String>,
    encoding: Option<String>,
    explicit_start: bool,
    explicit_end: bool,
    version: Option<(i64, i64)>,
    tags: Option<Vec<(String, String)>>,
    sort_keys: bool,
}

fn opts(kw: &[(String, V)]) -> R<Opts> {
    let mut o = Opts {
        default_style: None,
        default_flow_style: Some(false),
        canonical: false,
        indent: None,
        width: None,
        allow_unicode: false,
        line_break: None,
        encoding: None,
        explicit_start: false,
        explicit_end: false,
        version: None,
        tags: None,
        sort_keys: true,
    };
    let unsupported = |k: &str, v: &V| Exc::type_error(format!("py2axum: yaml dump {k}={} is not supported", ops::repr(v).unwrap_or_default()));
    for (k, v) in kw {
        match k.as_str() {
            "default_style" => {
                o.default_style = match v {
                    V::None => None,
                    V::Str(s) => Some(s.to_string()),
                    _ => return Err(unsupported(k, v)),
                }
            }
            "default_flow_style" => o.default_flow_style = if v.is_none() { None } else { Some(ops::truthy(v)?) },
            "canonical" => o.canonical = ops::truthy(v)?,
            "indent" => {
                o.indent = match v {
                    V::None => None,
                    V::Int(i) => Some(*i),
                    V::Bool(b) => Some(*b as i64),
                    _ => return Err(unsupported(k, v)),
                }
            }
            "width" => {
                o.width = match v {
                    V::None => None,
                    V::Int(i) => Some(*i as f64),
                    V::Bool(b) => Some(*b as i64 as f64),
                    V::Float(f) => Some(*f),
                    _ => return Err(unsupported(k, v)),
                }
            }
            "allow_unicode" => o.allow_unicode = ops::truthy(v)?,
            "line_break" => {
                o.line_break = match v {
                    V::Str(s) => Some(s.to_string()),
                    _ => None,
                }
            }
            "encoding" => {
                o.encoding = match v {
                    V::None => None,
                    V::Str(s) => Some(s.to_string()),
                    _ => return Err(unsupported(k, v)),
                }
            }
            "explicit_start" => o.explicit_start = ops::truthy(v)?,
            "explicit_end" => o.explicit_end = ops::truthy(v)?,
            "version" => {
                o.version = match v {
                    V::None => None,
                    V::Tuple(_) | V::List(_) => {
                        let items = ops::iter(v)?;
                        if items.is_empty() {
                            None
                        } else {
                            let int = |x: &V| match x {
                                V::Int(i) => Some(*i),
                                V::Bool(b) => Some(*b as i64),
                                _ => None,
                            };
                            match (&items[..], items.iter().all(|x| int(x).is_some())) {
                                ([a, b], true) => Some((int(a).unwrap(), int(b).unwrap())),
                                _ => return Err(unsupported(k, v)),
                            }
                        }
                    }
                    _ => return Err(unsupported(k, v)),
                }
            }
            "tags" => {
                o.tags = match v {
                    V::None => None,
                    V::Dict(d) => {
                        let mut out = vec![];
                        for (k2, v2) in d.lock().values() {
                            match (k2, v2) {
                                (V::Str(a), V::Str(b)) => out.push((a.to_string(), b.to_string())),
                                _ => return Err(unsupported(k, v)),
                            }
                        }
                        if out.is_empty() { None } else { Some(out) }
                    }
                    _ => return Err(unsupported(k, v)),
                }
            }
            "sort_keys" => o.sort_keys = ops::truthy(v)?,
            _ => return Err(Exc::type_error(format!("py2axum: yaml dump {k}= is not supported"))),
        }
    }
    Ok(o)
}

// ---------------------------------------------------------------- representer

enum DK {
    Scalar(String, Option<String>),
    Seq(Vec<usize>, Option<bool>),
    Map(Vec<(usize, usize)>, Option<bool>),
}

struct DNode {
    tag: String,
    kind: DK,
}

struct Repr {
    nodes: Vec<DNode>,
    represented: HashMap<usize, usize>,
    keep: Vec<V>,
    default_style: Option<String>,
    default_flow_style: Option<bool>,
    sort_keys: bool,
    /// Representer (yaml.dump) rather than SafeRepresenter
    full: bool,
}

/// Nesting depth past which representing raises RecursionError (CPython's depends on its stack: measured
/// from an async endpoint under uvicorn, a list nested 316 deep dumps and one 317 deep does not)
pub const MAX_DEPTH: usize = 316;

fn recursion() -> Exc {
    Exc::msg(&RECURSION_ERROR, "maximum recursion depth exceeded")
}

/// `id(data)` of the values SafeRepresenter does not ignore (None: no identity here)
fn identity(v: &V) -> Option<usize> {
    match v {
        V::List(l) => Some(Arc::as_ptr(l) as *const () as usize),
        V::Dict(d) => Some(Arc::as_ptr(d) as *const () as usize),
        V::Set(s) => Some(Arc::as_ptr(s) as *const () as usize),
        V::Tuple(t) if !t.is_empty() => Some(Arc::as_ptr(t) as *const () as usize),
        V::Native(n) => Some(Arc::as_ptr(n) as *const () as usize),
        _ => None,
    }
}

/// `sorted(list(mapping.items()))`: None when a comparison raises TypeError (the items stay in order)
fn sorted_items(items: &[(V, V)]) -> Option<Vec<(V, V)>> {
    let lt = |a: &(V, V), b: &(V, V)| -> R<bool> {
        let o = if ops::eq_bool(&a.0, &b.0) { ops::cmp(&a.1, &b.1)? } else { ops::cmp(&a.0, &b.0)? };
        Ok(o == std::cmp::Ordering::Less)
    };
    // a stable merge sort that only asks `<` (a comparator that is not a total order cannot make it panic)
    fn merge_sort(v: Vec<(V, V)>, lt: &dyn Fn(&(V, V), &(V, V)) -> R<bool>) -> R<Vec<(V, V)>> {
        if v.len() <= 1 {
            return Ok(v);
        }
        let mut v = v;
        let right = v.split_off(v.len() / 2);
        let (l, r) = (merge_sort(v, lt)?, merge_sort(right, lt)?);
        let mut out = Vec::with_capacity(l.len() + r.len());
        let (mut i, mut j) = (0, 0);
        while i < l.len() && j < r.len() {
            if lt(&r[j], &l[i])? {
                out.push(r[j].clone());
                j += 1;
            } else {
                out.push(l[i].clone());
                i += 1;
            }
        }
        out.extend_from_slice(&l[i..]);
        out.extend_from_slice(&r[j..]);
        Ok(out)
    }
    merge_sort(items.to_vec(), &lt).ok()
}

fn b64_lines(b: &[u8]) -> String {
    use base64::Engine;
    let mut out = String::new();
    for chunk in b.chunks(57) {
        out.push_str(&base64::engine::general_purpose::STANDARD.encode(chunk));
        out.push('\n');
    }
    out
}

impl Repr {
    fn push(&mut self, n: DNode, alias_key: Option<usize>) -> usize {
        self.nodes.push(n);
        let id = self.nodes.len() - 1;
        if let Some(k) = alias_key {
            self.represented.insert(k, id);
        }
        id
    }

    fn scalar(&mut self, tag: &str, value: String, style: Option<String>, alias_key: Option<usize>) -> usize {
        let style = style.or_else(|| self.default_style.clone());
        self.push(DNode { tag: tag.to_string(), kind: DK::Scalar(value, style) }, alias_key)
    }

    fn plain_scalar(&self, n: usize) -> bool {
        matches!(&self.nodes[n].kind, DK::Scalar(_, s) if s.as_deref().unwrap_or("").is_empty())
    }

    fn sequence(&mut self, tag: &str, items: &[V], alias_key: Option<usize>, depth: usize) -> R<usize> {
        let id = self.push(DNode { tag: tag.to_string(), kind: DK::Seq(vec![], None) }, alias_key);
        let mut best = true;
        for item in items {
            let n = self.represent(item, depth + 1)?;
            if !self.plain_scalar(n) {
                best = false;
            }
            if let DK::Seq(v, _) = &mut self.nodes[id].kind {
                v.push(n);
            }
        }
        let flow = Some(self.default_flow_style.unwrap_or(best));
        if let DK::Seq(_, f) = &mut self.nodes[id].kind {
            *f = if self.default_flow_style.is_some() { self.default_flow_style } else { flow };
        }
        Ok(id)
    }

    fn mapping(&mut self, tag: &str, items: Vec<(V, V)>, alias_key: Option<usize>, depth: usize) -> R<usize> {
        let id = self.push(DNode { tag: tag.to_string(), kind: DK::Map(vec![], None) }, alias_key);
        let mut best = true;
        let items = if self.sort_keys { sorted_items(&items).unwrap_or(items) } else { items };
        for (k, v) in items {
            let nk = self.represent(&k, depth + 1)?;
            let nv = self.represent(&v, depth + 1)?;
            if !self.plain_scalar(nk) || !self.plain_scalar(nv) {
                best = false;
            }
            if let DK::Map(m, _) = &mut self.nodes[id].kind {
                m.push((nk, nv));
            }
        }
        if let DK::Map(_, f) = &mut self.nodes[id].kind {
            *f = Some(self.default_flow_style.unwrap_or(best));
        }
        Ok(id)
    }

    fn represent(&mut self, data: &V, depth: usize) -> R<usize> {
        if depth > MAX_DEPTH {
            return Err(recursion());
        }
        super::stack_guard()?;
        let alias_key = match data {
            V::None | V::Str(_) | V::Bytes(_) | V::Bool(_) | V::Int(_) | V::Float(_) => None,
            V::Tuple(t) if t.is_empty() => None,
            _ => identity(data),
        };
        if let Some(k) = alias_key {
            if let Some(n) = self.represented.get(&k) {
                return Ok(*n);
            }
            self.keep.push(data.clone());
        }
        let y = |t: &str| format!("tag:yaml.org,2002:{t}");
        Ok(match data {
            V::None => self.scalar(&y("null"), "null".into(), None, alias_key),
            V::Str(s) => self.scalar(TAG_STR, s.to_string(), None, alias_key),
            V::Bytes(b) => self.scalar(&y("binary"), b64_lines(b), Some("|".into()), alias_key),
            V::Bool(b) => self.scalar(&y("bool"), if *b { "true" } else { "false" }.into(), None, alias_key),
            V::Int(i) => self.scalar(&y("int"), i.to_string(), None, alias_key),
            V::Float(f) => {
                let value = if f.is_nan() {
                    ".nan".to_string()
                } else if *f == f64::INFINITY {
                    ".inf".to_string()
                } else if *f == f64::NEG_INFINITY {
                    "-.inf".to_string()
                } else {
                    let r = ops::float_repr(*f).to_lowercase();
                    if !r.contains('.') && r.contains('e') { r.replacen('e', ".0e", 1) } else { r }
                };
                self.scalar(&y("float"), value, None, alias_key)
            }
            V::List(l) => {
                let items = l.lock().clone();
                self.sequence(&y("seq"), &items, alias_key, depth)?
            }
            V::Tuple(t) => self.sequence(&if self.full { y("python/tuple") } else { y("seq") }, &t.clone(), alias_key, depth)?,
            V::Dict(d) => {
                let items: Vec<(V, V)> = d.lock().values().cloned().collect();
                self.mapping(&y("map"), items, alias_key, depth)?
            }
            V::Set(s) => {
                let items: Vec<(V, V)> = s.lock().values().map(|k| (k.clone(), V::None)).collect();
                self.mapping(&y("set"), items, alias_key, depth)?
            }
            V::Date(d) => self.scalar(&y("timestamp"), super::dt::date_iso(d), None, alias_key),
            V::DateTime(d) => self.scalar(&y("timestamp"), d.isoformat(' ', "auto"), None, alias_key),
            V::Native(n) if self.full && matches!(&**n, Native::Complex(..)) => {
                let Native::Complex(re, im) = &**n else { unreachable!() };
                let r = |x: f64| ops::float_repr(x);
                let value = if *im == 0.0 {
                    r(*re)
                } else if *re == 0.0 {
                    format!("{}j", r(*im))
                } else if *im > 0.0 {
                    format!("{}+{}j", r(*re), r(*im))
                } else {
                    format!("{}{}j", r(*re), r(*im))
                };
                self.scalar(&y("python/complex"), value, None, alias_key)
            }
            _ if self.full => {
                return Err(Exc::type_error(format!(
                    "py2axum: yaml.dump() of a {} object is not supported (PyYAML writes a !!python/object tag for it); use yaml.safe_dump() with builtin values",
                    data.type_name()
                )))
            }
            _ => return Err(Exc::new(&YAML_REPRESENTER_ERROR, vec![V::str("cannot represent an object"), data.clone()])),
        })
    }
}

// ---------------------------------------------------------------- events, serializer

#[derive(Clone)]
enum EE {
    StreamStart(Option<String>),
    StreamEnd,
    DocStart { explicit: bool, version: Option<(i64, i64)>, tags: Option<Vec<(String, String)>> },
    DocEnd { explicit: bool },
    Alias(Option<String>),
    Scalar { anchor: Option<String>, tag: Option<String>, implicit: (bool, bool), value: String, style: Option<String> },
    SeqStart { anchor: Option<String>, tag: Option<String>, implicit: bool, flow: Option<bool> },
    SeqEnd,
    MapStart { anchor: Option<String>, tag: Option<String>, implicit: bool, flow: Option<bool> },
    MapEnd,
}

impl EE {
    fn anchor(&self) -> Option<&String> {
        match self {
            EE::Alias(a) => a.as_ref(),
            EE::Scalar { anchor, .. } | EE::SeqStart { anchor, .. } | EE::MapStart { anchor, .. } => anchor.as_ref(),
            _ => None,
        }
    }
    fn tag(&self) -> Option<&String> {
        match self {
            EE::Scalar { tag, .. } | EE::SeqStart { tag, .. } | EE::MapStart { tag, .. } => tag.as_ref(),
            _ => None,
        }
    }
    fn is_node(&self) -> bool {
        matches!(self, EE::Alias(_) | EE::Scalar { .. } | EE::SeqStart { .. } | EE::MapStart { .. })
    }
}

struct Serializer<'a> {
    nodes: &'a [DNode],
    anchors: HashMap<usize, Option<String>>,
    serialized: std::collections::HashSet<usize>,
    last_anchor_id: usize,
}

impl Serializer<'_> {
    fn anchor_node(&mut self, node: usize, depth: usize) -> R<()> {
        if depth > MAX_DEPTH * 3 {
            return Err(recursion());
        }
        super::stack_guard()?;
        if let Some(a) = self.anchors.get(&node) {
            if a.is_none() {
                self.last_anchor_id += 1;
                self.anchors.insert(node, Some(format!("id{:03}", self.last_anchor_id)));
            }
            return Ok(());
        }
        self.anchors.insert(node, None);
        match &self.nodes[node].kind {
            DK::Seq(items, _) => {
                for i in items.clone() {
                    self.anchor_node(i, depth + 1)?;
                }
            }
            DK::Map(items, _) => {
                for (k, v) in items.clone() {
                    self.anchor_node(k, depth + 1)?;
                    self.anchor_node(v, depth + 1)?;
                }
            }
            DK::Scalar(..) => {}
        }
        Ok(())
    }

    fn serialize_node(&mut self, node: usize, e: &mut Emitter, depth: usize) -> R<()> {
        if depth > MAX_DEPTH * 3 {
            return Err(recursion());
        }
        super::stack_guard()?;
        let alias = self.anchors.get(&node).cloned().flatten();
        if self.serialized.contains(&node) {
            return e.emit(EE::Alias(alias));
        }
        self.serialized.insert(node);
        let n = &self.nodes[node];
        match &n.kind {
            DK::Scalar(value, style) => {
                let detected = implicit_tag(value).unwrap_or(TAG_STR);
                e.emit(EE::Scalar {
                    anchor: alias,
                    tag: Some(n.tag.clone()),
                    implicit: (n.tag == detected, n.tag == TAG_STR),
                    value: value.clone(),
                    style: style.clone(),
                })?;
            }
            DK::Seq(items, flow) => {
                e.emit(EE::SeqStart { anchor: alias, tag: Some(n.tag.clone()), implicit: n.tag == "tag:yaml.org,2002:seq", flow: *flow })?;
                for i in items.clone() {
                    self.serialize_node(i, e, depth + 1)?;
                }
                e.emit(EE::SeqEnd)?;
            }
            DK::Map(items, flow) => {
                e.emit(EE::MapStart { anchor: alias, tag: Some(n.tag.clone()), implicit: n.tag == "tag:yaml.org,2002:map", flow: *flow })?;
                for (k, v) in items.clone() {
                    self.serialize_node(k, e, depth + 1)?;
                    self.serialize_node(v, e, depth + 1)?;
                }
                e.emit(EE::MapEnd)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- emitter

#[derive(Clone, Copy)]
enum ES {
    StreamStart,
    FirstDocumentStart,
    DocumentStart,
    DocumentEnd,
    DocumentRoot,
    Nothing,
    FirstFlowSequenceItem,
    FlowSequenceItem,
    FirstFlowMappingKey,
    FlowMappingKey,
    FlowMappingSimpleValue,
    FlowMappingValue,
    FirstBlockSequenceItem,
    BlockSequenceItem,
    FirstBlockMappingKey,
    BlockMappingKey,
    BlockMappingSimpleValue,
    BlockMappingValue,
}

struct Analysis {
    scalar: Vec<char>,
    empty: bool,
    multiline: bool,
    allow_flow_plain: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
    allow_block: bool,
}

/// what the stream received: text, or bytes once `encoding` is set (a stream without an `encoding` attribute)
enum Out {
    Text(String),
    Bytes(Vec<u8>),
}

struct Emitter {
    out: Out,
    /// the stream has an `encoding` attribute (a text file, io.StringIO): `encoding=` is then not applied
    text_stream: bool,
    encoding: Option<String>,
    states: Vec<ES>,
    state: ES,
    events: VecDeque<EE>,
    event: Option<EE>,
    indents: Vec<Option<i64>>,
    indent: Option<i64>,
    flow_level: usize,
    root_context: bool,
    sequence_context: bool,
    mapping_context: bool,
    simple_key_context: bool,
    column: i64,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
    canonical: bool,
    allow_unicode: bool,
    best_indent: i64,
    best_width: f64,
    best_line_break: String,
    tag_prefixes: IndexMap<String, String>,
    prepared_anchor: Option<String>,
    prepared_tag: Option<String>,
    analysis: Option<Analysis>,
    style: Option<String>,
}

/// `str.encode(encoding)` for the codecs a dump can name; a codec error is CPython's
fn encode(s: &str, encoding: &str) -> R<Vec<u8>> {
    let norm = encoding.to_ascii_lowercase().replace(['-', ' '], "_");
    let limited = |limit: u32, name: &str| -> R<Vec<u8>> {
        let chars: Vec<char> = s.chars().collect();
        if let Some(i) = chars.iter().position(|c| *c as u32 >= limit) {
            let end = (i..chars.len()).find(|j| (chars[*j] as u32) < limit).unwrap_or(chars.len());
            let n = chars[i] as u32;
            let esc = if n < 0x100 { format!("'\\x{n:02x}'") } else if n < 0x10000 { format!("'\\u{n:04x}'") } else { format!("'\\U{n:08x}'") };
            let msg = if end - i == 1 {
                format!("'{name}' codec can't encode character {esc} in position {i}: ordinal not in range({limit})")
            } else {
                format!("'{name}' codec can't encode characters in position {i}-{}: ordinal not in range({limit})", end - 1)
            };
            return Err(Exc::msg(&UNICODE_ENCODE_ERROR, msg));
        }
        Ok(chars.iter().map(|c| *c as u32 as u8).collect())
    };
    let utf16 = |le: bool, bom: bool| {
        let mut out = vec![];
        if bom {
            out.extend([0xFF, 0xFE]);
        }
        for u in s.encode_utf16() {
            out.extend(if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        out
    };
    let utf32 = |le: bool, bom: bool| {
        let mut out = vec![];
        if bom {
            out.extend([0xFF, 0xFE, 0, 0]);
        }
        for c in s.chars() {
            out.extend(if le { (c as u32).to_le_bytes() } else { (c as u32).to_be_bytes() });
        }
        out
    };
    Ok(match norm.as_str() {
        "utf_8" | "utf8" | "u8" | "utf" | "utf8_ucs2" | "utf8_ucs4" | "cp65001" => s.as_bytes().to_vec(),
        "utf_16" | "utf16" | "u16" => utf16(true, true),
        "utf_16_le" | "utf_16le" => utf16(true, false),
        "utf_16_be" | "utf_16be" => utf16(false, false),
        "utf_32" | "utf32" | "u32" => utf32(true, true),
        "utf_32_le" | "utf_32le" => utf32(true, false),
        "utf_32_be" | "utf_32be" => utf32(false, false),
        "ascii" | "us_ascii" | "646" | "us" => limited(0x80, "ascii")?,
        "latin_1" | "latin1" | "latin" | "l1" | "iso_8859_1" | "iso8859_1" | "8859" | "cp819" | "iso_ir_100" => limited(0x100, "latin-1")?,
        _ => return Err(Exc::type_error(format!("py2axum: yaml dump encoding={} is not supported (utf-8, utf-16, utf-32, ascii, latin-1)", ops::str_repr(encoding)))),
    })
}

impl Emitter {
    fn new(o: &Opts, text_stream: bool) -> Emitter {
        let mut best_indent = 2;
        if let Some(i) = o.indent {
            if i != 0 && 1 < i && i < 10 {
                best_indent = i;
            }
        }
        let mut best_width = 80.0;
        if let Some(w) = o.width {
            if w != 0.0 && w > (best_indent * 2) as f64 {
                best_width = w;
            }
        }
        let best_line_break = match o.line_break.as_deref() {
            Some(b @ ("\r" | "\n" | "\r\n")) => b.to_string(),
            _ => "\n".to_string(),
        };
        Emitter {
            out: Out::Text(String::new()),
            text_stream,
            encoding: None,
            states: vec![],
            state: ES::StreamStart,
            events: VecDeque::new(),
            event: None,
            indents: vec![],
            indent: None,
            flow_level: 0,
            root_context: false,
            sequence_context: false,
            mapping_context: false,
            simple_key_context: false,
            column: 0,
            whitespace: true,
            indention: true,
            open_ended: false,
            canonical: o.canonical,
            allow_unicode: o.allow_unicode,
            best_indent,
            best_width,
            best_line_break,
            tag_prefixes: IndexMap::new(),
            prepared_anchor: None,
            prepared_tag: None,
            analysis: None,
            style: None,
        }
    }

    fn write(&mut self, data: &str) -> R<()> {
        match &self.encoding {
            Some(enc) => {
                let b = encode(data, enc)?;
                match &mut self.out {
                    Out::Bytes(o) => o.extend(b),
                    Out::Text(t) if t.is_empty() => self.out = Out::Bytes(b),
                    Out::Text(_) => {}
                }
            }
            None => match &mut self.out {
                Out::Text(t) => t.push_str(data),
                Out::Bytes(_) => {}
            },
        }
        Ok(())
    }

    fn emit(&mut self, e: EE) -> R<()> {
        self.events.push_back(e);
        while !self.need_more_events() {
            self.event = self.events.pop_front();
            self.run()?;
            self.event = None;
        }
        Ok(())
    }

    fn need_more_events(&self) -> bool {
        let Some(first) = self.events.front() else { return true };
        let count = match first {
            EE::DocStart { .. } => 1,
            EE::SeqStart { .. } => 2,
            EE::MapStart { .. } => 3,
            _ => return false,
        };
        let mut level: i64 = 0;
        for e in self.events.iter().skip(1) {
            match e {
                EE::DocStart { .. } | EE::SeqStart { .. } | EE::MapStart { .. } => level += 1,
                EE::DocEnd { .. } | EE::SeqEnd | EE::MapEnd => level -= 1,
                EE::StreamEnd => level = -1,
                _ => {}
            }
            if level < 0 {
                return false;
            }
        }
        self.events.len() < count + 1
    }

    fn ev(&self) -> &EE {
        self.event.as_ref().unwrap()
    }

    fn pop_state(&mut self) -> ES {
        self.states.pop().unwrap_or(ES::Nothing)
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        match self.indent {
            None => self.indent = Some(if flow { self.best_indent } else { 0 }),
            Some(i) if !indentless => self.indent = Some(i + self.best_indent),
            _ => {}
        }
    }

    fn run(&mut self) -> R<()> {
        match self.state {
            ES::StreamStart => {
                if let EE::StreamStart(enc) = self.ev().clone() {
                    if enc.as_deref().is_some_and(|e| !e.is_empty()) && !self.text_stream {
                        self.encoding = enc;
                    }
                    if let Some(enc) = self.encoding.clone() {
                        if enc.starts_with("utf-16") {
                            self.write("\u{FEFF}")?;
                        }
                    }
                    self.state = ES::FirstDocumentStart;
                    Ok(())
                } else {
                    Err(emitter_err("expected StreamStartEvent"))
                }
            }
            ES::FirstDocumentStart => self.expect_document_start(true),
            ES::DocumentStart => self.expect_document_start(false),
            ES::DocumentEnd => {
                if let EE::DocEnd { explicit } = self.ev().clone() {
                    self.write_indent()?;
                    if explicit {
                        self.write_indicator("...", true, false, false)?;
                        self.write_indent()?;
                    }
                    self.state = ES::DocumentStart;
                    Ok(())
                } else {
                    Err(emitter_err("expected DocumentEndEvent"))
                }
            }
            ES::DocumentRoot => {
                self.states.push(ES::DocumentEnd);
                self.expect_node(true, false, false, false)
            }
            ES::Nothing => Err(emitter_err("expected nothing")),
            ES::FirstFlowSequenceItem => {
                if matches!(self.ev(), EE::SeqEnd) {
                    self.indent = self.indents.pop().flatten();
                    self.flow_level -= 1;
                    self.write_indicator("]", false, false, false)?;
                    self.state = self.pop_state();
                } else {
                    if self.canonical || self.column as f64 > self.best_width {
                        self.write_indent()?;
                    }
                    self.states.push(ES::FlowSequenceItem);
                    self.expect_node(false, true, false, false)?;
                }
                Ok(())
            }
            ES::FlowSequenceItem => {
                if matches!(self.ev(), EE::SeqEnd) {
                    self.indent = self.indents.pop().flatten();
                    self.flow_level -= 1;
                    if self.canonical {
                        self.write_indicator(",", false, false, false)?;
                        self.write_indent()?;
                    }
                    self.write_indicator("]", false, false, false)?;
                    self.state = self.pop_state();
                } else {
                    self.write_indicator(",", false, false, false)?;
                    if self.canonical || self.column as f64 > self.best_width {
                        self.write_indent()?;
                    }
                    self.states.push(ES::FlowSequenceItem);
                    self.expect_node(false, true, false, false)?;
                }
                Ok(())
            }
            ES::FirstFlowMappingKey | ES::FlowMappingKey => {
                let first = matches!(self.state, ES::FirstFlowMappingKey);
                if matches!(self.ev(), EE::MapEnd) {
                    self.indent = self.indents.pop().flatten();
                    self.flow_level -= 1;
                    if !first && self.canonical {
                        self.write_indicator(",", false, false, false)?;
                        self.write_indent()?;
                    }
                    self.write_indicator("}", false, false, false)?;
                    self.state = self.pop_state();
                } else {
                    if !first {
                        self.write_indicator(",", false, false, false)?;
                    }
                    if self.canonical || self.column as f64 > self.best_width {
                        self.write_indent()?;
                    }
                    if !self.canonical && self.check_simple_key()? {
                        self.states.push(ES::FlowMappingSimpleValue);
                        self.expect_node(false, false, true, true)?;
                    } else {
                        self.write_indicator("?", true, false, false)?;
                        self.states.push(ES::FlowMappingValue);
                        self.expect_node(false, false, true, false)?;
                    }
                }
                Ok(())
            }
            ES::FlowMappingSimpleValue => {
                self.write_indicator(":", false, false, false)?;
                self.states.push(ES::FlowMappingKey);
                self.expect_node(false, false, true, false)
            }
            ES::FlowMappingValue => {
                if self.canonical || self.column as f64 > self.best_width {
                    self.write_indent()?;
                }
                self.write_indicator(":", true, false, false)?;
                self.states.push(ES::FlowMappingKey);
                self.expect_node(false, false, true, false)
            }
            ES::FirstBlockSequenceItem | ES::BlockSequenceItem => {
                let first = matches!(self.state, ES::FirstBlockSequenceItem);
                if !first && matches!(self.ev(), EE::SeqEnd) {
                    self.indent = self.indents.pop().flatten();
                    self.state = self.pop_state();
                    Ok(())
                } else {
                    self.write_indent()?;
                    self.write_indicator("-", true, false, true)?;
                    self.states.push(ES::BlockSequenceItem);
                    self.expect_node(false, true, false, false)
                }
            }
            ES::FirstBlockMappingKey | ES::BlockMappingKey => {
                let first = matches!(self.state, ES::FirstBlockMappingKey);
                if !first && matches!(self.ev(), EE::MapEnd) {
                    self.indent = self.indents.pop().flatten();
                    self.state = self.pop_state();
                    Ok(())
                } else {
                    self.write_indent()?;
                    if self.check_simple_key()? {
                        self.states.push(ES::BlockMappingSimpleValue);
                        self.expect_node(false, false, true, true)
                    } else {
                        self.write_indicator("?", true, false, true)?;
                        self.states.push(ES::BlockMappingValue);
                        self.expect_node(false, false, true, false)
                    }
                }
            }
            ES::BlockMappingSimpleValue => {
                self.write_indicator(":", false, false, false)?;
                self.states.push(ES::BlockMappingKey);
                self.expect_node(false, false, true, false)
            }
            ES::BlockMappingValue => {
                self.write_indent()?;
                self.write_indicator(":", true, false, true)?;
                self.states.push(ES::BlockMappingKey);
                self.expect_node(false, false, true, false)
            }
        }
    }

    fn expect_document_start(&mut self, first: bool) -> R<()> {
        match self.ev().clone() {
            EE::DocStart { explicit, version, tags } => {
                if (version.is_some() || tags.is_some()) && self.open_ended {
                    self.write_indicator("...", true, false, false)?;
                    self.write_indent()?;
                }
                if let Some((major, minor)) = version {
                    if major != 1 {
                        return Err(emitter_err(format!("unsupported YAML version: {major}.{minor}")));
                    }
                    self.write(&format!("%YAML {major}.{minor}"))?;
                    self.write_line_break(None)?;
                }
                self.tag_prefixes = IndexMap::from([("!".to_string(), "!".to_string()), ("tag:yaml.org,2002:".to_string(), "!!".to_string())]);
                if let Some(tags) = &tags {
                    let mut handles: Vec<&(String, String)> = tags.iter().collect();
                    handles.sort_by(|a, b| a.0.cmp(&b.0));
                    for (handle, prefix) in handles {
                        self.tag_prefixes.insert(prefix.clone(), handle.clone());
                        let handle_text = prepare_tag_handle(handle)?;
                        let prefix_text = prepare_tag_prefix(prefix)?;
                        self.write(&format!("%TAG {handle_text} {prefix_text}"))?;
                        self.write_line_break(None)?;
                    }
                }
                let implicit = first && !explicit && !self.canonical && version.is_none() && tags.is_none();
                if !implicit {
                    self.write_indent()?;
                    self.write_indicator("---", true, false, false)?;
                    if self.canonical {
                        self.write_indent()?;
                    }
                }
                self.state = ES::DocumentRoot;
                Ok(())
            }
            EE::StreamEnd => {
                if self.open_ended {
                    self.write_indicator("...", true, false, false)?;
                    self.write_indent()?;
                }
                self.state = ES::Nothing;
                Ok(())
            }
            _ => Err(emitter_err("expected DocumentStartEvent")),
        }
    }

    fn expect_node(&mut self, root: bool, sequence: bool, mapping: bool, simple_key: bool) -> R<()> {
        self.root_context = root;
        self.sequence_context = sequence;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match self.ev().clone() {
            EE::Alias(anchor) => {
                if anchor.is_none() {
                    return Err(emitter_err("anchor is not specified for alias"));
                }
                self.process_anchor("*")?;
                self.state = self.pop_state();
                Ok(())
            }
            EE::Scalar { .. } => {
                self.process_anchor("&")?;
                self.process_tag()?;
                self.increase_indent(true, false);
                self.process_scalar()?;
                self.indent = self.indents.pop().flatten();
                self.state = self.pop_state();
                Ok(())
            }
            EE::SeqStart { flow, .. } => {
                self.process_anchor("&")?;
                self.process_tag()?;
                if self.flow_level != 0 || self.canonical || flow == Some(true) || self.check_empty(true) {
                    self.write_indicator("[", true, true, false)?;
                    self.flow_level += 1;
                    self.increase_indent(true, false);
                    self.state = ES::FirstFlowSequenceItem;
                } else {
                    let indentless = self.mapping_context && !self.indention;
                    self.increase_indent(false, indentless);
                    self.state = ES::FirstBlockSequenceItem;
                }
                Ok(())
            }
            EE::MapStart { flow, .. } => {
                self.process_anchor("&")?;
                self.process_tag()?;
                if self.flow_level != 0 || self.canonical || flow == Some(true) || self.check_empty(false) {
                    self.write_indicator("{", true, true, false)?;
                    self.flow_level += 1;
                    self.increase_indent(true, false);
                    self.state = ES::FirstFlowMappingKey;
                } else {
                    self.increase_indent(false, false);
                    self.state = ES::FirstBlockMappingKey;
                }
                Ok(())
            }
            _ => Err(emitter_err("expected NodeEvent")),
        }
    }

    /// check_empty_sequence / check_empty_mapping
    fn check_empty(&self, seq: bool) -> bool {
        match (self.ev(), self.events.front()) {
            (EE::SeqStart { .. }, Some(EE::SeqEnd)) => seq,
            (EE::MapStart { .. }, Some(EE::MapEnd)) => !seq,
            _ => false,
        }
    }

    fn check_simple_key(&mut self) -> R<bool> {
        let mut length = 0;
        let ev = self.ev().clone();
        if ev.is_node() {
            if let Some(a) = ev.anchor() {
                if self.prepared_anchor.is_none() {
                    self.prepared_anchor = Some(prepare_anchor(a)?);
                }
                length += self.prepared_anchor.as_ref().unwrap().chars().count();
            }
        }
        if let Some(t) = ev.tag() {
            if self.prepared_tag.is_none() {
                self.prepared_tag = Some(self.prepare_tag(t)?);
            }
            length += self.prepared_tag.as_ref().unwrap().chars().count();
        }
        if let EE::Scalar { value, .. } = &ev {
            if self.analysis.is_none() {
                self.analysis = Some(self.analyze_scalar(value));
            }
            length += self.analysis.as_ref().unwrap().scalar.len();
        }
        let a = self.analysis.as_ref();
        Ok(length < 128
            && (matches!(ev, EE::Alias(_))
                || (matches!(ev, EE::Scalar { .. }) && !a.unwrap().empty && !a.unwrap().multiline)
                || self.check_empty(true)
                || self.check_empty(false)))
    }

    fn process_anchor(&mut self, indicator: &str) -> R<()> {
        let Some(anchor) = self.ev().anchor().cloned() else {
            self.prepared_anchor = None;
            return Ok(());
        };
        if self.prepared_anchor.is_none() {
            self.prepared_anchor = Some(prepare_anchor(&anchor)?);
        }
        let a = self.prepared_anchor.take().unwrap();
        if !a.is_empty() {
            self.write_indicator(&format!("{indicator}{a}"), true, false, false)?;
        }
        Ok(())
    }

    fn process_tag(&mut self) -> R<()> {
        let ev = self.ev().clone();
        let mut tag = ev.tag().cloned();
        if let EE::Scalar { implicit, .. } = &ev {
            if self.style.is_none() {
                self.style = Some(self.choose_scalar_style());
            }
            let style = self.style.clone().unwrap();
            if (!self.canonical || tag.is_none()) && ((style.is_empty() && implicit.0) || (!style.is_empty() && implicit.1)) {
                self.prepared_tag = None;
                return Ok(());
            }
            if implicit.0 && tag.is_none() {
                tag = Some("!".into());
                self.prepared_tag = None;
            }
        } else {
            let implicit = match &ev {
                EE::SeqStart { implicit, .. } | EE::MapStart { implicit, .. } => *implicit,
                _ => false,
            };
            if (!self.canonical || tag.is_none()) && implicit {
                self.prepared_tag = None;
                return Ok(());
            }
        }
        let Some(tag) = tag else { return Err(emitter_err("tag is not specified")) };
        if self.prepared_tag.is_none() {
            self.prepared_tag = Some(self.prepare_tag(&tag)?);
        }
        let t = self.prepared_tag.take().unwrap();
        if !t.is_empty() {
            self.write_indicator(&t, true, false, false)?;
        }
        Ok(())
    }

    fn choose_scalar_style(&mut self) -> String {
        let EE::Scalar { value, implicit, style, .. } = self.ev().clone() else { return "\"".into() };
        if self.analysis.is_none() {
            self.analysis = Some(self.analyze_scalar(&value));
        }
        let a = self.analysis.as_ref().unwrap();
        let st = style.unwrap_or_default();
        if st == "\"" || self.canonical {
            return "\"".into();
        }
        if st.is_empty() && implicit.0 {
            if !(self.simple_key_context && (a.empty || a.multiline))
                && ((self.flow_level != 0 && a.allow_flow_plain) || (self.flow_level == 0 && a.allow_block_plain))
            {
                return String::new();
            }
        }
        if !st.is_empty() && "|>".contains(st.as_str()) && self.flow_level == 0 && !self.simple_key_context && a.allow_block {
            return st;
        }
        if (st.is_empty() || st == "'") && a.allow_single_quoted && !(self.simple_key_context && a.multiline) {
            return "'".into();
        }
        "\"".into()
    }

    fn process_scalar(&mut self) -> R<()> {
        if self.analysis.is_none() {
            let EE::Scalar { value, .. } = self.ev().clone() else { return Ok(()) };
            self.analysis = Some(self.analyze_scalar(&value));
        }
        if self.style.is_none() {
            self.style = Some(self.choose_scalar_style());
        }
        let split = !self.simple_key_context;
        let text = self.analysis.as_ref().unwrap().scalar.clone();
        match self.style.clone().unwrap().as_str() {
            "\"" => self.write_double_quoted(&text, split)?,
            "'" => self.write_single_quoted(&text, split)?,
            ">" => self.write_folded(&text)?,
            "|" => self.write_literal(&text)?,
            _ => self.write_plain(&text, split)?,
        }
        self.analysis = None;
        self.style = None;
        Ok(())
    }

    fn prepare_tag(&self, tag: &str) -> R<String> {
        if tag.is_empty() {
            return Err(emitter_err("tag must not be empty"));
        }
        if tag == "!" {
            return Ok(tag.into());
        }
        let mut handle: Option<String> = None;
        let mut suffix: &str = tag;
        let mut prefixes: Vec<&String> = self.tag_prefixes.keys().collect();
        prefixes.sort();
        for prefix in prefixes {
            if tag.starts_with(prefix.as_str()) && (prefix == "!" || prefix.chars().count() < tag.chars().count()) {
                handle = Some(self.tag_prefixes[prefix].clone());
                suffix = &tag[prefix.len()..];
            }
        }
        let mut out = String::new();
        for ch in suffix.chars() {
            if ch.is_ascii_alphanumeric() || "-;/?:@&=+$,_.~*'()[]".contains(ch) || (ch == '!' && handle.as_deref() != Some("!")) {
                out.push(ch);
            } else {
                let mut b = [0u8; 4];
                for x in ch.encode_utf8(&mut b).bytes() {
                    out.push_str(&format!("%{x:02X}"));
                }
            }
        }
        Ok(match handle {
            Some(h) if !h.is_empty() => format!("{h}{out}"),
            _ => format!("!<{out}>"),
        })
    }

    fn analyze_scalar(&self, scalar: &str) -> Analysis {
        let chars: Vec<char> = scalar.chars().collect();
        if chars.is_empty() {
            return Analysis { scalar: chars, empty: true, multiline: false, allow_flow_plain: false, allow_block_plain: true, allow_single_quoted: true, allow_block: false };
        }
        let (mut block_indicators, mut flow_indicators, mut line_breaks, mut special_characters) = (false, false, false, false);
        let (mut leading_space, mut leading_break, mut trailing_space, mut trailing_break, mut break_space, mut space_break) = (false, false, false, false, false, false);
        if scalar.starts_with("---") || scalar.starts_with("...") {
            block_indicators = true;
            flow_indicators = true;
        }
        let mut preceded_by_whitespace = true;
        let mut followed_by_whitespace = chars.len() == 1 || has(chars[1], SPACE_BREAKS);
        let (mut previous_space, mut previous_break) = (false, false);
        let n = chars.len();
        let mut index = 0;
        while index < n {
            let ch = chars[index];
            if index == 0 {
                if has(ch, "#,[]{}&*!|>'\"%@`") {
                    flow_indicators = true;
                    block_indicators = true;
                }
                if has(ch, "?:") {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                if ch == '-' && followed_by_whitespace {
                    flow_indicators = true;
                    block_indicators = true;
                }
            } else {
                if has(ch, ",?[]{}") {
                    flow_indicators = true;
                }
                if ch == ':' {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                if ch == '#' && preceded_by_whitespace {
                    flow_indicators = true;
                    block_indicators = true;
                }
            }
            if has(ch, BREAKS) {
                line_breaks = true;
            }
            if !(ch == '\n' || ('\x20'..='\x7E').contains(&ch)) {
                if (ch == '\u{85}' || ('\u{A0}'..='\u{D7FF}').contains(&ch) || ('\u{E000}'..='\u{FFFD}').contains(&ch) || ('\u{10000}'..'\u{10FFFF}').contains(&ch)) && ch != '\u{FEFF}' {
                    if !self.allow_unicode {
                        special_characters = true;
                    }
                } else {
                    special_characters = true;
                }
            }
            if ch == ' ' {
                if index == 0 {
                    leading_space = true;
                }
                if index == n - 1 {
                    trailing_space = true;
                }
                if previous_break {
                    break_space = true;
                }
                previous_space = true;
                previous_break = false;
            } else if has(ch, BREAKS) {
                if index == 0 {
                    leading_break = true;
                }
                if index == n - 1 {
                    trailing_break = true;
                }
                if previous_space {
                    space_break = true;
                }
                previous_space = false;
                previous_break = true;
            } else {
                previous_space = false;
                previous_break = false;
            }
            index += 1;
            preceded_by_whitespace = has(ch, SPACE_BREAKS);
            followed_by_whitespace = index + 1 >= n || has(chars[index + 1], SPACE_BREAKS);
        }
        let (mut allow_flow_plain, mut allow_block_plain, mut allow_single_quoted, mut allow_block) = (true, true, true, true);
        if leading_space || leading_break || trailing_space || trailing_break {
            allow_flow_plain = false;
            allow_block_plain = false;
        }
        if trailing_space {
            allow_block = false;
        }
        if break_space {
            allow_flow_plain = false;
            allow_block_plain = false;
            allow_single_quoted = false;
        }
        if space_break || special_characters {
            allow_flow_plain = false;
            allow_block_plain = false;
            allow_single_quoted = false;
            allow_block = false;
        }
        if line_breaks {
            allow_flow_plain = false;
            allow_block_plain = false;
        }
        if flow_indicators {
            allow_flow_plain = false;
        }
        if block_indicators {
            allow_block_plain = false;
        }
        Analysis { scalar: chars, empty: false, multiline: line_breaks, allow_flow_plain, allow_block_plain, allow_single_quoted, allow_block }
    }

    // writers

    fn write_indicator(&mut self, indicator: &str, need_whitespace: bool, whitespace: bool, indention: bool) -> R<()> {
        let data = if self.whitespace || !need_whitespace { indicator.to_string() } else { format!(" {indicator}") };
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
        self.column += data.chars().count() as i64;
        self.open_ended = false;
        self.write(&data)
    }

    fn write_indent(&mut self) -> R<()> {
        let indent = self.indent.unwrap_or(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.write_line_break(None)?;
        }
        if self.column < indent {
            self.whitespace = true;
            let data = " ".repeat((indent - self.column) as usize);
            self.column = indent;
            self.write(&data)?;
        }
        Ok(())
    }

    fn write_line_break(&mut self, data: Option<char>) -> R<()> {
        let data = match data {
            Some(c) => c.to_string(),
            None => self.best_line_break.clone(),
        };
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
        self.write(&data)
    }

    fn write_text(&mut self, t: &[char]) -> R<()> {
        let s: String = t.iter().collect();
        self.column += t.len() as i64;
        self.write(&s)
    }

    fn write_breaks(&mut self, t: &[char]) -> R<()> {
        for br in t {
            if *br == '\n' {
                self.write_line_break(None)?;
            } else {
                self.write_line_break(Some(*br))?;
            }
        }
        Ok(())
    }

    fn write_single_quoted(&mut self, text: &[char], split: bool) -> R<()> {
        self.write_indicator("'", true, false, false)?;
        let (mut spaces, mut breaks) = (false, false);
        let (mut start, mut end) = (0usize, 0usize);
        let n = text.len();
        while end <= n {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column as f64 > self.best_width && split && start != 0 && end != n {
                        self.write_indent()?;
                    } else {
                        self.write_text(&text[start..end])?;
                    }
                    start = end;
                }
            } else if breaks {
                if ch.is_none_or(|c| !has(c, BREAKS)) {
                    if text[start] == '\n' {
                        self.write_line_break(None)?;
                    }
                    self.write_breaks(&text[start..end])?;
                    self.write_indent()?;
                    start = end;
                }
            } else if (ch.is_none() || ch.is_some_and(|c| has(c, " \n\u{85}\u{2028}\u{2029}") || c == '\'')) && start < end {
                self.write_text(&text[start..end])?;
                start = end;
            }
            if ch == Some('\'') {
                self.column += 2;
                self.write("''")?;
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = has(c, BREAKS);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false)
    }

    fn write_double_quoted(&mut self, text: &[char], split: bool) -> R<()> {
        self.write_indicator("\"", true, false, false)?;
        let (mut start, mut end) = (0usize, 0usize);
        let n = text.len();
        while end <= n {
            let ch = text.get(end).copied();
            let escape = match ch {
                None => true,
                Some(c) => {
                    has(c, "\"\\\u{85}\u{2028}\u{2029}\u{FEFF}")
                        || !(('\x20'..='\x7E').contains(&c) || (self.allow_unicode && (('\u{A0}'..='\u{D7FF}').contains(&c) || ('\u{E000}'..='\u{FFFD}').contains(&c))))
                }
            };
            if escape {
                if start < end {
                    self.write_text(&text[start..end])?;
                    start = end;
                }
                if let Some(c) = ch {
                    let rep = match c {
                        '\0' => Some('0'),
                        '\x07' => Some('a'),
                        '\x08' => Some('b'),
                        '\x09' => Some('t'),
                        '\x0A' => Some('n'),
                        '\x0B' => Some('v'),
                        '\x0C' => Some('f'),
                        '\x0D' => Some('r'),
                        '\x1B' => Some('e'),
                        '"' => Some('"'),
                        '\\' => Some('\\'),
                        '\u{85}' => Some('N'),
                        '\u{A0}' => Some('_'),
                        '\u{2028}' => Some('L'),
                        '\u{2029}' => Some('P'),
                        _ => None,
                    };
                    let data = match rep {
                        Some(r) => format!("\\{r}"),
                        None if (c as u32) <= 0xFF => format!("\\x{:02X}", c as u32),
                        None if (c as u32) <= 0xFFFF => format!("\\u{:04X}", c as u32),
                        None => format!("\\U{:08X}", c as u32),
                    };
                    self.column += data.chars().count() as i64;
                    self.write(&data)?;
                    start = end + 1;
                }
            }
            if 0 < end && end + 1 < n && (ch == Some(' ') || start >= end) && (self.column + end as i64 - start as i64) as f64 > self.best_width && split {
                let mut data: String = if start < end { text[start..end].iter().collect() } else { String::new() };
                data.push('\\');
                if start < end {
                    start = end;
                }
                self.column += data.chars().count() as i64;
                self.write(&data)?;
                self.write_indent()?;
                self.whitespace = false;
                self.indention = false;
                if text[start] == ' ' {
                    self.column += 1;
                    self.write("\\")?;
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false)
    }

    fn determine_block_hints(&self, text: &[char]) -> String {
        let mut hints = String::new();
        if !text.is_empty() {
            if has(text[0], " \n\u{85}\u{2028}\u{2029}") {
                hints.push_str(&self.best_indent.to_string());
            }
            if !has(text[text.len() - 1], BREAKS) {
                hints.push('-');
            } else if text.len() == 1 || has(text[text.len() - 2], BREAKS) {
                hints.push('+');
            }
        }
        hints
    }

    fn write_folded(&mut self, text: &[char]) -> R<()> {
        let hints = self.determine_block_hints(text);
        self.write_indicator(&format!(">{hints}"), true, false, false)?;
        if hints.ends_with('+') {
            self.open_ended = true;
        }
        self.write_line_break(None)?;
        let (mut leading_space, mut spaces, mut breaks) = (true, false, true);
        let (mut start, mut end) = (0usize, 0usize);
        let n = text.len();
        while end <= n {
            let ch = text.get(end).copied();
            if breaks {
                if ch.is_none_or(|c| !has(c, BREAKS)) {
                    if !leading_space && ch.is_some() && ch != Some(' ') && text[start] == '\n' {
                        self.write_line_break(None)?;
                    }
                    leading_space = ch == Some(' ');
                    self.write_breaks(&text[start..end])?;
                    if ch.is_some() {
                        self.write_indent()?;
                    }
                    start = end;
                }
            } else if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column as f64 > self.best_width {
                        self.write_indent()?;
                    } else {
                        self.write_text(&text[start..end])?;
                    }
                    start = end;
                }
            } else if ch.is_none_or(|c| has(c, " \n\u{85}\u{2028}\u{2029}")) {
                self.write_text(&text[start..end])?;
                if ch.is_none() {
                    self.write_line_break(None)?;
                }
                start = end;
            }
            if let Some(c) = ch {
                breaks = has(c, BREAKS);
                spaces = c == ' ';
            }
            end += 1;
        }
        Ok(())
    }

    fn write_literal(&mut self, text: &[char]) -> R<()> {
        let hints = self.determine_block_hints(text);
        self.write_indicator(&format!("|{hints}"), true, false, false)?;
        if hints.ends_with('+') {
            self.open_ended = true;
        }
        self.write_line_break(None)?;
        let mut breaks = true;
        let (mut start, mut end) = (0usize, 0usize);
        let n = text.len();
        while end <= n {
            let ch = text.get(end).copied();
            if breaks {
                if ch.is_none_or(|c| !has(c, BREAKS)) {
                    self.write_breaks(&text[start..end])?;
                    if ch.is_some() {
                        self.write_indent()?;
                    }
                    start = end;
                }
            } else if ch.is_none_or(|c| has(c, BREAKS)) {
                // no column update here (as PyYAML)
                let s: String = text[start..end].iter().collect();
                self.write(&s)?;
                if ch.is_none() {
                    self.write_line_break(None)?;
                }
                start = end;
            }
            if let Some(c) = ch {
                breaks = has(c, BREAKS);
            }
            end += 1;
        }
        Ok(())
    }

    fn write_plain(&mut self, text: &[char], split: bool) -> R<()> {
        if self.root_context {
            self.open_ended = true;
        }
        if text.is_empty() {
            return Ok(());
        }
        if !self.whitespace {
            self.column += 1;
            self.write(" ")?;
        }
        self.whitespace = false;
        self.indention = false;
        let (mut spaces, mut breaks) = (false, false);
        let (mut start, mut end) = (0usize, 0usize);
        let n = text.len();
        while end <= n {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column as f64 > self.best_width && split {
                        self.write_indent()?;
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        self.write_text(&text[start..end])?;
                    }
                    start = end;
                }
            } else if breaks {
                match ch {
                    None => return Err(Exc::type_error("'in <string>' requires string as left operand, not NoneType")),
                    Some(c) if !has(c, BREAKS) => {
                        if text[start] == '\n' {
                            self.write_line_break(None)?;
                        }
                        self.write_breaks(&text[start..end])?;
                        self.write_indent()?;
                        self.whitespace = false;
                        self.indention = false;
                        start = end;
                    }
                    _ => {}
                }
            } else if ch.is_none_or(|c| has(c, " \n\u{85}\u{2028}\u{2029}")) {
                self.write_text(&text[start..end])?;
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = has(c, BREAKS);
            }
            end += 1;
        }
        Ok(())
    }
}

fn prepare_anchor(anchor: &str) -> R<String> {
    if anchor.is_empty() {
        return Err(emitter_err("anchor must not be empty"));
    }
    for ch in anchor.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return Err(emitter_err(format!("invalid character {} in the anchor: {}", super::pyyaml::py_repr(&ch.to_string()), super::pyyaml::py_repr(anchor))));
        }
    }
    Ok(anchor.to_string())
}

fn prepare_tag_handle(handle: &str) -> R<String> {
    if handle.is_empty() {
        return Err(emitter_err("tag handle must not be empty"));
    }
    if !handle.starts_with('!') || !handle.ends_with('!') {
        return Err(emitter_err(format!("tag handle must start and end with '!': {}", super::pyyaml::py_repr(handle))));
    }
    let chars: Vec<char> = handle.chars().collect();
    if chars.len() > 2 {
        for ch in &chars[1..chars.len() - 1] {
            if !(ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_') {
                return Err(emitter_err(format!("invalid character {} in the tag handle: {}", super::pyyaml::py_repr(&ch.to_string()), super::pyyaml::py_repr(handle))));
            }
        }
    }
    Ok(handle.to_string())
}

fn prepare_tag_prefix(prefix: &str) -> R<String> {
    if prefix.is_empty() {
        return Err(emitter_err("tag prefix must not be empty"));
    }
    for (i, ch) in prefix.chars().enumerate() {
        if i == 0 && ch == '!' {
            continue;
        }
        if !(ch.is_ascii_alphanumeric() || "-;/?!:@&=+$,_.~*'()[]".contains(ch)) {
            // PyYAML 6.0 applies ord() to the bytes of the character: TypeError
            return Err(Exc::type_error("ord() expected string of length 1, but int found"));
        }
    }
    Ok(prefix.to_string())
}

// ---------------------------------------------------------------- yaml.dump / yaml.safe_dump

/// `yaml.safe_dump(data, stream=None, **kw)` (full: `yaml.dump`, Representer's builtin types); `all`: the
/// `*_dump_all` variants, `data` an iterable of documents
pub fn dump(full: bool, all: bool, data: &V, stream: &V, kw: &[(String, V)]) -> R {
    let o = opts(kw)?;
    let docs = if all { ops::iter(data)? } else { vec![data.clone()] };
    // a stream with an `encoding` attribute (text file, io.StringIO) receives text whatever `encoding=` says
    let text_stream = match stream {
        V::None => o.encoding.is_none(),
        V::Native(n) => match &**n {
            Native::File(f) => !super::pathio::is_binary(f),
            Native::StringIO(_) => true,
            Native::BytesIO(_) => false,
            _ => return Err(Exc::type_error(format!("py2axum: yaml dump writes to a file object (open(), io.StringIO, io.BytesIO), not {}", stream.type_name()))),
        },
        _ => return Err(Exc::attr_error(format!("'{}' object has no attribute 'write'", stream.type_name()))),
    };
    let mut e = Emitter::new(&o, text_stream);
    let r = (|| -> R<()> {
        e.emit(EE::StreamStart(o.encoding.clone()))?;
        for d in &docs {
            let mut rep = Repr {
                nodes: vec![],
                represented: HashMap::new(),
                keep: vec![],
                default_style: o.default_style.clone(),
                default_flow_style: o.default_flow_style,
                sort_keys: o.sort_keys,
                full,
            };
            let root = rep.represent(d, 0)?;
            e.emit(EE::DocStart { explicit: o.explicit_start, version: o.version, tags: o.tags.clone() })?;
            let mut s = Serializer { nodes: &rep.nodes, anchors: HashMap::new(), serialized: Default::default(), last_anchor_id: 0 };
            s.anchor_node(root, 0)?;
            s.serialize_node(root, &mut e, 0)?;
            e.emit(EE::DocEnd { explicit: o.explicit_end })?;
        }
        e.emit(EE::StreamEnd)
    })();
    let out = match std::mem::replace(&mut e.out, Out::Text(String::new())) {
        Out::Text(t) if e.encoding.is_some() && t.is_empty() => V::Bytes(Arc::from(Vec::new())),
        Out::Text(t) => V::str(t),
        Out::Bytes(b) => V::Bytes(Arc::from(b)),
    };
    if stream.is_none() {
        r?;
        return Ok(out);
    }
    // what PyYAML wrote before an error is in the stream
    let written = match &out {
        V::Str(s) => !s.is_empty(),
        V::Bytes(b) => !b.is_empty(),
        _ => false,
    };
    if written {
        let V::Native(n) = stream else { unreachable!() };
        match &**n {
            Native::File(f) => {
                super::pathio::file_method(f, "write", &[out])?;
            }
            Native::StringIO(s) => {
                super::stdlib::stringio_method(s, "write", &[out])?;
            }
            Native::BytesIO(b) => {
                super::files::bytesio_method(b, "write", &[out])?;
            }
            _ => {}
        }
    }
    r?;
    Ok(V::None)
}
