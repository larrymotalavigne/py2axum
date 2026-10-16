//! `email._header_value_parser` (the policy.default header parser and folder) for unstructured and address headers,
//! ported from CPython 3.14.7 with the behaviours of the other supported releases behind `Ver` (each one measured on
//! that release: the folder changed in 3.12.13/3.13.12/3.14.3, 3.13.13/3.14.4, 3.13.14/3.14.5-6 and 3.15).
//! Python lengths and indexes count code points, as here (`clen`, `cslice`).
use super::methods::py_space;
use super::v::*;

/// The behaviours of the project's Python release (see `ver()`).
#[derive(Clone, Copy, Debug)]
pub struct Ver {
    /// 3.15: `as_ew_allowed` a class attribute (True), the refold split in with/without encoded words,
    /// HeaderWriteError for non-ASCII under utf8=False
    pub v315: bool,
    /// WhiteSpaceTerminal.startswith_fws() = `self and self[0] in WSP` and comments refolded with their
    /// parentheses (3.12.13, 3.13.12, 3.14.3)
    pub ws_fix: bool,
    /// gh-142006: a line emptied by stealing its trailing blank is removed (3.13.12, 3.14.3)
    pub pop_empty: bool,
    /// `last_word_is_ew` replaces `leading_whitespace` in the folder (3.13.13, 3.14.4)
    pub last_word_ew: bool,
    /// local-part dots, `""` closes a bare quoted string, junk after an address is its own invalid address
    /// (3.13.14, 3.14.5)
    pub addr_fix: bool,
    /// linear white space between encoded words of a phrase (3.13.14, 3.14.6)
    pub ew_phrase: bool,
    /// 3.12: a domain-literal cut by the end of input appends its defect to the token list
    pub dl_bug: bool,
    /// header names checked against RFC 5322 (3.14)
    pub check_name: bool,
    /// BytesGenerator checks the folded headers too, as Generator does (3.12.13, 3.13.12, 3.14.3)
    pub verify_bytes: bool,
}

pub fn ver() -> Ver {
    static V: std::sync::OnceLock<Ver> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let (_, minor) = super::python();
        let at = |m13: u32, m14: u32| match minor {
            13 => super::python_at_least(3, 13, m13),
            14 => super::python_at_least(3, 14, m14),
            m => m >= 15,
        };
        let feat = |name: &str, dflt: bool| super::python_feature(name).unwrap_or(dflt);
        Ver {
            v315: minor >= 15,
            ws_fix: feat("email_ws_fix", if minor == 12 { super::python_at_least(3, 12, 13) } else { at(12, 3) }),
            pop_empty: feat("email_pop_empty", minor > 12 && at(12, 3)),
            last_word_ew: feat("email_last_word_ew", minor > 12 && at(13, 4)),
            addr_fix: feat("email_addr_fix", minor > 12 && at(14, 5)),
            ew_phrase: feat("email_ew_phrase", minor > 12 && at(14, 6)),
            dl_bug: minor == 12,
            check_name: minor >= 14,
            verify_bytes: feat("email_verify_bytes", if minor == 12 { super::python_at_least(3, 12, 13) } else { at(12, 3) }),
        }
    })
}

// ---------------------------------------------------------------- strings, counted as Python does

pub fn clen(s: &str) -> usize {
    s.chars().count()
}

/// `s[a:b]` in code points (clamped)
pub fn cslice(s: &str, a: usize, b: usize) -> String {
    s.chars().skip(a).take(b.saturating_sub(a)).collect()
}

