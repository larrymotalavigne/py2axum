//! reportlab/lib/styles.py (ParagraphStyle, getSampleStyleSheet) and lib/enums.py.

use super::colors::Color;
use super::RlError;

pub const TA_LEFT: i64 = 0;
pub const TA_CENTER: i64 = 1;
pub const TA_RIGHT: i64 = 2;
pub const TA_JUSTIFY: i64 = 4;

/// `borderPadding`: a number or a 1..4 sequence (normalizeTRBL).
#[derive(Clone, Debug, PartialEq)]
pub enum Padding {
    Num(f64),
    Seq(Vec<f64>),
}

impl Padding {
    /// `normalizeTRBL(p)` -> (top, right, bottom, left).
    pub fn trbl(&self) -> (f64, f64, f64, f64) {
        match self {
            Padding::Num(p) => (*p, *p, *p, *p),
            Padding::Seq(v) => match v.len() {
                1 => (v[0], v[0], v[0], v[0]),
                2 => (v[0], v[1], v[0], v[1]),
                3 => (v[0], v[1], v[2], v[1]),
                _ => (v[0], v[1], v[2], v[3]),
            },
        }
    }
    pub fn is_zero(&self) -> bool {
        matches!(self, Padding::Num(p) if *p == 0.0)
    }
}

/// A ParagraphStyle: the class defaults, then the parent's attributes, then the keywords
/// (PropertySet copies everything at construction; later changes to the parent are not seen).
/// Attributes that are not in the defaults are Options (None = absent).
#[derive(Clone, Debug, PartialEq)]
pub struct ParagraphStyle {
    pub name: String,
    pub font_name: String,
    pub font_size: f64,
    pub leading: f64,
    pub left_indent: f64,
    pub right_indent: f64,
    pub first_line_indent: f64,
    pub alignment: i64,
    pub space_before: f64,
    pub space_after: f64,
    pub bullet_font_name: String,
    pub bullet_font_size: f64,
    pub bullet_indent: f64,
    pub text_color: Option<Color>,
    pub back_color: Option<Color>,
    pub word_wrap: Option<String>,
    pub shaping: bool,
    pub border_width: f64,
    pub border_padding: Padding,
    pub border_color: Option<Color>,
    pub border_radius: Option<f64>,
    pub allow_widows: bool,
    pub allow_orphans: bool,
    pub text_transform: Option<String>,
    pub end_dots: Option<String>,
    pub split_long_words: bool,
    pub bullet_anchor: String,
    pub justify_last_line: i64,
    pub justify_breaks: bool,
    pub space_shrinkage: f64,
    pub hyphenation_lang: String,
    pub embedded_hyphenation: f64,
    pub uri_waste_reduce: f64,
    pub link_underline: bool,
    // not in the class defaults
    pub auto_leading: Option<String>,
    pub keep_with_next: Option<bool>,
    pub bullet_text: Option<String>,
}

/// A keyword value for `ParagraphStyle::set`.
#[derive(Clone, Debug)]
pub enum StyleVal {
    Num(f64),
    Bool(bool),
    Str(String),
    Color(Color),
    Seq(Vec<f64>),
    None,
}

impl Default for ParagraphStyle {
    /// `ParagraphStyle('', None)`: the class defaults.
    fn default() -> Self {
        ParagraphStyle {
            name: String::new(),
            font_name: "Helvetica".into(),
            font_size: 10.0,
            leading: 12.0,
            left_indent: 0.0,
            right_indent: 0.0,
            first_line_indent: 0.0,
            alignment: TA_LEFT,
            space_before: 0.0,
            space_after: 0.0,
            bullet_font_name: "Helvetica".into(),
            bullet_font_size: 10.0,
            bullet_indent: 0.0,
            text_color: Some(Color::rgb(0.0, 0.0, 0.0)),
            back_color: None,
            word_wrap: None,
            shaping: false,
            border_width: 0.0,
            border_padding: Padding::Num(0.0),
            border_color: None,
            border_radius: None,
            allow_widows: true,
            allow_orphans: false,
            text_transform: None,
            end_dots: None,
            split_long_words: true,
            bullet_anchor: "start".into(),
            justify_last_line: 0,
            justify_breaks: false,
            space_shrinkage: 0.05,
            hyphenation_lang: String::new(),
            embedded_hyphenation: 0.0,
            uri_waste_reduce: 0.0,
            link_underline: false,
            auto_leading: None,
            keep_with_next: None,
            bullet_text: None,
        }
    }
}

fn type_err(key: &str, v: &StyleVal) -> RlError {
    RlError::Unsupported(format!("ParagraphStyle {key}={v:?}"))
}

impl ParagraphStyle {
    /// `ParagraphStyle(name, parent=parent)` before keywords.
    pub fn new(name: &str, parent: Option<&ParagraphStyle>) -> ParagraphStyle {
        let mut s = parent.cloned().unwrap_or_default();
        s.name = name.to_string();
        s
    }

