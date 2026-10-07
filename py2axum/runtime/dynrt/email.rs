//! `pydantic.EmailStr`: Pydantic's wrapper around email-validator 2.3 (`check_deliverability=False`),
//! ported rule by rule with the same messages. Not ported: IDNA encoding and NFC normalisation of
//! internationalised domains (they are accepted and lower-cased) — a documented difference.

const ATEXT: &str = "_!#$%&'*+-/=?^`{|}~";
const CASE_INSENSITIVE: [&str; 15] = [
    "info", "marketing", "sales", "support", "abuse", "noc", "security", "postmaster", "hostmaster", "usenet",
    "news", "webmaster", "www", "uucp", "ftp",
];
const SPECIAL_USE: [&str; 6] = ["arpa", "invalid", "local", "localhost", "onion", "test"];

fn is_atext(c: char) -> bool {
    c.is_ascii_alphanumeric() || ATEXT.contains(c)
}

fn is_atext_intl(c: char) -> bool {
    is_atext(c) || (c as u32) >= 0x80
}

fn display(c: char) -> String {
    match c {
        '\\' => "\"\\\"".to_string(),
        ' ' => "SPACE".to_string(),
        '\u{a0}' => "NO-BREAK SPACE".to_string(),
        c if c.is_alphanumeric() || (c.is_ascii_graphic()) || ((c as u32) >= 0x80 && !c.is_control() && !c.is_whitespace()) => {
            super::ops::str_repr(&c.to_string())
        }
        c => format!("U+{:04X}", c as u32),
    }
}

fn bad(chars: impl Iterator<Item = char>) -> String {
    let mut v: Vec<String> = chars.map(display).collect();
    v.sort();
    v.dedup();
    v.join(", ")
}

/// dot-atom: atext+ ( "." atext+ )*
fn dot_atom(s: &str, intl: bool) -> bool {
    !s.is_empty()
        && s.split('.').all(|p| !p.is_empty() && p.chars().all(|c| if intl { is_atext_intl(c) } else { is_atext(c) }))
}

fn hostname(s: &str) -> bool {
    s.split('.').all(|l| {
        let b = l.as_bytes();
        !b.is_empty()
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
    })
}

fn check_dot_atom(label: &str, start: &str, end: &str, is_hostname: bool) -> Result<(), String> {
    if label.ends_with('.') {
        return Err(end.replace("{}", "period"));
    }
    if label.starts_with('.') {
        return Err(start.replace("{}", "period"));
    }
    if label.contains("..") {
        return Err("An email address cannot have two periods in a row.".into());
    }
    if is_hostname {
        if label.ends_with('-') {
            return Err(end.replace("{}", "hyphen"));
        }
        if label.starts_with('-') {
            return Err(start.replace("{}", "hyphen"));
        }
        if label.contains(".-") || label.contains("-.") {
            return Err("An email address cannot have a period and a hyphen next to each other.".into());
        }
    }
    Ok(())
}

fn unsafe_chars(s: &str, allow_space: bool) -> Result<(), String> {
    let badc: Vec<char> = s.chars().filter(|c| c.is_control() || (c.is_whitespace() && !(allow_space && *c == ' '))).collect();
    if badc.is_empty() {
        Ok(())
    } else {
        let mut v: Vec<char> = badc;
        v.sort();
        v.dedup();
        Err(format!("The email address contains unsafe characters: {}.", v.into_iter().map(display).collect::<Vec<_>>().join(", ")))
    }
}

/// email-validator's `split_email` (display name, local part, domain, quoted local part).
fn split(email: &str) -> Result<(Option<String>, String, String, bool), String> {
    fn at_unquoted(text: &str, specials: &[char]) -> Result<(String, String), String> {
        let (mut inside, mut esc) = (false, false);
        let mut left = String::new();
        for c in text.chars() {
            if inside {
                left.push(c);
                if c == '\\' && !esc {
                    esc = true;
                } else if c == '"' && !esc {
                    inside = false;
                    esc = false;
                } else {
                    esc = false;
                }
            } else if c == '"' {
                left.push(c);
                inside = true;
            } else if specials.contains(&c) {
                break;
            } else {
                left.push(c);
            }
        }
        if left.chars().count() == text.chars().count() {
            if text.contains('＠') {
                return Err("The email address has the \"full-width\" at-sign (@) character instead of a regular at-sign.".into());
            }
            return Err("An email address must have an @-sign.".into());
        }
        let right = text[left.len()..].to_string();
        Ok((left, right))
    }
    fn unquote(text: &str) -> Result<(String, bool), String> {
        let (mut quoted, mut esc) = (false, false);
        let mut value = String::new();
        let chars: Vec<char> = text.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if quoted {
                if esc {
                    value.push(*c);
                    esc = false;
                } else if *c == '\\' {
                    esc = true;
                } else if *c == '"' {
                    if i != chars.len() - 1 {
                        return Err(format!("Extra character(s) found after close quote: {}", chars[i + 1..].iter().map(|c| display(*c)).collect::<Vec<_>>().join(", ")));
                    }
                    break;
                } else {
                    value.push(*c);
                }
            } else if i == 0 && *c == '"' {
                quoted = true;
            } else {
                value.push(*c);
            }
        }
        Ok((value, quoted))
    }
    let (left, right) = at_unquoted(email, &['@', '<'])?;
    let (display_name, local, domain) = if right.starts_with('<') {
        let (name, _) = unquote(left.trim_end())?;
        if !right.contains('>') {
            return Err("An open angle bracket at the start of the email address has to be followed by a close angle bracket at the end.".into());
        }
        let r = right.trim_end_matches(' ');
        if !r.ends_with('>') {
            return Err("There can't be anything after the email address.".into());
        }
        let spec = r[1..].trim_end_matches('>');
        let (l, d) = at_unquoted(spec, &['@'])?;
        (Some(name), l, d)
    } else {
        (None, left, right)
    };
    let domain = domain.strip_prefix('@').unwrap_or(&domain).to_string();
    let (local, q) = unquote(&local)?;
    Ok((display_name, local, domain, q))
}