fn from(s: &str, a: usize) -> &str {
    match s.char_indices().nth(a) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

fn first(s: &str) -> Option<char> {
    s.chars().next()
}

fn rest1(s: &str) -> &str {
    let mut c = s.chars();
    c.next();
    c.as_str()
}

const WSP: &[char] = &[' ', '\t'];
const SPECIALS: &str = "()<>@,:;.\\\"[]";

fn is_wsp(c: char) -> bool {
    c == ' ' || c == '\t'
}
fn cfws_leader(c: char) -> bool {
    is_wsp(c) || c == '('
}
fn special(c: char) -> bool {
    SPECIALS.contains(c)
}
fn atom_end(c: char) -> bool {
    special(c) || is_wsp(c)
}
pub fn dot_atom_end(c: char) -> bool {
    atom_end(c) && c != '.'
}
fn phrase_end(c: char) -> bool {
    special(c) && !".\"(".contains(c)
}
fn starts(s: &str, f: impl Fn(char) -> bool) -> bool {
    first(s).is_some_and(f)
}

/// `quote_string(value)`
pub fn quote_string(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn has_special(s: &str) -> bool {
    s.chars().any(special)
}

/// `str.strip()` / `lstrip()` / `rstrip()` with Python's whitespace
fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(py_space)
}

/// `''.join(s.split())`
fn no_ws(s: &str) -> String {
    s.chars().filter(|c| !py_space(*c)).collect()
}

/// `_wsp_splitter(value, 1)`: the text up to the first run of blanks, and the rest (blanks included)
fn wsp_split(s: &str) -> (&str, &str) {
    match s.find(WSP) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

/// `rfc2047_matcher.search(s)`: `=\?[^?]*\?[qQbB]\?.*?\?=` (`.` not matching a newline)
fn rfc2047_search(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while let Some(p) = s[i..].find("=?") {
        let start = i + p;
        let mut j = start + 2;
        while j < b.len() && b[j] != b'?' {
            j += 1;
        }
        if j + 2 < b.len() && matches!(b[j + 1], b'q' | b'Q' | b'b' | b'B') && b[j + 2] == b'?' {
            let mut k = j + 3;
            while k + 1 < b.len() && b[k] != b'\n' {
                if b[k] == b'?' && b[k + 1] == b'=' {
                    return true;
                }
                k += 1;
            }
        }
        i = start + 1;
    }
    false
}

// ---------------------------------------------------------------- tokens

/// The Python class of a token (its properties); `tt` is its `token_type`, which the parser may change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum C {
    Vt,
    Wst,
    EwWs,
    Tl,
    Unstructured,
    Phrase,
    Cfws,
    Atom,
    EncodedWord,
    QuotedString,
    BareQuotedString,
    Comment,
    AddressList,
    Address,
    MailboxList,
    GroupList,
    Group,
    NameAddr,
    AngleAddr,
    ObsRoute,
    Mailbox,
    InvalidMailbox,
    Domain,
    DotAtom,
    DotAtomText,
    AddrSpec,
    ObsLocalPart,
    DisplayName,
    LocalPart,
    DomainLiteral,
    Header,
    HeaderLabel,
    /// a value rendered as is (Content-Type, Content-Transfer-Encoding, MIME-Version written by set_content)
    Literal,
    /// 3.12: an InvalidHeaderDefect appended to a domain-literal's list (its str is the defect's message)
    DefectItem,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dk {
    Invalid,
    Other,
}

#[derive(Clone, Debug)]
pub struct Tok {
    pub c: C,
    pub tt: &'static str,
    /// a terminal's text
    pub s: String,
    pub k: Vec<Tok>,
    pub d: Vec<Dk>,
    /// the ListSeparator singleton: as_ew_allowed and syntactic_break False
    pub sep: bool,
}

pub fn vt(s: &str, tt: &'static str) -> Tok {
    Tok { c: C::Vt, tt, s: s.to_string(), k: vec![], d: vec![], sep: false }
}
pub fn wst(s: &str, tt: &'static str) -> Tok {
    Tok { c: C::Wst, tt, s: s.to_string(), k: vec![], d: vec![], sep: false }
}
fn ewws(s: String) -> Tok {
    Tok { c: C::EwWs, tt: "fws", s, k: vec![], d: vec![], sep: false }
}
pub fn tl(c: C, k: Vec<Tok>) -> Tok {
    let tt = match c {
        C::Tl | C::Literal => "",
        C::Unstructured => "unstructured",
        C::Phrase => "phrase",
        C::Cfws => "cfws",
        C::Atom => "atom",
        C::EncodedWord => "encoded-word",
        C::QuotedString => "quoted-string",
        C::BareQuotedString => "bare-quoted-string",
        C::Comment => "comment",
        C::AddressList => "address-list",
        C::Address => "address",
        C::MailboxList => "mailbox-list",
        C::GroupList => "group-list",
        C::Group => "group",
        C::NameAddr => "name-addr",
        C::AngleAddr => "angle-addr",
        C::ObsRoute => "obs-route",
        C::Mailbox => "mailbox",
        C::InvalidMailbox => "invalid-mailbox",
        C::Domain => "domain",
        C::DotAtom => "dot-atom",
        C::DotAtomText => "dot-atom-text",
        C::AddrSpec => "addr-spec",
        C::ObsLocalPart => "obs-local-part",
        C::DisplayName => "display-name",
        C::LocalPart => "local-part",
        C::DomainLiteral => "domain-literal",
        C::Header => "header",
        C::HeaderLabel => "header-label",
        C::Vt | C::Wst | C::EwWs | C::DefectItem => "",
    };
    Tok { c, tt, s: String::new(), k, d: vec![], sep: false }
}
fn dot() -> Tok {
    vt(".", "dot")
}
fn list_sep() -> Tok {
    Tok { sep: true, ..vt(",", "list-separator") }
}

/// Raised when a token's attribute is read on the defect a 3.12 domain-literal holds
fn no_attr(what: &str) -> Exc {
    Exc::attr_error(format!("'InvalidHeaderDefect' object has no attribute '{what}'"))
}

impl Tok {
    pub fn terminal(&self) -> bool {
        matches!(self.c, C::Vt | C::Wst | C::EwWs | C::DefectItem)
    }
    pub fn is_list(&self) -> bool {
        !self.terminal()
    }
    fn ty(&self) -> R<&'static str> {
        if self.c == C::DefectItem {
            return Err(no_attr("token_type"));
        }
        Ok(self.tt)
    }
    fn is(&self, tt: &str) -> bool {
        self.c != C::DefectItem && self.tt == tt
    }

    pub fn str_(&self) -> String {
        match self.c {
            C::Vt | C::Wst | C::DefectItem | C::Literal => self.s.clone(),
            C::EwWs => String::new(),
            C::BareQuotedString => quote_string(&self.k.iter().map(Tok::str_).collect::<String>()),
            C::Comment => {
                let mut out = String::from("(");
                for x in &self.k {
                    if x.is("comment") {
                        out += &x.str_();
                    } else {
                        out += &x.str_().replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)");
                    }
                }
                out + ")"
            }
            _ => self.k.iter().map(Tok::str_).collect(),
        }
    }

    /// TokenList.value: the non-empty values of the children
    fn list_value(&self) -> R<String> {
        let mut out = String::new();
        for x in &self.k {
            out += &x.value()?;
        }
        Ok(out)
    }

    pub fn value(&self) -> R<String> {
        Ok(match self.c {
            C::DefectItem => return Err(no_attr("value")),
            C::Vt | C::Literal => self.s.clone(),
            C::Wst => " ".into(),
            C::EwWs => String::new(),
            C::Cfws | C::Comment => " ".into(),
            C::BareQuotedString => self.k.iter().map(Tok::str_).collect(),
            C::DisplayName => {
                let quote = !self.d.is_empty() || self.k.iter().any(|x| x.is("quoted-string"));
                if !self.k.is_empty() && quote {
                    let pre = if self.k[0].is("cfws") || self.k[0].is_list() && self.k[0].k.first().is_some_and(|y| y.is("cfws")) {
                        " "
                    } else {
                        ""
                    };
                    let last = self.k.last().unwrap();
                    let post = if last.is("cfws") || last.is_list() && last.k.last().is_some_and(|y| y.is("cfws")) { " " } else { "" };
                    format!("{pre}{}{post}", quote_string(&self.display_name()?.unwrap_or_default()))
                } else {
                    self.list_value()?
                }
            }
            C::LocalPart => {
                let f = &self.k[0];
                if f.is("quoted-string") {
                    // QuotedString.quoted_value
                    let mut out = String::new();
                    for x in &f.k {
                        out += &if x.is("bare-quoted-string") { x.str_() } else { x.value()? };
                    }
                    out
                } else {
                    f.value()?
                }
            }
            C::AddrSpec => {
                if self.k.len() < 3 {
                    self.k[0].value()?
                } else {
                    format!("{}{}{}", self.k[0].value()?.trim_end_matches(py_space), self.k[1].value()?, py_lstrip(&self.k[2].value()?))
                }
            }
            _ => self.list_value()?,
        })
    }

    pub fn all_defects(&self) -> R<Vec<Dk>> {
        if self.c == C::DefectItem {
            return Err(no_attr("all_defects"));
        }
        let mut out = self.d.clone();
        for x in &self.k {
            out.extend(x.all_defects()?);
        }
        Ok(out)
    }

    pub fn as_ew_allowed(&self, v: &Ver) -> bool {
        if self.sep {
            return false;
        }
        match self.c {
            C::Vt | C::Wst | C::EwWs | C::DefectItem => true,
            C::DotAtomText => true,
            C::Domain | C::AddrSpec | C::ObsLocalPart | C::LocalPart | C::DomainLiteral => false,
            C::Literal => false,
            C::HeaderLabel => v.v315,
            C::AngleAddr if v.v315 => false,
            _ if v.v315 => true,
            _ => self.k.iter().all(|x| x.as_ew_allowed(v)),
        }
    }

    pub fn syntactic_break(&self) -> bool {
        // ParameterizedHeaderValue (Content-Type): its value is rendered as is here
        !self.sep && self.c != C::Literal
    }

    pub fn ew_combine_allowed(&self) -> bool {
        self.c != C::DisplayName
    }

    pub fn startswith_fws(&self, v: &Ver) -> R<bool> {
        match self.c {
            C::Vt | C::Literal => Ok(false),
            C::DefectItem => Err(no_attr("startswith_fws")),
            C::Wst | C::EwWs => Ok(if v.ws_fix { first(&self.s).is_some_and(is_wsp) } else { true }),
            _ => match self.k.first() {
                Some(f) => f.startswith_fws(v),
                None => Err(Exc::msg(&INDEX_ERROR, "list index out of range")),
            },
        }
    }

    fn comments_nonempty(&self) -> bool {
        // CFWSList.comments: its comment children
        self.k.iter().any(|x| x.is("comment"))
    }

    // ------------------------------------------------ address properties (None for an InvalidMailbox)

    pub fn display_name(&self) -> R<Option<String>> {
        Ok(match self.c {
            C::Address => {
                if self.k[0].is("group") {
                    self.k[0].display_name()?
                } else {
                    None
                }
            }
            C::Group => self.k[0].display_name()?,
            C::NameAddr => {
                if self.k.len() == 1 {
                    None
                } else {
                    self.k[0].display_name()?
                }
            }
            C::Mailbox => {
                if self.k[0].is("name-addr") {
                    self.k[0].display_name()?
                } else {
                    None
                }
            }
            C::DisplayName => {
                let mut res: Vec<Tok> = self.k.clone();
                if res.is_empty() {
                    return Ok(Some(String::new()));
                }
                if res[0].is("cfws") {
                    res.remove(0);
                } else if res[0].is_list() && res[0].k.first().is_some_and(|y| y.is("cfws")) {
                    res[0] = tl(C::Tl, res[0].k[1..].to_vec());
                }
                match res.last() {
                    Some(l) if l.is("cfws") => {
                        res.pop();
                    }
                    Some(l) if l.is_list() && l.k.last().is_some_and(|y| y.is("cfws")) => {
                        let n = res.len() - 1;
                        let kk = res[n].k[..res[n].k.len() - 1].to_vec();
                        res[n] = tl(C::Tl, kk);
                    }
                    None => return Err(Exc::msg(&INDEX_ERROR, "list index out of range")),
                    _ => {}
                }
                Some(tl(C::Tl, res).value()?)
            }
            _ => None,
        })
    }

    pub fn local_part(&self) -> R<Option<String>> {
        Ok(match self.c {
            C::NameAddr | C::Mailbox => self.k.last().map(|x| if self.c == C::Mailbox { &self.k[0] } else { x }).unwrap().local_part()?,
            C::AngleAddr => match self.k.iter().find(|x| x.is("addr-spec")) {
                Some(a) => a.local_part()?,
                None => None,
            },
            C::AddrSpec => self.k[0].local_part()?,
            C::LocalPart => {
                // strip whitespace from front, back, and around dots
                let v = ver();
                let mut items: Vec<Tok> = if self.k[0].is_list() { self.k[0].k.clone() } else { vec![self.k[0].clone()] };
                items.push(dot());
                let mut res: Vec<Tok> = vec![dot()];
                let mut last_is_tl = false;
                for tok in items {
                    if tok.is("cfws") {
                        continue;
                    }
                    let last = res.last().unwrap().clone();
                    let last_nonempty = !last.terminal() && !last.k.is_empty() || last.terminal() && !last.s.is_empty();
                    if last_is_tl && tok.is("dot") && (!v.addr_fix || last_nonempty) && last.k.last().is_some_and(|y| y.is("cfws")) {
                        let n = res.len() - 1;
                        res[n] = tl(C::Tl, last.k[..last.k.len() - 1].to_vec());
                    } else if last_is_tl && tok.is("dot") && !v.addr_fix && last.is_list() && last.k.is_empty() {
                        return Err(Exc::msg(&INDEX_ERROR, "list index out of range"));
                    }
                    let is_tl = tok.is_list();
                    let last = res.last().unwrap().clone();
                    let tok_nonempty = !tok.terminal() && !tok.k.is_empty();
                    if is_tl && last.is("dot") && (!v.addr_fix || tok_nonempty) && tok.k.first().is_some_and(|y| y.is("cfws")) {
                        res.push(tl(C::Tl, tok.k[1..].to_vec()));
                    } else if is_tl && last.is("dot") && !v.addr_fix && tok.k.is_empty() {
                        return Err(Exc::msg(&INDEX_ERROR, "list index out of range"));
                    } else {
                        res.push(tok);
                    }
                    last_is_tl = is_tl;
                }
                let inner = res[1..res.len() - 1].to_vec();
                Some(tl(C::Tl, inner).value()?)
            }
            _ => None,
        })
    }

    pub fn domain(&self) -> R<Option<String>> {
        Ok(match self.c {
            C::NameAddr => self.k.last().unwrap().domain()?,
            C::Mailbox => self.k[0].domain()?,
            C::AngleAddr => match self.k.iter().find(|x| x.is("addr-spec")) {
                Some(a) => a.domain()?,
                None => None,
            },
            C::AddrSpec => {
                if self.k.len() < 3 {
                    None
                } else {
                    self.k.last().unwrap().domain()?
                }
            }
            C::Domain | C::DomainLiteral => Some(no_ws(&self.list_value()?)),
            _ => None,
        })
    }

    /// AddressList.addresses / Address.all_mailboxes
    pub fn addresses(&self) -> Vec<&Tok> {
        self.k.iter().filter(|x| x.is("address")).collect()
    }

    pub fn all_mailboxes(&self) -> Vec<&Tok> {
        match self.c {
            C::Address => {
                let f = &self.k[0];
                if f.is("mailbox") || f.is("invalid-mailbox") {
                    vec![f]
                } else {
                    f.all_mailboxes()
                }
            }
            C::Group => {
                if !self.k[2].is("group-list") {
                    vec![]
                } else {
                    self.k[2].all_mailboxes()
                }
            }
            C::GroupList => match self.k.first() {
                Some(f) if f.is("mailbox-list") => f.all_mailboxes(),
                _ => vec![],
            },
            C::MailboxList => self.k.iter().filter(|x| x.is("mailbox") || x.is("invalid-mailbox")).collect(),
            _ => vec![],
        }
    }
}

