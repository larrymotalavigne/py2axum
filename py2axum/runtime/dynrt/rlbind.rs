//! reportlab 4.4.5 as Python values, over the native engine of `rl/`: `SimpleDocTemplate` (build into an
//! `io.BytesIO`, page callbacks), `Paragraph`, `ParagraphStyle` and `getSampleStyleSheet()`, `Spacer`,
//! `PageBreak`, `HRFlowable`, colours, the shapes of reportlab.graphics (`Drawing`, `Group`, `Rect`,
//! `Circle`, `Ellipse`, `Line`, `Polygon`, `PolyLine`) and the canvas a page callback receives.
//! Objects keep Python's reference semantics: a Paragraph reads its style when the document is built, a
//! Group added to a Drawing and changed afterwards is drawn changed. What is outside this surface raises a
//! `py2axum:` TypeError (docs/advanced/documents.md).
use std::sync::Arc;

use parking_lot::Mutex;

use super::rl::canvas::{Canvas, CanvasOpts};
use super::rl::graphics::{self, Circle, Dash, Ellipse, Group, Line, Node, Rect, Style, Transform};
use super::rl::hr::{parse_h_align, HRFlowable, HrDash, HrWidth};
use super::rl::paraparser::{set_html_rev, HtmlRev};
use super::rl::{
    self, CellInput, CmdVal, Color, ColWidth, DocInfo, Flowable, HAlign, PageBreak, Paragraph, ParagraphStyle, RepeatRows, RlError,
    SimpleDocTemplate, Sizes, Spacer, StyleVal, Table, TableCmd, TableOpts,
};
use super::v::*;
use super::Cx;

fn unsupported(what: impl AsRef<str>) -> Exc {
    Exc::type_error(format!("py2axum: reportlab {} is not supported", what.as_ref()))
}

impl From<RlError> for Exc {
    fn from(e: RlError) -> Exc {
        match e {
            RlError::Layout(m) => Exc::msg(&RL_LAYOUT_ERROR, m),
            RlError::Value(m) => Exc::value_error(m),
            RlError::Key(m) => Exc::new(&KEY_ERROR, vec![V::str(m)]),
            RlError::Attribute(m) => Exc::attr_error(m),
            RlError::Type(m) => Exc::type_error(m),
            RlError::Index(m) => Exc::msg(&INDEX_ERROR, m),
            RlError::Font(m) => Exc::type_error(format!("py2axum: reportlab: {m}")),
            RlError::Unsupported(m) => unsupported(m),
            RlError::Context(outer, inner) => {
                let (o, i) = (Exc::from(*outer), Exc::from(*inner));
                o.0.attrs.lock().insert("__context__".into(), V::Exc(i));
                o
            }
        }
    }
}

// ---------------------------------------------------------------- objects

pub struct Doc {
    t: SimpleDocTemplate,
    file: V,
    /// keyword values as given (read back as such: `doc.leftMargin` is the int it was given)
    vals: Vec<(String, V)>,
    /// `doc.page`: set by build
    page: Option<i64>,
}

pub struct StyleObj {
    st: ParagraphStyle,
    parent: V,
    /// the values set by keyword, inherited from the parent's or assigned (the rest: the class defaults)
    vals: Vec<(String, V)>,
}

pub struct Sheet {
    by_name: Vec<(String, V)>,
    by_alias: Vec<(String, V)>,
}

/// The engine's Paragraph is not Send (its frags are shared by Rc): it is parsed again at build time, from
/// the text and the style of the construction (the frags' fonts), then laid out with the style as it is then.
pub struct Para {
    text: String,
    parsed_with: ParagraphStyle,
    style: V,
}

pub struct DrawObj {
    width: V,
    height: V,
    group: GroupObj,
    h_align: HAlign,
    v_align: String,
    render_scale: f64,
}

#[derive(Default)]
pub struct GroupObj {
    kids: Vec<V>,
    transform: Option<Transform>,
}

/// The engine's Table is not Send (shared cell flowables): it is made again at build time from the data and the
/// commands, as the constructor and setStyle checked them.
pub struct TableObj {
    data: Vec<Vec<V>>,
    opts: TableOpts,
    cmds: Vec<TableCmd>,
}

pub struct CanvasCell {
    canvas: Option<Canvas>,
    /// inside a page callback: the canvas is the document's
    active: bool,
    pagesize: V,
}

pub enum Rl {
    Doc(Mutex<Doc>),
    Style(Mutex<StyleObj>),
    Sheet(Mutex<Sheet>),
    Para(Mutex<Para>),
    Spacer(Mutex<(V, V)>),
    PageBreak,
    Hr(Mutex<HRFlowable>),
    Drawing(Mutex<DrawObj>),
    Group(Mutex<GroupObj>),
    Shape(Mutex<Node>),
    Color(Color),
    Canvas(Mutex<CanvasCell>),
    Table(Mutex<TableObj>),
    TableStyle(Mutex<Vec<TableCmd>>),
}

fn wrap(x: Rl) -> V {
    V::native(Native::Rl(Arc::new(x)))
}

fn rl_of(v: &V) -> Option<&Arc<Rl>> {
    match v {
        V::Native(n) => match &**n {
            Native::Rl(x) => Some(x),
            _ => None,
        },
        _ => None,
    }
}

pub fn type_name(x: &Rl) -> &'static str {
    match x {
        Rl::Doc(_) => "SimpleDocTemplate",
        Rl::Style(_) => "ParagraphStyle",
        Rl::Sheet(_) => "StyleSheet1",
        Rl::Para(_) => "Paragraph",
        Rl::Spacer(_) => "Spacer",
        Rl::PageBreak => "PageBreak",
        Rl::Hr(_) => "HRFlowable",
        Rl::Drawing(_) => "Drawing",
        Rl::Group(_) => "Group",
        Rl::Shape(s) => match &*s.lock() {
            Node::Rect(_) => "Rect",
            Node::Circle(_) => "Circle",
            Node::Ellipse(_) => "Ellipse",
            Node::Line(_) => "Line",
            Node::Polygon(_) => "Polygon",
            Node::PolyLine(_) => "PolyLine",
            Node::Group(_) => "Group",
        },
        Rl::Color(_) => "Color",
        Rl::Canvas(_) => "Canvas",
        Rl::Table(_) => "Table",
        Rl::TableStyle(_) => "TableStyle",
    }
}

// ---------------------------------------------------------------- arguments

/// `name`'s argument: positional `i`, else the keyword.
fn arg<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v))
}

fn num(v: &V, what: &str) -> R<f64> {
    match v {
        V::Int(n) => Ok(*n as f64),
        V::Float(x) => Ok(*x),
        V::Bool(b) => Ok(*b as i64 as f64),
        o => Err(unsupported(format!("{what}=<{}> (a number expected)", o.type_name()))),
    }
}

fn opt_num(v: Option<&V>, what: &str) -> R<Option<f64>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(x) => num(x, what).map(Some),
    }
}

fn text(v: &V, what: &str) -> R<String> {
    match v {
        V::Str(s) => Ok(s.to_string()),
        o => Err(unsupported(format!("{what}=<{}> (a str expected)", o.type_name()))),
    }
}

fn opt_text(v: Option<&V>, what: &str) -> R<Option<String>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(x) => text(x, what).map(Some),
    }
}

fn truth(v: &V) -> R<bool> {
    super::ops::truthy(v)
}

/// A number as Python wrote it: reportlab's defaults and sample styles are ints when integral.
fn pynum(x: f64) -> V {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        V::Int(x as i64)
    } else {
        V::Float(x)
    }
}

fn seq(v: &V) -> Option<Vec<V>> {
    match v {
        V::List(l) => Some(l.lock().clone()),
        V::Tuple(t) => Some(t.to_vec()),
        _ => None,
    }
}

fn nums(v: &V, what: &str) -> R<Vec<f64>> {
    match seq(v) {
        Some(items) => items.iter().map(|x| num(x, what)).collect(),
        None => Err(unsupported(format!("{what}=<{}> (a list of numbers expected)", v.type_name()))),
    }
}

/// A colour argument: a reportlab Color, None, or (for the canvas) a 3/4-tuple or a colour name / hex string.
fn color_of(v: &V, what: &str, loose: bool) -> R<Option<Color>> {
    if let Some(x) = rl_of(v) {
        if let Rl::Color(c) = &**x {
            return Ok(Some(c.clone()));
        }
    }
    match v {
        V::None => Ok(None),
        V::Str(s) if loose => Color::parse(s).map(Some).map_err(Exc::from),
        V::List(_) | V::Tuple(_) if loose => {
            let c = nums(v, what)?;
            match c.len() {
                3 => Ok(Some(Color::tuple3(c[0], c[1], c[2]))),
                4 => Ok(Some(Color::tuple4(c[0], c[1], c[2], c[3]))),
                n => Err(unsupported(format!("{what}= a sequence of {n} numbers"))),
            }
        }
        o => Err(unsupported(format!("{what}=<{}> (a reportlab.lib.colors.Color expected)", o.type_name()))),
    }
}