fn validate_email(email: &str) -> Result<String, String> {
    let (display_name, local, domain, quoted) = split(email)?;
    // local part
    if local.is_empty() {
        return Err("There must be something before the @-sign.".into());
    }
    let mut local_norm = local.clone();
    if !dot_atom(&local, false) {
        if dot_atom(&local, true) {
            unsafe_chars(&local, false)?;
        } else if quoted {
            unsafe_chars(&local, true)?;
        } else {
            let b: Vec<char> = local.chars().filter(|c| !(is_atext_intl(*c) || *c == '.')).collect();
            if !b.is_empty() {
                return Err(format!("The email address contains invalid characters before the @-sign: {}.", bad(b.into_iter())));
            }
            check_dot_atom(&local, "An email address cannot start with a {}.", "An email address cannot have a {} immediately before the @-sign.", false)?;
            return Err("The email address contains invalid characters before the @-sign.".into());
        }
    }
    if quoted {
        return Err("Quoting the part before the @-sign is not allowed here.".into());
    }
    if CASE_INSENSITIVE.contains(&local.to_ascii_lowercase().as_str()) {
        local_norm = local.to_lowercase();
    }
    // domain
    if domain.is_empty() {
        return Err("There must be something after the @-sign.".into());
    }
    if domain.starts_with('[') && domain.ends_with(']') {
        return Err("A bracketed IP address after the @-sign is not allowed here.".into());
    }
    let b: Vec<char> = domain.chars().filter(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || (*c as u32) >= 0x80)).collect();
    if !b.is_empty() {
        return Err(format!("The part after the @-sign contains invalid characters: {}.", bad(b.into_iter())));
    }
    unsafe_chars(&domain, false)?;
    let domain_lc = domain.to_lowercase();
    check_dot_atom(&domain_lc, "An email address cannot have a {} immediately after the @-sign.", "An email address cannot end with a {}.", true)?;
    for label in domain_lc.split('.') {
        let lb = label.as_bytes();
        if lb.len() >= 4 && &lb[2..4] == b"--" && !label.to_ascii_lowercase().starts_with("xn") {
            return Err("An email address cannot have two letters followed by two dashes immediately after the @-sign or after a period, except Punycode.".into());
        }
    }
    if domain_lc.is_ascii() && !hostname(&domain_lc) {
        return Err("The email address contains invalid characters after the @-sign after IDNA encoding.".into());
    }
    if domain_lc.len() > 253 {
        return Err(format!("The email address is too long after the @-sign ({} character{} too many).", domain_lc.len() - 253, if domain_lc.len() - 253 > 1 { "s" } else { "" }));
    }
    for label in domain_lc.split('.') {
        if label.len() > 63 {
            let d = label.len() - 63;
            return Err(format!("After the @-sign, periods cannot be separated by so many characters ({d} character{} too many).", if d > 1 { "s" } else { "" }));
        }
    }
    if !domain_lc.contains('.') {
        return Err("The part after the @-sign is not valid. It should have a period.".into());
    }
    if !domain_lc.chars().last().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) && domain_lc.is_ascii() {
        return Err("The part after the @-sign is not valid. It is not within a valid top-level domain.".into());
    }
    for d in SPECIAL_USE {
        if domain_lc == d || domain_lc.ends_with(&format!(".{d}")) {
            return Err("The part after the @-sign is a special-use or reserved name that cannot be used with email.".into());
        }
    }
    let normalized = format!("{local_norm}@{domain_lc}");
    let original = format!("{}@{}", if quoted { format!("\"{local}\"") } else { local.clone() }, domain);
    for addr in [&original, &normalized] {
        let n = addr.len();
        if n > 254 {
            let d = n - 254;
            return Err(format!("The email address is too long ({d} character{} too many).", if d > 1 { "s" } else { "" }));
        }
    }
    let _ = display_name;
    Ok(normalized)
}

/// Pydantic's `validate_email`: length cap, "pretty" `Name <addr>` form, strip, then email-validator.
pub fn validate(value: &str) -> Result<String, String> {
    if value.chars().count() > 2048 {
        return Err("Length must not exceed 2048 characters".into());
    }
    let mut email = value.to_string();
    if let Some(caps) = pretty_re().captures(value) {
        if caps.get(0).map(|m| m.as_str().len() == value.len()).unwrap_or(false) {
            email = caps.get(3).unwrap().as_str().to_string();
        }
    }
    validate_email(email.trim())
}

fn pretty_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        let name = r"[\w!#$%&'*+\-/=?^_`{|}~]";
        regex::Regex::new(&format!(r#"^\s*(?:((?:{name}+\s+)*{name}+)|"((?:[^"]|\")+)")?\s*<(.+)>\s*$"#)).unwrap()
    })
}
