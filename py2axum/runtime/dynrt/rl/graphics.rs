//! reportlab.graphics: shapes.py (Drawing, Group, Rect, Circle, Ellipse, Line, PolyLine, Polygon),
//! transform.py and the PDF renderer of renderPDF.py / renderbase.py.
//! Each node re-emits its whole state delta (no diffing), exactly like _PDFRenderer.

use super::canvas::Canvas;
use super::colors::Color;
use super::flowables::{Flowable, HAlign};
use super::RlError;

/// A 2D affine transform `(a, b, c, d, e, f)`.
pub type Transform = [f64; 6];

pub const IDENTITY: Transform = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `transform.mmult(A, B)`: A postmultiplied by B.
pub fn mmult(a: &Transform, b: &Transform) -> Transform {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

/// `transform.rotate(angle, cx, cy)` (cx, cy land in e, f as in reportlab).
pub fn rotate_t(angle: f64, cx: f64, cy: f64) -> Transform {
    let a = angle.to_radians();
    let (s, c) = (a.sin(), a.cos());
    [c, s, -s, c, cx, cy]
}

/// `strokeDashArray`: `[numbers]`, or `[phase, [numbers]]`; `phase` is written with str().
#[derive(Clone, Debug, PartialEq)]
pub struct Dash {
    pub array: Vec<f64>,
    pub phase: String,
}

impl Dash {
    pub fn new(array: Vec<f64>) -> Dash {
        Dash { array, phase: "0".into() }
    }
    pub fn phased(phase: &str, array: Vec<f64>) -> Dash {
        Dash { array, phase: phase.to_string() }
    }
}

/// `canvas.setDash(array, phase)` with its validation.
pub fn set_dash_checked(canv: &mut Canvas, array: &[f64], phase: &str) -> Result<(), RlError> {
    let ph: f64 = phase.parse().unwrap_or(-1.0);
    let bad = array.iter().any(|&x| x < 0.0) || ph < 0.0;
    let reason = if bad {
        "array & phase should be non-negative numbers"
    } else if !array.is_empty() && array.iter().sum::<f64>() <= 0.0 {
        "dash cycle should be larger than zero"
    } else {
        ""
    };
    if !reason.is_empty() {
        return Err(RlError::Value(format!("setDash: array={array:?} phase={phase}\n{reason}")));
    }
    canv.set_dash(array, phase);
    Ok(())
}

/// The STATE_DEFAULTS keys a LineShape/SolidShape carries, in their `__dict__` order.
#[derive(Clone, Debug)]
pub struct Style {
    /// SolidShape only (fillColor, fillOpacity come first in its __dict__).
    solid: bool,
    /// None = `fillColor=None` (no fill).
    pub fill_color: Option<Color>,
    pub fill_opacity: Option<f64>,
    /// None = `strokeColor=None` (no stroke).
    pub stroke_color: Option<Color>,
    pub stroke_width: f64,
    pub stroke_line_cap: i32,
    pub stroke_line_join: i32,
    /// Ignored by renderPDF (no branch for it).
    pub stroke_miter_limit: f64,
    pub stroke_dash_array: Option<Dash>,
    pub stroke_opacity: Option<f64>,
    /// `fillMode=` keyword (absent unless given: appended last to __dict__).
    pub fill_mode: Option<u8>,
}

fn black() -> Color {
    Color::named("black").expect("colors.black")
}

impl Style {
    /// SolidShape defaults: black fill and stroke, width 1.
    pub fn solid() -> Style {
        Style {
            solid: true,
            fill_color: Some(black()),
            fill_opacity: None,
            stroke_color: Some(black()),
            stroke_width: 1.0,
            stroke_line_cap: 0,
            stroke_line_join: 0,
            stroke_miter_limit: 0.0,
            stroke_dash_array: None,
            stroke_opacity: None,
            fill_mode: None,
        }
    }
    /// LineShape defaults (no fill keys).
    pub fn line() -> Style {
        Style { solid: false, fill_color: None, ..Style::solid() }
    }
}

#[derive(Clone, Debug)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub rx: f64,
    pub ry: f64,
    pub style: Style,
}