fn color_v(c: Color) -> V {
    wrap(Rl::Color(c))
}

fn tuple2(a: f64, b: f64) -> V {
    V::tuple(vec![V::Float(a), V::Float(b)])
}

// ---------------------------------------------------------------- module values

/// `reportlab.lib.pagesizes.<name>`
pub fn pagesize(name: &str) -> V {
    let (mm, inch) = (rl::mm(), rl::INCH);
    let (w, h) = match name {
        "A3" => (297.0 * mm, 420.0 * mm),
        "A4" => rl::a4(),
        "A5" => (148.0 * mm, 210.0 * mm),
        "legal" | "LEGAL" => (8.5 * inch, 14.0 * inch),
        _ => rl::letter(),
    };
    tuple2(w, h)
}

/// `landscape(size)` / `portrait(size)`
pub fn orient(landscape: bool, args: &[V]) -> R {
    let [s] = args else { return Err(Exc::type_error(format!("{}() takes 1 positional argument", if landscape { "landscape" } else { "portrait" }))) };
    let Some(items) = seq(s).filter(|i| i.len() == 2) else { return Err(unsupported("pagesize other than a (width, height) pair")) };
    let (a, b) = (items[0].clone(), items[1].clone());
    let (fa, fb) = (num(&a, "pagesize")?, num(&b, "pagesize")?);
    let (wide, tall) = if fa >= fb { (a, b) } else { (b, a) };
    Ok(V::tuple(if landscape { vec![wide, tall] } else { vec![tall, wide] }))
}

/// `reportlab.lib.colors.<name>`
pub fn named_color(name: &str) -> V {
    color_v(Color::named(name).expect("a colour of reportlab.lib.colors"))
}

/// `HexColor(val)` / `Color(red, green, blue, alpha)`
pub fn color(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match kind {
        "HexColor" => match arg(args, kwargs, 0, "val") {
            Some(V::Str(s)) => Ok(color_v(Color::hex(s)?)),
            Some(V::Int(n)) => {
                let n = *n;
                Ok(color_v(Color::rgb(((n >> 16) & 0xFF) as f64 / 255.0, ((n >> 8) & 0xFF) as f64 / 255.0, (n & 0xFF) as f64 / 255.0)))
            }
            Some(o) => Err(unsupported(format!("HexColor(<{}>)", o.type_name()))),
            None => Err(Exc::type_error("HexColor.__init__() missing 1 required positional argument: 'val'")),
        },
        _ => {
            let c = |i: usize, n: &str, d: f64| -> R<f64> { arg(args, kwargs, i, n).map(|v| num(v, n)).unwrap_or(Ok(d)) };
            Ok(color_v(Color::rgba(c(0, "red", 0.0)?, c(1, "green", 0.0)?, c(2, "blue", 0.0)?, c(3, "alpha", 1.0)?)))
        }
    }
}

fn color_attr(c: &Color, name: &str) -> R {
    match (c, name) {
        (Color::Rgb { r, .. }, "red") => Ok(V::Float(*r)),
        (Color::Rgb { g, .. }, "green") => Ok(V::Float(*g)),
        (Color::Rgb { b, .. }, "blue") => Ok(V::Float(*b)),
        (_, "alpha") => Ok(V::Float(c.alpha().unwrap_or(1.0))),
        _ => Err(unsupported(format!("Color.{name}"))),
    }
}

// ---------------------------------------------------------------- the html.parser of the project's Python

/// The html.parser behaviours of the project's Python (rl/paraparser.rs follows 3.14.0's), or a refusal for a
/// parser older than CPython's HTML5 rewrite (3.12.12, 3.13.8: the security releases of 2025).
fn html_rev() -> R<()> {
    static REV: std::sync::OnceLock<Option<HtmlRev>> = std::sync::OnceLock::new();
    let rev = REV.get_or_init(|| {
        let (_, minor) = super::python();
        let feat = |n: &str, d: bool| super::python_feature(n).unwrap_or(d);
        let at = |m12: u32, m13: u32, m14: u32| match minor {
            12 => super::python_at_least(3, 12, m12),
            13 => super::python_at_least(3, 13, m13),
            14 => super::python_at_least(3, 14, m14),
            m => m >= 15,
        };
        if !feat("htmlparser_html5", at(12, 8, 0)) {
            return None;
        }
        let rev = HtmlRev {
            rawtext_more: feat("htmlparser_rawtext_more", at(13, 10, 1)),
            charref_data: feat("htmlparser_charref_data", minor > 12 && at(99, 10, 1)),
            abrupt_comment_first: feat("htmlparser_abrupt_comment", minor > 12 && at(99, 15, 7)),
        };
        set_html_rev(rev);
        Some(rev)
    });
    match rev {
        Some(_) => Ok(()),
        None => {
            let (ma, mi) = super::python();
            Err(unsupported(format!(
                "Paragraph under Python {ma}.{mi}.{}: its html.parser predates the HTML5 rewrite of CPython 3.12.12 / 3.13.8 \
                 (3.14 and later have it); run the project on a later patch release",
                super::python_patch()
            )))
        }
    }
}

// ---------------------------------------------------------------- constructors

/// `SimpleDocTemplate`, `Paragraph`, `ParagraphStyle`, `Spacer`, `PageBreak`, `HRFlowable`, the shapes.
pub fn new(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match kind {
        "SimpleDocTemplate" => doc_new(args, kwargs),
        "ParagraphStyle" => {
            let name = arg(args, kwargs, 0, "name").ok_or_else(|| Exc::type_error("ParagraphStyle.__init__() missing 1 required positional argument: 'name'"))?;
            let name = text(name, "ParagraphStyle(name")?;
            let parent = arg(args, kwargs, 1, "parent").cloned().unwrap_or(V::None);
            let kw: Vec<(String, V)> = kwargs.iter().filter(|(k, _)| k != "name" && k != "parent").cloned().collect();
            style_new(&name, parent, &kw)
        }
        "getSampleStyleSheet" => Ok(sample_sheet()),
        "Paragraph" => {
            let t = arg(args, kwargs, 0, "text").ok_or_else(|| Exc::type_error("Paragraph.__init__() missing 1 required positional argument: 'text'"))?;
            let t = text(t, "Paragraph(text")?;
            let style = match arg(args, kwargs, 1, "style") {
                None | Some(V::None) => style_new("paragraphImplicitDefaultStyle", V::None, &[])?,
                Some(s) => s.clone(),
            };
            html_rev()?;
            let st = style_snapshot(&style)?;
            Paragraph::new(&t, &st)?; // the parser's errors are the constructor's
            Ok(wrap(Rl::Para(Mutex::new(Para { text: t, parsed_with: st, style }))))
        }
        "Spacer" => {
            let w = arg(args, kwargs, 0, "width").ok_or_else(|| Exc::type_error("Spacer.__init__() missing 2 required positional arguments: 'width' and 'height'"))?;
            let h = arg(args, kwargs, 1, "height").ok_or_else(|| Exc::type_error("Spacer.__init__() missing 1 required positional argument: 'height'"))?;
            num(w, "Spacer(width")?;
            num(h, "Spacer(height")?;
            Ok(wrap(Rl::Spacer(Mutex::new((w.clone(), h.clone())))))
        }
        "PageBreak" => Ok(wrap(Rl::PageBreak)),
        "HRFlowable" => {
            let mut h = HRFlowable::new();
            const P: [&str; 9] = ["width", "thickness", "lineCap", "color", "spaceBefore", "spaceAfter", "hAlign", "vAlign", "dash"];
            for (i, p) in P.iter().enumerate() {
                if let Some(v) = arg(args, kwargs, i, p) {
                    hr_set(&mut h, p, v)?;
                }
            }
            Ok(wrap(Rl::Hr(Mutex::new(h))))
        }
        "Drawing" => {
            let width = arg(args, kwargs, 0, "width").cloned().unwrap_or(V::Int(400));
            let height = arg(args, kwargs, 1, "height").cloned().unwrap_or(V::Int(200));
            num(&width, "Drawing(width")?;
            num(&height, "Drawing(height")?;
            let mut d = DrawObj { width, height, group: GroupObj::default(), h_align: HAlign::Left, v_align: "BOTTOM".into(), render_scale: 1.0 };
            for n in args.iter().skip(2) {
                d.group.kids.push(child(n)?);
            }
            for (k, v) in kwargs {
                if k != "width" && k != "height" {
                    draw_set(&mut d, k, v)?;
                }
            }
            Ok(wrap(Rl::Drawing(Mutex::new(d))))
        }
        "Table" => table_new(args, kwargs),
        "TableStyle" => {
            if arg(args, kwargs, 1, "parent").is_some_and(|p| !matches!(p, V::None)) {
                return Err(unsupported("TableStyle(parent=)"));
            }
            let cmds = match arg(args, kwargs, 0, "cmds") {
                None | Some(V::None) => Vec::new(),
                Some(c) => table_cmds(c)?,
            };
            Ok(wrap(Rl::TableStyle(Mutex::new(cmds))))
        }
        "Group" => {
            let mut g = GroupObj::default();
            for n in args {
                g.kids.push(child(n)?);
            }
            Ok(wrap(Rl::Group(Mutex::new(g))))
        }
        _ => shape_new(kind, args, kwargs),
    }
}

