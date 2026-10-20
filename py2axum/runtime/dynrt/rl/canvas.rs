//! reportlab/pdfgen/canvas.py with the parts of textobject.py and pathobject.py the canvas uses.
//! The canvas owns its PdfDoc (no borrow): it can be moved out of a doc template during a callback.

use super::accel::{escape_pdf, fp_str, unicode2t1};
use super::colors::Color;
use super::fonts::{self, FontId};
use super::pdfdoc::{PageRec, PdfDoc};
use super::RlError;

/// `PATH_OPS[stroke, fill, fillMode]` (fill mode 0 = even-odd, 1 = non-zero).
pub fn path_op(stroke: bool, fill: bool, fill_mode: u8) -> &'static str {
    match (stroke, fill) {
        (false, false) => "n",
        (true, false) => "S",
        (false, true) => {
            if fill_mode == 0 {
                "f*"
            } else {
                "f"
            }
        }
        (true, true) => {
            if fill_mode == 0 {
                "B*"
            } else {
                "B"
            }
        }
    }
}

pub const FILL_EVEN_ODD: u8 = 0;
pub const FILL_NON_ZERO: u8 = 1;

/// Canvas construction options (the subset SimpleDocTemplate passes).
#[derive(Clone, Default)]
pub struct CanvasOpts {
    /// None = rl_config.pageCompression (1).
    pub page_compression: Option<bool>,
    pub invariant: bool,
    pub initial_font_name: Option<String>,
    pub initial_font_size: Option<f64>,
    pub initial_leading: Option<f64>,
}

/// The graphics state saved by saveState (STATE_ATTRIBUTES that matter for output).
#[derive(Clone)]
struct GState {
    fontname: FontId,
    fontsize: f64,
    leading: f64,
    matrix: [f64; 6],
    fill_mode: u8,
    line_width: f64,
    line_cap: i32,
    line_join: i32,
    fill_color: Option<Color>,
    stroke_color: Option<Color>,
    /// ExtGState values (`_extgstate._d`): CA, ca.
    ext: Vec<(&'static str, f64)>,
}

pub struct Canvas {
    doc: PdfDoc,
    pagesize: (f64, f64),
    page_number: i64,
    page_compression: bool,
    rotation: f64,
    code: Vec<String>,
    preamble: String,
    initial_font: FontId,
    initial_size: f64,
    initial_leading: f64,
    st: GState,
    stack: Vec<GState>,
    /// ExtGState names of the current page (`_extgstate._c`, shared across save/restore).
    ext_names: Vec<(&'static str, f64, String)>,
}

impl Canvas {
    pub fn new(pagesize: (f64, f64), opts: &CanvasOpts) -> Result<Canvas, RlError> {
        let fname = opts.initial_font_name.as_deref().unwrap_or("Helvetica");
        let initial_font = font_id(fname)?;
        let initial_size = opts.initial_font_size.unwrap_or(12.0);
        let initial_leading = opts.initial_leading.unwrap_or(initial_size * 1.2);
        let mut c = Canvas {
            doc: PdfDoc::new(opts.invariant),
            pagesize,
            page_number: 1,
            page_compression: opts.page_compression.unwrap_or(true),
            rotation: 0.0,
            code: Vec::new(),
            preamble: String::new(),
            initial_font,
            initial_size,
            initial_leading,
            st: GState {
                fontname: initial_font,
                fontsize: initial_size,
                leading: initial_leading,
                matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                fill_mode: FILL_EVEN_ODD,
                line_width: 1.0,
                line_cap: 0,
                line_join: 0,
                fill_color: None,
                stroke_color: None,
                ext: Vec::new(),
            },
            stack: Vec::new(),
            ext_names: Vec::new(),
        };
        c.make_preamble();
        Ok(c)
    }