// ---------------------------------------------------------------- parser

/// HeaderParseError; `ew` for _InvalidEwError
#[derive(Debug)]
pub struct PErr {
    ew: bool,
}
type P<'a> = Result<(Tok, &'a str), PErr>;

fn perr() -> PErr {
    PErr { ew: false }
}

/// errors that are not HeaderParseError (an unsupported encoded word charset)
pub enum Fail {
    Parse(PErr),
    Rt(Exc),
}
impl From<PErr> for Fail {
    fn from(e: PErr) -> Self {
        Fail::Parse(e)
    }
}
impl From<Exc> for Fail {
    fn from(e: Exc) -> Self {
        Fail::Rt(e)
    }
}
type PR<'a> = Result<(Tok, &'a str), Fail>;

fn pe<T>() -> Result<T, Fail> {
    Err(Fail::Parse(perr()))
}

/// `_get_ptext_to_endchars`: (unquoted text, remaining value, had quoted pairs)
fn ptext_to_endchars<'a>(value: &'a str, endchars: &str) -> (String, &'a str, bool) {
    let (fragment, remainder) = wsp_split(value);
    let mut vchars = String::new();
    let mut escape = false;
    let mut had_qp = false;
    let mut stop = fragment.len();
    for (pos, c) in fragment.char_indices() {
        if c == '\\' {
            if escape {
                escape = false;
                had_qp = true;
            } else {
                escape = true;
                continue;
            }
        }
        if escape {
            escape = false;
        } else if endchars.contains(c) {
            stop = pos;
            break;
        }
        vchars.push(c);
    }
    // the rest of the fragment, then the blanks and what follows them (contiguous in `value`)
    let _ = remainder;
    (vchars, &value[stop..], had_qp)
}

fn get_fws(value: &str) -> (Tok, &str) {
    let nv = py_lstrip(value);
    (wst(&value[..value.len() - nv.len()], "fws"), nv)
}

/// `_ew.decode(ew)`: (text, charset)
fn ew_decode(ew: &str) -> Result<(String, String), Fail> {
    let parts: Vec<&str> = ew.split('?').collect();
    if parts.len() != 5 {
        return Err(Fail::Parse(PErr { ew: true }));
    }
    let (charset, _lang) = parts[1].split_once('*').unwrap_or((parts[1], ""));
    let cte = parts[2].to_lowercase();
    if !parts[3].is_ascii() {
        return Err(Fail::Parse(PErr { ew: true }));
    }
    let bstring = parts[3].as_bytes();
    let bytes = match cte.as_str() {
        "q" => {
            let b: Vec<u8> = bstring.iter().map(|&c| if c == b'_' { b' ' } else { c }).collect();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'=' && i + 2 < b.len() + 0 && i + 2 <= b.len() - 1 + 0 && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
                    out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap(), 16).unwrap());
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            out
        }
        "b" => b_decode(bstring),
        _ => return Err(Fail::Parse(PErr { ew: true })),
    };
    let text = match super::methods::bytes_decode(&bytes, &[V::str(charset)], &[], "decode") {
        Ok(V::Str(s)) => s.to_string(),
        _ => {
            return Err(Fail::Rt(Exc::type_error(format!(
                "py2axum: an encoded word in charset {charset:?} that is not utf-8, latin-1 or ascii, or that does not decode, is not supported"
            ))))
        }
    };
    Ok((text, charset.to_string()))
}

/// `_ew.decode_b`: the decoded bytes, whatever the defects
fn b_decode(encoded: &[u8]) -> Vec<u8> {
    let pad_err = encoded.len() % 4;
    let mut padded = encoded.to_vec();
    if pad_err > 0 {
        padded.extend_from_slice(&b"==="[..4 - pad_err]);
    }
    if let Ok(b) = super::stdlib::b64dec(&padded, true) {
        return b;
    }
    if let Ok(b) = super::stdlib::b64dec(encoded, false) {
        return b;
    }
    let mut p2 = encoded.to_vec();
    p2.extend_from_slice(b"==");
    if let Ok(b) = super::stdlib::b64dec(&p2, false) {
        return b;
    }
    encoded.to_vec()
}

fn get_encoded_word<'a>(value: &'a str, terminal_type: &'static str) -> PR<'a> {
    if !value.starts_with("=?") {
        return pe();
    }
    let body = &value[2..];
    let (mut tok, mut remainder): (String, &str) = match body.split_once("?=") {
        Some((t, r)) => (t.to_string(), r),
        None => return pe(),
    };
    let r = remainder.as_bytes();
    if r.len() > 1 && r[0].is_ascii_hexdigit() && r[1].is_ascii_hexdigit() && tok.matches('?').count() < 2 {
        let (rest, rem) = match remainder.split_once("?=") {
            Some((a, b)) => (a, b),
            None => (remainder, ""),
        };
        tok = format!("{tok}?={rest}");
        remainder = rem;
    }
    let mut ew = tl(C::EncodedWord, vec![]);
    if tok.split(py_space).filter(|x| !x.is_empty()).count() > 1 {
        ew.d.push(Dk::Invalid);
    }
    let (text, _charset) = ew_decode(&format!("=?{tok}?="))?;
    let mut text: &str = &text;
    let owned = text.to_string();
    text = &owned;
    while !text.is_empty() {
        if starts(text, is_wsp) {
            let (t, rest) = get_fws(text);
            ew.k.push(t);
            text = rest;
            continue;
        }
        let (chars, rest) = wsp_split(text);
        ew.k.push(vt(chars, terminal_type));
        text = rest;
    }
    if !remainder.is_empty() && !starts(remainder, is_wsp) {
        ew.d.push(Dk::Invalid);
    }
    Ok((ew, remainder))
}