fn need(args: &[V], kwargs: &[(String, V)], kind: &str, params: &[&str]) -> R<Vec<f64>> {
    let mut out = Vec::new();
    for (i, p) in params.iter().enumerate() {
        match arg(args, kwargs, i, p) {
            Some(v) => out.push(num(v, p)?),
            None => {
                let missing: Vec<String> = params[i..].iter().filter(|q| arg(args, kwargs, usize::MAX, q).is_none()).map(|q| format!("'{q}'")).collect();
                return Err(Exc::type_error(format!(
                    "{kind}.__init__() missing {} required positional argument{}: {}",
                    missing.len(),
                    if missing.len() > 1 { "s" } else { "" },
                    match missing.len() {
                        1 => missing[0].clone(),
                        _ => format!("{} and {}", missing[..missing.len() - 1].join(", "), missing[missing.len() - 1]),
                    }
                )));
            }
        }
    }
    Ok(out)
}

fn shape_new(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let (geo, mut node): (&[&str], Node) = match kind {
        "Rect" => {
            let a = need(args, kwargs, kind, &["x", "y", "width", "height"])?;
            let mut r = Rect::new(a[0], a[1], a[2], a[3]);
            r.rx = opt_num(arg(args, kwargs, 4, "rx"), "rx")?.unwrap_or(0.0);
            r.ry = opt_num(arg(args, kwargs, 5, "ry"), "ry")?.unwrap_or(0.0);
            (&["x", "y", "width", "height", "rx", "ry"], Node::Rect(r))
        }
        "Circle" => {
            let a = need(args, kwargs, kind, &["cx", "cy", "r"])?;
            (&["cx", "cy", "r"], Node::Circle(Circle::new(a[0], a[1], a[2])))
        }
        "Ellipse" => {
            let a = need(args, kwargs, kind, &["cx", "cy", "rx", "ry"])?;
            (&["cx", "cy", "rx", "ry"], Node::Ellipse(Ellipse::new(a[0], a[1], a[2], a[3])))
        }
        "Line" => {
            let a = need(args, kwargs, kind, &["x1", "y1", "x2", "y2"])?;
            (&["x1", "y1", "x2", "y2"], Node::Line(Line::new(a[0], a[1], a[2], a[3])))
        }
        "Polygon" | "PolyLine" => {
            let pts = match arg(args, kwargs, 0, "points") {
                None => Vec::new(),
                Some(v) => nums(v, "points")?,
            };
            if pts.len() % 2 != 0 {
                return Err(Exc::msg(&ASSERTION_ERROR, "Point list must have even number of elements!"));
            }
            let n = if kind == "Polygon" { graphics::polygon(pts)? } else { graphics::polyline(pts)? };
            (&["points"], n)
        }
        _ => return Err(unsupported(format!("{kind}()"))),
    };
    if args.len() > geo.len() {
        return Err(Exc::type_error(format!("{kind}.__init__() takes at most {} positional arguments", geo.len() + 1)));
    }
    for (k, v) in kwargs {
        if !geo.contains(&k.as_str()) {
            shape_set(&mut node, k, v)?;
        }
    }
    Ok(wrap(Rl::Shape(Mutex::new(node))))
}

/// A child of a Group / Drawing (`isValidChild`: a shape or a group).
fn child(v: &V) -> R<V> {
    match rl_of(v).map(|x| &**x) {
        Some(Rl::Shape(_) | Rl::Group(_)) => Ok(v.clone()),
        _ => Err(Exc::msg(&ASSERTION_ERROR, "Can only add Shape or UserNode objects to a Group")),
    }
}

// ---------------------------------------------------------------- styles

const STYLE_KEYS: [&str; 33] = [
    "fontName", "fontSize", "leading", "leftIndent", "rightIndent", "firstLineIndent", "alignment", "spaceBefore",
    "spaceAfter", "bulletFontName", "bulletFontSize", "bulletIndent", "textColor", "backColor", "wordWrap",
    "borderWidth", "borderPadding", "borderColor", "borderRadius", "allowWidows", "allowOrphans", "textTransform",
    "endDots", "splitLongWords", "bulletAnchor", "justifyLastLine", "justifyBreaks", "spaceShrinkage",
    "hyphenationLang", "embeddedHyphenation", "uriWasteReduce", "linkUnderline", "shaping",
];

fn style_val(key: &str, v: &V) -> R<StyleVal> {
    if let Some(x) = rl_of(v) {
        if let Rl::Color(c) = &**x {
            return Ok(StyleVal::Color(c.clone()));
        }
    }
    Ok(match v {
        V::None => StyleVal::None,
        V::Bool(b) => StyleVal::Bool(*b),
        V::Int(n) => StyleVal::Num(*n as f64),
        V::Float(x) => StyleVal::Num(*x),
        V::Str(s) => StyleVal::Str(s.to_string()),
        V::List(_) | V::Tuple(_) => StyleVal::Seq(nums(v, key)?),
        o => return Err(unsupported(format!("ParagraphStyle {key}=<{}>", o.type_name()))),
    })
}

fn style_new(name: &str, parent: V, kw: &[(String, V)]) -> R {
    let (base, mut vals) = match &parent {
        V::None => (None, Vec::new()),
        p => match rl_of(p).map(|x| &**x) {
            Some(Rl::Style(s)) => {
                let s = s.lock();
                (Some(s.st.clone()), s.vals.clone())
            }
            _ => {
                return Err(Exc::msg(
                    &ASSERTION_ERROR,
                    format!("Parent style {} must have same class as new style ParagraphStyle", p.type_name()),
                ))
            }
        },
    };
    let mut st = ParagraphStyle::new(name, base.as_ref());
    for (k, v) in kw {
        st.set(k, style_val(k, v)?)?;
        upsert(&mut vals, k, v.clone());
    }
    Ok(wrap(Rl::Style(Mutex::new(StyleObj { st, parent, vals }))))
}

fn upsert(vals: &mut Vec<(String, V)>, k: &str, v: V) {
    match vals.iter_mut().find(|(n, _)| n == k) {
        Some(slot) => slot.1 = v,
        None => vals.push((k.to_string(), v)),
    }
}

fn style_snapshot(v: &V) -> R<ParagraphStyle> {
    match rl_of(v).map(|x| &**x) {
        Some(Rl::Style(s)) => Ok(s.lock().st.clone()),
        _ => Err(unsupported(format!("a paragraph style of type {}", v.type_name()))),
    }
}

/// A class default (or a sample sheet value) as reportlab holds it.
fn style_default(s: &ParagraphStyle, key: &str) -> Option<V> {
    let os = |o: &Option<String>| o.as_ref().map(V::str).unwrap_or(V::None);
    let oc = |o: &Option<Color>| o.clone().map(color_v).unwrap_or(V::None);
    let b = |x: bool| V::Int(x as i64);
    Some(match key {
        "fontName" => V::str(&s.font_name),
        "fontSize" => pynum(s.font_size),
        "leading" => pynum(s.leading),
        "leftIndent" => pynum(s.left_indent),
        "rightIndent" => pynum(s.right_indent),
        "firstLineIndent" => pynum(s.first_line_indent),
        "alignment" => V::Int(s.alignment),
        "spaceBefore" => pynum(s.space_before),
        "spaceAfter" => pynum(s.space_after),
        "bulletFontName" => V::str(&s.bullet_font_name),
        "bulletFontSize" => pynum(s.bullet_font_size),
        "bulletIndent" => pynum(s.bullet_indent),
        "textColor" => oc(&s.text_color),
        "backColor" => oc(&s.back_color),
        "borderColor" => oc(&s.border_color),
        "wordWrap" => os(&s.word_wrap),
        "borderWidth" => pynum(s.border_width),
        "borderRadius" => s.border_radius.map(pynum).unwrap_or(V::None),
        "allowWidows" => b(s.allow_widows),
        "allowOrphans" => b(s.allow_orphans),
        "textTransform" => os(&s.text_transform),
        "endDots" => os(&s.end_dots),
        "splitLongWords" => b(s.split_long_words),
        "bulletAnchor" => V::str(&s.bullet_anchor),
        "justifyLastLine" => V::Int(s.justify_last_line),
        "justifyBreaks" => b(s.justify_breaks),
        "spaceShrinkage" => V::Float(s.space_shrinkage),
        "hyphenationLang" => V::str(&s.hyphenation_lang),
        "embeddedHyphenation" => pynum(s.embedded_hyphenation),
        "uriWasteReduce" => pynum(s.uri_waste_reduce),
        "linkUnderline" => b(s.link_underline),
        "shaping" => b(s.shaping),
        _ => return None,
    })
}

