//! python-dotenv 1.2: `load_dotenv(dotenv_path, ...)` and `dotenv_values(dotenv_path, ...)`, ported from
//! `dotenv/parser.py` (one function per regular expression, the same resumption after an error: the rest of the
//! line from where the error was found), `dotenv/variables.py` (`${NAME}` / `${NAME:-default}`, resolved in
//! every value) and `dotenv/main.py` (`resolve_variables`, `set_as_environment_variables`,
//! `PYTHON_DOTENV_DISABLED`). A line the library cannot parse is skipped; its warning is not printed (library
//! warnings are not). The path is required: without one python-dotenv searches from the caller's frame
//! (`find_dotenv`), which the binary does not have.
use super::v::*;

/// what changed between python-dotenv's 1.2 releases (the project's locked one, passed by the generated code):
/// 1.2.3 reads a quoted value as `'((?:\\.|[^'\\])*)'` (before: `'((?:\\'|[^'])*)'`, backtracking) and drops a
/// leading BOM; 1.2.4 reads `KEY= # comment` as an empty value (before: the value `# comment`)
#[derive(Clone, Copy)]
pub struct Rev {
    pub escaped_quotes: bool,
    pub bom: bool,
    pub empty_before_comment: bool,
}

impl Rev {
    pub fn of(patch: u32) -> Rev {
        Rev { escaped_quotes: patch >= 3, bom: patch >= 3, empty_before_comment: patch >= 4 }
    }
}

/// Python's `\s` in a str pattern (`str.isspace()`)
fn space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `[^\S\r\n]`
fn inline_space(c: char) -> bool {
    space(c) && c != '\r' && c != '\n'
}

struct Reader {
    s: Vec<char>,
    i: usize,
    rev: Rev,
}

struct ParseError;

impl Reader {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }
    fn at(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }
    fn skip_while(&mut self, f: impl Fn(char) -> bool) -> String {
        let start = self.i;
        while self.peek().is_some_and(&f) {
            self.i += 1;
        }
        self.s[start..self.i].iter().collect()
    }
    /// `_rest_of_line`: `[^\r\n]*(?:\r|\n|\r\n)?` (`\r` first: a CRLF leaves its `\n` to the next binding's whitespace)
    fn rest_of_line(&mut self) {
        self.skip_while(|c| c != '\r' && c != '\n');
        if matches!(self.peek(), Some('\r') | Some('\n')) {
            self.i += 1;
        }
    }
    /// `_end_of_line`: `[^\S\r\n]*(?:\r\n|\n|\r|$)`
    fn end_of_line(&mut self) -> Result<(), ParseError> {
        let save = self.i;
        self.skip_while(inline_space);
        match (self.peek(), self.at(1)) {
            (None, _) => Ok(()),
            (Some('\r'), Some('\n')) => {
                self.i += 2;
                Ok(())
            }
            (Some('\r') | Some('\n'), _) => {
                self.i += 1;
                Ok(())
            }
            _ => {
                self.i = save;
                Err(ParseError)
            }
        }
    }
    /// `_comment`: `(?:[^\S\r\n]*#[^\r\n]*)?`
    fn comment(&mut self) {
        let save = self.i;
        self.skip_while(inline_space);
        if self.peek() == Some('#') {
            self.skip_while(|c| c != '\r' && c != '\n');
        } else {
            self.i = save;
        }
    }
    /// `'((?:\\.|[^'\\])*)'` / `"((?:\\.|[^"\\])*)"` (DOTALL): the raw text between the quotes
    fn quoted(&mut self, q: char) -> Result<String, ParseError> {
        if !self.rev.escaped_quotes {
            return self.quoted_old(q);
        }
        let save = self.i;
        self.i += 1;
        let mut raw = String::new();
        loop {
            match self.peek() {
                None => {
                    self.i = save;
                    return Err(ParseError);
                }
                Some('\\') => match self.at(1) {
                    Some(c) => {
                        raw.push('\\');
                        raw.push(c);
                        self.i += 2;
                    }
                    None => {
                        self.i = save;
                        return Err(ParseError);
                    }
                },
                Some(c) if c == q => {
                    self.i += 1;
                    return Ok(raw);
                }
                Some(c) => {
                    raw.push(c);
                    self.i += 1;
                }
            }
        }
    }
}

