//! Native port of the subset of reportlab 4.4.5 applications use (pure Python path, no _rl_accel):
//! the output PDF is byte for byte reportlab's, except /CreationDate, /ModDate and /ID.
//! Depends only on std, flate2 (system zlib), md-5 and chrono; nothing from the rest of dynrt.

pub mod accel;
pub mod canvas;
pub mod colors;
pub mod doctemplate;
pub mod entities;
pub mod flowables;
pub mod fontdata;
pub mod fonts;
pub mod frames;
pub mod graphics;
pub mod hr;
pub mod paragraph;
pub mod paraparser;
pub mod pdfdoc;
pub mod styles;
pub mod tables;
pub mod textobject;

pub use canvas::{Canvas, CanvasOpts, PathObject};
pub use colors::Color;
pub use doctemplate::{DocInfo, SimpleDocTemplate};
pub use flowables::{Flowable, FlowKind, HAlign, PageBreak, Spacer};
pub use paragraph::Paragraph;
pub use tables::{CellInput, CmdVal, ColWidth, RepeatRows, Sizes, Table, TableCmd, TableOpts};
pub use styles::{sample_style_sheet, ParagraphStyle, StyleSheet, StyleVal};

/// Errors raised by reportlab (the Python class is named in each variant).
#[derive(Debug, Clone, PartialEq)]
pub enum RlError {
    /// reportlab.platypus.doctemplate.LayoutError
    Layout(String),
    /// reportlab.pdfbase.pdfdoc.PDFError / KeyError on an unknown font
    Font(String),
    /// ValueError (and the IndexError of an unbalanced restoreState)
    Value(String),
    /// KeyError (unknown style in a stylesheet)
    Key(String),
    /// AttributeError / TypeError raised by the paragraph parser on malformed markup
    Attribute(String),
    Type(String),
    /// IndexError
    Index(String),
    /// Input the port does not reproduce (markup or style feature outside the subset): never approximated
    Unsupported(String),
    /// `annotateException`: the error raised (same class, message prefixed) and the one it was raised while
    /// handling (its `__context__`, which tracebacks print)
    Context(Box<RlError>, Box<RlError>),
}

impl RlError {
    /// `annotateException(msg)` on a ValueError, AttributeError, TypeError or IndexError (the classes the
    /// paragraph parser raises): a new one of the same class, `msg str(e)`, raised while handling `self`.
    pub fn annotate(self, msg: &str) -> RlError {
        let s = self.to_string();
        let text = if s.is_empty() { msg.to_string() } else { format!("{msg} {s}") };
        let outer = match self.class_of() {
            RlError::Value(_) => RlError::Value(text),
            RlError::Attribute(_) => RlError::Attribute(text),
            RlError::Type(_) => RlError::Type(text),
            RlError::Index(_) => RlError::Index(text),
            _ => return self,
        };
        RlError::Context(Box::new(outer), Box::new(self))
    }

    /// The error that is raised (the outer one of a Context).
    pub fn class_of(&self) -> &RlError {
        match self {
            RlError::Context(o, _) => o.class_of(),
            e => e,
        }
    }
}

impl std::fmt::Display for RlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RlError::Layout(s)
            | RlError::Font(s)
            | RlError::Value(s)
            | RlError::Key(s)
            | RlError::Attribute(s)
            | RlError::Type(s)
            | RlError::Index(s)
            | RlError::Unsupported(s) => f.write_str(s),
            RlError::Context(o, _) => o.fmt(f),
        }
    }
}

impl std::error::Error for RlError {}

/// reportlab.lib.units
pub const INCH: f64 = 72.0;
pub fn cm() -> f64 {
    INCH / 2.54
}
pub fn mm() -> f64 {
    cm() * 0.1
}

/// reportlab.lib.pagesizes.A4 = (210*mm, 297*mm)
pub fn a4() -> (f64, f64) {
    (210.0 * mm(), 297.0 * mm())
}

/// reportlab.lib.pagesizes.LETTER
pub fn letter() -> (f64, f64) {
    (8.5 * INCH, 11.0 * INCH)
}