    /// `ParagraphStyle(name, parent, **kw)`: keywords applied in order.
    pub fn with(name: &str, parent: Option<&ParagraphStyle>, kw: &[(&str, StyleVal)]) -> Result<ParagraphStyle, RlError> {
        let mut s = ParagraphStyle::new(name, parent);
        for (k, v) in kw {
            s.set(k, v.clone())?;
        }
        Ok(s)
    }

    /// `setattr(style, key, value)`. Unknown keys are accepted and ignored, like the Python object
    /// (no reportlab code reads them); a value of a type the port does not model is refused.
    pub fn set(&mut self, key: &str, v: StyleVal) -> Result<(), RlError> {
        let num = |v: &StyleVal| match v {
            StyleVal::Num(x) => Ok(*x),
            StyleVal::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            _ => Err(type_err(key, v)),
        };
        let truth = |v: &StyleVal| match v {
            StyleVal::Num(x) => Ok(*x != 0.0),
            StyleVal::Bool(b) => Ok(*b),
            StyleVal::None => Ok(false),
            StyleVal::Str(s) => Ok(!s.is_empty()),
            _ => Err(type_err(key, v)),
        };
        let color = |v: &StyleVal| match v {
            StyleVal::Color(c) => Ok(Some(c.clone())),
            StyleVal::None => Ok(None),
            StyleVal::Str(s) => Color::parse(s).map(Some),
            _ => Err(type_err(key, v)),
        };
        let ostr = |v: &StyleVal| match v {
            StyleVal::Str(s) => Ok(Some(s.clone())),
            StyleVal::None => Ok(None),
            _ => Err(type_err(key, v)),
        };
        match key {
            "name" => self.name = ostr(&v)?.unwrap_or_default(),
            "fontName" => self.font_name = ostr(&v)?.ok_or_else(|| type_err(key, &v))?,
            "fontSize" => self.font_size = num(&v)?,
            "leading" => self.leading = num(&v)?,
            "leftIndent" => self.left_indent = num(&v)?,
            "rightIndent" => self.right_indent = num(&v)?,
            "firstLineIndent" => self.first_line_indent = num(&v)?,
            "alignment" => {
                let a = num(&v)?;
                if ![0.0, 1.0, 2.0, 4.0].contains(&a) {
                    return Err(type_err(key, &v));
                }
                self.alignment = a as i64
            }
            "spaceBefore" => self.space_before = num(&v)?,
            "spaceAfter" => self.space_after = num(&v)?,
            "bulletFontName" => self.bullet_font_name = ostr(&v)?.ok_or_else(|| type_err(key, &v))?,
            "bulletFontSize" => self.bullet_font_size = num(&v)?,
            "bulletIndent" => self.bullet_indent = num(&v)?,
            "textColor" => self.text_color = color(&v)?,
            "backColor" => self.back_color = color(&v)?,
            "borderColor" => self.border_color = color(&v)?,
            "wordWrap" => self.word_wrap = ostr(&v)?,
            "shaping" => self.shaping = truth(&v)?,
            "borderWidth" => self.border_width = num(&v)?,
            "borderPadding" => {
                self.border_padding = match &v {
                    StyleVal::Seq(s) if (1..=4).contains(&s.len()) => Padding::Seq(s.clone()),
                    _ => Padding::Num(num(&v)?),
                }
            }
            "borderRadius" => {
                self.border_radius = match v {
                    StyleVal::None => None,
                    _ => Some(num(&v)?),
                }
            }
            "allowWidows" => self.allow_widows = truth(&v)?,
            "allowOrphans" => self.allow_orphans = truth(&v)?,
            "textTransform" => self.text_transform = ostr(&v)?,
            "endDots" => self.end_dots = ostr(&v)?,
            "splitLongWords" => self.split_long_words = truth(&v)?,
            "bulletAnchor" => self.bullet_anchor = ostr(&v)?.unwrap_or_default(),
            "justifyLastLine" => self.justify_last_line = num(&v)? as i64,
            "justifyBreaks" => self.justify_breaks = truth(&v)?,
            "spaceShrinkage" => self.space_shrinkage = num(&v)?,
            "hyphenationLang" => self.hyphenation_lang = ostr(&v)?.unwrap_or_default(),
            "embeddedHyphenation" => self.embedded_hyphenation = num(&v)?,
            "uriWasteReduce" => self.uri_waste_reduce = num(&v)?,
            "linkUnderline" => self.link_underline = truth(&v)?,
            "autoLeading" => self.auto_leading = ostr(&v)?,
            "keepWithNext" => self.keep_with_next = Some(truth(&v)?),
            "bulletText" => self.bullet_text = ostr(&v)?,
            // read by nothing in the ported paths
            _ => {}
        }
        Ok(())
    }
}

/// `StyleSheet1`: styles by name and by alias.
#[derive(Clone, Debug, Default)]
pub struct StyleSheet {
    pub by_name: Vec<ParagraphStyle>,
    pub by_alias: Vec<(String, usize)>,
}

impl StyleSheet {
    /// `stylesheet[key]`: aliases first, then names.
    pub fn get(&self, key: &str) -> Result<&ParagraphStyle, RlError> {
        if let Some((_, i)) = self.by_alias.iter().find(|(a, _)| a == key) {
            return Ok(&self.by_name[*i]);
        }
        self.by_name
            .iter()
            .find(|s| s.name == key)
            .ok_or_else(|| RlError::Key(format!("Style '{key}' not found in stylesheet")))
    }