#[derive(Clone, Debug)]
pub struct Circle {
    pub cx: f64,
    pub cy: f64,
    pub r: f64,
    pub style: Style,
}

#[derive(Clone, Debug)]
pub struct Ellipse {
    pub cx: f64,
    pub cy: f64,
    pub rx: f64,
    pub ry: f64,
    pub style: Style,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub style: Style,
}

/// Polygon (closed, SolidShape) and PolyLine (open, LineShape): flat `[x1, y1, x2, y2, ...]`.
#[derive(Clone, Debug)]
pub struct Poly {
    pub points: Vec<f64>,
    pub style: Style,
}

/// `Group(...)`: contents and a transform.
#[derive(Clone, Debug)]
pub struct Group {
    pub contents: Vec<Node>,
    pub transform: Transform,
}

#[derive(Clone, Debug)]
pub enum Node {
    Group(Group),
    Rect(Rect),
    Circle(Circle),
    Ellipse(Ellipse),
    Line(Line),
    Polygon(Poly),
    PolyLine(Poly),
}

impl Rect {
    /// `Rect(x, y, width, height)` (rx = ry = 0, SolidShape defaults).
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect { x, y, width, height, rx: 0.0, ry: 0.0, style: Style::solid() }
    }
}
impl Circle {
    pub fn new(cx: f64, cy: f64, r: f64) -> Circle {
        Circle { cx, cy, r, style: Style::solid() }
    }
}
impl Ellipse {
    pub fn new(cx: f64, cy: f64, rx: f64, ry: f64) -> Ellipse {
        Ellipse { cx, cy, rx, ry, style: Style::solid() }
    }
}
impl Line {
    pub fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Line {
        Line { x1, y1, x2, y2, style: Style::line() }
    }
}

/// `Polygon(points)`.
pub fn polygon(points: Vec<f64>) -> Result<Node, RlError> {
    check_points(&points)?;
    Ok(Node::Polygon(Poly { points, style: Style::solid() }))
}

/// `PolyLine(points)`.
pub fn polyline(points: Vec<f64>) -> Result<Node, RlError> {
    check_points(&points)?;
    Ok(Node::PolyLine(Poly { points, style: Style::line() }))
}

fn check_points(points: &[f64]) -> Result<(), RlError> {
    if points.len() % 2 != 0 {
        return Err(RlError::Value("Point list must have even number of elements!".into()));
    }
    Ok(())
}

impl Node {
    /// The shape's style (None for a Group).
    pub fn style_mut(&mut self) -> Option<&mut Style> {
        match self {
            Node::Group(_) => None,
            Node::Rect(s) => Some(&mut s.style),
            Node::Circle(s) => Some(&mut s.style),
            Node::Ellipse(s) => Some(&mut s.style),
            Node::Line(s) => Some(&mut s.style),
            Node::Polygon(s) | Node::PolyLine(s) => Some(&mut s.style),
        }
    }
}

impl Default for Group {
    fn default() -> Group {
        Group::new()
    }
}

impl Group {
    pub fn new() -> Group {
        Group { contents: Vec::new(), transform: IDENTITY }
    }
    pub fn add(&mut self, n: Node) {
        self.contents.push(n);
    }
    /// `scale(sx, sy=1)`.
    pub fn scale(&mut self, sx: f64, sy: f64) {
        self.transform = mmult(&self.transform, &[sx, 0.0, 0.0, sy, 0.0, 0.0]);
    }
    /// `translate(dx, dy=0)`.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.transform = mmult(&self.transform, &[1.0, 0.0, 0.0, 1.0, dx, dy]);
    }
    /// `rotate(theta, cx=0, cy=0)`.
    pub fn rotate(&mut self, theta: f64, cx: f64, cy: f64) {
        self.transform = mmult(&self.transform, &rotate_t(theta, cx, cy));
    }
    /// `skew(kx, ky=0)`.
    pub fn skew(&mut self, kx: f64, ky: f64) {
        let sx = [1.0, 0.0, kx.to_radians().tan(), 1.0, 0.0, 0.0];
        let sy = [1.0, ky.to_radians().tan(), 0.0, 1.0, 0.0, 0.0];
        self.transform = mmult(&mmult(&self.transform, &sx), &sy);
    }
    /// `shift(x, y=0)`.
    pub fn shift(&mut self, x: f64, y: f64) {
        self.transform[4] = x;
        self.transform[5] = y;
    }
}