fn style_attr(s: &StyleObj, name: &str) -> R {
    match name {
        "name" => Ok(V::str(&s.st.name)),
        "parent" => Ok(s.parent.clone()),
        _ => {
            if let Some((_, v)) = s.vals.iter().find(|(k, _)| k == name) {
                return Ok(v.clone());
            }
            if name == "borderPadding" {
                return Ok(match &s.st.border_padding {
                    rl::styles::Padding::Num(x) => pynum(*x),
                    rl::styles::Padding::Seq(v) => V::list(v.iter().map(|x| pynum(*x)).collect()),
                });
            }
            style_default(&s.st, name).ok_or_else(|| unsupported(format!("ParagraphStyle.{name}")))
        }
    }
}

fn style_setattr(s: &mut StyleObj, name: &str, v: V) -> R<()> {
    match name {
        "parent" => Err(unsupported("setting ParagraphStyle.parent")),
        "name" => {
            s.st.name = text(&v, "ParagraphStyle.name")?;
            Ok(())
        }
        _ => {
            s.st.set(name, style_val(name, &v)?)?;
            upsert(&mut s.vals, name, v);
            Ok(())
        }
    }
}

const SAMPLE_PARENTS: [(&str, &str); 14] = [
    ("BodyText", "Normal"), ("Italic", "BodyText"), ("Heading1", "Normal"), ("Title", "Normal"), ("Heading2", "Normal"),
    ("Heading3", "Normal"), ("Heading4", "Normal"), ("Heading5", "Normal"), ("Heading6", "Normal"), ("Bullet", "Normal"),
    ("Definition", "Normal"), ("Code", "Normal"), ("Normal", ""), ("", ""),
];
/// getSampleStyleSheet()'s ListStyles, not reproduced
const LIST_STYLES: [&str; 4] = ["UnorderedList", "OrderedList", "ul", "ol"];

fn sample_sheet() -> V {
    let ss = rl::sample_style_sheet();
    let mut sheet = Sheet { by_name: Vec::new(), by_alias: Vec::new() };
    for st in &ss.by_name {
        let parent_name = SAMPLE_PARENTS.iter().find(|(n, _)| *n == st.name).map(|(_, p)| *p).unwrap_or("");
        let parent = sheet.by_name.iter().find(|(n, _)| n == parent_name).map(|(_, v)| v.clone()).unwrap_or(V::None);
        let v = wrap(Rl::Style(Mutex::new(StyleObj { st: st.clone(), parent, vals: Vec::new() })));
        sheet.by_name.push((st.name.clone(), v));
    }
    for (a, i) in &ss.by_alias {
        sheet.by_alias.push((a.clone(), sheet.by_name[*i].1.clone()));
    }
    wrap(Rl::Sheet(Mutex::new(sheet)))
}

fn sheet_key(k: &V) -> R<String> {
    match k {
        V::Str(s) if LIST_STYLES.contains(&&**s) => Err(unsupported(format!("getSampleStyleSheet()['{s}'] (a ListStyle)"))),
        V::Str(s) => Ok(s.to_string()),
        o => Err(unsupported(format!("a style sheet key of type {}", o.type_name()))),
    }
}

fn sheet_find(s: &Sheet, key: &str) -> Option<V> {
    s.by_alias.iter().chain(s.by_name.iter()).find(|(n, _)| n == key).map(|(_, v)| v.clone())
}

pub fn getitem(x: &Arc<Rl>, k: &V) -> R {
    match &**x {
        Rl::Sheet(s) => {
            let key = sheet_key(k)?;
            sheet_find(&s.lock(), &key).ok_or_else(|| Exc::new(&KEY_ERROR, vec![V::str(format!("Style '{key}' not found in stylesheet"))]))
        }
        _ => Err(Exc::type_error(format!("'{}' object is not subscriptable", type_name(x)))),
    }
}

pub fn contains(x: &Arc<Rl>, k: &V) -> R<bool> {
    match &**x {
        Rl::Sheet(s) => {
            let key = sheet_key(k)?;
            Ok(sheet_find(&s.lock(), &key).is_some())
        }
        _ => Err(Exc::type_error(format!("argument of type '{}' is not iterable", type_name(x)))),
    }
}

fn sheet_add(s: &Mutex<Sheet>, style: &V, alias: Option<&V>) -> R {
    let name = match rl_of(style).map(|x| &**x) {
        Some(Rl::Style(st)) => st.lock().st.name.clone(),
        _ => return Err(unsupported(format!("StyleSheet1.add(<{}>)", style.type_name()))),
    };
    let alias = opt_text(alias, "alias")?;
    let mut s = s.lock();
    let key_err = |m: String| Err(Exc::new(&KEY_ERROR, vec![V::str(m)]));
    if s.by_name.iter().any(|(n, _)| *n == name) {
        return key_err(format!("Style '{name}' already defined in stylesheet"));
    }
    if s.by_alias.iter().any(|(n, _)| *n == name) {
        return key_err(format!("Style name '{name}' is already an alias in stylesheet"));
    }
    if let Some(a) = &alias {
        if s.by_name.iter().any(|(n, _)| n == a) {
            return key_err(format!("Style '{a}' already defined in stylesheet"));
        }
        if s.by_alias.iter().any(|(n, _)| n == a) {
            return key_err(format!("Alias name '{a}' is already an alias in stylesheet"));
        }
    }
    s.by_name.push((name, style.clone()));
    if let Some(a) = alias {
        s.by_alias.push((a, style.clone()));
    }
    Ok(V::None)
}

// ---------------------------------------------------------------- flowables, shapes

fn hr_set(h: &mut HRFlowable, key: &str, v: &V) -> R<()> {
    match key {
        "width" => {
            h.width = match v {
                V::Str(s) => HrWidth::Str(s.to_string()),
                o => HrWidth::Abs(num(o, "width")?),
            }
        }
        "thickness" | "lineWidth" => h.line_width = num(v, key)?,
        "lineCap" => h.line_cap = text(v, key)?,
        "color" => h.color = color_of(v, key, false)?,
        "spaceBefore" => h.space_before = num(v, key)?,
        "spaceAfter" => h.space_after = num(v, key)?,
        "hAlign" => h.h_align = parse_h_align(&text(v, key)?)?,
        "vAlign" => h.v_align = text(v, key)?,
        "dash" => {
            h.dash = match v {
                V::None => None,
                V::List(_) | V::Tuple(_) => Some(HrDash::Array(nums(v, key)?)),
                o => Some(HrDash::Number(num(o, key)?)),
            }
        }
        _ => return Err(unsupported(format!("HRFlowable.{key}"))),
    }
    Ok(())
}

fn draw_set(d: &mut DrawObj, key: &str, v: &V) -> R<()> {
    match key {
        "width" => {
            num(v, key)?;
            d.width = v.clone()
        }
        "height" => {
            num(v, key)?;
            d.height = v.clone()
        }
        "hAlign" => d.h_align = parse_h_align(&text(v, key)?)?,
        "vAlign" => d.v_align = text(v, key)?,
        "renderScale" => d.render_scale = num(v, key)?,
        _ => return Err(unsupported(format!("Drawing.{key}"))),
    }
    Ok(())
}

