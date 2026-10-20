//! `HRFlowable` of reportlab/platypus/flowables.py (the hr tag).

use super::canvas::Canvas;
use super::colors::Color;
use super::flowables::{Flowable, HAlign};
use super::graphics::set_dash_checked;
use super::RlError;

/// `width=`: a number, or a string ("80%" of the available width, or a number as text).
#[derive(Clone, Debug, PartialEq)]
pub enum HrWidth {
    Abs(f64),
    Str(String),
}

/// `dash=`: a number n (drawn as `[n 0]`) or an array.
#[derive(Clone, Debug, PartialEq)]
pub enum HrDash {
    Number(f64),
    Array(Vec<f64>),
}

#[derive(Clone, Debug)]
pub struct HRFlowable {
    pub width: HrWidth,
    /// `thickness` (the `lineWidth` attribute).
    pub line_width: f64,
    /// 'butt' | 'round' | 'square' (any case).
    pub line_cap: String,
    /// None raises ValueError('Unknown color None') at draw time, as reportlab.
    pub color: Option<Color>,
    pub space_before: f64,
    pub space_after: f64,
    pub h_align: HAlign,
    pub v_align: String,
    pub dash: Option<HrDash>,
    /// `height` attribute (0 from Flowable.__init__; the line goes to (_width, height)).
    pub height: f64,
    w: f64,
}

/// `hAlign` strings ('LEFT', 'CENTER'/'CENTRE', 'RIGHT'); reportlab raises lazily, we raise here.
pub fn parse_h_align(s: &str) -> Result<HAlign, RlError> {
    match s {
        "LEFT" => Ok(HAlign::Left),
        "CENTER" | "CENTRE" => Ok(HAlign::Center),
        "RIGHT" => Ok(HAlign::Right),
        _ => Err(RlError::Value(format!("Bad hAlign value {s}"))),
    }
}

impl Default for HRFlowable {
    fn default() -> HRFlowable {
        HRFlowable {
            width: HrWidth::Str("80%".into()),
            line_width: 1.0,
            line_cap: "round".into(),
            color: Color::named("lightgrey"),
            space_before: 1.0,
            space_after: 1.0,
            h_align: HAlign::Center,
            v_align: "BOTTOM".into(),
            dash: None,
            height: 0.0,
            w: 0.0,
        }
    }
}

impl HRFlowable {
    /// `HRFlowable()` with reportlab's defaults; set the public fields for the keywords.
    pub fn new() -> HRFlowable {
        HRFlowable::default()
    }
}

/// Python's `float(str)` for the forms used here.
fn py_float(s: &str) -> Result<f64, RlError> {
    s.trim().parse::<f64>().map_err(|_| RlError::Value(format!("could not convert string to float: {s:?}")))
}

impl Flowable for HRFlowable {
    fn wrap(&mut self, _c: &Canvas, avail_width: f64, _h: f64) -> Result<(f64, f64), RlError> {
        let w = match &self.width {
            HrWidth::Abs(v) => *v,
            HrWidth::Str(s) => {
                let s = s.trim();
                match s.strip_suffix('%') {
                    Some(p) => avail_width * py_float(p)? * 0.01,
                    None => py_float(s)?,
                }
            }
        };
        // min(w, availWidth): the first one on ties
        let w = if avail_width < w { avail_width } else { w };
        self.w = w;
        Ok((w, self.line_width))
    }

    fn draw(&mut self, canv: &mut Canvas) -> Result<(), RlError> {
        canv.save_state();
        canv.set_line_width(self.line_width);
        let cap = match self.line_cap.to_lowercase().as_str() {
            "butt" => 0,
            "round" => 1,
            "square" => 2,
            o => return Err(RlError::Value(format!("KeyError: {o:?}"))),
        };
        canv.set_line_cap(cap)?;
        match &self.color {
            Some(c) => canv.set_stroke_color(c, None)?,
            None => return Err(RlError::Value("Unknown color None".into())),
        }
        match &self.dash {
            Some(HrDash::Number(n)) if *n != 0.0 => set_dash_checked(canv, &[*n, 0.0], "0")?,
            Some(HrDash::Array(a)) if !a.is_empty() => set_dash_checked(canv, a, "0")?,
            _ => {}
        }
        canv.line(0.0, 0.0, self.w, self.height);
        canv.restore_state()
    }

    fn h_align(&self) -> Option<HAlign> {
        Some(self.h_align)
    }
    fn space_before(&self) -> f64 {
        self.space_before
    }
    fn space_after(&self) -> f64 {
        self.space_after
    }
    fn class_name(&self) -> &'static str {
        "HRFlowable"
    }
}