impl Reader {
    /// before 1.2.3: `'((?:\\'|[^'])*)'`, with the regular expression engine's backtracking: the greedy loop tries
    /// `\'` then any other character, and leaves only when neither continues to a closing quote. `end[i]`: where
    /// the match ends from position i (memoised from the end: linear)
    fn quoted_old(&mut self, q: char) -> Result<String, ParseError> {
        let start = self.i + 1;
        let s = &self.s;
        let n = s.len();
        let mut end: Vec<Option<usize>> = vec![None; n + 2];
        for i in (start..n).rev() {
            let alt1 = if s[i] == '\\' && s.get(i + 1) == Some(&q) { end[i + 2] } else { None };
            end[i] = alt1.or(if s[i] != q { end[i + 1] } else { None }).or(if s[i] == q { Some(i) } else { None });
        }
        match end.get(start).copied().flatten() {
            Some(close) => {
                let raw: String = s[start..close].iter().collect();
                self.i = close + 1;
                Ok(raw)
            }
            None => Err(ParseError),
        }
    }
}

/// `decode_escapes`: each match of `\\[<set>]` decoded with `unicode-escape`
fn decode_escapes(raw: &str, set: &str) -> String {
    let cs: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\' && cs.get(i + 1).is_some_and(|c| set.contains(*c)) {
            out.push(match cs[i + 1] {
                'a' => '\x07',
                'b' => '\x08',
                'f' => '\x0c',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\x0b',
                c => c,
            });
            i += 2;
        } else {
            out.push(cs[i]);
            i += 1;
        }
    }
    out
}

/// `parse_unquoted_value`: `[^\r\n]*`, then `re.sub(r"\s+#.*", "", part).rstrip()`
fn unquoted_value(r: &mut Reader) -> String {
    let part: Vec<char> = r.skip_while(|c| c != '\r' && c != '\n').chars().collect();
    let mut cut = part.len();
    let mut k = 0;
    while k < part.len() {
        if space(part[k]) {
            let mut j = k;
            while j < part.len() && space(part[j]) {
                j += 1;
            }
            if j < part.len() && part[j] == '#' {
                cut = k;
                break;
            }
            k = j;
        } else {
            k += 1;
        }
    }
    let v: String = part[..cut].iter().collect();
    v.trim_end_matches(space).to_string()
}

fn parse_value(r: &mut Reader) -> Result<String, ParseError> {
    match r.peek() {
        Some('\'') => Ok(decode_escapes(&r.quoted('\'')?, "\\'")),
        Some('"') => Ok(decode_escapes(&r.quoted('"')?, "\\'\"abfnrtv")),
        None | Some('\n') | Some('\r') => Ok(String::new()),
        _ => Ok(unquoted_value(r)),
    }
}

/// `parse_binding`: Ok(None) for a binding without a key (blank, comment, end), Err for a line in error
fn parse_binding(r: &mut Reader) -> Result<Option<(String, Option<String>)>, ParseError> {
    r.skip_while(space);
    if r.peek().is_none() {
        return Ok(None);
    }
    // `_export`: `(?:export[^\S\r\n]+)?`
    let e: String = r.s[r.i..(r.i + 6).min(r.s.len())].iter().collect();
    if e == "export" && r.at(6).is_some_and(inline_space) {
        r.i += 6;
        r.skip_while(inline_space);
    }
    let key = match r.peek() {
        Some('#') => None,
        Some('\'') => {
            // `'([^']+)'`
            let save = r.i;
            r.i += 1;
            let k = r.skip_while(|c| c != '\'');
            if k.is_empty() || r.peek() != Some('\'') {
                r.i = save;
                return Err(ParseError);
            }
            r.i += 1;
            Some(k)
        }
        _ => {
            // `([^=\#\s]+)`
            let k = r.skip_while(|c| c != '=' && c != '#' && !space(c));
            if k.is_empty() {
                return Err(ParseError);
            }
            Some(k)
        }
    };
    r.skip_while(inline_space);
    let value = if r.peek() == Some('=') {
        r.i += 1;
        let ws = r.skip_while(inline_space);
        if r.rev.empty_before_comment && !ws.is_empty() && r.peek() == Some('#') {
            Some(String::new())
        } else {
            Some(parse_value(r)?)
        }
    } else {
        None
    };
    r.comment();
    r.end_of_line()?;
    Ok(key.map(|k| (k, value)))
}

fn parse_stream(text: &str, rev: Rev) -> Vec<(String, Option<String>)> {
    let text = if rev.bom { text.strip_prefix('\u{feff}').unwrap_or(text) } else { text };
    let mut r = Reader { s: text.chars().collect(), i: 0, rev };
    let mut out = Vec::new();
    while r.i < r.s.len() {
        match parse_binding(&mut r) {
            Ok(Some(b)) => out.push(b),
            Ok(None) => {}
            Err(ParseError) => r.rest_of_line(),
        }
    }
    out
}