    /// `stylesheet.add(style, alias)`.
    pub fn add(&mut self, style: ParagraphStyle, alias: Option<&str>) -> Result<(), RlError> {
        let key = style.name.clone();
        if self.by_name.iter().any(|s| s.name == key) {
            return Err(RlError::Key(format!("Style '{key}' already defined in stylesheet")));
        }
        if self.by_alias.iter().any(|(a, _)| *a == key) {
            return Err(RlError::Key(format!("Style name '{key}' is already an alias in stylesheet")));
        }
        if let Some(a) = alias {
            if self.by_name.iter().any(|s| s.name == a) {
                return Err(RlError::Key(format!("Style '{a}' already defined in stylesheet")));
            }
            if self.by_alias.iter().any(|(x, _)| x == a) {
                return Err(RlError::Key(format!("Alias name '{a}' is already an alias in stylesheet")));
            }
        }
        self.by_name.push(style);
        if let Some(a) = alias {
            self.by_alias.push((a.to_string(), self.by_name.len() - 1));
        }
        Ok(())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_ok()
    }
}

/// `getSampleStyleSheet()` (its two ListStyles, UnorderedList/OrderedList, are left out).
pub fn sample_style_sheet() -> StyleSheet {
    use StyleVal::{Num, Str};
    let mut ss = StyleSheet::default();
    let mk = |ss: &StyleSheet, name: &str, parent: Option<&str>, kw: &[(&str, StyleVal)]| {
        let p = parent.map(|p| ss.get(p).unwrap().clone());
        ParagraphStyle::with(name, p.as_ref(), kw).unwrap()
    };
    let s = mk(&ss, "Normal", None, &[("fontName", Str("Helvetica".into())), ("fontSize", Num(10.0)), ("leading", Num(12.0))]);
    ss.add(s, None).unwrap();
    let s = mk(&ss, "BodyText", Some("Normal"), &[("spaceBefore", Num(6.0))]);
    ss.add(s, None).unwrap();
    let s = mk(&ss, "Italic", Some("BodyText"), &[("fontName", Str("Helvetica-Oblique".into()))]);
    ss.add(s, None).unwrap();
    let b = || Str("Helvetica-Bold".into());
    let bi = || Str("Helvetica-BoldOblique".into());
    let s = mk(&ss, "Heading1", Some("Normal"), &[("fontName", b()), ("fontSize", Num(18.0)), ("leading", Num(22.0)), ("spaceAfter", Num(6.0))]);
    ss.add(s, Some("h1")).unwrap();
    let s = mk(
        &ss,
        "Title",
        Some("Normal"),
        &[("fontName", b()), ("fontSize", Num(18.0)), ("leading", Num(22.0)), ("alignment", Num(1.0)), ("spaceAfter", Num(6.0))],
    );
    ss.add(s, Some("title")).unwrap();
    let heads: [(&str, &str, fn() -> StyleVal, f64, f64, f64, f64); 5] = [
        ("Heading2", "h2", || Str("Helvetica-Bold".into()), 14.0, 18.0, 12.0, 6.0),
        ("Heading3", "h3", || Str("Helvetica-BoldOblique".into()), 12.0, 14.0, 12.0, 6.0),
        ("Heading4", "h4", || Str("Helvetica-BoldOblique".into()), 10.0, 12.0, 10.0, 4.0),
        ("Heading5", "h5", || Str("Helvetica-Bold".into()), 9.0, 10.8, 8.0, 4.0),
        ("Heading6", "h6", || Str("Helvetica-Bold".into()), 7.0, 8.4, 6.0, 2.0),
    ];
    for (n, a, f, size, lead, sb, sa) in heads {
        let s = mk(
            &ss,
            n,
            Some("Normal"),
            &[("fontName", f()), ("fontSize", Num(size)), ("leading", Num(lead)), ("spaceBefore", Num(sb)), ("spaceAfter", Num(sa))],
        );
        ss.add(s, Some(a)).unwrap();
    }
    let s = mk(&ss, "Bullet", Some("Normal"), &[("firstLineIndent", Num(0.0)), ("spaceBefore", Num(3.0))]);
    ss.add(s, Some("bu")).unwrap();
    let s = mk(
        &ss,
        "Definition",
        Some("Normal"),
        &[
            ("firstLineIndent", Num(0.0)),
            ("leftIndent", Num(36.0)),
            ("bulletIndent", Num(0.0)),
            ("spaceBefore", Num(6.0)),
            ("bulletFontName", bi()),
        ],
    );
    ss.add(s, Some("df")).unwrap();
    let s = mk(
        &ss,
        "Code",
        Some("Normal"),
        &[
            ("fontName", Str("Courier".into())),
            ("fontSize", Num(8.0)),
            ("leading", Num(8.8)),
            ("firstLineIndent", Num(0.0)),
            ("leftIndent", Num(36.0)),
            ("hyphenationLang", Str(String::new())),
        ],
    );
    ss.add(s, None).unwrap();
    ss
}