    fn init_graphics_state(&mut self) {
        self.st = GState {
            fontname: self.initial_font,
            fontsize: self.initial_size,
            leading: self.initial_leading,
            matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            fill_mode: FILL_EVEN_ODD,
            line_width: 1.0,
            line_cap: 0,
            line_join: 0,
            fill_color: None,
            stroke_color: None,
            ext: Vec::new(),
        };
        self.ext_names.clear();
    }

    fn make_preamble(&mut self) {
        // bottomup; no initial colour (canvas_baseColor None): ' '.join of an empty list
        let f = self.doc.internal_font_name(self.st.fontname);
        self.preamble = format!("1 0 0 1 0 0 cm  BT {f} 12 Tf 14.4 TL ET");
    }

    // ---- document info ----
    pub fn set_title(&mut self, t: Option<&str>) {
        self.doc.info.title = t.unwrap_or("(anonymous)").to_string();
    }
    pub fn set_author(&mut self, t: Option<&str>) {
        self.doc.info.author = t.unwrap_or("(anonymous)").to_string();
    }
    pub fn set_subject(&mut self, t: Option<&str>) {
        self.doc.info.subject = t.unwrap_or("(unspecified)").to_string();
    }
    pub fn set_creator(&mut self, t: Option<&str>) {
        self.doc.info.creator = t.unwrap_or("(unspecified)").to_string();
    }
    pub fn set_producer(&mut self, t: Option<&str>) {
        self.doc.info.producer = t.unwrap_or(super::pdfdoc::DEFAULT_PRODUCER).to_string();
    }
    /// `setKeywords`: a list is joined with ", ".
    pub fn set_keywords(&mut self, t: Option<&str>) {
        self.doc.info.keywords = t.unwrap_or("").to_string();
    }

    // ---- page ----
    pub fn get_page_number(&self) -> i64 {
        self.page_number
    }
    /// `canvas._pagesize`.
    pub fn pagesize(&self) -> (f64, f64) {
        self.pagesize
    }
    pub fn set_page_size(&mut self, size: (f64, f64)) {
        self.pagesize = size;
        self.make_preamble();
    }
    pub fn set_page_rotation(&mut self, rot: f64) {
        self.rotation = rot;
    }
    pub fn set_page_compression(&mut self, on: Option<bool>) {
        self.page_compression = on.unwrap_or(true);
    }
    /// `pageHasData` is reversed in reportlab; this is "the page has content".
    pub fn has_code(&self) -> bool {
        !self.code.is_empty()
    }

    pub fn show_page(&mut self) {
        self.code.push(" ".to_string());
        let mut s = String::with_capacity(self.preamble.len() + self.code.iter().map(|c| c.len() + 1).sum::<usize>() + 1);
        s.push_str(&self.preamble);
        s.push('\n');
        for c in &self.code {
            s.push_str(c);
            s.push('\n');
        }
        let ext = self.ext_names.iter().map(|(k, v, n)| (n.clone(), *k, *v)).collect();
        self.doc.add_page(PageRec {
            width: self.pagesize.0,
            height: self.pagesize.1,
            rotate: self.rotation,
            stream: s,
            compression: self.page_compression,
            ext_gstate: ext,
        });
        self.page_number += 1;
        self.code.clear();
        self.init_graphics_state();
        self.stack.clear();
    }

    /// `getpdfdata()`: shows the pending page, then the whole file.
    pub fn get_pdf_data(&mut self) -> Vec<u8> {
        if !self.code.is_empty() {
            self.show_page();
        }
        self.doc.get_pdf_data()
    }

    pub fn add_literal(&mut self, s: &str) {
        self.code.push(s.to_string());
    }

