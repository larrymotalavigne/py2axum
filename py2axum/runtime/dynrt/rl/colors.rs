//! The colours of reportlab/lib/colors.py that the canvas writes.

use super::fontdata::{NamedColor, NAMED_COLORS};
use super::RlError;

/// A colour as the canvas sees it. `alpha` is None for a plain tuple (`setFillColorRGB`, `(r, g, b)`),
/// Some for a `Color` object (default 1: it still raises the PDF version to 1.4, like reportlab).
#[derive(Clone, Debug, PartialEq)]
pub enum Color {
    Rgb { r: f64, g: f64, b: f64, alpha: Option<f64> },
    /// `CMYKColor` (density, alpha) or a plain 4-tuple (density 1, no alpha).
    Cmyk { c: f64, m: f64, y: f64, k: f64, density: f64, alpha: Option<f64> },
}

impl Color {
    /// `Color(r, g, b, alpha)`.
    pub fn rgba(r: f64, g: f64, b: f64, alpha: f64) -> Color {
        Color::Rgb { r, g, b, alpha: Some(alpha) }
    }

    /// `Color(r, g, b)`.
    pub fn rgb(r: f64, g: f64, b: f64) -> Color {
        Color::rgba(r, g, b, 1.0)
    }

    /// A plain `(r, g, b)` tuple.
    pub fn tuple3(r: f64, g: f64, b: f64) -> Color {
        Color::Rgb { r, g, b, alpha: None }
    }

    /// A plain `(c, m, y, k)` tuple.
    pub fn tuple4(c: f64, m: f64, y: f64, k: f64) -> Color {
        Color::Cmyk { c, m, y, k, density: 1.0, alpha: None }
    }

    /// `CMYKColor(c, m, y, k, density=, alpha=)`.
    pub fn cmyk(c: f64, m: f64, y: f64, k: f64, density: f64, alpha: f64) -> Color {
        Color::Cmyk { c, m, y, k, density: density.clamp(0.0, 1.0), alpha: Some(alpha) }
    }

    pub fn alpha(&self) -> Option<f64> {
        match self {
            Color::Rgb { alpha, .. } | Color::Cmyk { alpha, .. } => *alpha,
        }
    }

    /// A module attribute of reportlab.lib.colors (`colors.red`, `colors.lightgrey`, ...).
    pub fn named(name: &str) -> Option<Color> {
        NAMED_COLORS.iter().find(|(n, _)| *n == name).map(|(_, c)| match *c {
            NamedColor::Rgb(r, g, b, a) => Color::rgba(r, g, b, a),
            NamedColor::Cmyk(c, m, y, k, d, a) => Color::Cmyk { c, m, y, k, density: d, alpha: Some(a) },
        })
    }

    /// `HexColor(val)`: "#RRGGBB", "0xRRGGBB" or a decimal integer string (no alpha form).
    pub fn hex(val: &str) -> Result<Color, RlError> {
        let (digits, base) = if let Some(v) = val.strip_prefix('#') {
            (v, 16)
        } else if val.len() >= 2 && val[..2].eq_ignore_ascii_case("0x") {
            (&val[2..], 16)
        } else {
            (val, 10)
        };
        let v = i64::from_str_radix(digits.trim(), base)
            .map_err(|_| RlError::Value(format!("invalid literal for int() with base {base}: {digits:?}")))?;
        Ok(Color::rgb(
            ((v >> 16) & 0xFF) as f64 / 255.0,
            ((v >> 8) & 0xFF) as f64 / 255.0,
            (v & 0xFF) as f64 / 255.0,
        ))
    }

    /// The subset of `toColor(str)` used by the canvas: a colour name, then a hex value.
    pub fn parse(s: &str) -> Result<Color, RlError> {
        if let Some(c) = Color::named(s) {
            return Ok(c);
        }
        if s.starts_with('#') || s.to_ascii_lowercase().starts_with("0x") {
            return Color::hex(s);
        }
        Err(RlError::Value(format!("Invalid color value {s:?}")))
    }
}