fn shape_set(n: &mut Node, key: &str, v: &V) -> R<()> {
    match (&mut *n, key) {
        (Node::Rect(r), "x") => r.x = num(v, key)?,
        (Node::Rect(r), "y") => r.y = num(v, key)?,
        (Node::Rect(r), "width") => r.width = num(v, key)?,
        (Node::Rect(r), "height") => r.height = num(v, key)?,
        (Node::Rect(r), "rx") => r.rx = num(v, key)?,
        (Node::Rect(r), "ry") => r.ry = num(v, key)?,
        (Node::Circle(c), "cx") => c.cx = num(v, key)?,
        (Node::Circle(c), "cy") => c.cy = num(v, key)?,
        (Node::Circle(c), "r") => c.r = num(v, key)?,
        (Node::Ellipse(e), "cx") => e.cx = num(v, key)?,
        (Node::Ellipse(e), "cy") => e.cy = num(v, key)?,
        (Node::Ellipse(e), "rx") => e.rx = num(v, key)?,
        (Node::Ellipse(e), "ry") => e.ry = num(v, key)?,
        (Node::Line(l), "x1") => l.x1 = num(v, key)?,
        (Node::Line(l), "y1") => l.y1 = num(v, key)?,
        (Node::Line(l), "x2") => l.x2 = num(v, key)?,
        (Node::Line(l), "y2") => l.y2 = num(v, key)?,
        _ => {
            let solid = matches!(n, Node::Rect(_) | Node::Circle(_) | Node::Ellipse(_) | Node::Polygon(_));
            let st: &mut Style = n.style_mut().ok_or_else(|| unsupported(format!("Group.{key}")))?;
            match key {
                "fillColor" if solid => st.fill_color = color_of(v, key, false)?,
                "fillOpacity" if solid => st.fill_opacity = opt_num(Some(v), key)?,
                "fillMode" if solid => st.fill_mode = Some(num(v, key)? as u8),
                "strokeColor" => st.stroke_color = color_of(v, key, false)?,
                "strokeWidth" => st.stroke_width = num(v, key)?,
                "strokeLineCap" => st.stroke_line_cap = num(v, key)? as i32,
                "strokeLineJoin" => st.stroke_line_join = num(v, key)? as i32,
                "strokeMiterLimit" => st.stroke_miter_limit = num(v, key)?,
                "strokeOpacity" => st.stroke_opacity = opt_num(Some(v), key)?,
                "strokeDashArray" => {
                    st.stroke_dash_array = match v {
                        V::None => None,
                        _ => {
                            let items = seq(v).ok_or_else(|| unsupported("strokeDashArray other than a list"))?;
                            match items.as_slice() {
                                [phase, arr] if seq(arr).is_some() => {
                                    let ph = match phase {
                                        V::Int(i) => i.to_string(),
                                        V::Float(x) => super::ops::float_repr(*x),
                                        o => return Err(unsupported(format!("strokeDashArray phase <{}>", o.type_name()))),
                                    };
                                    Some(Dash::phased(&ph, nums(arr, key)?))
                                }
                                _ => Some(Dash::new(nums(v, key)?)),
                            }
                        }
                    }
                }
                _ => return Err(unsupported(format!("setting {key} on a shape"))),
            }
        }
    }
    Ok(())
}

fn shape_attr(n: &Node, key: &str) -> R {
    let f = |x: f64| Ok(pynum(x));
    match (n, key) {
        (Node::Rect(r), "x") => f(r.x),
        (Node::Rect(r), "y") => f(r.y),
        (Node::Rect(r), "width") => f(r.width),
        (Node::Rect(r), "height") => f(r.height),
        (Node::Circle(c), "cx") => f(c.cx),
        (Node::Circle(c), "cy") => f(c.cy),
        (Node::Circle(c), "r") => f(c.r),
        (Node::Polygon(p) | Node::PolyLine(p), "points") => Ok(V::list(p.points.iter().map(|x| pynum(*x)).collect())),
        (_, "fillColor") => match n {
            Node::Rect(Rect { style, .. }) | Node::Circle(Circle { style, .. }) | Node::Ellipse(Ellipse { style, .. }) => {
                Ok(style.fill_color.clone().map(color_v).unwrap_or(V::None))
            }
            Node::Polygon(p) => Ok(p.style.fill_color.clone().map(color_v).unwrap_or(V::None)),
            _ => Err(unsupported(format!("{key} of a line shape"))),
        },
        _ => Err(unsupported(format!("reading {key} of a shape"))),
    }
}

/// The engine's node for a shape or group value (read now: later changes to it are not seen by this copy).
fn node(v: &V) -> R<Node> {
    match rl_of(v).map(|x| &**x) {
        Some(Rl::Shape(s)) => Ok(s.lock().clone()),
        Some(Rl::Group(g)) => Ok(Node::Group(group(&g.lock())?)),
        _ => Err(unsupported(format!("a {} in a Group", v.type_name()))),
    }
}

fn group(g: &GroupObj) -> R<Group> {
    let mut out = Group::new();
    if let Some(t) = g.transform {
        out.transform = t;
    }
    for k in &g.kids {
        out.add(node(k)?);
    }
    Ok(out)
}

/// Group.scale / translate / rotate / skew / shift (the Group or Drawing's transform).
fn group_op(g: &mut GroupObj, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let mut tmp = Group::new();
    if let Some(t) = g.transform {
        tmp.transform = t;
    }
    let p = |i: usize, n: &str, d: Option<f64>| -> R<f64> {
        match arg(args, kwargs, i, n) {
            Some(v) => num(v, n),
            None => d.ok_or_else(|| Exc::type_error(format!("Group.{name}() missing 1 required positional argument: '{n}'"))),
        }
    };
    match name {
        "scale" => tmp.scale(p(0, "sx", None)?, p(1, "sy", Some(1.0))?),
        "translate" => tmp.translate(p(0, "dx", None)?, p(1, "dy", Some(0.0))?),
        "rotate" => tmp.rotate(p(0, "theta", None)?, p(1, "cx", Some(0.0))?, p(2, "cy", Some(0.0))?),
        "skew" => tmp.skew(p(0, "kx", None)?, p(1, "ky", Some(0.0))?),
        _ => tmp.shift(p(0, "x", None)?, p(1, "y", Some(0.0))?),
    }
    g.transform = Some(tmp.transform);
    Ok(V::None)
}

fn group_add(g: &mut GroupObj, args: &[V], kwargs: &[(String, V)]) -> R {
    if arg(args, kwargs, 1, "name").is_some_and(|n| !matches!(n, V::None)) {
        return Err(unsupported("Group.add(node, name=)"));
    }
    match arg(args, kwargs, 0, "node") {
        None => Err(Exc::type_error("Group.add() missing 1 required positional argument: 'node'")),
        Some(V::None) => Ok(V::None),
        Some(n) => {
            g.kids.push(child(n)?);
            Ok(V::None)
        }
    }
}

fn drawing(d: &DrawObj) -> R<rl::graphics::Drawing> {
    let mut out = rl::graphics::Drawing::new(num(&d.width, "width")?, num(&d.height, "height")?);
    out.group = group(&d.group)?;
    out.h_align = d.h_align;
    out.v_align = d.v_align.clone();
    out.render_scale = d.render_scale;
    out.size_repr = Some((super::ops::str_(&d.width)?, super::ops::str_(&d.height)?));
    Ok(out)
}

/// A story item as the engine's flowable.
fn flowable(v: &V) -> R<Box<dyn Flowable>> {
    match rl_of(v).map(|x| &**x) {
        Some(Rl::Para(p)) => {
            let p = p.lock();
            let mut f = Paragraph::new(&p.text, &p.parsed_with)?;
            // a Paragraph keeps its style object: what was set on it since is what reportlab lays out
            let now = style_snapshot(&p.style)?;
            if now != f.style {
                Paragraph::new("", &now)?; // the style checks of the constructor
                f.style = now;
            }
            Ok(Box::new(f))
        }
        Some(Rl::Spacer(s)) => {
            let s = s.lock();
            let mut sp = Spacer::new(num(&s.0, "width")?, num(&s.1, "height")?);
            sp.size_repr = Some((super::ops::str_(&s.0)?, super::ops::str_(&s.1)?));
            Ok(Box::new(sp))
        }
        Some(Rl::PageBreak) => Ok(Box::new(PageBreak::new())),
        Some(Rl::Hr(h)) => Ok(Box::new(h.lock().clone())),
        Some(Rl::Drawing(d)) => Ok(Box::new(drawing(&d.lock())?)),
        Some(Rl::Table(t)) => Ok(Box::new(table_engine(&t.lock())?)),
        _ => Err(unsupported(format!("a {} in the story", v.type_name()))),
    }
}

// ---------------------------------------------------------------- tables

fn cmd_val(v: &V) -> R<CmdVal> {
    if let Some(x) = rl_of(v) {
        if let Rl::Color(c) = &**x {
            return Ok(CmdVal::Color(c.clone()));
        }
    }
    Ok(match v {
        V::None => CmdVal::None,
        V::Bool(b) => CmdVal::Int(*b as i64),
        V::Int(n) => CmdVal::Int(*n),
        V::Float(x) => CmdVal::Num(*x),
        V::Str(s) => CmdVal::Str(s.to_string()),
        V::List(_) | V::Tuple(_) => CmdVal::List(seq(v).unwrap_or_default().iter().map(cmd_val).collect::<R<_>>()?),
        o => return Err(unsupported(format!("a table style value of type {}", o.type_name()))),
    })
}

fn cell_xy(v: &V) -> R<(i64, i64)> {
    match seq(v).as_deref() {
        Some([V::Int(c), V::Int(r)]) => Ok((*c, *r)),
        _ => Err(unsupported("a table style cell other than a (column, row) pair of ints")),
    }
}