    // ---- transformations ----
    pub fn transform(&mut self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) {
        let [a0, b0, c0, d0, e0, f0] = self.st.matrix;
        self.st.matrix = [a0 * a + c0 * b, b0 * a + d0 * b, a0 * c + c0 * d, b0 * c + d0 * d, a0 * e + c0 * f + e0, b0 * e + d0 * f + f0];
        if let Some(last) = self.code.last() {
            if last.ends_with(" cm") {
                let l: Vec<&str> = last.split_whitespace().collect();
                if l.len() == 7 {
                    let v: Vec<f64> = l[..6].iter().map(|x| x.parse().unwrap_or(0.0)).collect();
                    let (a0, b0, c0, d0, e0, f0) = (v[0], v[1], v[2], v[3], v[4], v[5]);
                    let s = format!(
                        "{} cm",
                        fp_str(&[a0 * a + c0 * b, b0 * a + d0 * b, a0 * c + c0 * d, b0 * c + d0 * d, a0 * e + c0 * f + e0, b0 * e + d0 * f + f0])
                    );
                    *self.code.last_mut().unwrap() = s;
                    return;
                }
            }
        }
        self.code.push(format!("{} cm", fp_str(&[a, b, c, d, e, f])));
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.transform(1.0, 0.0, 0.0, 1.0, dx, dy);
    }
    pub fn scale(&mut self, x: f64, y: f64) {
        self.transform(x, 0.0, 0.0, y, 0.0, 0.0);
    }
    pub fn rotate(&mut self, theta: f64) {
        let c = (theta * std::f64::consts::PI / 180.0).cos();
        let s = (theta * std::f64::consts::PI / 180.0).sin();
        self.transform(c, s, -s, c, 0.0, 0.0);
    }
    pub fn skew(&mut self, alpha: f64, beta: f64) {
        let ta = (alpha * std::f64::consts::PI / 180.0).tan();
        let tb = (beta * std::f64::consts::PI / 180.0).tan();
        self.transform(1.0, ta, tb, 1.0, 0.0, 0.0);
    }
    /// `absolutePosition(x, y)`.
    pub fn absolute_position(&self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.st.matrix;
        (a * x + c * y + e, b * x + d * y + f)
    }

    // ---- graphics state ----
    pub fn save_state(&mut self) {
        self.stack.push(self.st.clone());
        self.code.push("q".to_string());
    }
    pub fn restore_state(&mut self) -> Result<(), RlError> {
        self.code.push("Q".to_string());
        self.st = self.stack.pop().ok_or_else(|| RlError::Index("pop from empty list".into()))?;
        Ok(())
    }

    pub fn set_line_width(&mut self, w: f64) {
        self.st.line_width = w;
        self.code.push(format!("{} w", fp_str(&[w])));
    }
    pub fn set_line_cap(&mut self, mode: i32) -> Result<(), RlError> {
        if !(0..=2).contains(&mode) {
            return Err(RlError::Value("Line caps allowed: 0=butt,1=round,2=square".into()));
        }
        self.st.line_cap = mode;
        self.code.push(format!("{mode} J"));
        Ok(())
    }
    pub fn set_line_join(&mut self, mode: i32) -> Result<(), RlError> {
        if !(0..=2).contains(&mode) {
            return Err(RlError::Value("Line Joins allowed: 0=mitre, 1=round, 2=bevel".into()));
        }
        self.st.line_join = mode;
        self.code.push(format!("{mode} j"));
        Ok(())
    }
    pub fn set_miter_limit(&mut self, limit: f64) {
        self.code.push(format!("{} M", fp_str(&[limit])));
    }
    /// `setDash(array, phase)`; phase is written with str() (ints stay ints: pass `phase_str`).
    pub fn set_dash(&mut self, array: &[f64], phase_str: &str) {
        self.code.push(format!("[{}] {} d", fp_str(array), phase_str));
    }
    pub fn set_fill_mode(&mut self, mode: u8) {
        self.st.fill_mode = mode;
    }