pub fn get_unstructured(mut value: &str) -> Result<Tok, Exc> {
    let mut u = tl(C::Unstructured, vec![]);
    while !value.is_empty() {
        if starts(value, is_wsp) {
            let (t, rest) = get_fws(value);
            u.k.push(t);
            value = rest;
            continue;
        }
        let mut valid_ew = true;
        if value.starts_with("=?") {
            match get_encoded_word(value, "utext") {
                Err(Fail::Parse(e)) => {
                    if e.ew {
                        valid_ew = false;
                    }
                }
                Err(Fail::Rt(e)) => return Err(e),
                Ok((token, rest)) => {
                    let mut have_ws = true;
                    if !u.k.is_empty() && !u.k.last().unwrap().is("fws") {
                        u.d.push(Dk::Invalid);
                        have_ws = false;
                    }
                    if have_ws && u.k.len() > 1 && u.k[u.k.len() - 2].is("encoded-word") {
                        let n = u.k.len() - 1;
                        u.k[n] = ewws(u.k[n].s.clone());
                    }
                    u.k.push(token);
                    value = rest;
                    continue;
                }
            }
        }
        let (mut tok, mut rest) = wsp_split(value);
        if valid_ew && rfc2047_search(tok) {
            let i = value.find("=?").unwrap();
            tok = &value[..i];
            rest = &value[i + 2..];
            // `tok, *remainder = value.partition('=?')`: the remainder keeps the separator
            let sep_rest = &value[i..];
            u.k.push(vt(tok, "utext"));
            let _ = rest;
            value = sep_rest;
            continue;
        }
        u.k.push(vt(tok, "utext"));
        value = rest;
    }
    Ok(u)
}

fn get_qp_ctext(value: &str) -> (Tok, &str) {
    let (p, rest, _) = ptext_to_endchars(value, "()");
    (wst(&p, "ptext"), rest)
}

fn get_qcontent(value: &str) -> (Tok, &str) {
    let (p, rest, _) = ptext_to_endchars(value, "\"");
    (vt(&p, "ptext"), rest)
}

fn get_atext(value: &str) -> PR<'_> {
    let end = value.find(atom_end).unwrap_or(value.len());
    if end == 0 {
        return pe();
    }
    Ok((vt(&value[..end], "atext"), &value[end..]))
}

fn get_bare_quoted_string(value: &str) -> PR<'_> {
    if !value.starts_with('"') {
        return pe();
    }
    let v = ver();
    let mut bqs = tl(C::BareQuotedString, vec![]);
    let mut value = &value[1..];
    if value.starts_with('"') {
        if v.addr_fix {
            return Ok((bqs, &value[1..]));
        }
        let (t, rest) = get_qcontent(value);
        bqs.k.push(t);
        value = rest;
    }
    while !value.is_empty() && !value.starts_with('"') {
        let token;
        if starts(value, is_wsp) {
            let (t, rest) = get_fws(value);
            token = t;
            value = rest;
        } else if value.starts_with("=?") {
            let mut valid_ew = false;
            match get_encoded_word(value, "vtext") {
                Ok((t, rest)) => {
                    bqs.d.push(Dk::Invalid);
                    valid_ew = true;
                    token = t;
                    value = rest;
                }
                Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
                Err(Fail::Parse(_)) => {
                    let (t, rest) = get_qcontent(value);
                    token = t;
                    value = rest;
                }
            }
            if valid_ew && bqs.k.len() > 1 {
                let n = bqs.k.len();
                if bqs.k[n - 1].is("fws") && bqs.k[n - 2].is("encoded-word") {
                    bqs.k[n - 1] = ewws(bqs.k[n - 1].s.clone());
                }
            }
        } else {
            let (t, rest) = get_qcontent(value);
            token = t;
            value = rest;
        }
        bqs.k.push(token);
    }
    if value.is_empty() {
        bqs.d.push(Dk::Invalid);
        return Ok((bqs, value));
    }
    Ok((bqs, &value[1..]))
}

fn get_comment(value: &str) -> PR<'_> {
    if !value.is_empty() && !value.starts_with('(') {
        return pe();
    }
    let mut comment = tl(C::Comment, vec![]);
    let mut value = rest1(value);
    while !value.is_empty() && !value.starts_with(')') {
        let (t, rest) = if starts(value, is_wsp) {
            get_fws(value)
        } else if value.starts_with('(') {
            get_comment(value)?
        } else {
            get_qp_ctext(value)
        };
        comment.k.push(t);
        value = rest;
    }
    if value.is_empty() {
        comment.d.push(Dk::Invalid);
        return Ok((comment, value));
    }
    Ok((comment, &value[1..]))
}

fn get_cfws(mut value: &str) -> PR<'_> {
    let mut cfws = tl(C::Cfws, vec![]);
    while starts(value, cfws_leader) {
        let (t, rest) = if starts(value, is_wsp) { get_fws(value) } else { get_comment(value)? };
        cfws.k.push(t);
        value = rest;
    }
    Ok((cfws, value))
}

fn get_quoted_string(mut value: &str) -> PR<'_> {
    let mut qs = tl(C::QuotedString, vec![]);
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        qs.k.push(t);
        value = rest;
    }
    let (t, rest) = get_bare_quoted_string(value)?;
    qs.k.push(t);
    value = rest;
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        qs.k.push(t);
        value = rest;
    }
    Ok((qs, value))
}

fn get_atom(mut value: &str) -> PR<'_> {
    let mut atom = tl(C::Atom, vec![]);
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        atom.k.push(t);
        value = rest;
    }
    if starts(value, atom_end) {
        return pe();
    }
    let (t, rest) = if value.starts_with("=?") {
        match get_encoded_word(value, "vtext") {
            Ok(x) => x,
            Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
            Err(Fail::Parse(_)) => get_atext(value)?,
        }
    } else {
        get_atext(value)?
    };
    atom.k.push(t);
    value = rest;
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        atom.k.push(t);
        value = rest;
    }
    Ok((atom, value))
}

fn get_dot_atom_text(mut value: &str) -> PR<'_> {
    let mut dat = tl(C::DotAtomText, vec![]);
    if value.is_empty() || starts(value, atom_end) {
        return pe();
    }
    while !value.is_empty() && !starts(value, atom_end) {
        let (t, rest) = get_atext(value)?;
        dat.k.push(t);
        value = rest;
        if value.starts_with('.') {
            dat.k.push(dot());
            value = &value[1..];
        }
    }
    if dat.k.last().is_some_and(|x| x.is("dot")) {
        return pe();
    }
    Ok((dat, value))
}

fn get_dot_atom(mut value: &str) -> PR<'_> {
    let mut da = tl(C::DotAtom, vec![]);
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        da.k.push(t);
        value = rest;
    } else if value.is_empty() {
        return Err(Fail::Rt(Exc::msg(&INDEX_ERROR, "string index out of range")));
    }
    let (t, rest) = if value.starts_with("=?") {
        match get_encoded_word(value, "vtext") {
            Ok(x) => x,
            Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
            Err(Fail::Parse(_)) => get_dot_atom_text(value)?,
        }
    } else {
        get_dot_atom_text(value)?
    };
    da.k.push(t);
    value = rest;
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        da.k.push(t);
        value = rest;
    }
    Ok((da, value))
}

fn get_word(value: &str) -> PR<'_> {
    let (leader, value) = if starts(value, cfws_leader) {
        let (l, rest) = get_cfws(value)?;
        (Some(l), rest)
    } else if value.is_empty() {
        return Err(Fail::Rt(Exc::msg(&INDEX_ERROR, "string index out of range")));
    } else {
        (None, value)
    };
    if value.is_empty() {
        return pe();
    }
    let (mut token, value) = if value.starts_with('"') {
        get_quoted_string(value)?
    } else if starts(value, special) {
        return pe();
    } else {
        get_atom(value)?
    };
    if let Some(l) = leader {
        token.k.insert(0, l);
    }
    Ok((token, value))
}

fn get_phrase(value: &str) -> PR<'_> {
    let v = ver();
    let mut phrase = tl(C::Phrase, vec![]);
    let mut value = value;
    match get_word(value) {
        Ok((t, rest)) => {
            phrase.k.push(t);
            value = rest;
        }
        Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
        Err(Fail::Parse(_)) => phrase.d.push(Dk::Invalid),
    }
    while !value.is_empty() && !starts(value, phrase_end) {
        if value.starts_with('.') {
            phrase.k.push(dot());
            phrase.d.push(Dk::Other);
            value = &value[1..];
        } else {
            let token = match get_word(value) {
                Ok((t, rest)) => {
                    value = rest;
                    if v.ew_phrase && t.k.first().is_some_and(|x| x.is("encoded-word")) {
                        if let Some(last) = phrase.k.last() {
                            let n = last.k.len();
                            if last.is("atom") && n > 1 && last.k[n - 2].is("encoded-word") && last.k[n - 1].is("cfws") && !last.k[n - 1].comments_nonempty() {
                                let content = last.str_();
                                let li = phrase.k.len() - 1;
                                phrase.k[li].k[n - 1] = ewws(content);
                            }
                        }
                    }
                    t
                }
                Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
                Err(Fail::Parse(e)) => {
                    if starts(value, cfws_leader) {
                        let (t, rest) = get_cfws(value)?;
                        phrase.d.push(Dk::Other);
                        value = rest;
                        t
                    } else {
                        return Err(Fail::Parse(e));
                    }
                }
            };
            phrase.k.push(token);
        }
    }
    Ok((phrase, value))
}