/// `Drawing(width, height)`: an outermost Group that is also a Flowable.
#[derive(Clone, Debug)]
pub struct Drawing {
    pub width: f64,
    pub height: f64,
    pub group: Group,
    pub h_align: HAlign,
    /// 'BOTTOM' / 'TOP' / 'CENTER' / 'CENTRE' (used by containers such as tables).
    pub v_align: String,
    pub render_scale: f64,
    /// `initialFontName` / `initialFontSize` (STATE_DEFAULTS overrides).
    pub initial_font_name: Option<String>,
    pub initial_font_size: Option<f64>,
    /// `str()` of the width and height as given, for error messages.
    pub size_repr: Option<(String, String)>,
}

impl Drawing {
    pub fn new(width: f64, height: f64) -> Drawing {
        Drawing {
            width,
            height,
            group: Group::new(),
            h_align: HAlign::Left,
            v_align: "BOTTOM".into(),
            render_scale: 1.0,
            initial_font_name: None,
            initial_font_size: None,
            size_repr: None,
        }
    }
    pub fn add(&mut self, n: Node) {
        self.group.add(n);
    }

    /// `renderPDF.draw(drawing, canvas, x, y)` (showBoundary off).
    pub fn render(&self, canv: &mut Canvas, x: f64, y: f64) -> Result<(), RlError> {
        let mut transform = self.group.transform;
        if self.render_scale != 1.0 {
            // renderScaledDrawing: d.scale(renderScale, renderScale)
            transform = mmult(&transform, &[self.render_scale, 0.0, 0.0, self.render_scale, 0.0, 0.0]);
        }
        let mut r = Renderer { stroke: false, fill: false };
        canv.save_state();
        // initState: the whole STATE_DEFAULTS as a delta, in its key order
        canv.transform(1.0, 0.0, 0.0, 1.0, x, y);
        let black = black();
        r.stroke = true;
        canv.set_stroke_color(&black, None)?;
        canv.set_line_width(1.0);
        canv.set_line_cap(0)?;
        canv.set_line_join(0)?;
        canv.set_dash(&[], "0");
        r.fill = true;
        canv.set_fill_color(&black, None)?;
        canv.set_fill_mode(super::canvas::FILL_EVEN_ODD);
        let font = self.initial_font_name.as_deref().unwrap_or("Times-Roman"); // _baseGFontName
        let size = self.initial_font_size.unwrap_or(10.0);
        // keys fontSize then fontName: setFont twice
        canv.set_font(font, size, None)?;
        canv.set_font(font, size, None)?;
        // drawNode(drawing)
        canv.save_state();
        canv.transform(transform[0], transform[1], transform[2], transform[3], transform[4], transform[5]);
        for n in &self.group.contents {
            r.node(canv, n, &transform)?;
        }
        pop_check(&transform, &[1.0, 0.0, 0.0, 1.0, x, y])?;
        canv.restore_state()?;
        canv.restore_state()
    }
}

/// `StateTracker.pop`: a transform that differs from the parent's is inverted (ZeroDivisionError
/// when singular, raised as a ValueError here).
fn pop_check(t: &Transform, parent: &Transform) -> Result<(), RlError> {
    if t != parent && t[0] * t[3] - t[2] * t[1] == 0.0 {
        return Err(RlError::Value("division by zero".into()));
    }
    Ok(())
}

/// `_PDFRenderer`: `_stroke` and `_fill` persist across nodes (they are not restored on pop).
struct Renderer {
    stroke: bool,
    fill: bool,
}

