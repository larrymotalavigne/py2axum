//! The 14 standard Type 1 fonts of reportlab/pdfbase/pdfmetrics.py (no embedded or TrueType font).

use super::fontdata::{Enc, StdFont, STD_FONTS, SYMBOL, WINANSI, ZAPFDINGBATS};

/// Index into `STD_FONTS`.
pub type FontId = usize;

pub const NOTDEF_CHAR: u8 = b'n';

pub fn font(id: FontId) -> &'static StdFont {
    &STD_FONTS[id]
}

pub fn font_by_name(name: &str) -> Option<FontId> {
    STD_FONTS.iter().position(|f| f.name == name)
}

fn symbol() -> FontId {
    font_by_name("Symbol").unwrap()
}

fn zapf() -> FontId {
    font_by_name("ZapfDingbats").unwrap()
}

/// `font.substitutionFonts`: Symbol then ZapfDingbats for the fonts without a required encoding.
pub fn substitution_fonts(id: FontId) -> &'static [FontId] {
    static SUBST: std::sync::OnceLock<[FontId; 2]> = std::sync::OnceLock::new();
    match font(id).enc {
        Enc::WinAnsi => SUBST.get_or_init(|| [symbol(), zapf()]),
        _ => &[],
    }
}

/// `font._notdefFont` (ZapfDingbats).
pub fn notdef_font(_id: FontId) -> FontId {
    zapf()
}

/// The byte of `c` in the font's encoding (the rl_codecs charmap), if any.
pub fn encode_char(id: FontId, c: char) -> Option<u8> {
    let table: &[(u32, u8)] = match font(id).enc {
        Enc::WinAnsi => &WINANSI,
        Enc::Symbol => &SYMBOL,
        Enc::ZapfDingbats => &ZAPFDINGBATS,
    };
    table.binary_search_by_key(&(c as u32), |&(u, _)| u).ok().map(|i| table[i].1)
}

/// The PDF /Encoding name written in the font dictionary, if any.
pub fn pdf_encoding(id: FontId) -> Option<&'static str> {
    match font(id).enc {
        Enc::WinAnsi => Some("WinAnsiEncoding"),
        _ => None,
    }
}

/// `pdfmetrics.stringWidth(text, fontName, size)`.
pub fn string_width(text: &str, id: FontId, size: f64) -> f64 {
    super::accel::string_width(text, id, size)
}

/// `getAscentDescent(fontName, fontSize)`.
pub fn ascent_descent(id: FontId, size: Option<f64>) -> (f64, f64) {
    let f = font(id);
    match size {
        Some(s) if s != 0.0 => {
            let norm = s / 1000.0;
            (f.ascent as f64 * norm, f.descent as f64 * norm)
        }
        _ => (f.ascent as f64, f.descent as f64),
    }
}

/// `_tt2ps_map` of reportlab/lib/fonts.py: (family, bold, italic) -> PostScript name.
const TT2PS: [(&str, u8, u8, &str); 20] = [
    ("courier", 0, 0, "Courier"),
    ("courier", 0, 1, "Courier-Oblique"),
    ("courier", 1, 0, "Courier-Bold"),
    ("courier", 1, 1, "Courier-BoldOblique"),
    ("helvetica", 0, 0, "Helvetica"),
    ("helvetica", 0, 1, "Helvetica-Oblique"),
    ("helvetica", 1, 0, "Helvetica-Bold"),
    ("helvetica", 1, 1, "Helvetica-BoldOblique"),
    ("symbol", 0, 0, "Symbol"),
    ("symbol", 0, 1, "Symbol"),
    ("symbol", 1, 0, "Symbol"),
    ("symbol", 1, 1, "Symbol"),
    ("times", 0, 0, "Times-Roman"),
    ("times", 0, 1, "Times-Italic"),
    ("times", 1, 0, "Times-Bold"),
    ("times", 1, 1, "Times-BoldItalic"),
    ("zapfdingbats", 0, 0, "ZapfDingbats"),
    ("zapfdingbats", 0, 1, "ZapfDingbats"),
    ("zapfdingbats", 1, 0, "ZapfDingbats"),
    ("zapfdingbats", 1, 1, "ZapfDingbats"),
];

/// `ps2tt(psfn)`: (family, bold, italic); keys built like `_ps2tt_map` (sorted keys, first wins).
pub fn ps2tt(psfn: &str) -> Result<(&'static str, u8, u8), super::RlError> {
    let l = psfn.to_lowercase();
    for &(fam, b, i, ps) in TT2PS.iter() {
        if ps.to_lowercase() == l || fam == l {
            return Ok((fam, b, i));
        }
    }
    Err(super::RlError::Value(format!("Can't map determine family/bold/italic for {l}")))
}

/// `tt2ps(fn, b, i)`.
pub fn tt2ps(fam: &str, b: u8, i: u8) -> Result<&'static str, super::RlError> {
    let l = fam.to_lowercase();
    if let Some(&(_, _, _, ps)) = TT2PS.iter().find(|t| t.0 == l && t.1 == b && t.2 == i) {
        return Ok(ps);
    }
    let (f, b1, i1) = ps2tt(&l)?;
    if let Some(&(_, _, _, ps)) = TT2PS.iter().find(|t| t.0 == f && t.1 == b1 | b && t.2 == i1 | i) {
        return Ok(ps);
    }
    Err(super::RlError::Value(format!("Can't find concrete font for family={f}, bold={b}, italic={i}")))
}