fn get_local_part(value: &str) -> PR<'_> {
    let v = ver();
    let mut lp = tl(C::LocalPart, vec![]);
    let (leader, value) = if starts(value, cfws_leader) {
        let (l, rest) = get_cfws(value)?;
        (Some(l), rest)
    } else {
        (None, value)
    };
    if value.is_empty() {
        return pe();
    }
    let (mut token, mut value) = match get_dot_atom(value) {
        Ok(x) => x,
        Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
        Err(Fail::Parse(_)) => match get_word(value) {
            Ok(x) => x,
            Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
            Err(Fail::Parse(e)) => {
                let c = first(value).unwrap();
                if c != '\\' && phrase_end(c) {
                    return Err(Fail::Parse(e));
                }
                (tl(C::Tl, vec![]), value)
            }
        },
    };
    if let Some(l) = leader {
        token.k.insert(0, l);
    }
    lp.k.push(token);
    if !value.is_empty() && (value.starts_with('\\') || !starts(value, phrase_end)) {
        let joined = format!("{}{}", lp.str_(), value);
        let (obs, rest) = get_obs_local_part(&joined)?;
        // `rest` is a suffix of `joined`, hence of `value`
        let consumed = joined.len() - rest.len();
        let consumed_in_value = consumed - (joined.len() - value.len());
        if obs.is("invalid-obs-local-part") {
            lp.d.push(Dk::Invalid);
        } else {
            lp.d.push(Dk::Other);
        }
        lp.k[0] = obs;
        value = &value[consumed_in_value..];
    }
    if !v.v315 && !lp.value()?.is_ascii() {
        lp.d.push(Dk::Other);
    }
    Ok((lp, value))
}

fn get_obs_local_part(value: &str) -> PR<'_> {
    let mut obs = tl(C::ObsLocalPart, vec![]);
    let mut last_non_ws_was_dot = false;
    let mut value = value;
    while !value.is_empty() && (value.starts_with('\\') || !starts(value, phrase_end)) {
        if value.starts_with('.') {
            if last_non_ws_was_dot {
                obs.d.push(Dk::Invalid);
            }
            obs.k.push(dot());
            last_non_ws_was_dot = true;
            value = &value[1..];
            continue;
        } else if value.starts_with('\\') {
            obs.k.push(vt("\\", "misplaced-special"));
            value = &value[1..];
            obs.d.push(Dk::Invalid);
            last_non_ws_was_dot = false;
            continue;
        }
        if obs.k.last().is_some_and(|x| !x.is("dot")) {
            obs.d.push(Dk::Invalid);
        }
        let token = match get_word(value) {
            Ok((t, rest)) => {
                last_non_ws_was_dot = false;
                value = rest;
                t
            }
            Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
            Err(Fail::Parse(e)) => {
                if !starts(value, cfws_leader) {
                    return Err(Fail::Parse(e));
                }
                let (t, rest) = get_cfws(value)?;
                value = rest;
                t
            }
        };
        obs.k.push(token);
    }
    if obs.k.is_empty() {
        return pe();
    }
    let n = obs.k.len();
    if obs.k[0].is("dot") || obs.k[0].is("cfws") && n > 1 && obs.k[1].is("dot") {
        obs.d.push(Dk::Invalid);
    }
    if obs.k[n - 1].is("dot") || obs.k[n - 1].is("cfws") && n > 1 && obs.k[n - 2].is("dot") {
        obs.d.push(Dk::Invalid);
    }
    if !obs.d.is_empty() {
        obs.tt = "invalid-obs-local-part";
    }
    Ok((obs, value))
}

fn get_dtext(value: &str) -> (Tok, &str) {
    let (p, rest, had_qp) = ptext_to_endchars(value, "[]");
    let mut t = vt(&p, "ptext");
    if had_qp {
        t.d.push(Dk::Other);
    }
    (t, rest)
}

fn get_domain_literal(mut value: &str) -> PR<'_> {
    let v = ver();
    let mut dl = tl(C::DomainLiteral, vec![]);
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        dl.k.push(t);
        value = rest;
    } else if value.is_empty() {
        return Err(Fail::Rt(Exc::msg(&INDEX_ERROR, "string index out of range")));
    }
    if value.is_empty() || !value.starts_with('[') {
        return pe();
    }
    value = &value[1..];
    let early_end = |dl: &mut Tok, value: &str| -> bool {
        if !value.is_empty() {
            return false;
        }
        if v.dl_bug {
            dl.k.push(Tok { c: C::DefectItem, s: "end of input inside domain-literal".into(), ..tl(C::Tl, vec![]) });
        } else {
            dl.d.push(Dk::Invalid);
        }
        dl.k.push(vt("]", "domain-literal-end"));
        true
    };
    if !v.dl_bug {
        dl.k.push(vt("[", "domain-literal-start"));
    }
    if early_end(&mut dl, value) {
        return Ok((dl, value));
    }
    if v.dl_bug {
        dl.k.push(vt("[", "domain-literal-start"));
    }
    if starts(value, is_wsp) {
        let (t, rest) = get_fws(value);
        dl.k.push(t);
        value = rest;
    }
    let (t, rest) = get_dtext(value);
    dl.k.push(t);
    value = rest;
    if early_end(&mut dl, value) {
        return Ok((dl, value));
    }
    if starts(value, is_wsp) {
        let (t, rest) = get_fws(value);
        dl.k.push(t);
        value = rest;
    }
    if early_end(&mut dl, value) {
        return Ok((dl, value));
    }
    if !value.starts_with(']') {
        return pe();
    }
    dl.k.push(vt("]", "domain-literal-end"));
    value = &value[1..];
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        dl.k.push(t);
        value = rest;
    }
    Ok((dl, value))
}

fn get_domain(value: &str) -> PR<'_> {
    let mut domain = tl(C::Domain, vec![]);
    let (leader, mut value) = if starts(value, cfws_leader) {
        let (l, rest) = get_cfws(value)?;
        (Some(l), rest)
    } else {
        (None, value)
    };
    if value.is_empty() {
        return pe();
    }
    if value.starts_with('[') {
        let (mut t, rest) = get_domain_literal(value)?;
        if let Some(l) = leader {
            t.k.insert(0, l);
        }
        domain.k.push(t);
        return Ok((domain, rest));
    }
    let (mut t, rest) = match get_dot_atom(value) {
        Ok(x) => x,
        Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
        Err(Fail::Parse(_)) => get_atom(value)?,
    };
    value = rest;
    if value.starts_with('@') {
        return pe();
    }
    if let Some(l) = leader {
        t.k.insert(0, l);
    }
    domain.k.push(t);
    if value.starts_with('.') {
        domain.d.push(Dk::Other);
        if domain.k[0].is("dot-atom") {
            let inner = domain.k.remove(0);
            domain.k = inner.k;
        }
        while value.starts_with('.') {
            domain.k.push(dot());
            let (t, rest) = get_atom(&value[1..])?;
            domain.k.push(t);
            value = rest;
        }
    }
    Ok((domain, value))
}

fn get_addr_spec(value: &str) -> PR<'_> {
    let mut a = tl(C::AddrSpec, vec![]);
    let (t, value) = get_local_part(value)?;
    a.k.push(t);
    if !value.starts_with('@') {
        a.d.push(Dk::Invalid);
        return Ok((a, value));
    }
    a.k.push(vt("@", "address-at-symbol"));
    let (t, value) = get_domain(&value[1..])?;
    a.k.push(t);
    Ok((a, value))
}

