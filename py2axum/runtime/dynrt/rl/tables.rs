//! reportlab/platypus/tables.py: Table and TableStyle (split by row only, no rounded corners, shadows,
//! gradients, hyperlinks or "splitfirst"-style special rows: those inputs are refused, never approximated).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::canvas::{font_id, Canvas};
use super::colors::Color;
use super::flowables::Flowable;
use super::fonts;
use super::frames::{py_float, FUZZ};
use super::graphics::set_dash_checked;
use super::paraparser::py_repr;
use super::RlError;

/// A cell flowable; shared like Python objects (a repeated header row lives in every split part).
pub type FlowRef = Rc<RefCell<Box<dyn Flowable>>>;

/// What the caller puts in a cell (`None` is `Text("")`, a number is its `str()`).
pub enum CellInput {
    Text(String),
    Flow(Box<dyn Flowable>),
    /// A list (or tuple) of flowables, nested lists flattened.
    List(Vec<CellInput>),
}

/// A value in a style command after the op and the two coordinates.
#[derive(Clone, Debug, PartialEq)]
pub enum CmdVal {
    None,
    Int(i64),
    Num(f64),
    Str(String),
    Color(Color),
    List(Vec<CmdVal>),
}

/// `(op, (sc, sr), (ec, er), *args)`.
#[derive(Clone, Debug)]
pub struct TableCmd {
    pub op: String,
    pub start: (i64, i64),
    pub end: (i64, i64),
    pub args: Vec<CmdVal>,
}

impl TableCmd {
    pub fn new(op: &str, start: (i64, i64), end: (i64, i64), args: Vec<CmdVal>) -> TableCmd {
        TableCmd { op: op.to_string(), start, end, args }
    }
}

/// One entry of `colWidths`: None, a number, or a string ('*', '20%', '' or a number as text).
#[derive(Clone, Debug, PartialEq)]
pub enum ColWidth {
    Auto,
    Abs(f64),
    Str(String),
}

/// `colWidths=` / `rowHeights=`: one value for every column (row), or a list.
#[derive(Clone, Debug)]
pub enum Sizes<T> {
    Each(T),
    Seq(Vec<T>),
}

/// `repeatRows=`: a count or a list of row indices.
#[derive(Clone, Debug, PartialEq)]
pub enum RepeatRows {
    Count(i64),
    Rows(Vec<i64>),
}

/// The keyword arguments of `Table(data, ...)` that the port reproduces.
#[derive(Clone, Debug)]
pub struct TableOpts {
    pub col_widths: Sizes<ColWidth>,
    pub row_heights: Sizes<Option<f64>>,
    pub repeat_rows: RepeatRows,
    pub h_align: Option<String>,
    pub v_align: Option<String>,
    pub space_before: Option<f64>,
    pub space_after: Option<f64>,
}

impl Default for TableOpts {
    fn default() -> Self {
        TableOpts {
            col_widths: Sizes::Each(ColWidth::Auto),
            row_heights: Sizes::Each(None),
            repeat_rows: RepeatRows::Count(0),
            h_align: None,
            v_align: None,
            space_before: None,
            space_after: None,
        }
    }
}

#[derive(Clone)]
enum Val {
    Text(String),
    Flows(Vec<FlowRef>),
}

/// `CellStyle.color`: the class default is the string 'black' (never equal to a Color).
#[derive(Clone, Debug, PartialEq)]
enum CellColor {
    Black,
    C(Color),
}

/// `CellStyle` (one per cell).
#[derive(Clone, Debug)]
struct CellStyle {
    fontname: String,
    fontsize: f64,
    leading: f64,
    left_padding: f64,
    right_padding: f64,
    top_padding: f64,
    bottom_padding: f64,
    color: CellColor,
    alignment: String,
    valign: String,
}

