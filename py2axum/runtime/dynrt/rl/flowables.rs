//! The Flowable protocol of reportlab/platypus/flowables.py, with Spacer and PageBreak.

use super::canvas::Canvas;
use super::RlError;

/// `hAlign` values ('LEFT', 'CENTER'/'CENTRE', 'RIGHT').
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// How the doc template treats a flowable before asking a frame (handle_flowable's isinstance tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowKind {
    /// Wrapped, split and drawn through the frame.
    Normal,
    /// `PageBreak` (nextTemplate None): ends the page.
    PageBreak,
}

/// A thing the frame lays out. Defaults are those of `Flowable` (attributes absent unless overridden).
pub trait Flowable {
    /// `wrap(availWidth, availHeight) -> (width, height)`; the canvas is `self.canv` in reportlab.
    fn wrap(&mut self, canv: &Canvas, avail_width: f64, avail_height: f64) -> Result<(f64, f64), RlError>;

    /// `split(availWidth, availHeight)`: the pieces, or nothing if no useful piece fits.
    fn split(&mut self, _canv: &Canvas, _avail_width: f64, _avail_height: f64) -> Result<Vec<Box<dyn Flowable>>, RlError> {
        Ok(Vec::new())
    }

    /// `draw()`: in the flowable's own coordinates (drawOn has translated).
    fn draw(&mut self, canv: &mut Canvas) -> Result<(), RlError>;

    /// `drawOn(canvas, x, y, _sW)`: hAlign adjustment, saveState, translate, draw, restoreState.
    fn draw_on(&mut self, canv: &mut Canvas, x: f64, y: f64, sw: f64) -> Result<(), RlError> {
        let x = h_align_adjust(self.h_align(), x, sw);
        canv.save_state();
        canv.translate(x, y);
        self.draw(canv)?;
        canv.restore_state()
    }

    /// `hAlign`, None when the attribute does not exist (Spacer, PageBreak never call Flowable.__init__).
    fn h_align(&self) -> Option<HAlign> {
        Some(HAlign::Left)
    }
    /// `getSpaceBefore()`.
    fn space_before(&self) -> f64 {
        0.0
    }
    /// `getSpaceAfter()`.
    fn space_after(&self) -> f64 {
        0.0
    }
    /// `getKeepWithNext()`.
    fn keep_with_next(&self) -> bool {
        false
    }
    fn kind(&self) -> FlowKind {
        FlowKind::Normal
    }
    /// `_ZEROSIZE`.
    fn zero_size(&self) -> bool {
        false
    }
    /// `_SPACETRANSFER`.
    fn space_transfer(&self) -> bool {
        false
    }
    /// `(width, height)` attributes as `_fSizeString` reads them (error messages).
    fn size(&self) -> Option<(f64, f64)> {
        None
    }
    /// Class name for `identity()` in error messages.
    fn class_name(&self) -> &'static str {
        "Flowable"
    }
    /// `_fSizeString(f)` of a LayoutError: the `width` and `height` attributes as Python prints them.
    fn size_string(&self) -> String {
        match self.size() {
            Some((w, h)) => format!("({} x {})", super::frames::py_float(w), super::frames::py_float(h)),
            None => String::new(),
        }
    }
    /// `identity(maxLen)` with the frame's ` frame=<id>` part: `<Class at 0x...>` then the text (`...` when
    /// the flowable has none). The address is a placeholder (CPython's `id()` cannot be reproduced).
    fn identity(&self, _max_len: usize, frame: &str) -> String {
        format!("<{} at 0x0{frame}>...", self.class_name())
    }
    /// `minWidth()` (tables size auto-width columns with it).
    fn min_width(&self) -> Result<f64, RlError> {
        Err(RlError::Unsupported(format!("{} in a table cell", self.class_name())))
    }
    /// `_fixedWidth` (then `_elementWidth` reads `width`, as `size().0`).
    fn fixed_width(&self) -> bool {
        false
    }
}

/// `Flowable._hAlignAdjust`.
pub fn h_align_adjust(a: Option<HAlign>, x: f64, sw: f64) -> f64 {
    if sw != 0.0 {
        match a {
            Some(HAlign::Center) => return x + 0.5 * sw,
            Some(HAlign::Right) => return x + sw,
            _ => {}
        }
    }
    x
}

/// `Spacer(width, height)`.
pub struct Spacer {
    pub width: f64,
    pub height: f64,
    /// `str()` of the width and height as given (an int prints without `.0`), for error messages.
    pub size_repr: Option<(String, String)>,
}

impl Spacer {
    pub fn new(width: f64, height: f64) -> Spacer {
        Spacer { width, height, size_repr: None }
    }
}

impl Flowable for Spacer {
    fn wrap(&mut self, _c: &Canvas, _w: f64, _h: f64) -> Result<(f64, f64), RlError> {
        Ok((self.width, self.height))
    }
    fn draw(&mut self, _c: &mut Canvas) -> Result<(), RlError> {
        Ok(())
    }
    fn h_align(&self) -> Option<HAlign> {
        None
    }
    fn size(&self) -> Option<(f64, f64)> {
        Some((self.width, self.height))
    }
    fn size_string(&self) -> String {
        match &self.size_repr {
            Some((w, h)) => format!("({w} x {h})"),
            None => format!("({} x {})", super::frames::py_float(self.width), super::frames::py_float(self.height)),
        }
    }
    fn class_name(&self) -> &'static str {
        "Spacer"
    }
    fn fixed_width(&self) -> bool {
        true
    }
}

/// `PageBreak()` (no nextTemplate).
#[derive(Default)]
pub struct PageBreak {
    width: f64,
    height: f64,
}

impl PageBreak {
    pub fn new() -> PageBreak {
        PageBreak::default()
    }
}

impl Flowable for PageBreak {
    /// `UseUpSpace.wrap` (only reached when a PageBreak is wrapped by a container).
    fn wrap(&mut self, _c: &Canvas, w: f64, h: f64) -> Result<(f64, f64), RlError> {
        self.width = w;
        self.height = h;
        Ok((w, h - 1e-8))
    }
    fn draw(&mut self, _c: &mut Canvas) -> Result<(), RlError> {
        Ok(())
    }
    fn h_align(&self) -> Option<HAlign> {
        None
    }
    fn kind(&self) -> FlowKind {
        FlowKind::PageBreak
    }
    fn class_name(&self) -> &'static str {
        "PageBreak"
    }
}