fn get_obs_route(mut value: &str) -> PR<'_> {
    let mut r = tl(C::ObsRoute, vec![]);
    while !value.is_empty() && (value.starts_with(',') || starts(value, cfws_leader)) {
        if starts(value, cfws_leader) {
            let (t, rest) = get_cfws(value)?;
            r.k.push(t);
            value = rest;
        } else {
            r.k.push(list_sep());
            value = &value[1..];
        }
    }
    if !value.starts_with('@') {
        return pe();
    }
    r.k.push(vt("@", "route-component-marker"));
    let (t, rest) = get_domain(&value[1..])?;
    r.k.push(t);
    value = rest;
    while value.starts_with(',') {
        r.k.push(list_sep());
        value = &value[1..];
        if value.is_empty() {
            break;
        }
        if starts(value, cfws_leader) {
            let (t, rest) = get_cfws(value)?;
            r.k.push(t);
            value = rest;
        }
        if value.is_empty() {
            break;
        }
        if value.starts_with('@') {
            r.k.push(vt("@", "route-component-marker"));
            let (t, rest) = get_domain(&value[1..])?;
            r.k.push(t);
            value = rest;
        }
    }
    if value.is_empty() || !value.starts_with(':') {
        return pe();
    }
    r.k.push(vt(":", "end-of-obs-route-marker"));
    Ok((r, &value[1..]))
}

fn get_angle_addr(mut value: &str) -> PR<'_> {
    let mut aa = tl(C::AngleAddr, vec![]);
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        aa.k.push(t);
        value = rest;
    }
    if !value.starts_with('<') {
        return pe();
    }
    aa.k.push(vt("<", "angle-addr-start"));
    value = &value[1..];
    if value.starts_with('>') {
        aa.k.push(vt(">", "angle-addr-end"));
        aa.d.push(Dk::Invalid);
        return Ok((aa, &value[1..]));
    }
    let (t, rest) = match get_addr_spec(value) {
        Ok(x) => x,
        Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
        Err(Fail::Parse(_)) => {
            let (route, rest) = match get_obs_route(value) {
                Ok(x) => x,
                Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
                Err(Fail::Parse(_)) => return pe(),
            };
            aa.d.push(Dk::Other);
            aa.k.push(route);
            get_addr_spec(rest)?
        }
    };
    aa.k.push(t);
    value = rest;
    if value.starts_with('>') {
        value = &value[1..];
    } else {
        aa.d.push(Dk::Invalid);
    }
    aa.k.push(vt(">", "angle-addr-end"));
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        aa.k.push(t);
        value = rest;
    }
    Ok((aa, value))
}

fn get_display_name(value: &str) -> PR<'_> {
    let (phrase, rest) = get_phrase(value)?;
    let mut dn = tl(C::DisplayName, phrase.k);
    dn.d = phrase.d;
    Ok((dn, rest))
}

fn get_name_addr(value: &str) -> PR<'_> {
    let mut na = tl(C::NameAddr, vec![]);
    if value.is_empty() {
        return pe();
    }
    let (mut leader, mut value) = (None, value);
    if starts(value, cfws_leader) {
        let (l, rest) = get_cfws(value)?;
        if rest.is_empty() {
            return pe();
        }
        leader = Some(l);
        value = rest;
    }
    if !value.starts_with('<') {
        if starts(value, phrase_end) {
            return pe();
        }
        let (mut token, rest) = get_display_name(value)?;
        value = rest;
        if value.is_empty() {
            return pe();
        }
        if let Some(l) = leader.take() {
            match token.k.first_mut() {
                Some(f) if f.is_list() => f.k.insert(0, l),
                _ => token.k.insert(0, l),
            }
        }
        na.k.push(token);
    }
    let (mut token, rest) = get_angle_addr(value)?;
    if let Some(l) = leader {
        token.k.insert(0, l);
    }
    na.k.push(token);
    Ok((na, rest))
}

fn get_mailbox(value: &str) -> PR<'_> {
    let mut mb = tl(C::Mailbox, vec![]);
    let (token, rest) = match get_name_addr(value) {
        Ok(x) => x,
        Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
        Err(Fail::Parse(_)) => match get_addr_spec(value) {
            Ok(x) => x,
            Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
            Err(Fail::Parse(_)) => return pe(),
        },
    };
    if token.all_defects().map_err(Fail::Rt)?.contains(&Dk::Invalid) {
        mb.tt = "invalid-mailbox";
    }
    mb.k.push(token);
    Ok((mb, rest))
}

fn get_invalid_mailbox<'a>(mut value: &'a str, endchars: &str) -> PR<'a> {
    let mut im = tl(C::InvalidMailbox, vec![]);
    while !value.is_empty() && !starts(value, |c| endchars.contains(c)) {
        if starts(value, phrase_end) {
            let c = first(value).unwrap();
            im.k.push(vt(&c.to_string(), "misplaced-special"));
            value = rest1(value);
        } else {
            let (t, rest) = get_phrase(value)?;
            im.k.push(t);
            value = rest;
        }
    }
    Ok((im, value))
}

fn get_mailbox_list(mut value: &str) -> PR<'_> {
    let mut ml = tl(C::MailboxList, vec![]);
    while !value.is_empty() && !value.starts_with(';') {
        match get_mailbox(value) {
            Ok((t, rest)) => {
                ml.k.push(t);
                value = rest;
            }
            Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
            Err(Fail::Parse(_)) => {
                if starts(value, cfws_leader) {
                    let (leader, rest) = get_cfws(value)?;
                    value = rest;
                    if value.is_empty() || starts(value, |c| c == ',' || c == ';') {
                        ml.k.push(leader);
                        ml.d.push(Dk::Other);
                    } else {
                        let (mut t, rest) = get_invalid_mailbox(value, ",;")?;
                        value = rest;
                        t.k.insert(0, leader);
                        ml.k.push(t);
                        ml.d.push(Dk::Invalid);
                    }
                } else if value.starts_with(',') {
                    ml.d.push(Dk::Other);
                } else {
                    let (t, rest) = get_invalid_mailbox(value, ",;")?;
                    value = rest;
                    ml.k.push(t);
                    ml.d.push(Dk::Invalid);
                }
            }
        }
        if !value.is_empty() && !starts(value, |c| c == ',' || c == ';') {
            let (t, rest) = get_invalid_mailbox(value, ",;")?;
            value = rest;
            match ml.k.last_mut() {
                Some(mb) => {
                    mb.tt = "invalid-mailbox";
                    mb.k.extend(t.k);
                }
                None => return Err(Fail::Rt(Exc::msg(&INDEX_ERROR, "list index out of range"))),
            }
            ml.d.push(Dk::Invalid);
        }
        if value.starts_with(',') {
            ml.k.push(list_sep());
            value = &value[1..];
        }
    }
    Ok((ml, value))
}

fn get_group_list(value: &str) -> PR<'_> {
    let mut gl = tl(C::GroupList, vec![]);
    if value.is_empty() {
        gl.d.push(Dk::Invalid);
        return Ok((gl, value));
    }
    let mut leader = None;
    let mut value = value;
    if starts(value, cfws_leader) {
        let (l, rest) = get_cfws(value)?;
        value = rest;
        if value.is_empty() {
            gl.d.push(Dk::Invalid);
            gl.k.push(l);
            return Ok((gl, value));
        }
        if value.starts_with(';') {
            gl.k.push(l);
            return Ok((gl, value));
        }
        leader = Some(l);
    }
    let (mut token, rest) = get_mailbox_list(value)?;
    if token.all_mailboxes().is_empty() {
        if let Some(l) = leader {
            gl.k.push(l);
        }
        gl.k.extend(token.k);
        gl.d.push(Dk::Other);
        return Ok((gl, rest));
    }
    if let Some(l) = leader {
        token.k.insert(0, l);
    }
    gl.k.push(token);
    Ok((gl, rest))
}

fn get_group(value: &str) -> PR<'_> {
    let mut g = tl(C::Group, vec![]);
    let (t, value) = get_display_name(value)?;
    if !value.starts_with(':') {
        return pe();
    }
    g.k.push(t);
    g.k.push(vt(":", "group-display-name-terminator"));
    let value = &value[1..];
    if value.starts_with(';') {
        g.k.push(vt(";", "group-terminator"));
        return Ok((g, &value[1..]));
    }
    let (t, value) = get_group_list(value)?;
    g.k.push(t);
    if value.is_empty() {
        g.d.push(Dk::Invalid);
    } else if !value.starts_with(';') {
        return pe();
    }
    g.k.push(vt(";", "group-terminator"));
    let mut value = from(value, 1);
    if starts(value, cfws_leader) {
        let (t, rest) = get_cfws(value)?;
        g.k.push(t);
        value = rest;
    }
    Ok((g, value))
}

fn get_address(value: &str) -> PR<'_> {
    let (t, rest) = match get_group(value) {
        Ok(x) => x,
        Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
        Err(Fail::Parse(_)) => match get_mailbox(value) {
            Ok(x) => x,
            Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
            Err(Fail::Parse(_)) => return pe(),
        },
    };
    Ok((tl(C::Address, vec![t]), rest))
}