    // ---- colours ----
    pub fn color_code(c: &Color, stroke: bool) -> String {
        match c {
            Color::Rgb { r, g, b, .. } => format!("{} {}", fp_str(&[*r, *g, *b]), if stroke { "RG" } else { "rg" }),
            Color::Cmyk { c, m, y, k, density, .. } => {
                let d = *density;
                format!("{} {}", fp_str(&[d * c, d * m, d * y, d * k]), if stroke { "K" } else { "k" })
            }
        }
    }
    pub fn set_fill_color(&mut self, c: &Color, alpha: Option<f64>) -> Result<(), RlError> {
        let code = Canvas::color_code(c, false);
        self.code.push(code);
        self.st.fill_color = Some(c.clone());
        if let Some(a) = alpha.or(c.alpha()) {
            self.set_fill_alpha(a)?;
        }
        Ok(())
    }
    pub fn set_stroke_color(&mut self, c: &Color, alpha: Option<f64>) -> Result<(), RlError> {
        let code = Canvas::color_code(c, true);
        self.code.push(code);
        self.st.stroke_color = Some(c.clone());
        if let Some(a) = alpha.or(c.alpha()) {
            self.set_stroke_alpha(a)?;
        }
        Ok(())
    }
    pub fn set_fill_color_rgb(&mut self, r: f64, g: f64, b: f64, alpha: Option<f64>) -> Result<(), RlError> {
        self.set_fill_color(&Color::tuple3(r, g, b), alpha)
    }
    pub fn set_stroke_color_rgb(&mut self, r: f64, g: f64, b: f64, alpha: Option<f64>) -> Result<(), RlError> {
        self.set_stroke_color(&Color::tuple3(r, g, b), alpha)
    }
    pub fn set_fill_color_cmyk(&mut self, c: f64, m: f64, y: f64, k: f64, alpha: Option<f64>) -> Result<(), RlError> {
        self.set_fill_color(&Color::tuple4(c, m, y, k), alpha)
    }
    pub fn set_stroke_color_cmyk(&mut self, c: f64, m: f64, y: f64, k: f64, alpha: Option<f64>) -> Result<(), RlError> {
        self.set_stroke_color(&Color::tuple4(c, m, y, k), alpha)
    }
    pub fn set_fill_gray(&mut self, gray: f64, alpha: Option<f64>) -> Result<(), RlError> {
        self.st.fill_color = Some(Color::tuple3(gray, gray, gray));
        self.code.push(format!("{} g", fp_str(&[gray])));
        if let Some(a) = alpha {
            self.set_fill_alpha(a)?;
        }
        Ok(())
    }
    /// `setStrokeGray`: its alpha goes to the fill alpha, as in reportlab.
    pub fn set_stroke_gray(&mut self, gray: f64, alpha: Option<f64>) -> Result<(), RlError> {
        self.st.stroke_color = Some(Color::tuple3(gray, gray, gray));
        self.code.push(format!("{} G", fp_str(&[gray])));
        if let Some(a) = alpha {
            self.set_fill_alpha(a)?;
        }
        Ok(())
    }
    pub fn set_fill_alpha(&mut self, a: f64) -> Result<(), RlError> {
        if !(0.0..=1.0).contains(&a) {
            return Err(RlError::Value(format!("setFillAlpha invalid value {a}")));
        }
        self.doc.ensure_transparency();
        self.ext_set("ca", a);
        Ok(())
    }
    pub fn set_stroke_alpha(&mut self, a: f64) -> Result<(), RlError> {
        if !(0.0..=1.0).contains(&a) {
            return Err(RlError::Value(format!("setStrokeAlpha invalid value {a}")));
        }
        self.doc.ensure_transparency();
        self.ext_set("CA", a);
        Ok(())
    }
    /// `ExtGState.set` for the numeric keys.
    /// `setFillAlpha` on a text object: same checks and ExtGState, the code returned for the text object.
    pub fn fill_alpha_code(&mut self, a: f64) -> Result<Option<String>, RlError> {
        if !(0.0..=1.0).contains(&a) {
            return Err(RlError::Value(format!("setFillAlpha invalid value {a}")));
        }
        self.doc.ensure_transparency();
        let n = self.code.len();
        self.ext_set("ca", a);
        Ok(if self.code.len() > n { self.code.pop() } else { None })
    }
    fn ext_set(&mut self, key: &'static str, v: f64) {
        let cur = self.st.ext.iter().find(|(k, _)| *k == key).map(|(_, x)| *x).unwrap_or(1.0);
        if v != cur {
            match self.st.ext.iter_mut().find(|(k, _)| *k == key) {
                Some(e) => e.1 = v,
                None => self.st.ext.push((key, v)),
            }
            let name = match self.ext_names.iter().find(|(k, x, _)| *k == key && *x == v) {
                Some((_, _, n)) => n.clone(),
                None => {
                    let n = format!("gRLs{}", self.ext_names.len());
                    self.ext_names.push((key, v, n.clone()));
                    n
                }
            };
            self.code.push(format!("/{name} gs"));
        }
    }

