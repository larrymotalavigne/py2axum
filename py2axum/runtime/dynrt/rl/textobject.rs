//! reportlab/pdfgen/textobject.py: PDFTextObject for the 14 standard fonts (bottom-up canvas).

use super::accel::fp_str;
use super::canvas::{font_id, Canvas};
use super::colors::Color;
use super::fonts;
use super::RlError;

pub struct TextObject {
    code: Vec<String>,
    /// `_fontname`, `_fontsize`, `_leading` (None after a paragraph resets them).
    pub font_name: Option<String>,
    pub font_size: Option<f64>,
    pub leading: Option<f64>,
    pub x0: f64,
    pub y0: f64,
    pub x: f64,
    pub y: f64,
    /// `_wordSpace` (getattr default 0).
    pub word_space: f64,
    rise: f64,
}

impl TextObject {
    /// `canvas.beginText(x, y)`.
    pub fn new(canv: &Canvas, x: f64, y: f64) -> TextObject {
        let mut t = TextObject {
            code: vec!["BT".to_string()],
            font_name: Some(canv.font_name().to_string()),
            font_size: Some(canv.font_size()),
            leading: Some(canv.leading()),
            x0: 0.0,
            y0: 0.0,
            x: 0.0,
            y: 0.0,
            word_space: 0.0,
            rise: 0.0,
        };
        t.set_text_origin(x, y);
        t
    }

    /// `getCode()`.
    pub fn get_code(mut self) -> String {
        self.code.push("ET".to_string());
        self.code.join(" ")
    }

    pub fn set_text_origin(&mut self, x: f64, y: f64) {
        self.code.push(format!("1 0 0 1 {} Tm", fp_str(&[x, y])));
        self.x0 = x;
        self.x = x;
        self.y0 = y;
        self.y = y;
    }

    /// `moveCursor(dx, dy)`: merges with a preceding Td, re-reading its printed numbers.
    pub fn move_cursor(&mut self, mut dx: f64, mut dy: f64) {
        if let Some(last) = self.code.last() {
            if last.ends_with(" Td") {
                let l: Vec<String> = last.split_whitespace().map(str::to_string).collect();
                if l.len() == 3 {
                    self.code.pop();
                } else {
                    let keep = l[..l.len() - 4].join("");
                    *self.code.last_mut().unwrap() = keep;
                }
                let last_dx: f64 = l[l.len() - 3].parse().unwrap();
                let last_dy: f64 = l[l.len() - 2].parse().unwrap();
                dx += last_dx;
                dy -= last_dy;
                self.x0 -= last_dx;
                self.y0 -= last_dy;
            }
        }
        self.code.push(format!("{} Td", fp_str(&[dx, -dy])));
        self.x0 += dx;
        self.y0 += dy;
        self.x = self.x0;
        self.y = self.y0;
    }

    pub fn set_x_pos(&mut self, dx: f64) {
        self.move_cursor(dx, 0.0);
    }

    /// `_setFont(psfontname, size)`.
    pub fn set_font_only(&mut self, canv: &mut Canvas, name: &str, size: f64) -> Result<(), RlError> {
        let n = canv.internal_font_name(name)?;
        self.font_name = Some(name.to_string());
        self.font_size = Some(size);
        self.code.push(format!("{} {} Tf", n, fp_str(&[size])));
        Ok(())
    }

    /// `setFont(psfontname, size, leading)`.
    pub fn set_font(&mut self, canv: &mut Canvas, name: &str, size: f64, leading: Option<f64>) -> Result<(), RlError> {
        let leading = leading.unwrap_or(size * 1.2);
        let n = canv.internal_font_name(name)?;
        self.font_name = Some(name.to_string());
        self.font_size = Some(size);
        self.leading = Some(leading);
        self.code.push(format!("{} {} Tf {} TL", n, fp_str(&[size]), fp_str(&[leading])));
        Ok(())
    }

    pub fn set_word_space(&mut self, ws: f64) {
        self.word_space = ws;
        self.code.push(format!("{} Tw", fp_str(&[ws])));
    }

    pub fn set_leading(&mut self, leading: f64) {
        self.leading = Some(leading);
        self.code.push(format!("{} TL", fp_str(&[leading])));
    }

    /// `setRise(rise)`.
    pub fn set_rise(&mut self, rise: f64) {
        let v = format!("{} Ts", fp_str(&[rise]));
        if self.code.last().is_some_and(|c| c.ends_with(" Ts")) {
            self.y += self.rise;
            *self.code.last_mut().unwrap() = v;
        } else {
            self.rise = rise;
            self.y -= rise;
            self.code.push(v);
        }
    }

    /// `setFillColor(aColor)` (its alpha goes through the canvas ExtGState, into this object's code).
    pub fn set_fill_color(&mut self, canv: &mut Canvas, c: Option<&Color>) -> Result<(), RlError> {
        let Some(c) = c else {
            return Err(RlError::Value("Unknown color None".into()));
        };
        self.code.push(Canvas::color_code(c, false));
        if let Some(a) = c.alpha() {
            if let Some(code) = canv.fill_alpha_code(a)? {
                self.code.push(code);
            }
        }
        Ok(())
    }

    fn font(&self) -> Result<fonts::FontId, RlError> {
        font_id(self.font_name.as_deref().unwrap_or(""))
    }

    /// `_textOut(text, TStar)`.
    pub fn text_out_raw(&mut self, canv: &mut Canvas, text: &str, tstar: bool) -> Result<(), RlError> {
        let f = self.font()?;
        let t = canv.format_text(text, f, self.font_size.unwrap_or(0.0), self.leading.unwrap_or(0.0));
        self.code.push(format!("{}{}", t, if tstar { " T*" } else { "" }));
        Ok(())
    }

    /// `textOut(text)`: the cursor moves across.
    pub fn text_out(&mut self, canv: &mut Canvas, text: &str) -> Result<(), RlError> {
        let f = self.font()?;
        self.x += fonts::string_width(text, f, self.font_size.unwrap_or(0.0));
        let t = canv.format_text(text, f, self.font_size.unwrap_or(0.0), self.leading.unwrap_or(0.0));
        self.code.push(t);
        Ok(())
    }

    /// `canvas.stringWidth(text, self._fontname, self._fontsize)`.
    pub fn string_width(&self, text: &str) -> Result<f64, RlError> {
        Ok(fonts::string_width(text, self.font()?, self.font_size.unwrap_or(0.0)))
    }
}