impl Default for CellStyle {
    fn default() -> Self {
        CellStyle {
            fontname: "Helvetica".into(),
            fontsize: 10.0,
            leading: 12.0,
            left_padding: 6.0,
            right_padding: 6.0,
            top_padding: 3.0,
            bottom_padding: 3.0,
            color: CellColor::Black,
            alignment: "LEFT".into(),
            valign: "BOTTOM".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum DashV {
    Num(f64),
    List(Vec<f64>),
}

/// A line command after `_addCommand`'s normalisation (cap, dash, join, count, space filled in).
#[derive(Clone, Debug)]
struct LineCmd {
    op: &'static str,
    sc: i64,
    sr: i64,
    ec: i64,
    er: i64,
    weight: f64,
    color: Option<Color>,
    cap: Option<i64>,
    dash: Option<DashV>,
    join: Option<i64>,
    count: i64,
    space: f64,
}

/// BACKGROUND / ROWBACKGROUNDS / COLBACKGROUNDS / SPAN / NOSPLIT.
#[derive(Clone, Debug)]
struct RangeCmd {
    op: String,
    sc: i64,
    sr: i64,
    ec: i64,
    er: i64,
    args: Vec<CmdVal>,
}

trait Ranged: Clone {
    fn rng(&self) -> (i64, i64, i64, i64);
    fn with(&self, sc: i64, sr: i64, ec: i64, er: i64) -> Self;
}

impl Ranged for LineCmd {
    fn rng(&self) -> (i64, i64, i64, i64) {
        (self.sc, self.sr, self.ec, self.er)
    }
    fn with(&self, sc: i64, sr: i64, ec: i64, er: i64) -> Self {
        LineCmd { sc, sr, ec, er, ..self.clone() }
    }
}

impl Ranged for RangeCmd {
    fn rng(&self) -> (i64, i64, i64, i64) {
        (self.sc, self.sr, self.ec, self.er)
    }
    fn with(&self, sc: i64, sr: i64, ec: i64, er: i64) -> Self {
        RangeCmd { sc, sr, ec, er, ..self.clone() }
    }
}

const LINE_OPS: [&str; 8] = ["GRID", "BOX", "OUTLINE", "INNERGRID", "LINEBELOW", "LINEABOVE", "LINEBEFORE", "LINEAFTER"];

type Span = (i64, i64, i64, i64);
type Blocks = Vec<(f64, Vec<(f64, f64)>)>;

/// `Table(data, colWidths=, rowHeights=, style=, repeatRows=, hAlign=, vAlign=, spaceBefore=, spaceAfter=)`.
#[derive(Clone)]
pub struct Table {
    h_align: String,
    v_align: String,
    nrows: usize,
    ncols: usize,
    cells: Vec<Vec<Val>>,
    styles: Vec<Vec<CellStyle>>,
    arg_w: Vec<ColWidth>,
    col_widths: Vec<ColWidth>,
    arg_h: Vec<Option<f64>>,
    row_heights: Vec<Option<f64>>,
    repeat_rows: RepeatRows,
    space_before: Option<f64>,
    space_after: Option<f64>,
    bkgrnd: Vec<RangeCmd>,
    lines: Vec<LineCmd>,
    spans: Vec<RangeCmd>,
    nosplits: Vec<RangeCmd>,
    width_once: bool,
    colpositions: Vec<f64>,
    rowpositions: Vec<f64>,
    width: f64,
    height: f64,
    // spans (`_calcSpanRanges` / `_calcSpanRects`)
    span_ranges: HashMap<(i64, i64), Option<Span>>,
    col_span_cells: HashSet<(i64, i64)>,
    row_span_cells: Vec<(i64, i64)>,
    span_rects: HashMap<(i64, i64), Option<(f64, f64, f64, f64)>>,
    h_blocks: Blocks,
    v_blocks: Blocks,
    nosplit_ranges: HashMap<(i64, i64), Option<Span>>,
    row_nosplit_cells: Vec<(i64, i64)>,
    // draw state
    cur_weight: Option<f64>,
    cur_color: Option<Color>,
}

/// Python's `sum()` of floats (Neumaier compensation, CPython >= 3.12).
fn py_sum(v: impl IntoIterator<Item = f64>) -> f64 {
    let mut f = 0.0f64;
    let mut c = 0.0f64;
    for x in v {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// `seq[a:b]` with Python's slice rules.
fn py_slice<T: Clone>(v: &[T], a: i64, b: i64) -> Vec<T> {
    let n = v.len() as i64;
    let norm = |i: i64| -> i64 {
        let i = if i < 0 { i + n } else { i };
        i.clamp(0, n)
    };
    let (a, b) = (norm(a), norm(b));
    if a >= b {
        Vec::new()
    } else {
        v[a as usize..b as usize].to_vec()
    }
}

/// `seq[i]` with negative indices.
fn py_index<T: Copy>(v: &[T], i: i64) -> Result<T, RlError> {
    let n = v.len() as i64;
    let j = if i < 0 { i + n } else { i };
    if j < 0 || j >= n {
        return Err(RlError::Index("list index out of range".into()));
    }
    Ok(v[j as usize])
}

fn cmd_num(v: &CmdVal, what: &str) -> Result<f64, RlError> {
    match v {
        CmdVal::Int(i) => Ok(*i as f64),
        CmdVal::Num(f) => Ok(*f),
        _ => Err(RlError::Unsupported(format!("table style {what} value {v:?}"))),
    }
}

fn cmd_str(v: &CmdVal, what: &str) -> Result<String, RlError> {
    match v {
        CmdVal::Str(s) => Ok(s.clone()),
        _ => Err(RlError::Unsupported(format!("table style {what} value {v:?}"))),
    }
}

/// `colors.toColor(arg)`.
pub fn to_color(v: &CmdVal) -> Result<Color, RlError> {
    match v {
        CmdVal::Color(c) => Ok(c.clone()),
        CmdVal::Str(s) => Color::parse(s).or_else(|_| {
            Color::named(&s.to_lowercase()).ok_or_else(|| RlError::Value(format!("Invalid color value {}", py_repr(s, false))))
        }),
        CmdVal::List(a) if (3..=4).contains(&a.len()) => {
            let n: Vec<f64> = a.iter().map(|x| cmd_num(x, "color")).collect::<Result<_, _>>()?;
            if n.iter().any(|&x| !(0.0..=1.0).contains(&x)) {
                return Err(RlError::Unsupported("color component outside [0, 1]".into()));
            }
            Ok(if n.len() == 3 { Color::rgb(n[0], n[1], n[2]) } else { Color::cmyk(n[0], n[1], n[2], n[3], 1.0, 1.0) })
        }
        _ => Err(RlError::Unsupported(format!("table style colour {v:?}"))),
    }
}

/// `colors.toColorOrNone(arg)`.
pub fn to_color_or_none(v: &CmdVal) -> Result<Option<Color>, RlError> {
    match v {
        CmdVal::None => Ok(None),
        _ => to_color(v).map(Some),
    }
}

/// `repr()` of a command value (error messages).
fn val_repr(v: &CmdVal) -> String {
    match v {
        CmdVal::None => "None".into(),
        CmdVal::Int(i) => i.to_string(),
        CmdVal::Num(f) => py_float(*f),
        CmdVal::Str(s) => py_repr(s, true),
        CmdVal::Color(c) => match c {
            Color::Rgb { r, g, b, alpha } => {
                format!("Color({})", super::accel::fp_str(&[*r, *g, *b, alpha.unwrap_or(1.0)]).replace(' ', ","))
            }
            Color::Cmyk { .. } => format!("{c:?}"),
        },
        CmdVal::List(a) => format!("[{}]", a.iter().map(val_repr).collect::<Vec<_>>().join(", ")),
    }
}

fn cmd_repr(c: &TableCmd) -> String {
    let mut parts = vec![py_repr(&c.op, true), format!("({}, {})", c.start.0, c.start.1), format!("({}, {})", c.end.0, c.end.1)];
    parts.extend(c.args.iter().map(val_repr));
    format!("[{}]", parts.join(", "))
}

/// `_convert2int(value, LINECAPS/LINEJOINS, 0, 2, name, cmd)`.
fn convert2int(v: &CmdVal, joins: bool, name: &str, cmd: &TableCmd) -> Result<Option<i64>, RlError> {
    let mapped = match v {
        CmdVal::None => return Ok(None),
        CmdVal::Str(s) => match (s.as_str(), joins) {
            ("butt", false) | ("miter", true) | ("mitre", true) => Some(0),
            ("round", _) => Some(1),
            ("projecting", false) | ("squared", false) | ("bevel", true) => Some(2),
            _ => s.trim().parse::<i64>().ok().filter(|i| (0..=2).contains(i)),
        },
        CmdVal::Int(i) => Some(*i).filter(|i| (0..=2).contains(i)),
        CmdVal::Num(f) if f.is_finite() => Some(f.trunc() as i64).filter(|i| (0..=2).contains(i)),
        _ => None,
    };
    let shown = match v {
        CmdVal::Str(s) => s.clone(),
        o => val_repr(o),
    };
    mapped.map(Some).ok_or_else(|| RlError::Value(format!("Bad {name} value {shown} in {}", cmd_repr(cmd))))
}

fn flatten(c: CellInput, out: &mut Vec<FlowRef>) -> Result<(), RlError> {
    match c {
        CellInput::Flow(f) => out.push(Rc::new(RefCell::new(f))),
        CellInput::List(v) => {
            for x in v {
                flatten(x, out)?;
            }
        }
        CellInput::Text(_) => return Err(RlError::Unsupported("a string inside a list of cell flowables".into())),
    }
    Ok(())
}

/// The `type(v)` of a flowable as Python prints it.
fn py_type(f: &dyn Flowable) -> String {
    let m = match f.class_name() {
        "Paragraph" => "reportlab.platypus.paragraph",
        "Table" => "reportlab.platypus.tables",
        "Drawing" => "reportlab.graphics.shapes",
        _ => "reportlab.platypus.flowables",
    };
    format!("<class '{m}.{}'>", f.class_name())
}

/// `_hLine`: a horizontal (or, with a swapping `line`, vertical) line skipping the blocked spans.
fn h_line(line: &mut dyn FnMut(f64, f64, f64, f64), mut scp: f64, ecp: f64, y: f64, blocks: &Blocks) {
    let hb = blocks.iter().find(|(k, _)| *k == y).map(|(_, v)| v);
    let hb = match hb {
        Some(b) if !b.is_empty() && !(scp >= b[b.len() - 1].1 - FUZZ || ecp <= b[0].0 + FUZZ) => b,
        _ => return line(scp, y, ecp, y),
    };
    let mut i = 0;
    while scp < ecp - FUZZ && i < hb.len() {
        let (x0, x1) = hb[i];
        if x1 <= scp + FUZZ || x0 >= ecp - FUZZ {
            i += 1;
            continue;
        }
        let i0 = scp.max(x0);
        let i1 = ecp.min(x1);
        if i0 > scp {
            line(scp, y, i0, y);
        }
        scp = i1;
    }
    if scp < ecp - FUZZ {
        line(scp, y, ecp, y);
    }
}

/// `_multiLine`.
fn multi_line(line: &mut dyn FnMut(f64, f64, f64, f64), scp: f64, ecp: f64, mut y: f64, ws: f64, count: i64) {
    let offset = 0.5 * (count - 1) as f64 * ws;
    y += offset;
    for _ in 0..count {
        line(scp, y, ecp, y);
        y -= ws;
    }
}

/// `spanFixDim(V0, V, spanCons)`.
fn span_fix_dim(v0: &[Option<f64>], v: &mut [Option<f64>], cons: &HashMap<(i64, i64), f64>) -> Result<(), RlError> {
    let mut m: Vec<(usize, f64)> = Vec::new();
    let lim = v0.len() as i64;
    let mut items: Vec<(f64, (i64, i64))> = cons.iter().map(|(k, v)| (*v, *k)).collect();
    items.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let get = |m: &Vec<(usize, f64)>, x: usize| m.iter().find(|e| e.0 == x).map(|e| e.1).unwrap_or(0.0);
    for (val, (x0, x1)) in items.into_iter().rev() {
        if x0 >= lim {
            continue;
        }
        let x1 = x1 + 1;
        let mut parts = Vec::new();
        for x in x0..x1 {
            let vx = v.get(x as usize).copied().flatten().ok_or_else(|| {
                RlError::Type("unsupported operand type(s) for +: 'NoneType' and 'int'".into())
            })?;
            parts.push(vx + get(&m, x as usize));
        }
        let t = py_sum(parts);
        if t >= val - FUZZ {
            continue;
        }
        let xs: Vec<usize> = (x0..x1).map(|x| x as usize).filter(|&x| v0[x].is_none()).collect();
        if xs.is_empty() {
            continue;
        }
        let d = (val - t) / xs.len() as f64;
        for x in xs {
            match m.iter_mut().find(|e| e.0 == x) {
                Some(e) => e.1 += d,
                None => m.push((x, d)),
            }
        }
    }
    for (x, d) in m {
        if let Some(w) = v[x].as_mut() {
            *w += d;
        }
    }
    Ok(())
}

/// `_calc_pc(V, avail)`.
fn calc_pc(v: &[ColWidth], avail: f64) -> Result<Vec<Option<f64>>, RlError> {
    let mut r: Vec<Option<f64>> = Vec::new();
    let mut pcs: Vec<usize> = Vec::new();
    let mut stars: Vec<usize> = Vec::new();
    let mut s = avail;
    let mut w = 0.0;
    let mut n = 0;
    for x in v {
        match x {
            ColWidth::Str(t) => {
                let t = t.trim();
                if t.is_empty() {
                    n += 1;
                    r.push(None);
                } else if let Some(p) = t.strip_suffix('%') {
                    let f = py_float_parse(p)?;
                    w += f;
                    pcs.push(r.len());
                    r.push(Some(f));
                } else if t == "*" {
                    stars.push(r.len());
                    r.push(None);
                } else {
                    let f = py_float_parse(t)?;
                    s -= f;
                    r.push(Some(f));
                }
            }
            ColWidth::Auto => {
                n += 1;
                r.push(None);
            }
            ColWidth::Abs(f) => {
                s -= f;
                r.push(Some(*f));
            }
        }
    }
    s = if s > 0.0 { s } else { 0.0 };
    let f = s / if w > 100.0 { w } else { 100.0 };
    for i in pcs {
        let x = r[i].unwrap() * f;
        r[i] = Some(x);
        s -= x;
    }
    s = if s > 0.0 { s } else { 0.0 };
    let m = stars.len();
    if m > 0 {
        let v = if n == 0 && s / m as f64 != 0.0 { Some(s / m as f64) } else { None };
        for j in stars {
            r[j] = v;
        }
    }
    Ok(r)
}

/// `float(s)` for a stripped string.
fn py_float_parse(s: &str) -> Result<f64, RlError> {
    let t = s.trim();
    let ok = !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || "+-.eE_".contains(c));
    match t.replace('_', "").parse::<f64>() {
        Ok(f) if ok => Ok(f),
        _ => Err(RlError::Value(format!("could not convert string to float: {}", py_repr(s, false)))),
    }
}

fn is_auto_or_star(w: &ColWidth) -> bool {
    matches!(w, ColWidth::Auto) || matches!(w, ColWidth::Str(s) if s == "*")
}

fn ends_pct(w: &ColWidth) -> bool {
    matches!(w, ColWidth::Str(s) if s.ends_with('%'))
}

impl Table {
    /// `Table(data, **opts)` (normalizeData, short rows padded, one CellStyle per cell).
    pub fn new(data: Vec<Vec<CellInput>>, opts: TableOpts) -> Result<Table, RlError> {
        let nrows = data.len();
        let ncols = match (&opts.col_widths, nrows) {
            (_, n) if n > 0 => data.iter().map(|r| r.len()).max().unwrap_or(0),
            (Sizes::Seq(cw), _) if !cw.is_empty() => cw.len(),
            _ => 0,
        };
        if nrows == 0 || ncols == 0 {
            let nc = if nrows == 0 { "unknown".to_string() } else { ncols.to_string() };
            return Err(RlError::Value(format!("<Table@0x00000000 {nrows} rows x {nc} cols>... must have at least a row and column")));
        }
        let mut cells = Vec::with_capacity(nrows);
        for row in data {
            let mut r = Vec::with_capacity(ncols);
            for c in row {
                r.push(match c {
                    CellInput::Text(s) => Val::Text(s),
                    other => {
                        let mut v = Vec::new();
                        flatten(other, &mut v)?;
                        Val::Flows(v)
                    }
                });
            }
            while r.len() < ncols {
                r.push(Val::Text(String::new()));
            }
            cells.push(r);
        }
        let col_widths = match opts.col_widths {
            Sizes::Each(w) => vec![w; ncols],
            Sizes::Seq(mut v) => {
                if v.is_empty() {
                    return Err(RlError::Index("list index out of range".into()));
                }
                let last = v[v.len() - 1].clone();
                v.resize(ncols, last);
                v
            }
        };
        let row_heights = match opts.row_heights {
            Sizes::Each(h) => vec![h; nrows],
            Sizes::Seq(v) if v.len() != nrows => {
                return Err(RlError::Value(format!(
                    "<Table@0x00000000 {nrows} rows x {ncols} cols>... data error - {nrows} rows in data but {} row heights",
                    v.len()
                )))
            }
            Sizes::Seq(v) => v,
        };
        let styles = vec![vec![CellStyle::default(); ncols]; nrows];
        let h_align = opts.h_align.filter(|s| !s.is_empty()).unwrap_or_else(|| "CENTER".into());
        let v_align = opts.v_align.filter(|s| !s.is_empty()).unwrap_or_else(|| "MIDDLE".into());
        Ok(Table::raw(cells, col_widths, row_heights, styles, opts.repeat_rows, h_align, v_align, opts.space_before, opts.space_after))
    }

    #[allow(clippy::too_many_arguments)]
    fn raw(
        cells: Vec<Vec<Val>>,
        col_widths: Vec<ColWidth>,
        row_heights: Vec<Option<f64>>,
        styles: Vec<Vec<CellStyle>>,
        repeat_rows: RepeatRows,
        h_align: String,
        v_align: String,
        space_before: Option<f64>,
        space_after: Option<f64>,
    ) -> Table {
        Table {
            h_align,
            v_align,
            nrows: cells.len(),
            ncols: col_widths.len(),
            cells,
            styles,
            arg_w: col_widths.clone(),
            col_widths,
            arg_h: row_heights.clone(),
            row_heights,
            repeat_rows,
            space_before,
            space_after,
            bkgrnd: Vec::new(),
            lines: Vec::new(),
            spans: Vec::new(),
            nosplits: Vec::new(),
            width_once: false,
            colpositions: Vec::new(),
            rowpositions: Vec::new(),
            width: 0.0,
            height: 0.0,
            span_ranges: HashMap::new(),
            col_span_cells: HashSet::new(),
            row_span_cells: Vec::new(),
            span_rects: HashMap::new(),
            h_blocks: Vec::new(),
            v_blocks: Vec::new(),
            nosplit_ranges: HashMap::new(),
            row_nosplit_cells: Vec::new(),
            cur_weight: None,
            cur_color: None,
        }
    }

    /// `setStyle(TableStyle(cmds))`: may be called several times, commands accumulate.
    pub fn set_style(&mut self, cmds: &[TableCmd]) -> Result<(), RlError> {
        for c in cmds {
            self.add_command(c)?;
        }
        Ok(())
    }

    pub fn v_align(&self) -> &str {
        &self.v_align
    }

    fn norm_cell_range(&self, sc: i64, ec: i64, sr: i64, er: i64) -> (i64, i64, i64, i64) {
        let (nc, nr) = (self.ncols as i64, self.nrows as i64);
        let sc = if sc < 0 { sc + nc } else { sc };
        let ec = if ec < 0 { ec + nc } else { ec };
        let sr = if sr < 0 { sr + nr } else { sr };
        let er = if er < 0 { er + nr } else { er };
        (sc.max(0), ec.min(nc - 1), sr.max(0), er.min(nr - 1))
    }

    /// `_addCommand`.
    fn add_command(&mut self, c: &TableCmd) -> Result<(), RlError> {
        let (sc, sr) = c.start;
        let (ec, er) = c.end;
        let op = c.op.as_str();
        let range = |c: &TableCmd| RangeCmd { op: c.op.clone(), sc, sr, ec, er, args: c.args.clone() };
        match op {
            "BACKGROUND" | "ROWBACKGROUNDS" | "COLBACKGROUNDS" => self.bkgrnd.push(range(c)),
            "SPAN" => self.spans.push(range(c)),
            "NOSPLIT" => self.nosplits.push(range(c)),
            "ROUNDEDCORNERS" => return Err(RlError::Unsupported("ROUNDEDCORNERS".into())),
            _ if LINE_OPS.contains(&op) => {
                let opn = *LINE_OPS.iter().find(|o| **o == op).unwrap();
                let a = &c.args;
                if a.len() < 2 {
                    return Err(RlError::Value(format!("bad line command {}", cmd_repr(c))));
                }
                let weight = cmd_num(&a[0], "line weight")?;
                let color = to_color_or_none(&a[1])?;
                let cap = match a.get(2) {
                    None => Some(1),
                    Some(v) => convert2int(v, false, "cap", c)?,
                };
                let dash = match a.get(3) {
                    None | Some(CmdVal::None) => None,
                    Some(CmdVal::List(l)) => Some(DashV::List(l.iter().map(|x| cmd_num(x, "dash")).collect::<Result<_, _>>()?)),
                    Some(v) => Some(DashV::Num(cmd_num(v, "dash")?)),
                };
                let join = match a.get(4) {
                    None => Some(1),
                    Some(v) => convert2int(v, true, "join", c)?,
                };
                let count = match a.get(5) {
                    None | Some(CmdVal::None) => 1,
                    Some(v) => cmd_num(v, "line count")? as i64,
                };
                if count < 1 {
                    return Err(RlError::Unsupported("AssertionError: line count < 1".into()));
                }
                let space = match a.get(6) {
                    None | Some(CmdVal::None) => weight,
                    Some(v) => cmd_num(v, "line space")?,
                };
                if a.len() > 7 {
                    return Err(RlError::Unsupported("AssertionError: line command longer than 10".into()));
                }
                self.lines.push(LineCmd { op: opn, sc, sr, ec, er, weight, color, cap, dash, join, count, space });
            }
            _ => {
                let (sc, ec, sr, er) = self.norm_cell_range(sc, ec, sr, er);
                for i in sr..=er {
                    for j in sc..=ec {
                        self.set_cell_style(i as usize, j as usize, op, &c.args)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// `_setCellStyle` (unknown ops are ignored, like reportlab).
    fn set_cell_style(&mut self, i: usize, j: usize, op: &str, v: &[CmdVal]) -> Result<(), RlError> {
        let first = || v.first().ok_or_else(|| RlError::Index("tuple index out of range".into()));
        let s = &mut self.styles[i][j];
        match op {
            "FONT" => {
                s.fontname = cmd_str(first()?, "FONT")?;
                if v.len() > 1 {
                    s.fontsize = cmd_num(&v[1], "FONT size")?;
                    s.leading = if v.len() > 2 { cmd_num(&v[2], "FONT leading")? } else { s.fontsize * 1.2 };
                }
            }
            "FONTNAME" | "FACE" => s.fontname = cmd_str(first()?, "FONTNAME")?,
            "SIZE" | "FONTSIZE" => s.fontsize = cmd_num(first()?, "FONTSIZE")?,
            "LEADING" => s.leading = cmd_num(first()?, "LEADING")?,
            "TEXTCOLOR" => {
                s.color = CellColor::C(match first()? {
                    CmdVal::None => Color::rgb(0.0, 0.0, 0.0),
                    x => to_color(x).unwrap_or_else(|_| Color::rgb(0.0, 0.0, 0.0)),
                })
            }
            "ALIGN" | "ALIGNMENT" => s.alignment = cmd_str(first()?, "ALIGN")?,
            "VALIGN" => s.valign = cmd_str(first()?, "VALIGN")?,
            "LEFTPADDING" => s.left_padding = cmd_num(first()?, "LEFTPADDING")?,
            "RIGHTPADDING" => s.right_padding = cmd_num(first()?, "RIGHTPADDING")?,
            "TOPPADDING" => s.top_padding = cmd_num(first()?, "TOPPADDING")?,
            "BOTTOMPADDING" => s.bottom_padding = cmd_num(first()?, "BOTTOMPADDING")?,
            "HREF" | "DESTINATION" | "DIRECTION" | "SHAPING" => {
                if !matches!(first()?, CmdVal::None) {
                    return Err(RlError::Unsupported(format!("table style {op}")));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn identity(&self) -> String {
        format!("<Table@0x00000000 {} rows x {} cols>...", self.nrows, self.ncols)
    }

    // ---- sizing ----

    /// `_elementWidth(v, s)`: None when a flowable cannot give one.
    fn element_width(v: &Val, s: &CellStyle) -> Result<Option<f64>, RlError> {
        match v {
            Val::Flows(fs) => {
                let mut w = 0.0;
                for f in fs {
                    let f = f.borrow();
                    let ew = if f.fixed_width() { f.size().map(|s| s.0).unwrap_or(0.0) } else { f.min_width()? };
                    if ew > w {
                        w = ew;
                    }
                }
                Ok(Some(w))
            }
            Val::Text(t) => {
                let id = font_id(&s.fontname)?;
                let mut m: Option<f64> = None;
                for x in t.split('\n') {
                    let w = fonts::string_width(x, id, s.fontsize);
                    m = Some(match m {
                        Some(m) if m >= w => m,
                        _ => w,
                    });
                }
                Ok(m)
            }
        }
    }

    fn can_get_width(v: &Val) -> bool {
        match v {
            Val::Text(_) => true,
            Val::Flows(fs) => fs.iter().all(|f| f.borrow().fixed_width()),
        }
    }

    fn has_vari_width_elements(&self) -> bool {
        self.cells.iter().any(|r| r.iter().any(|v| !Table::can_get_width(v)))
    }

    /// `_listCellGeom(V, w, s, W, H, aH)`.
    fn list_cell_geom(
        &self,
        canv: &Canvas,
        vs: &[FlowRef],
        w: f64,
        s: &CellStyle,
        mut ws: Option<&mut Vec<f64>>,
        mut hs: Option<&mut Vec<f64>>,
        ah: f64,
    ) -> Result<(f64, f64), RlError> {
        if vs.is_empty() {
            return Ok((0.0, 0.0));
        }
        let aw = w - s.left_padding - s.right_padding;
        let ah = ah - s.top_padding - s.bottom_padding;
        if aw < 0.0 {
            return Err(RlError::Value(format!(
                "{}: flowable given negative availWidth={} == width={} - leftPadding={} - rightPadding={}",
                self.identity(),
                py_float(aw),
                py_float(w),
                py_float(s.left_padding),
                py_float(s.right_padding)
            )));
        }
        let (mut t, mut wm) = (0.0, 0.0);
        let mut sb0: Option<f64> = None;
        let mut sa = 0.0;
        for v in vs {
            let mut v = v.borrow_mut();
            let (vw, vh) = v.wrap(canv, aw, ah)?;
            let sb = v.space_before();
            sa = v.space_after();
            if let Some(w) = ws.as_deref_mut() {
                w.push(vw);
            }
            if let Some(h) = hs.as_deref_mut() {
                h.push(vh);
            }
            if vw > wm {
                wm = vw;
            }
            t += vh + sa + sb;
            if sb0.is_none() {
                sb0 = Some(sb);
            }
        }
        Ok((wm, t - sb0.unwrap() - sa))
    }

    /// `_listValueWidth(V)`.
    fn list_value_width(canv: &Canvas, vs: &[FlowRef]) -> Result<f64, RlError> {
        let mut m: Option<f64> = None;
        for v in vs {
            let w = v.borrow_mut().wrap(canv, 72000.0, 72000.0)?.0;
            m = Some(match m {
                Some(m) if m >= w => m,
                _ => w,
            });
        }
        m.ok_or_else(|| RlError::Unsupported("empty cell list with a zero column width".into()))
    }

    /// `_calcPreliminaryWidths(availWidth)`.
    fn calc_preliminary_widths(&mut self, avail: f64) -> Result<Option<Vec<f64>>, RlError> {
        let mut w: Vec<ColWidth> = self.arg_w.clone();
        let mut total_defined = 0.0;
        let mut percent_total = 0.0;
        let mut n_undef = 0usize;
        let mut n_greedy = 0usize;
        for x in &w {
            match x {
                ColWidth::Auto => n_undef += 1,
                ColWidth::Str(s) if s == "*" => {
                    n_undef += 1;
                    n_greedy += 1;
                }
                ColWidth::Str(s) if s.ends_with('%') => percent_total += py_float_parse(&s[..s.len() - 1])?,
                ColWidth::Abs(f) => total_defined += f,
                ColWidth::Str(_) => return Err(RlError::Unsupported("AssertionError: non numeric column width".into())),
            }
        }
        let mut given = 0usize;
        let mut minimums: Vec<(usize, f64)> = Vec::new();
        let mut total_minimum = 0.0;
        for col in 0..self.ncols {
            let x = &w[col];
            if is_auto_or_star(x) || ends_pct(x) {
                let mut fin = 0.0;
                for row in 0..self.nrows {
                    let st = &self.styles[row][col];
                    let mut new = Table::element_width(&self.cells[row][col], st)?.unwrap_or(0.0);
                    new += st.left_padding + st.right_padding;
                    if new > fin {
                        fin = new;
                    }
                }
                minimums.push((col, fin));
                total_minimum += fin;
            } else {
                given += 1;
            }
        }
        if given == self.ncols {
            return Ok(None);
        }
        let mut remaining = avail - (total_minimum + total_defined);
        let mut out: Vec<f64> = w.iter().map(|x| if let ColWidth::Abs(f) = x { *f } else { 0.0 }).collect();
        if remaining > 0.0 {
            let defined_pct = (total_defined / avail) * 100.0;
            percent_total += defined_pct;
            let default_desired;
            if n_undef > 0 && percent_total < 100.0 {
                let undefined = if n_greedy > 0 { n_greedy } else { n_undef };
                let default_weight = (100.0 - percent_total) / undefined as f64;
                percent_total = 100.0;
                default_desired = (default_weight / percent_total) * avail;
            } else {
                default_desired = 1.0;
            }
            let mut desired_widths: Vec<(f64, f64, f64, usize)> = Vec::new();
            let mut total_desired = 0.0;
            let mut eff_remaining = remaining;
            for &(col, minimum) in &minimums {
                let x = &w[col];
                let desired = if let ColWidth::Str(s) = x.clone().filter_pct() {
                    (py_float_parse(&s[..s.len() - 1])? / percent_total) * avail
                } else if is_auto_or_star(x) && !matches!(x, ColWidth::Auto) {
                    default_desired
                } else if n_greedy == 0 && default_desired != 0.0 {
                    default_desired
                } else {
                    1.0
                };
                if desired <= minimum {
                    out[col] = minimum;
                } else {
                    desired_widths.push((desired - minimum, minimum, desired, col));
                    total_desired += desired;
                    eff_remaining += minimum;
                }
            }
            if !desired_widths.is_empty() {
                let mut proportion = eff_remaining / total_desired;
                desired_widths.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let mut final_set = Vec::new();
                for (_d, minimum, desired, col) in desired_widths {
                    let adjusted = proportion * desired;
                    if adjusted < minimum {
                        out[col] = minimum;
                        total_desired -= desired;
                        eff_remaining -= minimum;
                        if total_desired != 0.0 {
                            proportion = eff_remaining / total_desired;
                        }
                    } else {
                        final_set.push((minimum, desired, col));
                    }
                }
                for (minimum, desired, col) in final_set {
                    let adjusted = proportion * desired;
                    if adjusted < minimum {
                        return Err(RlError::Unsupported("AssertionError: adjusted < minimum".into()));
                    }
                    out[col] = adjusted;
                }
            }
        } else {
            if percent_total > 0.0 {
                return Err(RlError::Attribute("'float' object has no attribute 'endswith'".into()));
            }
            let adj;
            if remaining < 0.0 && total_defined * 0.02 + remaining >= 0.0 {
                let a = -remaining / total_defined;
                for col in 0..self.ncols {
                    if !minimums.iter().any(|m| m.0 == col) {
                        let dw = a * out[col];
                        out[col] -= dw;
                        total_defined -= dw;
                    }
                }
                let _ = avail - (total_defined + total_minimum);
                adj = 1.0;
            } else {
                remaining = avail - total_defined;
                adj = if remaining <= 0.0 {
                    1.0
                } else {
                    if total_minimum == 0.0 {
                        return Err(RlError::Value("float division by zero".into()));
                    }
                    remaining / total_minimum
                };
            }
            for &(col, minimum) in &minimums {
                out[col] = minimum * adj;
            }
        }
        w = out.iter().map(|f| ColWidth::Abs(*f)).collect();
        self.arg_w = w.clone();
        self.col_widths = w;
        Ok(Some(out))
    }

    /// `minWidth()`.
    fn table_min_width(&self) -> Result<f64, RlError> {
        let mut width = 0.0;
        for col in 0..self.arg_w.len() {
            let w = &self.arg_w[col];
            if is_auto_or_star(w) || ends_pct(w) {
                let mut fin = 0.0;
                for row in 0..self.nrows {
                    let st = &self.styles[row][col];
                    let ew = Table::element_width(&self.cells[row][col], st)?.ok_or_else(|| {
                        RlError::Type("unsupported operand type(s) for +: 'NoneType' and 'int'".into())
                    })?;
                    let new = ew + st.left_padding + st.right_padding;
                    if new > fin {
                        fin = new;
                    }
                }
                width += fin;
            } else {
                width += match w {
                    ColWidth::Abs(f) => *f,
                    ColWidth::Str(s) => py_float_parse(s)?,
                    ColWidth::Auto => unreachable!(),
                };
            }
        }
        Ok(width)
    }

    fn calc_span_ranges(&mut self) {
        let (nc, nr) = (self.ncols as i64, self.nrows as i64);
        self.span_ranges.clear();
        for x in 0..nc {
            for y in 0..nr {
                self.span_ranges.insert((x, y), Some((x, y, x, y)));
            }
        }
        self.col_span_cells.clear();
        self.row_span_cells.clear();
        for c in self.spans.clone() {
            let (mut x0, mut y0, mut x1, mut y1) = (c.sc, c.sr, c.ec, c.er);
            if x0 < 0 {
                x0 += nc
            }
            if x1 < 0 {
                x1 += nc
            }
            if y0 < 0 {
                y0 += nr
            }
            if y1 < 0 {
                y1 += nr
            }
            if x0 > x1 {
                std::mem::swap(&mut x0, &mut x1)
            }
            if y0 > y1 {
                std::mem::swap(&mut y0, &mut y1)
            }
            if x0 != x1 || y0 != y1 {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        if x0 != x1 {
                            self.col_span_cells.insert((x, y));
                        }
                        if y0 != y1 {
                            self.row_span_cells.push((x, y));
                        }
                        self.span_ranges.insert((x, y), None);
                    }
                }
                self.span_ranges.insert((x0, y0), Some((x0, y0, x1, y1)));
            }
        }
    }

    fn calc_nosplit_ranges(&mut self) {
        let (nc, nr) = (self.ncols as i64, self.nrows as i64);
        self.nosplit_ranges.clear();
        for x in 0..nc {
            for y in 0..nr {
                self.nosplit_ranges.insert((x, y), Some((x, y, x, y)));
            }
        }
        self.row_nosplit_cells.clear();
        for c in self.nosplits.clone() {
            let (mut x0, mut y0, mut x1, mut y1) = (c.sc, c.sr, c.ec, c.er);
            if x0 < 0 {
                x0 += nc
            }
            if x1 < 0 {
                x1 += nc
            }
            if y0 < 0 {
                y0 += nr
            }
            if y1 < 0 {
                y1 += nr
            }
            if x0 > x1 {
                std::mem::swap(&mut x0, &mut x1)
            }
            if y0 > y1 {
                std::mem::swap(&mut y0, &mut y1)
            }
            if x0 != x1 || y0 != y1 {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        if y0 != y1 {
                            self.row_nosplit_cells.push((x, y));
                        }
                        self.nosplit_ranges.insert((x, y), None);
                    }
                }
                self.nosplit_ranges.insert((x0, y0), Some((x0, y0, x1, y1)));
            }
        }
    }

    fn calc_span_rects(&mut self) -> Result<(), RlError> {
        let mut rects = HashMap::new();
        let mut vb: Blocks = Vec::new();
        let mut hb: Blocks = Vec::new();
        fn push(b: &mut Blocks, k: f64, v: (f64, f64)) {
            match b.iter_mut().find(|e| e.0 == k) {
                Some(e) => e.1.push(v),
                None => b.push((k, vec![v])),
            }
        }
        let cp = &self.colpositions;
        let rp = &self.rowpositions;
        let rlim = rp.len() as i64 - 1;
        let idx = |v: &Vec<f64>, i: i64| py_index(v, i);
        // dict order: x-major, then overwritten span entries keep their slot
        let (nc, nr) = (self.ncols as i64, self.nrows as i64);
        for x in 0..nc {
            for y in 0..nr {
                let coord = (x, y);
                match self.span_ranges.get(&coord).copied().flatten() {
                    None => {
                        rects.insert(coord, None);
                    }
                    Some((col0, row0, col1, row1)) => {
                        if row1 >= rlim {
                            continue;
                        }
                        if col1 - col0 > 0 {
                            for c in col0 + 1..=col1 {
                                push(&mut vb, idx(cp, c)?, (idx(rp, row1 + 1)?, idx(rp, row0)?));
                            }
                        }
                        if row1 - row0 > 0 {
                            for r in row0 + 1..=row1 {
                                push(&mut hb, idx(rp, r)?, (idx(cp, col0)?, idx(cp, col1 + 1)?));
                            }
                        }
                        let x0 = idx(cp, col0)?;
                        let y0 = idx(rp, row1 + 1)?;
                        rects.insert(coord, Some((x0, y0, idx(cp, col1 + 1)? - x0, idx(rp, row0)? - y0)));
                    }
                }
            }
        }
        for b in [&mut hb, &mut vb] {
            for e in b.iter_mut() {
                e.1.sort_by(|a, b| a.partial_cmp(b).unwrap());
            }
        }
        self.span_rects = rects;
        self.h_blocks = hb;
        self.v_blocks = vb;
        Ok(())
    }

    /// `_calc_width(availWidth, W)`.
    fn calc_width(&mut self, avail: f64, w_in: Option<&Vec<f64>>) -> Result<(), RlError> {
        if self.width_once {
            return Ok(());
        }
        let mut w: Vec<Option<f64>> = match w_in {
            Some(w) => w.iter().map(|x| Some(*x)).collect(),
            None => calc_pc(&self.arg_w, avail)?,
        };
        if w.iter().any(|x| x.is_none()) {
            let spans = !self.spans.is_empty();
            let mut cons: HashMap<(i64, i64), f64> = HashMap::new();
            let w0 = w.clone();
            while let Some(j) = w.iter().position(|x| x.is_none()) {
                let mut wm = 0.0;
                for i in 0..self.nrows {
                    let s = &self.styles[i][j];
                    let ji = (j as i64, i as i64);
                    let span = if spans { self.span_ranges.get(&ji).copied().flatten() } else { None };
                    let mut t;
                    if self.col_span_cells.contains(&ji) && span.is_none() {
                        t = 0.0;
                    } else {
                        t = Table::element_width(&self.cells[i][j], s)?.ok_or_else(|| {
                            RlError::Value(format!("Flowable in cell({i},{j}) can't have auto width\n{}", self.identity()))
                        })?;
                        t += s.left_padding + s.right_padding;
                        if let Some((c0, _, c1, _)) = span {
                            if c0 != c1 {
                                let e = cons.entry((c0, c1)).or_insert(t);
                                if t > *e {
                                    *e = t;
                                }
                                t = 0.0;
                            }
                        }
                    }
                    if t > wm {
                        wm = t;
                    }
                }
                w[j] = Some(wm);
            }
            if !cons.is_empty() {
                span_fix_dim(&w0, &mut w, &cons)?;
            }
        }
        let w: Vec<f64> = w.into_iter().map(|x| x.unwrap()).collect();
        let mut width = 0.0;
        self.colpositions = vec![0.0];
        for x in &w {
            width += x;
            self.colpositions.push(width);
        }
        self.col_widths = w.into_iter().map(ColWidth::Abs).collect();
        self.width = width;
        self.width_once = true;
        Ok(())
    }

    fn col_width_values(&self) -> Vec<f64> {
        self.col_widths.iter().map(|c| if let ColWidth::Abs(f) = c { *f } else { 0.0 }).collect()
    }

    /// `_calc_height(availHeight, availWidth, W)`.
    fn calc_height(&mut self, canv: &Canvas, avail_h: f64, avail_w: f64, w_in: Option<&Vec<f64>>) -> Result<(), RlError> {
        let w: Vec<Option<f64>> = match w_in {
            Some(w) => w.iter().map(|x| Some(*x)).collect(),
            None => calc_pc(&self.arg_w, avail_w)?,
        };
        let lim = self.arg_h.len();
        let mut hmax = lim;
        let mut h: Vec<Option<f64>> = self.arg_h.clone();
        if h.iter().any(|x| x.is_none()) {
            let spans = !self.spans.is_empty();
            let h0 = h.clone();
            let mut cons: HashMap<(i64, i64), f64> = HashMap::new();
            while let Some(i) = h.iter().position(|x| x.is_none()) {
                let mut hm = 0.0;
                for j in 0..self.ncols {
                    let ji = (j as i64, i as i64);
                    let span = if spans { self.span_ranges.get(&ji).copied().flatten() } else { None };
                    if spans && self.row_span_cells.contains(&ji) && span.is_none() {
                        continue;
                    }
                    let s = self.styles[i][j].clone();
                    let mut t;
                    match &self.cells[i][j] {
                        Val::Flows(fs) => {
                            let fs = fs.clone();
                            let mut wj = w[j];
                            if wj.is_none() && !Table::can_get_width(&self.cells[i][j]) {
                                return Err(RlError::Value(format!(
                                    "Flowable in cell({i},{j}) can't have auto width\n{}",
                                    self.identity()
                                )));
                            }
                            if self.col_span_cells.contains(&ji) {
                                let Some(sp) = span else { continue };
                                let a = py_index(&self.colpositions, sp.2 + 1)? - py_index(&self.colpositions, sp.0)?;
                                let b = wj.unwrap_or(0.0);
                                wj = Some(if b > a { b } else { a });
                            }
                            let ww = match wj {
                                Some(x) if x != 0.0 => x,
                                _ => Table::list_value_width(canv, &fs)?,
                            };
                            t = self.list_cell_geom(canv, &fs, ww, &s, None, None, 72000.0)?.1;
                        }
                        Val::Text(txt) => {
                            let lead = if s.leading != 0.0 { s.leading } else { 1.2 * s.fontsize };
                            t = lead * txt.split('\n').count() as f64;
                        }
                    }
                    t += s.bottom_padding + s.top_padding;
                    if let Some((_, r0, _, r1)) = span {
                        if r0 != r1 {
                            let e = cons.entry((r0, r1)).or_insert(t);
                            if t > *e {
                                *e = t;
                            }
                            t = 0.0;
                        }
                    }
                    if t > hm {
                        hm = t;
                    }
                }
                h[i] = Some(hm);
                // longTableOptimize
                hmax = i + 1;
                let height = py_sum(h[..hmax].iter().map(|x| x.unwrap()));
                if height > avail_h && !cons.is_empty() {
                    let msr = cons.keys().map(|k| k.1).max().unwrap();
                    if hmax as i64 > msr {
                        break;
                    }
                }
            }
            if !h.iter().any(|x| x.is_none()) {
                hmax = lim;
            }
            if !cons.is_empty() {
                span_fix_dim(&h0, &mut h, &cons)?;
            }
            self.row_heights = h.clone();
        } else {
            self.row_heights = h.clone();
        }
        let mut pos = Vec::with_capacity(hmax + 1);
        let (mut height, mut c) = (0.0f64, 0.0f64);
        for i in (0..hmax).rev() {
            pos.push(height);
            let y = h[i].unwrap() - c;
            let t = height + y;
            c = (t - height) - y;
            height = t;
        }
        pos.push(height);
        pos.reverse();
        self.rowpositions = pos;
        self.height = height;
        Ok(())
    }

    /// `_calc(availWidth, availHeight)`.
    fn calc(&mut self, canv: &Canvas, aw: f64, ah: f64) -> Result<(), RlError> {
        let w = if self.col_widths.iter().any(is_auto_or_star) && self.has_vari_width_elements() {
            self.calc_preliminary_widths(aw)?
        } else {
            None
        };
        if !self.spans.is_empty() {
            self.calc_span_ranges();
            if self.arg_h.iter().any(|x| x.is_none()) {
                self.calc_width(aw, w.as_ref())?;
            }
        }
        if !self.nosplits.is_empty() {
            self.calc_nosplit_ranges();
        }
        self.calc_height(canv, ah, aw, w.as_ref())?;
        self.calc_width(aw, w.as_ref())?;
        if !self.spans.is_empty() {
            self.calc_span_rects()?;
        }
        Ok(())
    }

    // ---- splitting ----

    fn first_possible_split(&self, avail_h: f64) -> Result<usize, RlError> {
        let mut impossible: HashSet<i64> = HashSet::new();
        let mut mark = |cells: &Vec<(i64, i64)>, ranges: &HashMap<(i64, i64), Option<Span>>| {
            for xy in cells {
                if let Some(Some(r)) = ranges.get(xy) {
                    let (y1, y2) = (r.1, r.3);
                    if y1 != y2 {
                        for y in y1.min(y2) + 1..=y1.max(y2) {
                            impossible.insert(y);
                        }
                    }
                }
            }
        };
        if !self.spans.is_empty() {
            mark(&self.row_span_cells, &self.span_ranges);
        }
        if !self.nosplits.is_empty() {
            mark(&self.row_nosplit_cells, &self.nosplit_ranges);
        }
        let (mut h, mut n, mut split_at) = (0.0, 1i64, 0usize);
        for rh in &self.row_heights {
            let rh = rh.ok_or_else(|| RlError::Type("unsupported operand type(s) for +: 'float' and 'NoneType'".into()))?;
            if h + rh > avail_h {
                break;
            }
            if !impossible.contains(&n) {
                split_at = n as usize;
            }
            h += rh;
            n += 1;
        }
        Ok(split_at)
    }

    fn split_line_cmds(&self, n: i64) -> Vec<LineCmd> {
        let (nr, nc) = (self.nrows as i64, self.ncols as i64);
        let mut a = Vec::new();
        for l in &self.lines {
            let (mut sc, mut sr, mut ec, mut er) = l.rng();
            if sc < 0 {
                sc += nc
            }
            if ec < 0 {
                ec += nc
            }
            if sr < 0 {
                sr += nr
            }
            if er < 0 {
                er += nr
            }
            let mk = |op: &'static str, sc, sr, ec, er| LineCmd { op, sc, sr, ec, er, ..l.clone() };
            match l.op {
                "BOX" | "OUTLINE" | "GRID" => {
                    if sr < n && er >= n {
                        a.push(mk("LINEABOVE", sc, sr, ec, sr));
                        a.push(mk("LINEBEFORE", sc, sr, sc, er));
                        a.push(mk("LINEAFTER", ec, sr, ec, er));
                        a.push(mk("LINEBELOW", sc, er, ec, er));
                        if l.op == "GRID" {
                            a.push(mk("LINEBELOW", sc, n - 1, ec, n - 1));
                            a.push(mk("LINEABOVE", sc, n, ec, n));
                            a.push(mk("INNERGRID", sc, sr, ec, er));
                        }
                    } else {
                        a.push(mk(l.op, sc, sr, ec, er));
                    }
                }
                "INNERGRID" => {
                    if sr < n && er >= n {
                        a.push(mk("LINEBELOW", sc, n - 1, ec, n - 1));
                        a.push(mk("LINEABOVE", sc, n, ec, n));
                    }
                    a.push(mk(l.op, sc, sr, ec, er));
                }
                "LINEBELOW" => {
                    if sr < n && er >= n - 1 {
                        a.push(mk("LINEABOVE", sc, n, ec, n));
                    }
                    a.push(mk(l.op, sc, sr, ec, er));
                }
                "LINEABOVE" => {
                    if sr <= n && er >= n {
                        a.push(mk("LINEBELOW", sc, n - 1, ec, n - 1));
                    }
                    a.push(mk(l.op, sc, sr, ec, er));
                }
                _ => a.push(mk(l.op, sc, sr, ec, er)),
            }
        }
        a
    }

    fn split_rows(&mut self, avail_h: f64) -> Result<Vec<Box<dyn Flowable>>, RlError> {
        let n = self.first_possible_split(avail_h)?;
        let maxrepeat = match &self.repeat_rows {
            RepeatRows::Count(k) => *k,
            RepeatRows::Rows(v) => {
                v.iter().max().ok_or_else(|| RlError::Value("max() iterable argument is empty".into()))? + 1
            }
        };
        if (n as i64) <= maxrepeat {
            return Ok(Vec::new());
        }
        let lim = self.row_heights.len();
        if n == lim {
            return Ok(vec![Box::new(self.clone())]);
        }
        let nrows = self.nrows as i64;
        let split_h = self.row_heights.clone();
        let cw: Vec<ColWidth> = self.col_width_values().into_iter().map(ColWidth::Abs).collect();
        let mut r0 = Table::raw(
            self.cells[..n].to_vec(),
            cw.clone(),
            split_h[..n].to_vec(),
            self.styles[..n].to_vec(),
            self.repeat_rows.clone(),
            self.h_align.clone(),
            self.v_align.clone(),
            self.space_before,
            None,
        );
        let ni = n as i64;
        let linecmds = self.split_line_cmds(ni);
        r0.lines.extend(cr_0(ni, &linecmds, nrows));
        r0.bkgrnd.extend(cr_0(ni, &self.bkgrnd, nrows));
        r0.spans.extend(cr_0(ni, &self.spans, nrows));
        r0.nosplits.extend(cr_0(ni, &self.nosplits, nrows));
        let repeat: Vec<i64> = match &self.repeat_rows {
            RepeatRows::Count(k) => (0..*k).collect(),
            RepeatRows::Rows(v) => {
                let mut v = v.clone();
                v.sort();
                v
            }
        };
        let r1 = if !repeat.is_empty() {
            let pick = |i: i64| -> Result<usize, RlError> {
                let j = if i < 0 { i + nrows } else { i };
                if j < 0 || j >= nrows {
                    return Err(RlError::Index("list index out of range".into()));
                }
                Ok(j as usize)
            };
            let mut cells = Vec::new();
            let mut hs = Vec::new();
            let mut sts = Vec::new();
            for &i in &repeat {
                let j = pick(i)?;
                cells.push(self.cells[j].clone());
                hs.push(split_h[j]);
                sts.push(self.styles[j].clone());
            }
            cells.extend_from_slice(&self.cells[n..]);
            hs.extend_from_slice(&split_h[n..]);
            sts.extend_from_slice(&self.styles[n..]);
            let mut r1 = Table::raw(
                cells,
                cw,
                hs,
                sts,
                RepeatRows::Count(repeat.len() as i64),
                self.h_align.clone(),
                self.v_align.clone(),
                None,
                self.space_after,
            );
            r1.lines.extend(cr_1_1(ni, nrows, &repeat, &linecmds));
            r1.bkgrnd.extend(cr_1_1(ni, nrows, &repeat, &self.bkgrnd));
            r1.spans.extend(cr_1_1(ni, nrows, &repeat, &self.spans));
            r1.nosplits.extend(cr_1_1(ni, nrows, &repeat, &self.nosplits));
            r1
        } else {
            let mut r1 = Table::raw(
                self.cells[n..].to_vec(),
                cw,
                split_h[n..].to_vec(),
                self.styles[n..].to_vec(),
                self.repeat_rows.clone(),
                self.h_align.clone(),
                self.v_align.clone(),
                None,
                self.space_after,
            );
            r1.lines.extend(cr_1_0(ni, &linecmds));
            r1.bkgrnd.extend(cr_1_0(ni, &self.bkgrnd));
            r1.spans.extend(cr_1_0(ni, &self.spans));
            r1.nosplits.extend(cr_1_0(ni, &self.nosplits));
            r1
        };
        Ok(vec![Box::new(r0), Box::new(r1)])
    }

    // ---- drawing ----

    fn draw_bkgrnd(&self, canv: &mut Canvas) -> Result<(), RlError> {
        let (nr, nc) = (self.nrows as i64, self.ncols as i64);
        let cp = &self.colpositions;
        let rp = &self.rowpositions;
        let rh: Vec<f64> = self.row_heights.iter().map(|x| x.unwrap_or(f64::NAN)).collect();
        let cw = self.col_width_values();
        for c in &self.bkgrnd {
            if c.args.len() != 1 {
                return Err(RlError::Value(if c.args.len() > 1 {
                    format!("too many values to unpack (expected 4, got {})", c.args.len() + 3)
                } else {
                    "not enough values to unpack (expected 4, got 3)".into()
                }));
            }
            let arg = &c.args[0];
            let (mut sc, mut sr, mut ec, mut er) = c.rng();
            if sc < 0 {
                sc += nc
            }
            if ec < 0 {
                ec += nc
            }
            if sr < 0 {
                sr += nr
            }
            if er < 0 {
                er += nr
            }
            let mut x0 = py_index(cp, sc)?;
            let mut y0 = py_index(rp, sr)?;
            let x1 = py_index(cp, (ec + 1).min(nc))?;
            let y1 = py_index(rp, (er + 1).min(nr))?;
            let (mut w, mut h) = (x1 - x0, y1 - y0);
            match c.op.as_str() {
                "ROWBACKGROUNDS" | "COLBACKGROUNDS" => {
                    let CmdVal::List(l) = arg else {
                        return Err(RlError::Unsupported(format!("{} argument {arg:?}", c.op)));
                    };
                    let cycle: Vec<Option<Color>> = l.iter().map(to_color_or_none).collect::<Result<_, _>>()?;
                    if cycle.is_empty() {
                        return Err(RlError::Value("integer modulo by zero".into()));
                    }
                    if c.op == "ROWBACKGROUNDS" {
                        for i in 0..(er - sr + 1).max(0) {
                            let col = &cycle[i as usize % cycle.len()];
                            let h = py_index(&rh, sr + i)?;
                            if let Some(col) = col {
                                canv.set_fill_color(col, None)?;
                                canv.rect(x0, y0, w, -h, false, true);
                            }
                            y0 -= h;
                        }
                    } else {
                        for i in 0..(ec - sc + 1).max(0) {
                            let col = &cycle[i as usize % cycle.len()];
                            let w = py_index(&cw, sc + i)?;
                            if let Some(col) = col {
                                canv.set_fill_color(col, None)?;
                                canv.rect(x0, y0, w, h, false, true);
                            }
                            x0 += w;
                        }
                    }
                }
                _ => {
                    if let CmdVal::List(l) = arg {
                        if let Some(CmdVal::Str(s)) = l.first() {
                            if ["VERTICAL", "HORIZONTAL", "VERTICAL2", "HORIZONTAL2", "LINEARGRADIENT", "RADIALGRADIENT"]
                                .contains(&s.as_str())
                            {
                                return Err(RlError::Unsupported("gradient table background".into()));
                            }
                        }
                    }
                    if let Some(col) = to_color_or_none(arg)? {
                        if ec == sc && er == sr && !self.span_rects.is_empty() {
                            if let Some(Some(r)) = self.span_rects.get(&(sc, sr)) {
                                (x0, y0, w, h) = *r;
                            }
                        }
                        canv.set_fill_color(&col, None)?;
                        canv.rect(x0, y0, w, h, false, true);
                    }
                }
            }
        }
        Ok(())
    }

    fn draw_cell(
        &self,
        canv: &mut Canvas,
        cur: &mut Option<CellStyle>,
        val: &Val,
        st: &CellStyle,
        pos: (f64, f64),
        size: (f64, f64),
    ) -> Result<(), RlError> {
        let (colpos, rowpos) = pos;
        let (colwidth, rowheight) = size;
        let set_color = cur.as_ref().is_none_or(|c| st.color != c.color);
        let set_font = cur
            .as_ref()
            .is_none_or(|c| st.leading != c.leading || st.fontname != c.fontname || st.fontsize != c.fontsize);
        if set_color {
            let c = match &st.color {
                CellColor::Black => Color::named("black").unwrap(),
                CellColor::C(c) => c.clone(),
            };
            canv.set_fill_color(&c, None)?;
        }
        if set_font {
            canv.set_font(&st.fontname, st.fontsize, Some(st.leading))?;
        }
        *cur = Some(st.clone());
        let just = st.alignment.as_str();
        let valign = st.valign.as_str();
        match val {
            Val::Flows(fs) => {
                let mut ws = Vec::new();
                let mut hs = Vec::new();
                let (_w, h) = self.list_cell_geom(canv, fs, colwidth, st, Some(&mut ws), Some(&mut hs), rowheight)?;
                let mut y = match valign {
                    "TOP" => rowpos + rowheight - st.top_padding,
                    "BOTTOM" => rowpos + st.bottom_padding + h,
                    _ => rowpos + (rowheight + st.bottom_padding - st.top_padding + h) / 2.0,
                };
                if let Some(f) = fs.first() {
                    y += f.borrow().space_before();
                }
                for ((v, w), h) in fs.iter().zip(ws).zip(hs) {
                    let mut v = v.borrow_mut();
                    let x = match just {
                        "LEFT" => colpos + st.left_padding,
                        "RIGHT" => colpos + colwidth - st.right_padding - w,
                        "CENTRE" | "CENTER" => colpos + (colwidth + st.left_padding - st.right_padding - w) / 2.0,
                        _ => {
                            return Err(RlError::Value(format!(
                                "Invalid justification {} for {}",
                                py_repr(just, true),
                                py_type(&**v)
                            )))
                        }
                    };
                    y -= v.space_before();
                    y -= h;
                    v.draw_on(canv, x, y, 0.0)?;
                    y -= v.space_after();
                }
            }
            Val::Text(t) => {
                #[derive(PartialEq)]
                enum D {
                    L,
                    C,
                    R,
                    Dec,
                }
                let (draw, x) = match just {
                    "LEFT" => (D::L, colpos + st.left_padding),
                    "CENTRE" | "CENTER" => (D::C, colpos + (colwidth + st.left_padding - st.right_padding) * 0.5),
                    "RIGHT" => (D::R, colpos + colwidth - st.right_padding),
                    "DECIMAL" => (D::Dec, colpos + colwidth - st.right_padding),
                    _ => return Err(RlError::Value(format!("Invalid justification {}", py_repr(just, true)))),
                };
                let vals: Vec<&str> = t.split('\n').collect();
                let n = vals.len() as f64;
                let (leading, fontsize) = (st.leading, st.fontsize);
                let mut y = match valign {
                    "BOTTOM" => rowpos + st.bottom_padding + n * leading - fontsize,
                    "TOP" => rowpos + rowheight - st.top_padding - fontsize,
                    "MIDDLE" => rowpos + (st.bottom_padding + rowheight - st.top_padding + n * leading) / 2.0 - fontsize,
                    _ => return Err(RlError::Value(format!("Bad valign: {}", py_repr(valign, true)))),
                };
                for v in vals {
                    match draw {
                        D::L => canv.draw_string(x, y, v),
                        D::C => canv.draw_centred_string(x, y, v),
                        D::R => canv.draw_right_string(x, y, v),
                        D::Dec => draw_aligned_string(canv, x, y, v)?,
                    }
                    y -= leading;
                }
            }
        }
        Ok(())
    }

    fn prep_line(&mut self, canv: &mut Canvas, weight: f64, color: &Option<Color>) -> Result<(), RlError> {
        if let Some(c) = color {
            if self.cur_color.as_ref() != Some(c) {
                canv.set_stroke_color(c, None)?;
                self.cur_color = Some(c.clone());
            }
        }
        if weight != 0.0 && self.cur_weight != Some(weight) {
            canv.set_line_width(weight);
            self.cur_weight = Some(weight);
        }
        Ok(())
    }

    fn draw_h_lines(&mut self, canv: &mut Canvas, l: &LineCmd, (sc, sr): (i64, i64), (ec, er): (i64, i64)) -> Result<(), RlError> {
        let ecp = py_slice(&self.colpositions, sc, ec + 2);
        let rp = py_slice(&self.rowpositions, sr, er + 1);
        if ecp.len() <= 1 || rp.is_empty() {
            return Ok(());
        }
        self.prep_line(canv, l.weight, &l.color)?;
        let (scp, ecp) = (ecp[0], ecp[ecp.len() - 1]);
        let blocks = self.h_blocks.clone();
        let mut line = |x1: f64, y1: f64, x2: f64, y2: f64| canv.line(x1, y1, x2, y2);
        for y in rp {
            if l.count == 1 {
                h_line(&mut line, scp, ecp, y, &blocks);
            } else {
                let ws = l.weight + l.space;
                let mut lf = |x0: f64, y0: f64, x1: f64, _y1: f64| multi_line(&mut line, x0, x1, y0, ws, l.count);
                h_line(&mut lf, scp, ecp, y, &blocks);
            }
        }
        Ok(())
    }

    fn draw_v_lines(&mut self, canv: &mut Canvas, l: &LineCmd, (sc, sr): (i64, i64), (ec, er): (i64, i64)) -> Result<(), RlError> {
        let erp = py_slice(&self.rowpositions, sr, er + 2);
        let cp = py_slice(&self.colpositions, sc, ec + 1);
        if erp.len() <= 1 || cp.is_empty() {
            return Ok(());
        }
        self.prep_line(canv, l.weight, &l.color)?;
        let (srp, erp) = (erp[0], erp[erp.len() - 1]);
        let blocks = self.v_blocks.clone();
        let mut line = |y0: f64, x0: f64, y1: f64, x1: f64| canv.line(x0, y0, x1, y1);
        for x in cp {
            if l.count == 1 {
                h_line(&mut line, erp, srp, x, &blocks);
            } else {
                let ws = l.weight + l.space;
                let mut lf = |x0: f64, y0: f64, x1: f64, _y1: f64| multi_line(&mut line, x0, x1, y0, ws, l.count);
                h_line(&mut lf, erp, srp, x, &blocks);
            }
        }
        Ok(())
    }

    fn draw_box(&mut self, canv: &mut Canvas, l: &LineCmd, (sc, sr): (i64, i64), (ec, er): (i64, i64)) -> Result<(), RlError> {
        self.draw_h_lines(canv, l, (sc, sr), (ec, sr))?;
        self.draw_h_lines(canv, l, (sc, er + 1), (ec, er + 1))?;
        self.draw_v_lines(canv, l, (sc, sr), (sc, er))?;
        self.draw_v_lines(canv, l, (ec + 1, sr), (ec + 1, er))
    }

    fn draw_inner_grid(&mut self, canv: &mut Canvas, l: &LineCmd, (sc, sr): (i64, i64), (ec, er): (i64, i64)) -> Result<(), RlError> {
        self.draw_h_lines(canv, l, (sc, sr + 1), (ec, er))?;
        self.draw_v_lines(canv, l, (sc + 1, sr), (ec, er))
    }

    fn draw_lines(&mut self, canv: &mut Canvas) -> Result<(), RlError> {
        let (mut ccap, mut cdash, mut cjoin): (Option<i64>, Option<DashV>, Option<i64>) = (None, None, None);
        canv.save_state();
        for l in self.lines.clone() {
            if let Some(cap) = l.cap {
                if ccap != Some(cap) {
                    canv.set_line_cap(cap as i32)?;
                    ccap = Some(cap);
                }
            }
            match &l.dash {
                None => {
                    if cdash.is_some() {
                        canv.set_dash(&[], "0");
                        cdash = None;
                    }
                }
                Some(DashV::List(v)) if v.is_empty() => {
                    if cdash.is_some() {
                        canv.set_dash(&[], "0");
                        cdash = None;
                    }
                }
                Some(d) => {
                    if cdash.as_ref() != Some(d) {
                        match d {
                            DashV::Num(x) => set_dash_checked(canv, &[*x, 0.0], "0")?,
                            DashV::List(v) => set_dash_checked(canv, v, "0")?,
                        }
                        cdash = Some(d.clone());
                    }
                }
            }
            if let Some(j) = l.join {
                if cjoin != Some(j) {
                    canv.set_line_join(j as i32)?;
                    cjoin = Some(j);
                }
            }
            let (sc, ec, sr, er) = self.norm_cell_range(l.sc, l.ec, l.sr, l.er);
            let (s, e) = ((sc, sr), (ec, er));
            match l.op {
                "GRID" => {
                    self.draw_box(canv, &l, s, e)?;
                    self.draw_inner_grid(canv, &l, s, e)?;
                }
                "BOX" | "OUTLINE" => self.draw_box(canv, &l, s, e)?,
                "INNERGRID" => self.draw_inner_grid(canv, &l, s, e)?,
                "LINEBELOW" => self.draw_h_lines(canv, &l, (sc, sr + 1), (ec, er + 1))?,
                "LINEABOVE" => self.draw_h_lines(canv, &l, s, e)?,
                "LINEBEFORE" => self.draw_v_lines(canv, &l, s, e)?,
                _ => self.draw_v_lines(canv, &l, (sc + 1, sr), (ec + 1, er))?,
            }
        }
        canv.restore_state()?;
        self.cur_color = None;
        Ok(())
    }
}

trait PctFilter {
    fn filter_pct(self) -> ColWidth;
}
impl PctFilter for ColWidth {
    /// The column width itself when it is a percentage, else Auto.
    fn filter_pct(self) -> ColWidth {
        if ends_pct(&self) {
            self
        } else {
            ColWidth::Auto
        }
    }
}

/// `canvas.drawAlignedString(x, y, text)` with pivot '.'.
fn draw_aligned_string(canv: &mut Canvas, x: f64, y: f64, text: &str) -> Result<(), RlError> {
    let piv_w = canv.string_width(".", None, None)?;
    let parts: Vec<&str> = text.splitn(2, '.').collect();
    let digit = |c: char| c.is_ascii_digit();
    let x = x - 0.5 * piv_w;
    if parts.len() == 1 && text.chars().any(|c| c.is_ascii_digit() || c.is_numeric()) && !text.ends_with(digit) {
        let chars: Vec<char> = text.chars().collect();
        let mut left: Vec<char> = chars[..chars.len() - 1].to_vec();
        let mut right: Vec<char> = vec![chars[chars.len() - 1]];
        loop {
            let Some(&last) = left.last() else {
                return Err(RlError::Index("string index out of range".into()));
            };
            if digit(last) {
                break;
            }
            right.insert(0, last);
            left.pop();
        }
        let (l, r): (String, String) = (left.into_iter().collect(), right.into_iter().collect());
        canv.draw_right_string(x, y, &l);
        canv.draw_string(x, y, &r);
    } else {
        canv.draw_right_string(x, y, parts[0]);
        if parts.len() > 1 {
            canv.draw_string(x, y, &format!(".{}", parts[1]));
        }
    }
    Ok(())
}

/// `_cr_0(n, cmds, nr0)`: the commands of the first part of a split.
fn cr_0<T: Ranged>(n: i64, cmds: &[T], nr0: i64) -> Vec<T> {
    let mut out = Vec::new();
    for c in cmds {
        let (sc, mut sr, ec, mut er) = c.rng();
        if sr < 0 {
            sr += nr0;
        }
        if sr >= n {
            continue;
        }
        if er >= n {
            er = n - 1;
        }
        out.push(c.with(sc, sr, ec, er));
    }
    out
}

/// `_cr_1_1(n, nRows, repeatRows, cmds)`: the second part, with repeated rows.
fn cr_1_1<T: Ranged>(n: i64, nrows: i64, repeat: &[i64], cmds: &[T]) -> Vec<T> {
    let nrr = repeat.len() as i64;
    let mut out = Vec::new();
    for c in cmds {
        let (sc, mut sr, ec, mut er) = c.rng();
        if sr < 0 {
            sr += nrows;
        }
        if er < 0 {
            er += nrows;
        }
        let cs: Vec<i64> = repeat.iter().copied().filter(|r| *r >= sr && *r <= er).collect();
        if !cs.is_empty() {
            let (mn, mx) = (*cs.iter().min().unwrap(), *cs.iter().max().unwrap());
            let i0 = repeat.iter().position(|r| *r == mn).unwrap() as i64;
            let i1 = repeat.iter().position(|r| *r == mx).unwrap() as i64;
            out.push(c.with(sc, i0, ec, i1));
        }
        if er < n {
            continue;
        }
        let sr = (sr - n).max(0) + nrr;
        let er = (er - n).max(0) + nrr;
        out.push(c.with(sc, sr, ec, er));
    }
    out
}

/// `_cr_1_0(n, cmds)`: the second part, no repeated rows.
fn cr_1_0<T: Ranged>(n: i64, cmds: &[T]) -> Vec<T> {
    let mut out = Vec::new();
    for c in cmds {
        let (sc, mut sr, ec, mut er) = c.rng();
        if er >= 0 && er < n {
            continue;
        }
        if sr >= 0 && sr < n {
            sr = 0;
        }
        if sr >= n {
            sr -= n;
        }
        if er >= n {
            er -= n;
        }
        out.push(c.with(sc, sr, ec, er));
    }
    out
}

impl Default for Table {
    fn default() -> Self {
        Table::raw(Vec::new(), Vec::new(), Vec::new(), Vec::new(), RepeatRows::Count(0), "CENTER".into(), "MIDDLE".into(), None, None)
    }
}

impl Flowable for Table {
    fn wrap(&mut self, canv: &Canvas, avail_width: f64, avail_height: f64) -> Result<(f64, f64), RlError> {
        self.calc(canv, avail_width, avail_height)?;
        Ok((self.width, self.height))
    }

    fn split(&mut self, canv: &Canvas, avail_width: f64, avail_height: f64) -> Result<Vec<Box<dyn Flowable>>, RlError> {
        self.calc(canv, avail_width, avail_height)?;
        self.split_rows(avail_height)
    }

    fn draw(&mut self, canv: &mut Canvas) -> Result<(), RlError> {
        canv.save_state();
        self.cur_weight = None;
        self.cur_color = None;
        self.draw_bkgrnd(canv)?;
        let mut cur: Option<CellStyle> = None;
        let rh: Vec<f64> = self.row_heights.iter().map(|x| x.unwrap_or(f64::NAN)).collect();
        let cw = self.col_width_values();
        if self.spans.is_empty() {
            let nrows = self.nrows.min(self.rowpositions.len().saturating_sub(1));
            for i in 0..nrows {
                let rowpos = self.rowpositions[i + 1];
                for j in 0..self.ncols {
                    let (v, s) = (self.cells[i][j].clone(), self.styles[i][j].clone());
                    self.draw_cell(canv, &mut cur, &v, &s, (self.colpositions[j], rowpos), (cw[j], rh[i]))?;
                }
            }
        } else {
            for i in 0..self.nrows {
                for j in 0..self.ncols {
                    let r = *self
                        .span_rects
                        .get(&(j as i64, i as i64))
                        .ok_or_else(|| RlError::Key(format!("({j}, {i})")))?;
                    if let Some((x, y, w, h)) = r {
                        let (v, s) = (self.cells[i][j].clone(), self.styles[i][j].clone());
                        self.draw_cell(canv, &mut cur, &v, &s, (x, y), (w, h))?;
                    }
                }
            }
        }
        self.draw_lines(canv)?;
        canv.restore_state()
    }

    fn draw_on(&mut self, canv: &mut Canvas, x: f64, y: f64, sw: f64) -> Result<(), RlError> {
        let x = if sw != 0.0 {
            match self.h_align.as_str() {
                "CENTER" | "CENTRE" => x + 0.5 * sw,
                "RIGHT" => x + sw,
                "LEFT" => x,
                a => return Err(RlError::Value(format!("Bad hAlign value {a}"))),
            }
        } else {
            x
        };
        canv.save_state();
        canv.translate(x, y);
        self.draw(canv)?;
        canv.restore_state()
    }

    fn space_before(&self) -> f64 {
        self.space_before.unwrap_or(0.0)
    }
    fn space_after(&self) -> f64 {
        self.space_after.unwrap_or(0.0)
    }
    fn min_width(&self) -> Result<f64, RlError> {
        self.table_min_width()
    }
    fn size(&self) -> Option<(f64, f64)> {
        Some((self.width, self.height))
    }
    fn class_name(&self) -> &'static str {
        "Table"
    }
}