    // ---- lines and shapes ----
    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64) {
        self.code.push(format!("n {} m {} l S", fp_str(&[x1, y1]), fp_str(&[x2, y2])));
    }
    pub fn lines(&mut self, l: &[(f64, f64, f64, f64)]) {
        self.code.push("n".to_string());
        for &(x1, y1, x2, y2) in l {
            self.code.push(format!("{} m {} l", fp_str(&[x1, y1]), fp_str(&[x2, y2])));
        }
        self.code.push("S".to_string());
    }
    pub fn grid(&mut self, xlist: &[f64], ylist: &[f64]) {
        let (y0, y1) = (ylist[0], ylist[ylist.len() - 1]);
        let (x0, x1) = (xlist[0], xlist[xlist.len() - 1]);
        let mut l = Vec::new();
        for &x in xlist {
            l.push((x, y0, x, y1));
        }
        for &y in ylist {
            l.push((x0, y, x1, y));
        }
        self.lines(&l);
    }
    pub fn bezier(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64, x4: f64, y4: f64) {
        self.code.push(format!("n {} m {} c S", fp_str(&[x1, y1]), fp_str(&[x2, y2, x3, y3, x4, y4])));
    }
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, stroke: bool, fill: bool) {
        self.code.push(format!("n {} re {}", fp_str(&[x, y, w, h]), path_op(stroke, fill, self.st.fill_mode)));
    }
    pub fn ellipse(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, stroke: bool, fill: bool) {
        let mut p = PathObject::new();
        p.ellipse(x1, y1, x2 - x1, y2 - y1);
        self.code.extend(p.code);
        self.code.push(path_op(stroke, fill, self.st.fill_mode).to_string());
    }
    pub fn circle(&mut self, xc: f64, yc: f64, r: f64, stroke: bool, fill: bool) {
        self.ellipse(xc - r, yc - r, xc + r, yc + r, stroke, fill);
    }
    pub fn arc(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, start: f64, extent: f64) {
        let mut p = PathObject::new();
        p.arc(x1, y1, x2, y2, start, extent);
        self.code.extend(p.code);
        self.code.push(path_op(true, false, self.st.fill_mode).to_string());
    }
    pub fn wedge(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, start: f64, extent: f64, stroke: bool, fill: bool) {
        let mut p = PathObject::new();
        p.move_to(0.5 * (x1 + x2), 0.5 * (y1 + y2));
        p.arc_to(x1, y1, x2, y2, start, extent);
        p.close();
        self.code.extend(p.code);
        self.code.push(path_op(stroke, fill, self.st.fill_mode).to_string());
    }
    pub fn round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, radius: f64, stroke: bool, fill: bool) {
        let mut p = PathObject::new();
        p.round_rect(x, y, w, h, radius);
        self.code.extend(p.code);
        self.code.push(path_op(stroke, fill, self.st.fill_mode).to_string());
    }

    // ---- paths ----
    pub fn begin_path(&self) -> PathObject {
        PathObject::new()
    }
    pub fn draw_path(&mut self, p: &PathObject, stroke: bool, fill: bool, fill_mode: Option<u8>) {
        let fm = fill_mode.or(p.fill_mode).unwrap_or(self.st.fill_mode);
        self.code.push(p.get_code());
        self.code.push(path_op(stroke, fill, fm).to_string());
    }
    pub fn clip_path(&mut self, p: &PathObject, stroke: bool, fill: bool, fill_mode: Option<u8>) {
        let fm = fill_mode.or(p.fill_mode).unwrap_or(self.st.fill_mode);
        let clip = if fm == FILL_EVEN_ODD { " W* " } else { " W " };
        self.code.push(format!("{}{}{}", p.get_code(), clip, path_op(stroke, fill, fm)));
    }

    // ---- text ----
    pub fn set_font(&mut self, name: &str, size: f64, leading: Option<f64>) -> Result<(), RlError> {
        let f = font_id(name)?;
        let leading = leading.unwrap_or(size * 1.2);
        self.st.fontname = f;
        self.st.fontsize = size;
        self.st.leading = leading;
        let pdfname = self.doc.internal_font_name(f);
        self.code.push(format!("BT {} {} Tf {} TL ET", pdfname, fp_str(&[size]), fp_str(&[leading])));
        Ok(())
    }
    pub fn set_font_size(&mut self, size: Option<f64>, leading: Option<f64>) {
        let size = size.unwrap_or(self.st.fontsize);
        let leading = leading.unwrap_or(self.st.leading);
        let name = fonts::font(self.st.fontname).name;
        let _ = self.set_font(name, size, Some(leading));
    }
    pub fn font_name(&self) -> &'static str {
        fonts::font(self.st.fontname).name
    }
    pub fn font_size(&self) -> f64 {
        self.st.fontsize
    }
    pub fn leading(&self) -> f64 {
        self.st.leading
    }
    pub fn string_width(&self, text: &str, font: Option<&str>, size: Option<f64>) -> Result<f64, RlError> {
        let f = match font {
            Some(n) => font_id(n)?,
            None => self.st.fontname,
        };
        Ok(fonts::string_width(text, f, size.unwrap_or(self.st.fontsize)))
    }

    /// `_formatText` of a standard font: text runs in their (substitution) fonts.
    pub fn format_text(&mut self, text: &str, font: FontId, size: f64, leading: f64) -> String {
        let mut r: Vec<String> = Vec::new();
        let mut fc = font;
        for (f, t) in unicode2t1(text, font) {
            if f != fc {
                let n = self.doc.internal_font_name(f);
                r.push(format!("{} {} Tf {} TL", n, fp_str(&[size]), fp_str(&[leading])));
                fc = f;
            }
            r.push(format!("({}) Tj", escape_pdf(&t)));
        }
        if font != fc {
            let n = self.doc.internal_font_name(font);
            r.push(format!("{} {} Tf {} TL", n, fp_str(&[size]), fp_str(&[leading])));
        }
        r.join(" ")
    }

    /// A text object with one `textLine`, as drawString writes it.
    fn draw_text_line(&mut self, x: f64, y: f64, text: &str, char_space: f64, word_space: f64) {
        let mut code = vec!["BT".to_string(), format!("1 0 0 1 {} Tm", fp_str(&[x, y]))];
        if char_space != 0.0 {
            code.push(format!("{} Tc", fp_str(&[char_space])));
        }
        if word_space != 0.0 {
            code.push(format!("{} Tw", fp_str(&[word_space])));
        }
        let (f, s, l) = (self.st.fontname, self.st.fontsize, self.st.leading);
        let t = self.format_text(text, f, s, l);
        code.push(format!("{t} T*"));
        if char_space != 0.0 {
            code.push("0 Tc".to_string());
        }
        if word_space != 0.0 {
            code.push("0 Tw".to_string());
        }
        code.push("ET".to_string());
        self.code.push(code.join(" "));
    }

    pub fn draw_string(&mut self, x: f64, y: f64, text: &str) {
        self.draw_string_ex(x, y, text, 0.0, 0.0);
    }
    pub fn draw_string_ex(&mut self, x: f64, y: f64, text: &str, char_space: f64, word_space: f64) {
        self.draw_text_line(x, y, text, char_space, word_space);
    }
    pub fn draw_right_string(&mut self, x: f64, y: f64, text: &str) {
        self.draw_right_string_ex(x, y, text, 0.0, 0.0);
    }
    pub fn draw_right_string_ex(&mut self, x: f64, y: f64, text: &str, char_space: f64, word_space: f64) {
        let width = self.aligned_width(text, char_space, word_space);
        self.draw_text_line(x - width, y, text, char_space, word_space);
    }
    pub fn draw_centred_string(&mut self, x: f64, y: f64, text: &str) {
        self.draw_centred_string_ex(x, y, text, 0.0, 0.0);
    }
    pub fn draw_centred_string_ex(&mut self, x: f64, y: f64, text: &str, char_space: f64, word_space: f64) {
        let width = self.aligned_width(text, char_space, word_space);
        self.draw_text_line(x - 0.5 * width, y, text, char_space, word_space);
    }
    fn aligned_width(&self, text: &str, char_space: f64, word_space: f64) -> f64 {
        let mut width = fonts::string_width(text, self.st.fontname, self.st.fontsize);
        if char_space != 0.0 {
            width += (text.chars().count() as f64 - 1.0) * char_space;
        }
        if word_space != 0.0 {
            let n = text.chars().filter(|&c| c == ' ' || c == '\u{a0}').count() as f64;
            width += (n - 1.0) * word_space;
        }
        width
    }

    /// Pushes the raw code of a text object built elsewhere (paragraphs).
    pub fn push_code(&mut self, s: String) {
        self.code.push(s);
    }
    /// The internal name (/F<n>) of a font, registering it.
    pub fn internal_font_name(&mut self, name: &str) -> Result<String, RlError> {
        let f = font_id(name)?;
        Ok(self.doc.internal_font_name(f))
    }
}