/// `parse_variables` + `Atom.resolve`: `\$\{(?P<name>[^\}:]*)(?::-(?P<default>[^\}]*))?\}`
fn resolve(value: &str, env: &dyn Fn(&str) -> Option<Option<String>>) -> String {
    let cs: Vec<char> = value.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    'outer: while i < cs.len() {
        if cs[i] == '$' && cs.get(i + 1) == Some(&'{') {
            let mut j = i + 2;
            while j < cs.len() && cs[j] != '}' && cs[j] != ':' {
                j += 1;
            }
            let name: String = cs[i + 2..j].iter().collect();
            let mut default = None;
            let mut k = j;
            if k + 1 < cs.len() && cs[k] == ':' && cs[k + 1] == '-' {
                let mut m = k + 2;
                while m < cs.len() && cs[m] != '}' {
                    m += 1;
                }
                default = Some(cs[k + 2..m].iter().collect::<String>());
                k = m;
            }
            if k < cs.len() && cs[k] == '}' {
                let got = match env(&name) {
                    Some(v) => v,
                    None => default.or(Some(String::new())),
                };
                out.push_str(&got.unwrap_or_default());
                i = k + 1;
                continue 'outer;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// `resolve_variables(values, override)`
fn resolve_variables(values: Vec<(String, Option<String>)>, override_: bool) -> Vec<(String, Option<String>)> {
    let mut new_values: Vec<(String, Option<String>)> = Vec::new();
    for (name, value) in values {
        let result = value.map(|v| {
            let lookup = |n: &str| -> Option<Option<String>> {
                let read = new_values.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
                let os = std::env::var_os(n).map(|v| Some(v.to_string_lossy().into_owned()));
                if override_ { read.or(os) } else { os.or(read) }
            };
            resolve(&v, &lookup)
        });
        dict_set(&mut new_values, name, result);
    }
    new_values
}

/// a dict assignment: a key seen again keeps its first place
fn dict_set(d: &mut Vec<(String, Option<String>)>, k: String, v: Option<String>) {
    match d.iter_mut().find(|(x, _)| *x == k) {
        Some(slot) => slot.1 = v,
        None => d.push((k, v)),
    }
}

fn read(path: Option<&V>, encoding: Option<&V>) -> R<String> {
    match encoding {
        None | Some(V::None) => {}
        Some(V::Str(s)) if matches!(s.to_lowercase().replace('_', "-").as_str(), "utf-8" | "utf8") => {}
        Some(V::Str(s)) => return Err(Exc::type_error(format!("py2axum: dotenv with encoding={s:?} is not supported (UTF-8 only)"))),
        Some(other) => return Err(Exc::type_error(format!("open() argument 'encoding' must be str or None, not {}", other.type_name()))),
    }
    let Some(path) = path else { return Ok(String::new()) };
    if !super::ops::truthy(path)? {
        return Ok(String::new()); // `if self.dotenv_path and ...`
    }
    let p = super::pathio::fspath(path)?;
    // `_is_file_or_fifo`: else an empty stream
    let is_file = std::fs::metadata(&p).map(|m| {
        use std::os::unix::fs::FileTypeExt;
        m.is_file() || m.file_type().is_fifo()
    });
    if !is_file.unwrap_or(false) {
        return Ok(String::new());
    }
    let b = std::fs::read(&p).map_err(|e| super::pathio::os_err(e, &p))?;
    String::from_utf8(b).map_err(|e| {
        let at = e.utf8_error().valid_up_to();
        Exc::value_error(format!("'utf-8' codec can't decode byte at position {at}: invalid start byte"))
    })
}

fn opt<'a>(args: &'a [V], kwargs: &'a [(String, V)], pos: usize, name: &str) -> Option<&'a V> {
    args.get(pos).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v))
}

fn check(fname: &str, args: &[V], kwargs: &[(String, V)], names: &[&str]) -> R<()> {
    if args.len() > names.len() {
        return Err(Exc::type_error(format!("{fname}() takes from 0 to {} positional arguments but {} were given", names.len(), args.len())));
    }
    for (k, _) in kwargs {
        match names.iter().position(|n| n == k) {
            None => return Err(Exc::type_error(format!("{fname}() got an unexpected keyword argument '{k}'"))),
            Some(p) if p < args.len() => return Err(Exc::type_error(format!("{fname}() got multiple values for argument '{k}'"))),
            _ => {}
        }
    }
    let path = opt(args, kwargs, 0, "dotenv_path").filter(|v| !matches!(v, V::None));
    let stream = opt(args, kwargs, 1, "stream").filter(|v| !matches!(v, V::None));
    if stream.is_some() {
        return Err(Exc::type_error(format!("py2axum: {fname}(stream=) is not supported")));
    }
    if path.is_none() {
        return Err(Exc::type_error(format!("py2axum: {fname}() needs dotenv_path (find_dotenv() reads the caller's frame)")));
    }
    Ok(())
}

fn flag(v: Option<&V>, default: bool) -> R<bool> {
    v.map(super::ops::truthy).transpose().map(|b| b.unwrap_or(default))
}

/// `load_dotenv(dotenv_path=None, stream=None, verbose=False, override=False, interpolate=True, encoding="utf-8")`
pub fn load_dotenv(rev: Rev, args: &[V], kwargs: &[(String, V)]) -> R {
    const NAMES: &[&str] = &["dotenv_path", "stream", "verbose", "override", "interpolate", "encoding"];
    check("load_dotenv", args, kwargs, NAMES)?;
    if std::env::var("PYTHON_DOTENV_DISABLED").is_ok_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "t" | "yes" | "y")) {
        return Ok(V::Bool(false));
    }
    let override_ = flag(opt(args, kwargs, 3, "override"), false)?;
    let interp = flag(opt(args, kwargs, 4, "interpolate"), true)?;
    let text = read(opt(args, kwargs, 0, "dotenv_path"), opt(args, kwargs, 5, "encoding").or(Some(&V::None)))?;
    let raw = parse_stream(&text, rev);
    let vals = if interp { resolve_variables(raw, override_) } else {
        let mut d = Vec::new();
        for (k, v) in raw {
            dict_set(&mut d, k, v);
        }
        d
    };
    if vals.is_empty() {
        return Ok(V::Bool(false));
    }
    for (k, v) in vals {
        if std::env::var_os(&k).is_some() && !override_ {
            continue;
        }
        if let Some(v) = v {
            if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
                return Err(Exc::value_error("illegal environment variable name"));
            }
            std::env::set_var(&k, &v);
        }
    }
    Ok(V::Bool(true))
}