pub fn get_address_list(mut value: &str) -> Result<Tok, Exc> {
    let mut run = || -> Result<Tok, Fail> {
        let v = ver();
        let mut al = tl(C::AddressList, vec![]);
        while !value.is_empty() {
            match get_address(value) {
                Ok((t, rest)) => {
                    al.k.push(t);
                    value = rest;
                }
                Err(Fail::Rt(e)) => return Err(Fail::Rt(e)),
                Err(Fail::Parse(_)) => {
                    if starts(value, cfws_leader) {
                        let (leader, rest) = get_cfws(value)?;
                        value = rest;
                        if value.is_empty() || value.starts_with(',') {
                            al.k.push(leader);
                            al.d.push(Dk::Other);
                        } else {
                            let (mut t, rest) = get_invalid_mailbox(value, ",")?;
                            value = rest;
                            t.k.insert(0, leader);
                            al.k.push(tl(C::Address, vec![t]));
                            al.d.push(Dk::Invalid);
                        }
                    } else if value.starts_with(',') {
                        al.d.push(Dk::Other);
                    } else {
                        let (t, rest) = get_invalid_mailbox(value, ",")?;
                        value = rest;
                        al.k.push(tl(C::Address, vec![t]));
                        al.d.push(Dk::Invalid);
                    }
                }
            }
            if !value.is_empty() && !value.starts_with(',') {
                let (t, rest) = get_invalid_mailbox(value, ",")?;
                value = rest;
                if v.addr_fix {
                    al.k.push(tl(C::Address, vec![t]));
                } else {
                    // the previous address's first token turns into an invalid mailbox that keeps its data
                    match al.k.last_mut().and_then(|a| a.k.first_mut()) {
                        Some(mb) => {
                            mb.tt = "invalid-mailbox";
                            mb.k.extend(t.k);
                        }
                        None => return Err(Fail::Rt(Exc::msg(&INDEX_ERROR, "list index out of range"))),
                    }
                }
                al.d.push(Dk::Invalid);
            }
            if !value.is_empty() {
                al.k.push(list_sep());
                value = rest1(value);
            }
        }
        Ok(al)
    };
    match run() {
        Ok(t) => Ok(t),
        Err(Fail::Rt(e)) => Err(e),
        // never raised by the address-list parser itself (each failure becomes an invalid mailbox)
        Err(Fail::Parse(_)) => Err(Exc::msg(&HEADER_PARSE_ERROR, "address list")),
    }
}

// ---------------------------------------------------------------- folding

fn steal_trailing_wsp(lines: &mut Vec<String>, v: &Ver) -> String {
    let mut wsp = String::new();
    if let Some(last) = lines.last_mut() {
        if last.ends_with(WSP) {
            let c = last.pop().unwrap();
            wsp.push(c);
            if v.pop_empty && last.is_empty() {
                lines.pop();
            }
        }
    }
    wsp
}

/// `_ew.encode(string, charset)`: the shortest of q and b (q unless 5 characters longer)
pub fn ew_encode(s: &str, charset: &str) -> String {
    let b = s.as_bytes();
    let qlen: usize = b.iter().map(|&c| q_byte(c).len()).sum();
    let blen = b.len() / 3 * 4 + if b.len() % 3 > 0 { 4 } else { 0 };
    if qlen as i64 - (blen as i64) < 5 {
        format!("=?{charset}?q?{}?=", b.iter().map(|&c| q_byte(c)).collect::<String>())
    } else {
        use base64::Engine;
        format!("=?{charset}?b?{}?=", base64::engine::general_purpose::STANDARD.encode(b))
    }
}

fn q_byte(c: u8) -> String {
    match c {
        b' ' => "_".into(),
        b'-' | b'!' | b'*' | b'+' | b'/' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => (c as char).to_string(),
        _ => format!("={c:02X}"),
    }
}

pub struct Policy {
    pub linesep: &'static str,
    /// None: no limit (sys.maxsize)
    pub max_line_length: Option<usize>,
    pub utf8: bool,
}

/// `parser.Header([HeaderLabel([name, ':']), CFWSList([' ']) if tree, tree]).fold(policy)`
pub fn fold_header(name: &str, tree: &Tok, policy: &Policy) -> R<String> {
    let mut header = tl(C::Header, vec![tl(C::HeaderLabel, vec![vt(name, "header-name"), vt(":", "header-sep")])]);
    let nonempty = if tree.terminal() || tree.c == C::Literal { !tree.s.is_empty() } else { !tree.k.is_empty() };
    if nonempty {
        header.k.push(tl(C::Cfws, vec![wst(" ", "fws")]));
    }
    header.k.push(tree.clone());
    refold(&header, policy)
}

fn refold(tree: &Tok, policy: &Policy) -> R<String> {
    let v = ver();
    let maxlen = policy.max_line_length.unwrap_or(i64::MAX as usize);
    let encoding = if policy.utf8 { "utf-8" } else { "us-ascii" };
    let mut lines = vec![String::new()];
    if v.v315 {
        if tree.as_ew_allowed(&v) {
            refold_with_ew(tree.k.clone(), &mut lines, maxlen, encoding, policy, &v)?;
        } else {
            refold_without_ew(tree.k.clone(), &mut lines, maxlen, encoding, policy, &v)?;
        }
    } else {
        refold_with_ew(tree.k.clone(), &mut lines, maxlen, encoding, policy, &v)?;
    }
    Ok(lines.join(policy.linesep) + policy.linesep)
}

const NLSET: &[char] = &['\n', '\r'];

fn header_write_error(part: &Tok, tstr: &str) -> Exc {
    Exc::msg(&HEADER_WRITE_ERROR, format!("Non-ASCII {} '{}' is invalid under current policy setting (utf8=False)", part.tt, tstr))
}

/// `_refold_without_ew` (3.15)
fn refold_without_ew(mut parts: Vec<Tok>, lines: &mut Vec<String>, maxlen: usize, encoding: &str, policy: &Policy, v: &Ver) -> R<()> {
    while !parts.is_empty() {
        let part = parts.remove(0);
        let tstr = part.str_();
        if encoding == "us-ascii" && !tstr.is_ascii() {
            if !policy.utf8 {
                return Err(header_write_error(&part, &tstr));
            }
        }
        let ll = clen(lines.last().unwrap());
        if clen(&tstr) <= maxlen.saturating_sub(ll) {
            lines.last_mut().unwrap().push_str(&tstr);
            continue;
        }
        if part.syntactic_break() && clen(&tstr) + 1 <= maxlen {
            let newline = steal_trailing_wsp(lines, v);
            if !newline.is_empty() || part.startswith_fws(v)? {
                lines.push(newline + &tstr);
                continue;
            }
        }
        if part.is_list() {
            let mut np = part.k.clone();
            np.extend(parts);
            parts = np;
            continue;
        }
        let newline = steal_trailing_wsp(lines, v);
        if !newline.is_empty() || part.startswith_fws(v)? {
            lines.push(newline + &tstr);
        } else {
            lines.last_mut().unwrap().push_str(&tstr);
        }
    }
    Ok(())
}

/// the marker closing a run of parts that may not be encoded (`end_ew_not_allowed`)
fn end_marker() -> Tok {
    vt("", "wrap_as_ew_blocked")
}