pub fn font_id(name: &str) -> Result<FontId, RlError> {
    // pdfmetrics.getTypeFace: KeyError(faceName) when no AFM file is found either
    fonts::font_by_name(name).ok_or_else(|| RlError::Key(name.to_string()))
}

/// `pdfgeom.bezierArc`.
pub fn bezier_arc(x1: f64, y1: f64, x2: f64, y2: f64, start: f64, extent: f64) -> Vec<[f64; 8]> {
    use std::f64::consts::PI;
    let (x1, y1, x2, y2) = (x1.min(x2), y1.max(y2), x1.max(x2), y1.min(y2));
    let (frag, nfrag) = if extent.abs() <= 90.0 {
        (extent, 1)
    } else {
        let n = (extent.abs() / 90.0).ceil() as usize;
        (extent / n as f64, n)
    };
    let xc = (x1 + x2) / 2.0;
    let yc = (y1 + y2) / 2.0;
    let rx = (x2 - x1) / 2.0;
    let ry = (y2 - y1) / 2.0;
    let half = frag * PI / 360.0;
    let kappa = (4.0 / 3.0 * (1.0 - half.cos()) / half.sin()).abs();
    let mut out = Vec::new();
    for i in 0..nfrag {
        let t0 = (start + i as f64 * frag) * PI / 180.0;
        let t1 = (start + (i + 1) as f64 * frag) * PI / 180.0;
        if frag > 0.0 {
            out.push([
                xc + rx * t0.cos(),
                yc - ry * t0.sin(),
                xc + rx * (t0.cos() - kappa * t0.sin()),
                yc - ry * (t0.sin() + kappa * t0.cos()),
                xc + rx * (t1.cos() + kappa * t1.sin()),
                yc - ry * (t1.sin() - kappa * t1.cos()),
                xc + rx * t1.cos(),
                yc - ry * t1.sin(),
            ]);
        } else {
            out.push([
                xc + rx * t0.cos(),
                yc - ry * t0.sin(),
                xc + rx * (t0.cos() + kappa * t0.sin()),
                yc - ry * (t0.sin() - kappa * t0.cos()),
                xc + rx * (t1.cos() - kappa * t1.sin()),
                yc - ry * (t1.sin() + kappa * t1.cos()),
                xc + rx * t1.cos(),
                yc - ry * t1.sin(),
            ]);
        }
    }
    out
}