/// `dotenv_values(dotenv_path=None, stream=None, verbose=False, interpolate=True, encoding="utf-8")`: a dict
pub fn dotenv_values(rev: Rev, args: &[V], kwargs: &[(String, V)]) -> R {
    const NAMES: &[&str] = &["dotenv_path", "stream", "verbose", "interpolate", "encoding"];
    check("dotenv_values", args, kwargs, NAMES)?;
    let interp = flag(opt(args, kwargs, 3, "interpolate"), true)?;
    let text = read(opt(args, kwargs, 0, "dotenv_path"), opt(args, kwargs, 4, "encoding").or(Some(&V::None)))?;
    let raw = parse_stream(&text, rev);
    let vals = if interp { resolve_variables(raw, true) } else {
        let mut d = Vec::new();
        for (k, v) in raw {
            dict_set(&mut d, k, v);
        }
        d
    };
    V::dict_from(vals.into_iter().map(|(k, v)| (V::str(k), v.map(V::str).unwrap_or(V::None))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Vec<(String, Option<String>)> {
        parse_stream(s, Rev::of(4))
    }

    fn owned(v: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        v.iter().map(|(k, v)| (k.to_string(), v.map(String::from))).collect()
    }

    // expected values: python-dotenv 1.2.4's `parse_stream` on the same text
    #[test]
    fn bindings() {
        let got = p("\u{feff}# c\nA=1\nexport B = two words  # comment\nC='x\\'y'\nD=\"l1\\nl2\"\nE\n'F'=f\nG=a#b\nH= # c\nI=\n");
        assert_eq!(got, owned(&[("A", Some("1")), ("B", Some("two words")), ("C", Some("x'y")), ("D", Some("l1\nl2")),
                                ("E", None), ("F", Some("f")), ("G", Some("a#b")), ("H", Some("")), ("I", Some(""))]));
    }

    #[test]
    fn errors_resume_after_the_value() {
        // a multi-line value followed by garbage: the rest of *that* line is skipped, not the binding's first one
        let got = p("A=\"l1\nl2\"junk\nB=2\n=x\nC='unterminated\nD=4\n");
        assert_eq!(got, owned(&[("B", Some("2")), ("D", Some("4"))]));
    }

    #[test]
    fn before_1_2_3() {
        // python-dotenv 1.2.2: `A='a\\'` in the file reads `a\` (the closing quote is not taken as escaped), the BOM is part of the key,
        // `B= # c` is the value `# c`
        let got = parse_stream("\u{feff}A='a\\\\'\nB= # c\nC=\"x\\\"y\"\n", Rev::of(2));
        assert_eq!(got, owned(&[("\u{feff}A", Some("a\\")), ("B", Some("# c")), ("C", Some("x\"y"))]));
    }

    #[test]
    fn variables() {
        let env = |n: &str| -> Option<Option<String>> { (n == "A").then(|| Some("1".into())) };
        assert_eq!(resolve("x${A}y${B:-d}z${}${C", &env), "x1ydz${C");
    }
}