/// `TableStyle(cmds)` / `setStyle([...])`: commands as tuples `(op, (c, r), (c, r), *args)`.
fn table_cmds(v: &V) -> R<Vec<TableCmd>> {
    if let Some(x) = rl_of(v) {
        if let Rl::TableStyle(c) = &**x {
            return Ok(c.lock().clone());
        }
    }
    let items = seq(v).ok_or_else(|| unsupported(format!("table style commands in a {}", v.type_name())))?;
    items
        .iter()
        .map(|c| {
            let c = seq(c).ok_or_else(|| unsupported(format!("a table style command of type {}", c.type_name())))?;
            match c.as_slice() {
                [V::Str(op), a, b, rest @ ..] => {
                    Ok(TableCmd::new(op, cell_xy(a)?, cell_xy(b)?, rest.iter().map(cmd_val).collect::<R<_>>()?))
                }
                _ => Err(unsupported("a table style command other than (op, (c, r), (c, r), ...)")),
            }
        })
        .collect()
}

fn cell_input(v: &V) -> R<CellInput> {
    if rl_of(v).is_some() {
        return Ok(CellInput::Flow(flowable(v)?));
    }
    Ok(match v {
        V::None => CellInput::Text(String::new()),
        V::Str(s) => CellInput::Text(s.to_string()),
        V::List(_) | V::Tuple(_) => CellInput::List(seq(v).unwrap_or_default().iter().map(cell_input).collect::<R<_>>()?),
        V::Int(_) | V::Float(_) | V::Bool(_) | V::Decimal(_) => CellInput::Text(super::ops::str_(v)?),
        o => return Err(unsupported(format!("a table cell of type {}", o.type_name()))),
    })
}

fn table_engine(t: &TableObj) -> R<Table> {
    let data = t.data.iter().map(|row| row.iter().map(cell_input).collect::<R<Vec<_>>>()).collect::<R<Vec<_>>>()?;
    let mut tb = Table::new(data, t.opts.clone())?;
    tb.set_style(&t.cmds)?;
    Ok(tb)
}

fn col_width(v: &V) -> R<ColWidth> {
    Ok(match v {
        V::None => ColWidth::Auto,
        V::Str(s) => ColWidth::Str(s.to_string()),
        o => ColWidth::Abs(num(o, "colWidths")?),
    })
}

fn table_set(o: &mut TableOpts, k: &str, v: &V) -> R<()> {
    match k {
        "colWidths" => {
            o.col_widths = match seq(v) {
                Some(items) => Sizes::Seq(items.iter().map(col_width).collect::<R<_>>()?),
                None => Sizes::Each(col_width(v)?),
            }
        }
        "rowHeights" => {
            o.row_heights = match seq(v) {
                Some(items) => Sizes::Seq(items.iter().map(|x| opt_num(Some(x), k)).collect::<R<_>>()?),
                None => Sizes::Each(opt_num(Some(v), k)?),
            }
        }
        "repeatRows" => {
            o.repeat_rows = match seq(v) {
                Some(items) => RepeatRows::Rows(items.iter().map(|x| num(x, k).map(|n| n as i64)).collect::<R<_>>()?),
                None => RepeatRows::Count(num(v, k)? as i64),
            }
        }
        "hAlign" => o.h_align = opt_text(Some(v), k)?,
        "vAlign" => o.v_align = opt_text(Some(v), k)?,
        "spaceBefore" => o.space_before = opt_num(Some(v), k)?,
        "spaceAfter" => o.space_after = opt_num(Some(v), k)?,
        _ => return Err(unsupported(format!("Table {k}="))),
    }
    Ok(())
}

fn table_new(args: &[V], kwargs: &[(String, V)]) -> R {
    const P: [&str; 5] = ["data", "colWidths", "rowHeights", "style", "repeatRows"];
    let data = arg(args, kwargs, 0, "data").ok_or_else(|| Exc::type_error("Table.__init__() missing 1 required positional argument: 'data'"))?;
    let rows = seq(data).ok_or_else(|| unsupported(format!("Table(data) of type {}", data.type_name())))?;
    let data = rows
        .iter()
        .map(|r| seq(r).ok_or_else(|| unsupported(format!("a table row of type {}", r.type_name()))))
        .collect::<R<Vec<_>>>()?;
    let mut t = TableObj { data, opts: TableOpts::default(), cmds: Vec::new() };
    for (i, p) in P.iter().enumerate().skip(1) {
        match (arg(args, kwargs, i, p), *p) {
            (None, _) => {}
            (Some(s), "style") => {
                if !matches!(s, V::None) {
                    t.cmds = table_cmds(s)?;
                }
            }
            (Some(v), p) => table_set(&mut t.opts, p, v)?,
        }
    }
    for (k, v) in kwargs {
        if !P.contains(&k.as_str()) {
            table_set(&mut t.opts, k, v)?;
        }
    }
    table_engine(&t)?; // the constructor's and setStyle's errors
    Ok(wrap(Rl::Table(Mutex::new(t))))
}

// ---------------------------------------------------------------- the document

fn doc_new(args: &[V], kwargs: &[(String, V)]) -> R {
    let file = arg(args, kwargs, 0, "filename").cloned().ok_or_else(|| Exc::type_error("BaseDocTemplate.__init__() missing 1 required positional argument: 'filename'"))?;
    if args.len() > 1 {
        return Err(Exc::type_error("SimpleDocTemplate() takes the filename and keyword arguments"));
    }
    let mut d = Doc { t: SimpleDocTemplate::default(), file, vals: Vec::new(), page: None };
    for (k, v) in kwargs {
        if k != "filename" {
            doc_set(&mut d, k, v.clone())?;
        }
    }
    Ok(wrap(Rl::Doc(Mutex::new(d))))
}

fn doc_set(d: &mut Doc, k: &str, v: V) -> R<()> {
    let t = &mut d.t;
    match k {
        "pagesize" => {
            let s = seq(&v).filter(|s| s.len() == 2).ok_or_else(|| unsupported("pagesize other than a (width, height) pair"))?;
            t.pagesize = (num(&s[0], k)?, num(&s[1], k)?);
        }
        "leftMargin" => t.left_margin = num(&v, k)?,
        "rightMargin" => t.right_margin = num(&v, k)?,
        "topMargin" => t.top_margin = num(&v, k)?,
        "bottomMargin" => t.bottom_margin = num(&v, k)?,
        "title" => t.title = opt_text(Some(&v), k)?,
        "author" => t.author = opt_text(Some(&v), k)?,
        "subject" => t.subject = opt_text(Some(&v), k)?,
        "creator" => t.creator = opt_text(Some(&v), k)?,
        "producer" => t.producer = opt_text(Some(&v), k)?,
        "keywords" => {
            t.keywords = match &v {
                V::Str(s) => s.to_string(),
                V::None => String::new(),
                _ => seq(&v)
                    .ok_or_else(|| unsupported("keywords other than a str or a list of str"))?
                    .iter()
                    .map(|x| text(x, k))
                    .collect::<R<Vec<_>>>()?
                    .join(", "),
            }
        }
        "invariant" => t.invariant = truth(&v)?,
        "pageCompression" => t.page_compression = if matches!(v, V::None) { None } else { Some(truth(&v)?) },
        "allowSplitting" => t.allow_splitting = truth(&v)?,
        "rotation" => t.rotation = num(&v, k)?,
        "initialFontName" => t.initial_font_name = opt_text(Some(&v), k)?,
        "initialFontSize" => t.initial_font_size = opt_num(Some(&v), k)?,
        "initialLeading" => t.initial_leading = opt_num(Some(&v), k)?,
        _ => return Err(unsupported(format!("SimpleDocTemplate {k}="))),
    }
    upsert(&mut d.vals, k, v);
    Ok(())
}

fn doc_attr(d: &Doc, name: &str) -> R {
    if let Some((_, v)) = d.vals.iter().find(|(k, _)| k == name) {
        return Ok(v.clone());
    }
    let t = &d.t;
    let os = |o: &Option<String>| o.as_ref().map(V::str).unwrap_or(V::None);
    Ok(match name {
        "pagesize" => tuple2(t.pagesize.0, t.pagesize.1),
        "width" => V::Float(t.calc().0),
        "height" => V::Float(t.calc().1),
        "leftMargin" => V::Float(t.left_margin),
        "rightMargin" => V::Float(t.right_margin),
        "topMargin" => V::Float(t.top_margin),
        "bottomMargin" => V::Float(t.bottom_margin),
        "title" => os(&t.title),
        "author" => os(&t.author),
        "subject" => os(&t.subject),
        "creator" => os(&t.creator),
        "filename" => d.file.clone(),
        "page" => match d.page {
            Some(p) => V::Int(p),
            None => return Err(Exc::attr_error("'SimpleDocTemplate' object has no attribute 'page'")),
        },
        _ => return Err(unsupported(format!("SimpleDocTemplate.{name}"))),
    })
}