/// `PDFPathObject`.
#[derive(Clone, Default)]
pub struct PathObject {
    code: Vec<String>,
    started: bool,
    /// `_fillMode` when set on the path.
    pub fill_mode: Option<u8>,
}

impl PathObject {
    pub fn new() -> PathObject {
        PathObject::default()
    }
    fn push(&mut self, c: String) {
        if !self.started {
            self.code.push("n".to_string());
            self.started = true;
        }
        self.code.push(c);
    }
    pub fn get_code(&self) -> String {
        self.code.join(" ")
    }
    pub fn move_to(&mut self, x: f64, y: f64) {
        self.push(format!("{} m", fp_str(&[x, y])));
    }
    pub fn line_to(&mut self, x: f64, y: f64) {
        self.push(format!("{} l", fp_str(&[x, y])));
    }
    pub fn curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) {
        self.push(format!("{} c", fp_str(&[x1, y1, x2, y2, x3, y3])));
    }
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.push(format!("{} re", fp_str(&[x, y, w, h])));
    }
    fn curves(&mut self, c: &[[f64; 8]], line_first: bool) {
        if line_first {
            self.line_to(c[0][0], c[0][1]);
        } else {
            self.move_to(c[0][0], c[0][1]);
        }
        for k in c {
            self.curve_to(k[2], k[3], k[4], k[5], k[6], k[7]);
        }
    }
    pub fn arc(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, start: f64, extent: f64) {
        self.curves(&bezier_arc(x1, y1, x2, y2, start, extent), false);
    }
    pub fn arc_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, start: f64, extent: f64) {
        self.curves(&bezier_arc(x1, y1, x2, y2, start, extent), true);
    }
    pub fn ellipse(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.curves(&bezier_arc(x, y, x + w, y + h, 0.0, 360.0), false);
    }
    pub fn circle(&mut self, xc: f64, yc: f64, r: f64) {
        let (x1, y1) = (xc - r, yc - r);
        let wh = 2.0 * r;
        self.ellipse(x1, y1, wh, wh);
    }
    pub fn round_rect(&mut self, x: f64, y: f64, width: f64, height: f64, radius: f64) {
        let m = 0.4472;
        let (xlo, xhi) = (x.min(x + width), x.max(x + width));
        let (ylo, yhi) = (y.min(y + height), y.max(y + height));
        let t = m * radius;
        self.move_to(xlo + radius, ylo);
        self.line_to(xhi - radius, ylo);
        self.curve_to(xhi - t, ylo, xhi, ylo + t, xhi, ylo + radius);
        self.line_to(xhi, yhi - radius);
        self.curve_to(xhi, yhi - t, xhi - t, yhi, xhi - radius, yhi);
        self.line_to(xlo + radius, yhi);
        self.curve_to(xlo + t, yhi, xlo, yhi - t, xlo, yhi - radius);
        self.line_to(xlo, ylo + radius);
        self.curve_to(xlo, ylo + t, xlo + t, ylo, xlo + radius, ylo);
        self.close();
    }
    pub fn close(&mut self) {
        self.push("h".to_string());
    }
}