fn refold_with_ew(mut parts: Vec<Tok>, lines: &mut Vec<String>, maxlen: usize, encoding: &str, policy: &Policy, v: &Ver) -> R<()> {
    let mut last_word_is_ew = false;
    let mut leading_whitespace = String::new();
    let mut last_ew: Option<usize> = None;
    let mut last_charset: Option<&'static str> = None;
    let mut wrap_as_ew_blocked = 0i32;
    let mut want_encoding = false;
    while !parts.is_empty() {
        let part = parts.remove(0);
        if part.tt == "wrap_as_ew_blocked" && part.c == C::Vt {
            wrap_as_ew_blocked -= 1;
            continue;
        }
        let tstr = part.str_();
        let ptt = part.ty()?;
        if !want_encoding {
            if ptt == "ptext" || ptt == "vtext" {
                want_encoding = tstr.chars().any(|c| special(c) || NLSET.contains(&c));
            } else {
                want_encoding = tstr.chars().any(|c| NLSET.contains(&c));
            }
        }
        let charset: &'static str = if encoding == "utf-8" || tstr.is_ascii() {
            if encoding == "utf-8" {
                "utf-8"
            } else {
                "us-ascii"
            }
        } else {
            want_encoding = true;
            "utf-8"
        };
        if want_encoding && wrap_as_ew_blocked == 0 {
            let not_allowed = if v.v315 {
                part.is_list() && part.k.iter().any(|x| !x.as_ew_allowed(v))
            } else {
                !part.as_ew_allowed(v)
            };
            if not_allowed {
                want_encoding = false;
                last_ew = None;
                if part.syntactic_break() {
                    let folded = refold_part(&part, policy, v)?;
                    let encoded_part = &folded[..folded.len() - policy.linesep.len()];
                    if !encoded_part.contains(policy.linesep) {
                        if clen(encoded_part) > maxlen.saturating_sub(clen(lines.last().unwrap())) {
                            let newline = steal_trailing_wsp(lines, v);
                            lines.push(newline);
                        }
                        lines.last_mut().unwrap().push_str(encoded_part);
                        last_word_is_ew = false;
                        continue;
                    }
                }
            }
            if part.is_list() {
                let mut np = part.k.clone();
                np.extend(parts);
                parts = np;
                want_encoding = false;
                continue;
            } else if part.as_ew_allowed(v) {
                if last_ew.is_some()
                    && Some(charset) != last_charset
                    && (last_charset == Some("unknown-8bit") || last_charset == Some("utf-8") && charset != "us-ascii")
                {
                    last_ew = None;
                }
                last_ew = fold_as_ew(&tstr, lines, maxlen, last_ew, part.ew_combine_allowed(), charset, last_word_is_ew, &mut leading_whitespace, v)?;
                last_word_is_ew = true;
                last_charset = Some(charset);
                want_encoding = false;
                continue;
            } else {
                last_ew = None;
                want_encoding = false;
            }
        }
        let ll = clen(lines.last().unwrap());
        if clen(&tstr) <= maxlen.saturating_sub(ll) {
            lines.last_mut().unwrap().push_str(&tstr);
            last_word_is_ew = last_word_is_ew && tstr.trim_matches(WSP).is_empty();
            continue;
        }
        if !v.last_word_ew {
            leading_whitespace.clear();
        }
        if part.syntactic_break() && clen(&tstr) + 1 <= maxlen {
            let newline = steal_trailing_wsp(lines, v);
            if !newline.is_empty() || part.startswith_fws(v)? {
                lines.push(newline + &tstr);
                if v.last_word_ew {
                    last_word_is_ew = last_word_is_ew && lines.last().unwrap().trim_matches(WSP).is_empty();
                } else {
                    leading_whitespace = lines.last().unwrap().chars().take_while(|c| is_wsp(*c)).collect();
                }
                last_ew = None;
                continue;
            }
        }
        if part.is_list() {
            let mut newparts = part.k.clone();
            if part.is("bare-quoted-string") {
                let mut q = vec![vt("\"", "ptext")];
                q.extend(newparts.iter().map(|p| vt(&p.str_().replace('\\', "\\\\").replace('"', "\\\""), "ptext")));
                q.push(vt("\"", "ptext"));
                if v.v315 {
                    // 3.15: a quoted string is folded without encoded words
                    refold_without_ew(q, lines, maxlen, encoding, policy, v)?;
                    continue;
                }
                newparts = q;
            }
            if v.ws_fix && part.is("comment") {
                let mut q = vec![vt("(", "ptext")];
                q.extend(newparts.into_iter().map(|p| {
                    if p.is("ptext") {
                        vt(&p.str_().replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)"), "ptext")
                    } else {
                        p
                    }
                }));
                q.push(vt(")", "ptext"));
                newparts = q;
            }
            if !part.as_ew_allowed(v) {
                wrap_as_ew_blocked += 1;
                newparts.push(end_marker());
            }
            newparts.extend(parts);
            parts = newparts;
            continue;
        }
        if part.as_ew_allowed(v) && wrap_as_ew_blocked == 0 {
            parts.insert(0, part);
            want_encoding = true;
            continue;
        }
        let newline = steal_trailing_wsp(lines, v);
        if !newline.is_empty() || part.startswith_fws(v)? {
            lines.push(newline + &tstr);
        } else {
            lines.last_mut().unwrap().push_str(&tstr);
        }
        last_word_is_ew = last_word_is_ew && tstr.trim_matches(WSP).is_empty();
    }
    Ok(())
}

/// `part.fold(policy=policy)` of a sub-token (TokenList.fold: `_refold_parse_tree(part)`)
fn refold_part(part: &Tok, policy: &Policy, v: &Ver) -> R<String> {
    let maxlen = policy.max_line_length.unwrap_or(i64::MAX as usize);
    let encoding = if policy.utf8 { "utf-8" } else { "us-ascii" };
    let mut lines = vec![String::new()];
    if v.v315 && !part.as_ew_allowed(v) {
        refold_without_ew(part.k.clone(), &mut lines, maxlen, encoding, policy, v)?;
    } else {
        refold_with_ew(part.k.clone(), &mut lines, maxlen, encoding, policy, v)?;
    }
    Ok(lines.join(policy.linesep) + policy.linesep)
}

#[allow(clippy::too_many_arguments)]
fn fold_as_ew(
    to_encode: &str,
    lines: &mut Vec<String>,
    maxlen: usize,
    last_ew: Option<usize>,
    ew_combine_allowed: bool,
    charset: &str,
    last_word_is_ew: bool,
    leading_whitespace: &mut String,
    v: &Ver,
) -> R<Option<usize>> {
    let mut to_encode: String = to_encode.to_string();
    if let (Some(le), true) = (last_ew, ew_combine_allowed) {
        let last = lines.last().unwrap().clone();
        let tail = from(&last, le).to_string();
        to_encode = get_unstructured(&(tail + &to_encode))?.str_();
        *lines.last_mut().unwrap() = cslice(&last, 0, le);
    } else if v.last_word_ew && last_word_is_ew {
        let last = lines.last().unwrap().clone();
        let without = last.trim_end_matches(WSP);
        let lw = last[without.len()..].to_string();
        *lines.last_mut().unwrap() = format!("{without}{}", if lw.is_empty() { "" } else { " " });
        to_encode = lw + &to_encode;
    } else if to_encode.is_empty() {
        return Err(Exc::msg(&INDEX_ERROR, "string index out of range"));
    } else if to_encode.starts_with(WSP) {
        let lw = to_encode.remove(0);
        if clen(lines.last().unwrap()) == maxlen {
            let s = steal_trailing_wsp(lines, v);
            lines.push(s);
        }
        lines.last_mut().unwrap().push(lw);
    }
    let mut trailing = String::new();
    if to_encode.is_empty() {
        return Err(Exc::msg(&INDEX_ERROR, "string index out of range"));
    }
    if to_encode.ends_with(WSP) {
        trailing.push(to_encode.pop().unwrap());
    }
    let mut new_last_ew = match last_ew {
        None => clen(lines.last().unwrap()),
        Some(le) => le,
    };
    let encode_as = if charset == "us-ascii" { "utf-8" } else { charset };
    let chrome_len = encode_as.len() + 7;
    if chrome_len + 1 >= maxlen {
        return Err(Exc::msg(&HEADER_PARSE_ERROR, "max_line_length is too small to fit an encoded word"));
    }
    while !to_encode.is_empty() {
        let remaining = maxlen as i64 - clen(lines.last().unwrap()) as i64;
        let text_space = remaining - chrome_len as i64 - if v.last_word_ew { 0 } else { clen(leading_whitespace) as i64 };
        if text_space <= 0 {
            if v.last_word_ew {
                let nl = steal_trailing_wsp(lines, v);
                lines.push(if nl.is_empty() { " ".into() } else { nl });
                new_last_ew = clen(lines.last().unwrap());
            } else {
                lines.push(" ".into());
            }
            continue;
        }
        if !v.last_word_ew && lines.len() > 1 && clen(lines.last().unwrap()) == 1 && !leading_whitespace.is_empty() {
            let ew = ew_encode(leading_whitespace, encode_as);
            lines.last_mut().unwrap().push_str(&ew);
            leading_whitespace.clear();
        }
        let mut word = cslice(&to_encode, 0, text_space as usize);
        let mut encoded = ew_encode(&word, encode_as);
        let mut excess = clen(&encoded) as i64 - remaining;
        while excess > 0 {
            word.pop();
            encoded = ew_encode(&word, encode_as);
            excess = clen(&encoded) as i64 - remaining;
        }
        lines.last_mut().unwrap().push_str(&encoded);
        to_encode = from(&to_encode, clen(&word)).to_string();
        if !v.last_word_ew {
            leading_whitespace.clear();
        }
        if !to_encode.is_empty() {
            lines.push(" ".into());
            new_last_ew = clen(lines.last().unwrap());
        }
    }
    lines.last_mut().unwrap().push_str(&trailing);
    if !v.last_word_ew {
        leading_whitespace.clear();
    }
    Ok(if ew_combine_allowed { Some(new_last_ew) } else { None })
}