/// `ActionFlowable.apply`: an exception of `handle_pageBegin` (the page callbacks) is raised again as a new one of
/// its class, its message prefixed (`annotateException`), but an AttributeError, re-raised as is.
fn annotate_page_begin(e: Exc) -> Exc {
    if e.0.class.is_subclass(&ATTRIBUTE_ERROR) {
        return e;
    }
    let msg = e.message();
    let head = "\nhandle_pageBegin args=()";
    let o = Exc::new(e.0.class, vec![V::str(if msg.is_empty() { head.to_string() } else { format!("{head} {msg}") })]);
    o.0.attrs.lock().insert("__context__".into(), V::Exc(e));
    o
}

async fn build(cx: &Cx, me: &V, d: &Mutex<Doc>, args: &[V], kwargs: &[(String, V)]) -> R {
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !matches!(k.as_str(), "flowables" | "onFirstPage" | "onLaterPages")) {
        return Err(unsupported(format!("SimpleDocTemplate.build({k}=)")));
    }
    if args.len() > 3 {
        return Err(unsupported("SimpleDocTemplate.build() with a canvasmaker"));
    }
    let story_v = arg(args, kwargs, 0, "flowables").ok_or_else(|| Exc::type_error("SimpleDocTemplate.build() missing 1 required positional argument: 'flowables'"))?;
    let items = seq(story_v).ok_or_else(|| unsupported(format!("build() of a {} (a list of flowables expected)", story_v.type_name())))?;
    let story = items.iter().map(flowable).collect::<R<Vec<_>>>()?;
    let first = arg(args, kwargs, 1, "onFirstPage").cloned();
    let later = arg(args, kwargs, 2, "onLaterPages").cloned();
    let (t, file) = {
        let d = d.lock();
        (d.t.clone(), d.file.clone())
    };
    if !matches!(&file, V::Native(n) if matches!(&**n, Native::BytesIO(_))) {
        return Err(unsupported("SimpleDocTemplate(filename) other than an io.BytesIO"));
    }
    let opts = CanvasOpts { page_compression: None, invariant: false, initial_font_name: None, initial_font_size: None, initial_leading: None };
    let cell = Arc::new(Rl::Canvas(Mutex::new(CanvasCell {
        canvas: Some(Canvas::new(t.pagesize, &opts)?),
        active: false,
        pagesize: doc_attr(&d.lock(), "pagesize")?,
    })));
    let canvas_v = V::native(Native::Rl(cell.clone()));
    let Rl::Canvas(slot) = &*cell else { unreachable!() };
    let call = |f: &V, c: &mut Canvas, info: &DocInfo| -> R<()> {
        d.lock().page = Some(info.page);
        let swap = |c: &mut Canvas, active: bool| {
            let mut g = slot.lock();
            let mut other = g.canvas.take().expect("the callback canvas");
            std::mem::swap(c, &mut other);
            g.canvas = Some(other);
            g.active = active;
        };
        swap(c, true);
        let r = super::ops::block_on_sync(super::methods::call_value(cx, f, vec![canvas_v.clone(), me.clone()], vec![]), "a reportlab page callback");
        swap(c, false);
        r.map(|_| ()).map_err(annotate_page_begin)
    };
    // without a callback, reportlab's `_doNothing` runs: `doc.page` still counts the pages
    let mut on_first = |c: &mut Canvas, info: &DocInfo| -> R<()> {
        match &first {
            Some(f) => call(f, c, info),
            None => Ok(d.lock().page = Some(info.page)),
        }
    };
    let mut on_later = |c: &mut Canvas, info: &DocInfo| -> R<()> {
        match &later {
            Some(f) => call(f, c, info),
            None => Ok(d.lock().page = Some(info.page)),
        }
    };
    let pdf = t.build::<Exc>(story, Some(&mut on_first), Some(&mut on_later))?;
    // BaseDocTemplate.build consumes the list it is given (del flowables[0])
    if let V::List(l) = story_v {
        l.lock().clear();
    }
    let V::Native(n) = &file else { unreachable!() };
    let Native::BytesIO(b) = &**n else { unreachable!() };
    super::files::bytesio_method(b, "write", &[V::Bytes(Arc::from(pdf))])?;
    Ok(V::None)
}

// ---------------------------------------------------------------- the canvas of a page callback

const CANVAS_METHODS: [&str; 22] = [
    "saveState", "restoreState", "setFont", "setFontSize", "setFillColor", "setStrokeColor", "setFillColorRGB",
    "setStrokeColorRGB", "setFillGray", "setStrokeGray", "setLineWidth", "line", "rect", "circle", "roundRect",
    "drawString", "drawRightString", "drawCentredString", "translate", "scale", "rotate", "stringWidth",
];
const CANVAS_METHODS2: [&str; 1] = ["getPageNumber"];

fn canvas_call(cell: &Mutex<CanvasCell>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let mut g = cell.lock();
    if !g.active {
        return Err(unsupported("the canvas outside of its page callback"));
    }
    let c = g.canvas.as_mut().expect("the document's canvas");
    let n = |i: usize, p: &str| -> R<f64> {
        match arg(args, kwargs, i, p) {
            Some(v) => num(v, p),
            None => Err(Exc::type_error(format!("Canvas.{name}() missing 1 required positional argument: '{p}'"))),
        }
    };
    let flag = |i: usize, p: &str, d: bool| -> R<bool> { arg(args, kwargs, i, p).map(truth).unwrap_or(Ok(d)) };
    let alpha = |i: usize| opt_num(arg(args, kwargs, i, "alpha"), "alpha");
    let s = |i: usize, p: &str| -> R<String> {
        match arg(args, kwargs, i, p) {
            Some(v) => text(v, p),
            None => Err(Exc::type_error(format!("Canvas.{name}() missing 1 required positional argument: '{p}'"))),
        }
    };
    let only = |allowed: &[&str]| -> R<()> {
        match kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
            Some((k, _)) => Err(unsupported(format!("Canvas.{name}({k}=)"))),
            None => Ok(()),
        }
    };
    match name {
        "saveState" => c.save_state(),
        "restoreState" => c.restore_state()?,
        "setFont" => {
            only(&["psfontname", "size", "leading"])?;
            let font = s(0, "psfontname")?;
            if rl::fonts::font_by_name(&font).is_none() {
                return Err(Exc::new(&KEY_ERROR, vec![V::str(font)]));
            }
            c.set_font(&font, n(1, "size")?, opt_num(arg(args, kwargs, 2, "leading"), "leading")?)?
        }
        "setFontSize" => c.set_font_size(opt_num(arg(args, kwargs, 0, "size"), "size")?, opt_num(arg(args, kwargs, 1, "leading"), "leading")?),
        "setFillColor" | "setStrokeColor" => {
            only(&["aColor", "alpha"])?;
            let col = arg(args, kwargs, 0, "aColor").ok_or_else(|| Exc::type_error(format!("Canvas.{name}() missing 1 required positional argument: 'aColor'")))?;
            let col = color_of(col, "aColor", true)?.ok_or_else(|| unsupported(format!("Canvas.{name}(None)")))?;
            if name == "setFillColor" {
                c.set_fill_color(&col, alpha(1)?)?
            } else {
                c.set_stroke_color(&col, alpha(1)?)?
            }
        }
        "setFillColorRGB" => c.set_fill_color_rgb(n(0, "r")?, n(1, "g")?, n(2, "b")?, alpha(3)?)?,
        "setStrokeColorRGB" => c.set_stroke_color_rgb(n(0, "r")?, n(1, "g")?, n(2, "b")?, alpha(3)?)?,
        "setFillGray" => c.set_fill_gray(n(0, "gray")?, alpha(1)?)?,
        "setStrokeGray" => c.set_stroke_gray(n(0, "gray")?, alpha(1)?)?,
        "setLineWidth" => c.set_line_width(n(0, "width")?),
        "line" => c.line(n(0, "x1")?, n(1, "y1")?, n(2, "x2")?, n(3, "y2")?),
        "rect" => {
            only(&["x", "y", "width", "height", "stroke", "fill"])?;
            c.rect(n(0, "x")?, n(1, "y")?, n(2, "width")?, n(3, "height")?, flag(4, "stroke", true)?, flag(5, "fill", false)?)
        }
        "circle" => {
            only(&["x_cen", "y_cen", "r", "stroke", "fill"])?;
            c.circle(n(0, "x_cen")?, n(1, "y_cen")?, n(2, "r")?, flag(3, "stroke", true)?, flag(4, "fill", false)?)
        }
        "roundRect" => {
            only(&["x", "y", "width", "height", "radius", "stroke", "fill"])?;
            c.round_rect(n(0, "x")?, n(1, "y")?, n(2, "width")?, n(3, "height")?, n(4, "radius")?, flag(5, "stroke", true)?, flag(6, "fill", false)?)
        }
        "drawString" | "drawRightString" | "drawCentredString" => {
            only(&["x", "y", "text"])?;
            if args.len() > 3 {
                return Err(unsupported(format!("Canvas.{name}() with mode, charSpace or direction")));
            }
            let (x, y, t) = (n(0, "x")?, n(1, "y")?, s(2, "text")?);
            match name {
                "drawString" => c.draw_string(x, y, &t),
                "drawRightString" => c.draw_right_string(x, y, &t),
                _ => c.draw_centred_string(x, y, &t),
            }
        }
        "translate" => c.translate(n(0, "dx")?, n(1, "dy")?),
        "scale" => c.scale(n(0, "x")?, n(1, "y")?),
        "rotate" => c.rotate(n(0, "theta")?),
        "stringWidth" => {
            let t = s(0, "text")?;
            let font = opt_text(arg(args, kwargs, 1, "fontName"), "fontName")?;
            if let Some(f) = &font {
                if rl::fonts::font_by_name(f).is_none() {
                    return Err(Exc::new(&KEY_ERROR, vec![V::str(f)]));
                }
            }
            return Ok(V::Float(c.string_width(&t, font.as_deref(), opt_num(arg(args, kwargs, 2, "fontSize"), "fontSize")?)?));
        }
        "getPageNumber" => return Ok(V::Int(c.get_page_number())),
        _ => return Err(unsupported(format!("Canvas.{name}()"))),
    }
    Ok(V::None)
}