impl Renderer {
    fn apply(&mut self, canv: &mut Canvas, s: &Style) -> Result<(), RlError> {
        if s.solid {
            self.fill_color(canv, &s.fill_color)?;
            if let Some(a) = s.fill_opacity {
                canv.set_fill_alpha(a)?;
            }
        }
        match &s.stroke_color {
            None => self.stroke = false,
            Some(c) => {
                self.stroke = true;
                canv.set_stroke_color(c, None)?;
            }
        }
        canv.set_line_width(s.stroke_width);
        canv.set_line_cap(s.stroke_line_cap)?;
        canv.set_line_join(s.stroke_line_join)?;
        match &s.stroke_dash_array {
            Some(d) if !d.array.is_empty() || d.phase != "0" => set_dash_checked(canv, &d.array, &d.phase)?,
            _ => canv.set_dash(&[], "0"),
        }
        if let Some(a) = s.stroke_opacity {
            canv.set_stroke_alpha(a)?;
        }
        if let Some(m) = s.fill_mode {
            canv.set_fill_mode(m);
        }
        Ok(())
    }

    fn fill_color(&mut self, canv: &mut Canvas, c: &Option<Color>) -> Result<(), RlError> {
        match c {
            None => self.fill = false,
            Some(c) => {
                self.fill = true;
                canv.set_fill_color(c, None)?;
            }
        }
        Ok(())
    }

    /// `drawNode`: saveState, the node's delta, dispatch, restoreState.
    fn node(&mut self, canv: &mut Canvas, n: &Node, parent: &Transform) -> Result<(), RlError> {
        canv.save_state();
        match n {
            Node::Group(g) => {
                let t = g.transform;
                canv.transform(t[0], t[1], t[2], t[3], t[4], t[5]);
                for c in &g.contents {
                    self.node(canv, c, &t)?;
                }
                pop_check(&t, parent)?;
            }
            Node::Rect(r) => {
                self.apply(canv, &r.style)?;
                if r.rx == r.ry && r.ry == 0.0 {
                    canv.rect(r.x, r.y, r.width, r.height, self.stroke, self.fill);
                } else {
                    canv.round_rect(r.x, r.y, r.width, r.height, r.rx, self.stroke, self.fill);
                }
            }
            Node::Circle(c) => {
                self.apply(canv, &c.style)?;
                canv.circle(c.cx, c.cy, c.r, self.stroke, self.fill);
            }
            Node::Ellipse(e) => {
                self.apply(canv, &e.style)?;
                canv.ellipse(e.cx - e.rx, e.cy - e.ry, e.cx + e.rx, e.cy + e.ry, self.stroke, self.fill);
            }
            Node::Line(l) => {
                self.apply(canv, &l.style)?;
                if self.stroke {
                    canv.line(l.x1, l.y1, l.x2, l.y2);
                }
            }
            Node::Polygon(p) => {
                self.apply(canv, &p.style)?;
                if p.points.len() < 2 {
                    return Err(RlError::Value("Polyline must have 2 or more points".into()));
                }
                let mut path = canv.begin_path();
                path.move_to(p.points[0], p.points[1]);
                for q in p.points[2..].chunks(2) {
                    path.line_to(q[0], q[1]);
                }
                path.close();
                canv.draw_path(&path, self.stroke, self.fill, None);
            }
            Node::PolyLine(p) => {
                self.apply(canv, &p.style)?;
                if self.stroke {
                    if p.points.len() < 2 {
                        return Err(RlError::Value("Polyline must have 2 or more points".into()));
                    }
                    let mut path = canv.begin_path();
                    path.move_to(p.points[0], p.points[1]);
                    for q in p.points[2..].chunks(2) {
                        path.line_to(q[0], q[1]);
                    }
                    // drawPath(path): stroke=1, fill=0
                    canv.draw_path(&path, true, false, None);
                }
            }
        }
        canv.restore_state()
    }
}

impl Flowable for Drawing {
    fn wrap(&mut self, _c: &Canvas, _w: f64, _h: f64) -> Result<(f64, f64), RlError> {
        let (mut w, mut h) = (self.width, self.height);
        if self.render_scale != 1.0 {
            w *= self.render_scale;
            h *= self.render_scale;
        }
        Ok((w, h))
    }
    fn draw(&mut self, canv: &mut Canvas) -> Result<(), RlError> {
        self.render(canv, 0.0, 0.0)
    }
    fn h_align(&self) -> Option<HAlign> {
        Some(self.h_align)
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
        "Drawing"
    }
    fn min_width(&self) -> Result<f64, RlError> {
        Ok(self.width)
    }
}
