//! reportlab/platypus/frames.py: a Frame filled top-down by flowables.

use super::canvas::Canvas;
use super::flowables::Flowable;
use super::RlError;

/// rl_config._FUZZ
pub const FUZZ: f64 = 1e-6;

pub struct Frame {
    pub id: Option<String>,
    x1: f64,
    y1: f64,
    width: f64,
    height: f64,
    left_padding: f64,
    bottom_padding: f64,
    right_padding: f64,
    top_padding: f64,
    /// rl_config.overlapAttachedSpace
    o_a_space: bool,
    // _geom
    x2: f64,
    y2: f64,
    y1p: f64,
    pub a_w: f64,
    pub a_h: f64,
    // _reset
    pub x: f64,
    pub y: f64,
    pub at_top: bool,
    prev_a_space: f64,
    pub left_extra_indent: f64,
    pub right_extra_indent: f64,
}

impl Frame {
    /// `Frame(x1, y1, width, height)` with the default 6 pt paddings.
    pub fn new(x1: f64, y1: f64, width: f64, height: f64, id: Option<&str>) -> Frame {
        Frame::with_padding(x1, y1, width, height, [6.0, 6.0, 6.0, 6.0], id)
    }

    /// paddings = [left, bottom, right, top].
    pub fn with_padding(x1: f64, y1: f64, width: f64, height: f64, p: [f64; 4], id: Option<&str>) -> Frame {
        let mut f = Frame {
            id: id.map(str::to_string),
            x1,
            y1,
            width,
            height,
            left_padding: p[0],
            bottom_padding: p[1],
            right_padding: p[2],
            top_padding: p[3],
            o_a_space: true,
            x2: 0.0,
            y2: 0.0,
            y1p: 0.0,
            a_w: 0.0,
            a_h: 0.0,
            x: 0.0,
            y: 0.0,
            at_top: true,
            prev_a_space: 0.0,
            left_extra_indent: 0.0,
            right_extra_indent: 0.0,
        };
        f.geom();
        f.reset();
        f
    }

    fn geom(&mut self) {
        self.x2 = self.x1 + self.width;
        self.y2 = self.y1 + self.height;
        self.y1p = self.y1 + self.bottom_padding;
        self.a_w = self.x2 - self.x1 - self.left_padding - self.right_padding;
        self.a_h = self.y2 - self.y1p - self.top_padding;
    }

    pub fn reset(&mut self) {
        self.x = self.x1 + self.left_padding;
        self.y = self.y2 - self.top_padding;
        self.at_top = true;
        self.prev_a_space = 0.0;
        self.left_extra_indent = 0.0;
        self.right_extra_indent = 0.0;
    }

    pub fn available_width(&self) -> f64 {
        self.a_w - self.left_extra_indent - self.right_extra_indent
    }

    /// `Frame._add`: draws the flowable at the current position, false if it does not fit.
    pub fn add(&mut self, f: &mut dyn Flowable, canv: &mut Canvas, _try_split: bool) -> Result<bool, RlError> {
        let mut y = self.y;
        let p = self.y1p;
        let mut s = 0.0;
        let aw = self.available_width();
        let zero = f.zero_size();
        if !self.at_top {
            s = f.space_before();
            if self.o_a_space {
                if f.space_transfer() || zero {
                    s = self.prev_a_space;
                }
                s = f64::max(s - self.prev_a_space, 0.0);
            }
        }
        let h = y - p - s;
        let (w, mut h) = if h > 0.0 || zero { f.wrap(canv, aw, h)? } else { return Ok(false) };
        h += s;
        y -= h;
        if y < p - FUZZ {
            // rl_config.allowTableBoundsErrors = 1: never a LayoutError here
            return Ok(false);
        }
        let mut sa = f.space_after();
        f.draw_on(canv, self.x + self.left_extra_indent, y, aw - w)?;
        y -= sa;
        if self.o_a_space {
            if f.space_transfer() {
                sa = self.prev_a_space;
            }
            self.prev_a_space = sa;
        }
        if y != self.y {
            self.at_top = false;
        }
        self.y = y;
        Ok(true)
    }

    /// `Frame.split`.
    pub fn split(&mut self, f: &mut dyn Flowable, canv: &Canvas) -> Result<Vec<Box<dyn Flowable>>, RlError> {
        let y = self.y;
        let p = self.y1p;
        let mut s = 0.0;
        if !self.at_top {
            s = f.space_before();
            if self.o_a_space {
                s = f64::max(s - self.prev_a_space, 0.0);
            }
        }
        let h = y - p - s;
        if h <= 0.0 && !f.zero_size() {
            return Ok(Vec::new());
        }
        f.split(canv, self.a_w, h)
    }

    /// `_aSpaceString()` for error messages.
    pub fn a_space_string(&self) -> String {
        format!("({} x {}{})", py_float(self.available_width()), py_float(self.a_h), if self.at_top { "*" } else { "" })
    }
}

/// `str(float)` for messages.
pub fn py_float(v: f64) -> String {
    let s = format!("{v:?}");
    s
}