// ---------------------------------------------------------------- attributes, methods

fn method_of(me: V, names: &[&'static str], name: &str) -> Option<V> {
    names.iter().find(|m| **m == name).map(|m| V::native(Native::MethodOf(me, m)))
}

const GROUP_METHODS: [&str; 6] = ["add", "scale", "translate", "rotate", "skew", "shift"];

pub fn attr(x: &Arc<Rl>, name: &str) -> R {
    let me = || V::native(Native::Rl(x.clone()));
    match &**x {
        Rl::Doc(d) => match name {
            "build" => Ok(V::native(Native::MethodOf(me(), "build"))),
            _ => doc_attr(&d.lock(), name),
        },
        Rl::Style(s) => style_attr(&s.lock(), name),
        Rl::Sheet(_) => method_of(me(), &["add", "get", "has_key"], name).ok_or_else(|| unsupported(format!("StyleSheet1.{name}"))),
        Rl::Para(p) => match name {
            "style" => Ok(p.lock().style.clone()),
            "text" => Ok(V::str(rl::paraparser::clean_block_quoted_text(&p.lock().text))),
            _ => Err(unsupported(format!("Paragraph.{name}"))),
        },
        Rl::Spacer(s) => match name {
            "width" => Ok(s.lock().0.clone()),
            "height" => Ok(s.lock().1.clone()),
            _ => Err(unsupported(format!("Spacer.{name}"))),
        },
        Rl::Drawing(d) => {
            if let Some(m) = method_of(me(), &GROUP_METHODS, name) {
                return Ok(m);
            }
            let d = d.lock();
            match name {
                "width" => Ok(d.width.clone()),
                "height" => Ok(d.height.clone()),
                "hAlign" => Ok(V::str(match d.h_align {
                    HAlign::Left => "LEFT",
                    HAlign::Center => "CENTER",
                    HAlign::Right => "RIGHT",
                })),
                "vAlign" => Ok(V::str(&d.v_align)),
                "renderScale" => Ok(V::Float(d.render_scale)),
                "contents" => Ok(V::list(d.group.kids.clone())),
                _ => Err(unsupported(format!("Drawing.{name}"))),
            }
        }
        Rl::Group(g) => match name {
            "contents" => Ok(V::list(g.lock().kids.clone())),
            _ => method_of(me(), &GROUP_METHODS, name).ok_or_else(|| unsupported(format!("Group.{name}"))),
        },
        Rl::Shape(s) => shape_attr(&s.lock(), name),
        Rl::Color(c) => color_attr(c, name),
        Rl::Canvas(c) => match name {
            "_pagesize" => Ok(c.lock().pagesize.clone()),
            _ => method_of(me(), &CANVAS_METHODS, name)
                .or_else(|| method_of(me(), &CANVAS_METHODS2, name))
                .ok_or_else(|| unsupported(format!("Canvas.{name}"))),
        },
        Rl::Table(_) => method_of(me(), &["setStyle"], name).ok_or_else(|| unsupported(format!("Table.{name}"))),
        Rl::TableStyle(_) => method_of(me(), &["add"], name).ok_or_else(|| unsupported(format!("TableStyle.{name}"))),
        Rl::Hr(_) | Rl::PageBreak => Err(unsupported(format!("{}.{name}", type_name(x)))),
    }
}

pub fn setattr(x: &Arc<Rl>, name: &str, v: V) -> R<()> {
    match &**x {
        Rl::Doc(d) => doc_set(&mut d.lock(), name, v),
        Rl::Style(s) => style_setattr(&mut s.lock(), name, v),
        Rl::Hr(h) => hr_set(&mut h.lock(), name, &v),
        Rl::Drawing(d) => draw_set(&mut d.lock(), name, &v),
        Rl::Shape(s) => shape_set(&mut s.lock(), name, &v),
        Rl::Table(t) if matches!(name, "hAlign" | "vAlign" | "spaceBefore" | "spaceAfter") => table_set(&mut t.lock().opts, name, &v),
        Rl::Para(p) if name == "style" => {
            style_snapshot(&v)?;
            p.lock().style = v;
            Ok(())
        }
        _ => Err(unsupported(format!("setting {}.{name}", type_name(x)))),
    }
}

pub async fn method(cx: &Cx, recv: &V, x: &Arc<Rl>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match (&**x, name) {
        (Rl::Doc(d), "build") => build(cx, recv, d, args, kwargs).await,
        (Rl::Sheet(s), "add") => sheet_add(s, arg(args, kwargs, 0, "style").ok_or_else(|| Exc::type_error("StyleSheet1.add() missing 1 required positional argument: 'style'"))?, arg(args, kwargs, 1, "alias")),
        (Rl::Sheet(s), "has_key") => {
            let k = arg(args, kwargs, 0, "key").ok_or_else(|| Exc::type_error("StyleSheet1.has_key() missing 1 required positional argument: 'key'"))?;
            Ok(V::Bool(sheet_find(&s.lock(), &sheet_key(k)?).is_some()))
        }
        (Rl::Sheet(s), "get") => {
            let k = arg(args, kwargs, 0, "key").ok_or_else(|| Exc::type_error("StyleSheet1.get() missing 1 required positional argument: 'key'"))?;
            let key = sheet_key(k)?;
            match (sheet_find(&s.lock(), &key), arg(args, kwargs, 1, "default")) {
                (Some(v), _) => Ok(v),
                (None, Some(d)) => Ok(d.clone()),
                (None, None) => Err(Exc::new(&KEY_ERROR, vec![V::str(format!("Style '{key}' not found in stylesheet"))])),
            }
        }
        (Rl::Table(t), "setStyle") => {
            let s = arg(args, kwargs, 0, "tblstyle").ok_or_else(|| Exc::type_error("Table.setStyle() missing 1 required positional argument: 'tblstyle'"))?;
            let cmds = table_cmds(s)?;
            let mut t = t.lock();
            let before = t.cmds.len();
            t.cmds.extend(cmds);
            if let Err(e) = table_engine(&t) {
                t.cmds.truncate(before);
                return Err(e);
            }
            Ok(V::None)
        }
        (Rl::TableStyle(c), "add") => {
            let cmd = table_cmds(&V::list(vec![V::tuple(args.to_vec())]))?;
            if !kwargs.is_empty() {
                return Err(unsupported("TableStyle.add() with keywords"));
            }
            c.lock().extend(cmd);
            Ok(V::None)
        }
        (Rl::Group(g), "add") => group_add(&mut g.lock(), args, kwargs),
        (Rl::Drawing(d), "add") => group_add(&mut d.lock().group, args, kwargs),
        (Rl::Group(g), m) if GROUP_METHODS.contains(&m) => group_op(&mut g.lock(), m, args, kwargs),
        (Rl::Drawing(d), m) if GROUP_METHODS.contains(&m) => group_op(&mut d.lock().group, m, args, kwargs),
        (Rl::Canvas(c), m) => canvas_call(c, m, args, kwargs),
        _ => Err(unsupported(format!("{}.{name}()", type_name(x)))),
    }
}
