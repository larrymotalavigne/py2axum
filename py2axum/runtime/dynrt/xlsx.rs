//! openpyxl 3.1 without lxml (et_xmlfile 2.0): `Workbook()`, its worksheets, cells, styles (`Font`,
//! `PatternFill`, `Border`/`Side`, `Alignment`), one merged range per sheet, column widths, and `save()` into
//! an `io.BytesIO`, with the bytes openpyxl writes (inline strings, ElementTree's escapes, the parts in its
//! order, the zip of CPython's zipfile at zlib's default level). Styles are kept in the workbook's indexed
//! lists, deduplicated by their raw values like openpyxl's `IndexedList` (hash of the descriptors' values).
//! What is outside this surface raises a `py2axum:` TypeError (docs/advanced/documents.md).
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::v::*;

const THEME: &str = include_str!("openpyxl_theme1.xml");
const DEFAULT_COLUMN_WIDTH: f64 = 13.0;

fn unsupported(what: impl AsRef<str>) -> Exc {
    Exc::type_error(format!("py2axum: openpyxl {} is not supported", what.as_ref()))
}

// ---------------------------------------------------------------- styles

#[derive(Clone, PartialEq, Eq, Hash, Default, Debug)]
pub struct Font {
    name: Option<String>,
    b: Option<bool>,
    i: Option<bool>,
    strike: Option<bool>,
    color: Option<String>,
    sz: Option<u64>,
    // the workbook's default font (Calibri 11, family 2, theme colour 1, scheme minor)
    default: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Fill {
    pattern: Option<String>,
    fg: String,
    bg: String,
}

#[derive(Clone, PartialEq, Eq, Hash, Default, Debug)]
pub struct Side {
    style: Option<String>,
    color: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Hash, Default, Debug)]
pub struct Border {
    left: Option<Side>,
    right: Option<Side>,
    top: Option<Side>,
    bottom: Option<Side>,
    diagonal: Option<Side>,
}

#[derive(Clone, PartialEq, Eq, Hash, Default, Debug)]
pub struct Align {
    horizontal: Option<String>,
    vertical: Option<String>,
    wrap: Option<bool>,
    shrink: Option<bool>,
}

const NO_COLOR: &str = "00000000";

impl Side {
    // Serialisable.__add__: attributes `a or b`, elements `a + b` when both are set
    fn add(&self, o: &Side) -> Side {
        Side { style: self.style.clone().or_else(|| o.style.clone()), color: self.color.clone().or_else(|| o.color.clone()) }
    }
}

fn add_side(a: &Option<Side>, b: &Option<Side>) -> Option<Side> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.add(y)),
        (x, y) => x.clone().or_else(|| y.clone()),
    }
}

impl Border {
    fn add(&self, o: &Border) -> Border {
        Border {
            left: add_side(&self.left, &o.left),
            right: add_side(&self.right, &o.right),
            top: add_side(&self.top, &o.top),
            bottom: add_side(&self.bottom, &o.bottom),
            diagonal: add_side(&self.diagonal, &o.diagonal),
        }
    }
    fn side(&self, name: &str) -> &Option<Side> {
        match name {
            "left" => &self.left,
            "right" => &self.right,
            "top" => &self.top,
            _ => &self.bottom,
        }
    }
}

pub enum Style {
    Font(Font),
    Fill(Fill),
    Side(Side),
    Border(Border),
    Align(Align),
}

// ---------------------------------------------------------------- objects

#[derive(Default)]
struct Lists {
    fonts: IndexedList<Font>,
    fills: IndexedList<Fill>,
    borders: IndexedList<Border>,
    aligns: IndexedList<Align>,
    // StyleArray: fontId, fillId, borderId, numFmtId, protectionId, alignmentId, pivotButton, quotePrefix, xfId
    xfs: IndexedList<[u32; 9]>,
}

/// openpyxl's IndexedList: the distinct values in insertion order, found by a dict (a linear search made a book
/// with one style per row quadratic, its lock held)
struct IndexedList<T> {
    items: Vec<T>,
    pos: std::collections::HashMap<T, u32>,
}

impl<T> Default for IndexedList<T> {
    fn default() -> Self {
        IndexedList { items: Vec::new(), pos: std::collections::HashMap::new() }
    }
}

impl<T> std::ops::Deref for IndexedList<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Vec<T> {
        &self.items
    }
}

impl<'a, T> IntoIterator for &'a IndexedList<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<T: Eq + std::hash::Hash + Clone> From<Vec<T>> for IndexedList<T> {
    fn from(items: Vec<T>) -> Self {
        let mut list = IndexedList::default();
        for v in &items {
            index_of(&mut list, v);
        }
        list
    }
}

fn index_of<T: Eq + std::hash::Hash + Clone>(list: &mut IndexedList<T>, v: &T) -> u32 {
    if let Some(i) = list.pos.get(v) {
        return *i;
    }
    let i = list.items.len() as u32;
    list.items.push(v.clone());
    list.pos.insert(v.clone(), i);
    i
}

pub struct Book {
    sheets: Mutex<Vec<Arc<Sheet>>>,
    lists: Mutex<Lists>,
    created: chrono::DateTime<chrono::Utc>,
}

pub struct Sheet {
    book: Weak<Book>,
    st: Mutex<SheetSt>,
}

struct ColDim {
    index: String,
    width: f64,
}

struct SheetSt {
    title: String,
    cells: BTreeMap<(u32, u32), Arc<Cell>>,
    merged: Vec<(u32, u32, u32, u32)>,
    cols: Vec<ColDim>,
    current_row: u32,
}

pub struct Cell {
    sheet: Weak<Sheet>,
    row: u32,
    col: u32,
    merged: bool,
    st: Mutex<CellSt>,
}

struct CellSt {
    value: V,
    dtype: char,
    style: Option<[u32; 9]>,
}

pub enum Xl {
    Book(Arc<Book>),
    Sheet(Arc<Sheet>),
    Cell(Arc<Cell>),
    Style(Style),
    Dims(Arc<Sheet>),
    Dim(Arc<Sheet>, String),
}

fn wrap(x: Xl) -> V {
    V::native(Native::Xl(Arc::new(x)))
}

pub fn type_name(x: &Xl) -> &'static str {
    match x {
        Xl::Book(_) => "Workbook",
        Xl::Sheet(_) => "Worksheet",
        Xl::Cell(c) if c.merged => "MergedCell",
        Xl::Cell(_) => "Cell",
        Xl::Style(Style::Font(_)) => "Font",
        Xl::Style(Style::Fill(_)) => "PatternFill",
        Xl::Style(Style::Side(_)) => "Side",
        Xl::Style(Style::Border(_)) => "Border",
        Xl::Style(Style::Align(_)) => "Alignment",
        Xl::Dims(_) => "DimensionHolder",
        Xl::Dim(..) => "ColumnDimension",
    }
}

fn book_of(s: &Sheet) -> R<Arc<Book>> {
    s.book.upgrade().ok_or_else(|| Exc::runtime("py2axum: the worksheet's workbook is gone"))
}

/// `openpyxl.Workbook()`
pub fn workbook(args: &[V], kwargs: &[(String, V)]) -> R {
    if !args.is_empty() || !kwargs.is_empty() {
        return Err(unsupported("Workbook() with arguments (write_only, iso_dates)"));
    }
    let lists = Lists {
        fonts: vec![Font { default: true, ..Font::default() }].into(),
        fills: vec![
            Fill { pattern: None, fg: NO_COLOR.into(), bg: NO_COLOR.into() },
            Fill { pattern: Some("gray125".into()), fg: NO_COLOR.into(), bg: NO_COLOR.into() },
        ]
        .into(),
        borders: vec![Border {
            left: Some(Side::default()),
            right: Some(Side::default()),
            top: Some(Side::default()),
            bottom: Some(Side::default()),
            diagonal: Some(Side::default()),
        }]
        .into(),
        aligns: vec![Align::default()].into(),
        xfs: vec![[0; 9]].into(),
    };
    let book = Arc::new(Book { sheets: Mutex::new(Vec::new()), lists: Mutex::new(lists), created: chrono::Utc::now() });
    let ws = new_sheet(&book, None)?;
    book.sheets.lock().push(ws);
    Ok(wrap(Xl::Book(book)))
}

fn new_sheet(book: &Arc<Book>, title: Option<&V>) -> R<Arc<Sheet>> {
    let ws = Arc::new(Sheet {
        book: Arc::downgrade(book),
        st: Mutex::new(SheetSt { title: String::new(), cells: BTreeMap::new(), merged: Vec::new(), cols: Vec::new(), current_row: 0 }),
    });
    let title = match title {
        None | Some(V::None) => V::str("Sheet"),
        Some(V::Str(s)) if s.is_empty() => V::str("Sheet"),
        Some(t) => t.clone(),
    };
    set_title(book, &ws, &title)?;
    Ok(ws)
}

// avoid_duplicate_name: duplicates are case insensitive, the next number after the highest one found in
// the comma-joined names (findall of `(title)(\d*),?`, re.I)
fn avoid_duplicate_name(names: &[String], value: &str) -> String {
    let low = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
    if !names.iter().any(|n| low(n) == low(value)) {
        return value.to_string();
    }
    let joined: Vec<char> = names.join(",").chars().collect();
    let pat: Vec<char> = value.chars().collect();
    let eq = |a: char, b: char| a == b || low(&a.to_string()) == low(&b.to_string());
    // the numbers as decimal strings without leading zeros: Python's int has no limit (a 20-digit suffix wrapped)
    let (mut i, mut highest, mut found) = (0, String::from("0"), false);
    while i + pat.len() <= joined.len() {
        if pat.iter().enumerate().all(|(k, c)| eq(joined[i + k], *c)) {
            found = true;
            let mut j = i + pat.len();
            let start = j;
            while j < joined.len() && joined[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                let n: String = joined[start..j].iter().collect();
                let n = n.trim_start_matches('0');
                let n = if n.is_empty() { "0" } else { n };
                if (n.len(), n) > (highest.len(), highest.as_str()) {
                    highest = n.to_string();
                }
            }
            if j < joined.len() && joined[j] == ',' {
                j += 1;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    if found { format!("{value}{}", decimal_succ(&highest)) } else { value.to_string() }
}

/// n + 1 of a decimal string
fn decimal_succ(n: &str) -> String {
    let mut d: Vec<u8> = n.bytes().collect();
    let mut i = d.len();
    loop {
        if i == 0 {
            d.insert(0, b'1');
            break;
        }
        i -= 1;
        if d[i] == b'9' {
            d[i] = b'0';
        } else {
            d[i] += 1;
            break;
        }
    }
    String::from_utf8(d).unwrap_or_default()
}

fn set_title(book: &Arc<Book>, ws: &Arc<Sheet>, v: &V) -> R<()> {
    let value = match v {
        V::Str(s) => s.to_string(),
        V::None => return Err(Exc::value_error("Title must have at least one character")),
        o => return Err(unsupported(format!("sheet title of type {}", o.type_name()))),
    };
    if value.is_empty() {
        return Err(Exc::value_error("Title must have at least one character"));
    }
    if let Some(c) = value.chars().find(|c| matches!(c, '\\' | '*' | '?' | ':' | '/' | '[' | ']')) {
        return Err(Exc::value_error(format!("Invalid character {c} found in sheet title")));
    }
    let current = ws.st.lock().title.clone();
    let value = if current != value {
        // sheetnames: the sheet itself counts once it is in the workbook
        let mut all = Vec::new();
        for s in book.sheets.lock().iter() {
            all.push(if Arc::ptr_eq(s, ws) { current.clone() } else { s.st.lock().title.clone() });
        }
        avoid_duplicate_name(&all, &value)
    } else {
        value
    };
    if value.chars().count() > 31 {
        // openpyxl warns (warnings.warn), a sheet name Excel would refuse
        return Err(unsupported("sheet title over 31 characters (openpyxl only warns)"));
    }
    ws.st.lock().title = value;
    Ok(())
}

// ---------------------------------------------------------------- coordinates

pub fn column_letter(n: i64) -> R<String> {
    if !(1..=18278).contains(&n) {
        return Err(Exc::value_error(format!("Invalid column index {n}")));
    }
    let mut n = n;
    let mut out = Vec::new();
    while n > 0 {
        let (q, r) = ((n - 1) / 26, (n - 1) % 26);
        out.push((b'A' + r as u8) as char);
        n = q;
    }
    Ok(out.iter().rev().collect())
}

/// `openpyxl.utils.get_column_letter(n)`
pub fn get_column_letter(v: &V) -> R {
    match v {
        V::Int(n) => Ok(V::str(column_letter(*n)?)),
        V::Bool(b) => Ok(V::str(column_letter(*b as i64)?)),
        o => Err(Exc::type_error(format!("'<=' not supported between instances of 'int' and '{}'", o.type_name()))),
    }
}

fn column_index(s: &str) -> R<u32> {
    if s.chars().count() > 3 || s.is_empty() {
        return Err(Exc::value_error(format!("'{s}' is not a valid column name. Column names are from A to ZZZ")));
    }
    let mut idx = 0u32;
    for (p, c) in s.to_ascii_uppercase().chars().rev().enumerate() {
        if !c.is_ascii_uppercase() {
            return Err(Exc::value_error(format!("'{s}' is not a valid column name. Column names are from A to ZZZ")));
        }
        idx += (c as u32 - 64) * 26u32.pow(p as u32);
    }
    Ok(idx)
}

type Bounds = (Option<u32>, Option<u32>, Option<u32>, Option<u32>);

/// range_boundaries: (min_col, min_row, max_col, max_row) of `A1`, `A`, `3`, `A1:B2`, `A:B`, `1:3`
fn range_boundaries(s: &str) -> R<Bounds> {
    let bad = || Exc::value_error(format!("{s} is not a valid coordinate or range"));
    let part = |p: &str| -> Option<(Option<String>, Option<String>)> {
        let p = p.strip_prefix('$').unwrap_or(p);
        let letters: String = p.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let rest = &p[letters.len()..];
        let rest = rest.strip_prefix('$').unwrap_or(rest);
        if letters.len() > 3 || !rest.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Some(((!letters.is_empty()).then_some(letters), (!rest.is_empty()).then(|| rest.to_string())))
    };
    let (a, b) = match s.split_once(':') {
        Some((x, y)) => (x, Some(y)),
        None => (s, None),
    };
    let (c1, r1) = part(a).ok_or_else(bad)?;
    let (c2, r2) = match b {
        Some(y) => part(y).ok_or_else(bad)?,
        None => (None, None),
    };
    if b.is_some() {
        let all = c1.is_some() && r1.is_some() && c2.is_some() && r2.is_some();
        let cols = c1.is_some() && c2.is_some() && r1.is_none() && r2.is_none();
        let rows = r1.is_some() && r2.is_some() && c1.is_none() && c2.is_none();
        if !(all || cols || rows) {
            return Err(bad());
        }
    }
    let num = |r: &Option<String>| r.as_ref().map(|x| x.parse::<u32>().unwrap_or(u32::MAX));
    let min_col = c1.as_deref().map(column_index).transpose()?;
    let min_row = num(&r1);
    let max_col = match c2.as_deref() {
        Some(c) => Some(column_index(c)?),
        None => min_col,
    };
    let max_row = if r2.is_some() { num(&r2) } else { min_row };
    Ok((min_col, min_row, max_col, max_row))
}

// ---------------------------------------------------------------- worksheet

fn get_cell(ws: &Arc<Sheet>, row: i64, col: i64) -> R<Arc<Cell>> {
    if !(0 < row && row < 1048577) {
        return Err(Exc::value_error(format!("Row numbers must be between 1 and 1048576. Row number supplied was {row}")));
    }
    if col > u32::MAX as i64 {
        return Err(unsupported("column index over 2**32"));
    }
    let key = (row as u32, col as u32);
    let mut st = ws.st.lock();
    if !st.cells.contains_key(&key) {
        // _add_cell: the row append() writes after, and what lets iter_rows() without bounds yield
        st.current_row = st.current_row.max(key.0);
    }
    Ok(st
        .cells
        .entry(key)
        .or_insert_with(|| {
            Arc::new(Cell {
                sheet: Arc::downgrade(ws),
                row: key.0,
                col: key.1,
                merged: false,
                st: Mutex::new(CellSt { value: V::None, dtype: 'n', style: None }),
            })
        })
        .clone())
}

fn int_arg(v: &V) -> R<i64> {
    match v {
        V::Int(n) => Ok(*n),
        V::Bool(b) => Ok(*b as i64),
        o => Err(Exc::type_error(format!("'<' not supported between instances of '{}' and 'int'", o.type_name()))),
    }
}

fn cell(ws: &Arc<Sheet>, row: &V, col: &V, value: Option<&V>) -> R {
    let (r, c) = (int_arg(row)?, int_arg(col)?);
    if r < 1 || c < 1 {
        return Err(Exc::value_error("Row or column values must be at least 1"));
    }
    let cl = get_cell(ws, r, c)?;
    if let Some(v) = value {
        if !matches!(v, V::None) {
            set_value(&cl, v)?;
        }
    }
    Ok(wrap(Xl::Cell(cl)))
}

fn extent(st: &SheetSt) -> (u32, u32, u32, u32) {
    if st.cells.is_empty() {
        return (1, 1, 1, 1);
    }
    let rows = st.cells.keys().map(|k| k.0);
    let cols = st.cells.keys().map(|k| k.1);
    (rows.clone().min().unwrap(), rows.max().unwrap(), cols.clone().min().unwrap(), cols.max().unwrap())
}

fn rows_of(ws: &Arc<Sheet>, min_col: u32, min_row: u32, max_col: u32, max_row: u32, by_col: bool, values: bool) -> R<Vec<V>> {
    let mut out = Vec::new();
    let (outer, inner) = if by_col { ((min_col, max_col), (min_row, max_row)) } else { ((min_row, max_row), (min_col, max_col)) };
    for a in outer.0..=outer.1 {
        let mut line = Vec::new();
        for b in inner.0..=inner.1 {
            let (r, c) = if by_col { (b, a) } else { (a, b) };
            let cl = get_cell(ws, r as i64, c as i64)?;
            line.push(if values { cl.st.lock().value.clone() } else { wrap(Xl::Cell(cl)) });
        }
        out.push(V::tuple(line));
    }
    Ok(out)
}

fn kw<'a>(args: &'a [V], kwargs: &'a [(String, V)], i: usize, name: &str) -> Option<&'a V> {
    args.get(i).or_else(|| kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v))
}

fn opt_u32(v: Option<&V>) -> R<Option<u32>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(x) => {
            let n = int_arg(x)?;
            // `min_row or 1`: 0 reads as absent
            Ok(if n == 0 { None } else { Some(n.clamp(1, u32::MAX as i64) as u32) })
        }
    }
}

fn iter_rows(ws: &Arc<Sheet>, args: &[V], kwargs: &[(String, V)], by_col: bool) -> R {
    let names = if by_col { ["min_col", "max_col", "min_row", "max_row"] } else { ["min_row", "max_row", "min_col", "max_col"] };
    let vals: Vec<Option<u32>> = (0..4).map(|i| opt_u32(kw(args, kwargs, i, names[i]))).collect::<R<_>>()?;
    let values = match kw(args, kwargs, 4, "values_only") {
        None => false,
        Some(v) => super::ops::truthy(v)?,
    };
    let (a, b, c, d) = (vals[0], vals[1], vals[2], vals[3]);
    let (min_row, max_row, min_col, max_col) = if by_col { (c, d, a, b) } else { (a, b, c, d) };
    let (cur, ext) = {
        let st = ws.st.lock();
        (st.current_row, extent(&st))
    };
    if cur == 0 && [min_row, max_row, min_col, max_col].iter().all(Option::is_none) {
        return Ok(V::list(vec![]));
    }
    let rows = rows_of(ws, min_col.unwrap_or(1), min_row.unwrap_or(1), max_col.unwrap_or(ext.3), max_row.unwrap_or(ext.1), by_col, values)?;
    Ok(V::list(rows))
}

fn sheet_getitem(ws: &Arc<Sheet>, key: &V) -> R {
    let key = match key {
        V::Str(s) => s.to_string(),
        V::Int(n) => n.to_string(),
        o => return Err(unsupported(format!("worksheet[{}]", o.type_name()))),
    };
    let (min_col, min_row, max_col, max_row) = range_boundaries(&key)?;
    if [min_col, min_row, max_col, max_row].iter().all(|x| x.unwrap_or(0) == 0) {
        return Err(Exc::new(&INDEX_ERROR, vec![V::str(format!("{key} is not a valid coordinate or range"))]));
    }
    let ext = extent(&ws.st.lock());
    let cur = ws.st.lock().current_row;
    let zero = |x: Option<u32>| x.filter(|n| *n != 0);
    if min_row.is_none() {
        let (a, b) = (zero(min_col), zero(max_col));
        if cur == 0 && a.is_none() && b.is_none() {
            return Err(Exc::new(&INDEX_ERROR, vec![V::str("tuple index out of range")]));
        }
        let cols = rows_of(ws, a.unwrap_or(1), 1, b.unwrap_or(ext.3), ext.1, true, false)?;
        return Ok(if min_col == max_col { cols.into_iter().next().unwrap_or(V::tuple(vec![])) } else { V::tuple(cols) });
    }
    if min_col.is_none() {
        let (a, b) = (zero(min_row), zero(max_row));
        let rows = rows_of(ws, 1, a.unwrap_or(1), ext.3, b.unwrap_or(ext.1), false, false)?;
        return Ok(if min_row == max_row { rows.into_iter().next().unwrap_or(V::tuple(vec![])) } else { V::tuple(rows) });
    }
    if !key.contains(':') {
        return Ok(wrap(Xl::Cell(get_cell(ws, min_row.unwrap() as i64, min_col.unwrap() as i64)?)));
    }
    let (r0, r1) = (zero(min_row).unwrap_or(1), zero(max_row).unwrap_or(ext.1));
    let (c0, c1) = (zero(min_col).unwrap_or(1), zero(max_col).unwrap_or(ext.3));
    Ok(V::tuple(rows_of(ws, c0, r0, c1, r1, false, false)?))
}

fn merge_cells(ws: &Arc<Sheet>, args: &[V], kwargs: &[(String, V)]) -> R {
    let range = match kw(args, kwargs, 0, "range_string") {
        Some(V::Str(s)) => s.to_string(),
        Some(o) if !matches!(o, V::None) => return Err(unsupported(format!("merge_cells({})", o.type_name()))),
        _ => {
            let get = |i: usize, n: &str| -> R<u32> {
                match kw(args, kwargs, i, n) {
                    Some(V::None) | None => Err(unsupported("merge_cells() without its four bounds")),
                    Some(v) => Ok(int_arg(v)?.clamp(0, u32::MAX as i64) as u32),
                }
            };
            let (r0, c0, r1, c1) = (get(1, "start_row")?, get(2, "start_column")?, get(3, "end_row")?, get(4, "end_column")?);
            if r0 == 0 || c0 == 0 || r1 < r0 || c1 < c0 {
                return Err(unsupported("merge_cells() with these bounds"));
            }
            format!("{}{}:{}{}", column_letter(c0 as i64)?, r0, column_letter(c1 as i64)?, r1)
        }
    };
    let (Some(c0), Some(r0), Some(c1), Some(r1)) = range_boundaries(&range)? else {
        return Err(unsupported("merge_cells() of whole rows or columns"));
    };
    if c1 < c0 || r1 < r0 {
        return Err(unsupported("merge_cells() of a reversed range"));
    }
    if !ws.st.lock().merged.is_empty() {
        // MultiCellRange keeps a set: the order of several ranges is that of CPython's set
        return Err(unsupported("more than one merged range in a worksheet"));
    }
    let book = book_of(ws)?;
    // MergedCellRange._get_borders: the start cell, and the end cell's right and bottom borders added to it
    let start = get_cell(ws, r0 as i64, c0 as i64)?;
    let end = ws.st.lock().cells.get(&(r1, c1)).cloned();
    if let Some(end) = end {
        let eb = border_of(&book, &end);
        let add = Border { right: eb.right.clone(), bottom: eb.bottom.clone(), ..Border::default() };
        let nb = border_of(&book, &start).add(&add);
        set_border(&book, &start, &nb);
    }
    ws.st.lock().merged.push((c0, r0, c1, r1));
    // _clean_merge_range: every cell but the first becomes a MergedCell
    {
        let mut st = ws.st.lock();
        for r in r0..=r1 {
            for c in c0..=c1 {
                if (r, c) != (r0, c0) {
                    st.cells.insert((r, c), merged_cell(ws, r, c));
                }
            }
        }
    }
    // format(): the start cell's styled edges given to the cells on that edge (protection: no change)
    let sb = border_of(&book, &start);
    for name in ["top", "left", "right", "bottom"] {
        let side = sb.side(name).clone();
        if side.as_ref().map(|s| s.style.is_none()).unwrap_or(false) {
            continue;
        }
        let mut add = Border::default();
        match name {
            "top" => add.top = side,
            "left" => add.left = side,
            "right" => add.right = side,
            _ => add.bottom = side,
        }
        let coords: Vec<(u32, u32)> = match name {
            "top" => (c0..=c1).map(|c| (r0, c)).collect(),
            "bottom" => (c0..=c1).map(|c| (r1, c)).collect(),
            "left" => (r0..=r1).map(|r| (r, c0)).collect(),
            _ => (r0..=r1).map(|r| (r, c1)).collect(),
        };
        for k in coords {
            let cl = ws.st.lock().cells.get(&k).cloned().unwrap_or_else(|| merged_cell(ws, k.0, k.1));
            let nb = border_of(&book, &cl).add(&add);
            set_border(&book, &cl, &nb);
        }
    }
    Ok(V::None)
}

fn merged_cell(ws: &Arc<Sheet>, r: u32, c: u32) -> Arc<Cell> {
    Arc::new(Cell { sheet: Arc::downgrade(ws), row: r, col: c, merged: true, st: Mutex::new(CellSt { value: V::None, dtype: 'n', style: None }) })
}

fn append(ws: &Arc<Sheet>, it: &V) -> R {
    let items = match it {
        V::List(_) | V::Tuple(_) => super::ops::iter(it)?,
        o => return Err(unsupported(format!("append() of a {}", o.type_name()))),
    };
    let row = ws.st.lock().current_row + 1;
    for (i, v) in items.iter().enumerate() {
        // a new Cell in its place, whatever was there (its style too)
        let cl = Arc::new(Cell {
            sheet: Arc::downgrade(ws),
            row,
            col: i as u32 + 1,
            merged: false,
            st: Mutex::new(CellSt { value: V::None, dtype: 'n', style: None }),
        });
        set_value(&cl, v)?;
        ws.st.lock().cells.insert((row, i as u32 + 1), cl);
    }
    ws.st.lock().current_row = row;
    Ok(V::None)
}

// ---------------------------------------------------------------- cells

const ERROR_CODES: [&str; 7] = ["#NULL!", "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A"];

fn set_value(cl: &Arc<Cell>, v: &V) -> R<()> {
    if cl.merged {
        return Err(Exc::attr_error("'MergedCell' object attribute 'value' is read-only"));
    }
    let (value, dtype) = match v {
        V::None => (V::None, 'n'),
        V::Bool(_) => (v.clone(), 'b'),
        V::Int(_) | V::Float(_) | V::Decimal(_) => (v.clone(), 'n'),
        V::Str(s) => {
            // check_string: at most 32,767 characters, no control character
            let s: String = s.chars().take(32767).collect();
            if s.chars().any(|c| matches!(c as u32, 0..=8 | 11 | 12 | 14..=31)) {
                return Err(Exc::new(&OPENPYXL_ILLEGAL_CHARACTER, vec![V::str(format!("{s} cannot be used in worksheets."))]));
            }
            let dt = if s.chars().count() > 1 && s.starts_with('=') {
                'f'
            } else if ERROR_CODES.contains(&s.as_str()) {
                'e'
            } else {
                's'
            };
            (V::str(s), dt)
        }
        V::DateTime(_) | V::Date(_) | V::Time(_) | V::Delta(_) => return Err(unsupported("date and time cell values")),
        o => return Err(Exc::value_error(format!("Cannot convert {} to Excel", super::ops::repr(o)?))),
    };
    let mut st = cl.st.lock();
    st.value = value;
    st.dtype = dtype;
    Ok(())
}

fn style_slot(book: &Book, cl: &Cell, slot: usize, idx: u32) {
    let _ = book;
    let mut st = cl.st.lock();
    let s = st.style.get_or_insert([0; 9]);
    s[slot] = idx;
}

fn border_of(book: &Book, cl: &Cell) -> Border {
    let id = cl.st.lock().style.map(|s| s[2]).unwrap_or(0);
    book.lists.lock().borders[id as usize].clone()
}

fn set_border(book: &Book, cl: &Cell, b: &Border) {
    let idx = index_of(&mut book.lists.lock().borders, b);
    style_slot(book, cl, 2, idx);
}

fn cell_setattr(cl: &Arc<Cell>, name: &str, v: V) -> R<()> {
    if name == "value" {
        return set_value(cl, &v);
    }
    let ws = cl.sheet.upgrade().ok_or_else(|| Exc::runtime("py2axum: the cell's worksheet is gone"))?;
    let book = book_of(&ws)?;
    let style = match &v {
        V::Native(n) => match &**n {
            Native::Xl(x) => match &**x {
                Xl::Style(s) => Some(s),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    };
    let expected = |cls: &str| Exc::type_error(format!("expected <class 'openpyxl.styles.{cls}'>"));
    match (name, style) {
        ("font", Some(Style::Font(f))) => {
            let i = index_of(&mut book.lists.lock().fonts, f);
            style_slot(&book, cl, 0, i);
        }
        ("fill", Some(Style::Fill(f))) => {
            let i = index_of(&mut book.lists.lock().fills, f);
            style_slot(&book, cl, 1, i);
        }
        ("border", Some(Style::Border(b))) => set_border(&book, cl, b),
        ("alignment", Some(Style::Align(a))) => {
            let i = index_of(&mut book.lists.lock().aligns, a);
            style_slot(&book, cl, 5, i);
        }
        ("font", _) => return Err(expected("fonts.Font")),
        ("fill", _) => return Err(expected("fills.Fill")),
        ("border", _) => return Err(expected("borders.Border")),
        ("alignment", _) => return Err(expected("alignment.Alignment")),
        _ => return Err(unsupported(format!("setting {name} on a cell"))),
    }
    Ok(())
}

fn cell_attr(cl: &Arc<Cell>, name: &str) -> R {
    Ok(match name {
        "value" => cl.st.lock().value.clone(),
        "row" => V::Int(cl.row as i64),
        "column" => V::Int(cl.col as i64),
        "coordinate" => V::str(format!("{}{}", column_letter(cl.col as i64)?, cl.row)),
        "column_letter" if cl.merged => return Err(Exc::attr_error("'MergedCell' object has no attribute 'column_letter'")),
        "column_letter" => V::str(column_letter(cl.col as i64)?),
        "data_type" => V::str(cl.st.lock().dtype.to_string()),
        "has_style" => V::Bool(cl.st.lock().style.map(|s| s.iter().any(|x| *x != 0)).unwrap_or(false)),
        _ => return Err(unsupported(format!("reading {name} of a cell"))),
    })
}

// ---------------------------------------------------------------- style constructors

fn color_arg(v: Option<&V>) -> R<Option<String>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(V::Str(s)) => {
            let ok = (s.len() == 6 || s.len() == 8) && s.chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err(Exc::value_error("Colors must be aRGB hex values"));
            }
            Ok(Some(if s.len() == 6 { format!("00{s}") } else { s.to_string() }))
        }
        Some(_) => Err(Exc::type_error("expected <class 'openpyxl.styles.colors.Color'>")),
    }
}

fn bool_arg(v: Option<&V>) -> Option<bool> {
    match v {
        None | Some(V::None) => None,
        Some(x) => Some(super::ops::truthy(x).unwrap_or(true)),
    }
}

fn float_arg(v: Option<&V>) -> R<Option<f64>> {
    Ok(match v {
        None | Some(V::None) => None,
        Some(V::Int(n)) => Some(*n as f64),
        Some(V::Bool(b)) => Some(*b as i64 as f64),
        Some(V::Float(x)) => Some(*x),
        Some(V::Decimal(d)) => Some(d.to_f64()),
        Some(o) => return Err(Exc::type_error(format!("expected <class 'float'>, got {}", o.type_name()))),
    })
}

fn choice(v: Option<&V>, allowed: &[&str]) -> R<Option<String>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(V::Str(s)) if &**s == "none" => Ok(None),
        Some(V::Str(s)) if allowed.contains(&&**s) => Ok(Some(s.to_string())),
        Some(_) => Err(Exc::value_error(format!("Value must be one of {{{}}}", allowed.iter().map(|a| format!("'{a}'")).collect::<Vec<_>>().join(", ")))),
    }
}

fn pick<'a>(kwargs: &'a [(String, V)], names: &[&str]) -> Option<&'a V> {
    // the later alias wins, as in the constructors (`if bold is not None: b = bold`)
    names.iter().rev().find_map(|n| kwargs.iter().find(|(k, v)| k == n && !matches!(v, V::None)).map(|(_, v)| v))
}

fn only(what: &str, args: &[V], kwargs: &[(String, V)], allowed: &[&str]) -> R<()> {
    if !args.is_empty() {
        return Err(unsupported(format!("{what}() with positional arguments")));
    }
    match kwargs.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(unsupported(format!("{what}({k}=)"))),
        None => Ok(()),
    }
}

const SIDE_STYLES: [&str; 13] = ["dashDot", "dashDotDot", "dashed", "dotted", "double", "hair", "medium", "mediumDashDot",
                                 "mediumDashDotDot", "mediumDashed", "slantDashDot", "thick", "thin"];
const FILLS: [&str; 18] = ["solid", "darkDown", "darkGray", "darkGrid", "darkHorizontal", "darkTrellis", "darkUp", "darkVertical",
                           "gray0625", "gray125", "lightDown", "lightGray", "lightGrid", "lightHorizontal", "lightTrellis",
                           "lightUp", "lightVertical", "mediumGray"];

/// `Font(...)`, `PatternFill(...)`, `Side(...)`, `Border(...)`, `Alignment(...)` from openpyxl.styles
pub fn style(kind: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    let s = match kind {
        "Font" => {
            only(kind, args, kwargs, &["name", "size", "sz", "bold", "b", "italic", "i", "strike", "color"])?;
            let sz = float_arg(pick(kwargs, &["sz", "size"]))?.map(|x| if x == 0.0 { 0.0f64.to_bits() } else { x.to_bits() });
            let name = match pick(kwargs, &["name"]) {
                None => None,
                Some(V::Str(s)) => Some(s.to_string()),
                Some(o) => return Err(Exc::type_error(format!("expected <class 'str'>, got {}", o.type_name()))),
            };
            Style::Font(Font {
                name,
                b: bool_arg(pick(kwargs, &["b", "bold"])),
                i: bool_arg(pick(kwargs, &["i", "italic"])),
                strike: bool_arg(pick(kwargs, &["strike"])),
                color: color_arg(pick(kwargs, &["color"]))?,
                sz,
                default: false,
            })
        }
        "PatternFill" => {
            only(kind, args, kwargs, &["patternType", "fill_type", "fgColor", "start_color", "bgColor", "end_color"])?;
            Style::Fill(Fill {
                pattern: choice(pick(kwargs, &["patternType", "fill_type"]), &FILLS)?,
                fg: color_arg(pick(kwargs, &["fgColor", "start_color"]))?.unwrap_or_else(|| NO_COLOR.into()),
                bg: color_arg(pick(kwargs, &["bgColor", "end_color"]))?.unwrap_or_else(|| NO_COLOR.into()),
            })
        }
        "Side" => {
            only(kind, args, kwargs, &["style", "border_style", "color"])?;
            Style::Side(Side { style: choice(pick(kwargs, &["style", "border_style"]), &SIDE_STYLES)?, color: color_arg(pick(kwargs, &["color"]))? })
        }
        "Border" => {
            only(kind, args, kwargs, &["left", "right", "top", "bottom", "diagonal"])?;
            let side = |n: &str| -> R<Option<Side>> {
                match pick(kwargs, &[n]) {
                    None => Ok(None),
                    Some(V::Native(x)) => match &**x {
                        Native::Xl(x) => match &**x {
                            Xl::Style(Style::Side(s)) => Ok(Some(s.clone())),
                            _ => Err(Exc::type_error("expected <class 'openpyxl.styles.borders.Side'>")),
                        },
                        _ => Err(Exc::type_error("expected <class 'openpyxl.styles.borders.Side'>")),
                    },
                    Some(_) => Err(Exc::type_error("expected <class 'openpyxl.styles.borders.Side'>")),
                }
            };
            Style::Border(Border { left: side("left")?, right: side("right")?, top: side("top")?, bottom: side("bottom")?, diagonal: side("diagonal")? })
        }
        "Alignment" => {
            only(kind, args, kwargs, &["horizontal", "vertical", "wrap_text", "wrapText", "shrink_to_fit", "shrinkToFit"])?;
            Style::Align(Align {
                horizontal: choice(pick(kwargs, &["horizontal"]), &["general", "left", "center", "right", "fill", "justify", "centerContinuous", "distributed"])?,
                vertical: choice(pick(kwargs, &["vertical"]), &["top", "center", "bottom", "justify", "distributed"])?,
                wrap: bool_arg(pick(kwargs, &["wrapText", "wrap_text"])),
                shrink: bool_arg(pick(kwargs, &["shrinkToFit", "shrink_to_fit"])),
            })
        }
        _ => return Err(unsupported(kind)),
    };
    Ok(wrap(Xl::Style(s)))
}

// ---------------------------------------------------------------- attributes, items, methods

pub fn attr(x: &Arc<Xl>, name: &str) -> R {
    let me = || V::native(Native::Xl(x.clone()));
    match &**x {
        Xl::Book(b) => match name {
            "active" => Ok(b.sheets.lock().first().map(|s| wrap(Xl::Sheet(s.clone()))).unwrap_or(V::None)),
            "worksheets" => Ok(V::list(b.sheets.lock().iter().map(|s| wrap(Xl::Sheet(s.clone()))).collect())),
            "sheetnames" => Ok(V::list(b.sheets.lock().iter().map(|s| V::str(s.st.lock().title.clone())).collect())),
            "create_sheet" | "save" => Ok(V::native(Native::MethodOf(me(), if name == "save" { "save" } else { "create_sheet" }))),
            _ => Err(unsupported(format!("Workbook.{name}"))),
        },
        Xl::Sheet(ws) => match name {
            "title" => Ok(V::str(ws.st.lock().title.clone())),
            "column_dimensions" => Ok(wrap(Xl::Dims(ws.clone()))),
            "max_row" => Ok(V::Int(extent(&ws.st.lock()).1 as i64)),
            "min_row" => Ok(V::Int(extent(&ws.st.lock()).0 as i64)),
            "max_column" => Ok(V::Int(extent(&ws.st.lock()).3 as i64)),
            "min_column" => Ok(V::Int(extent(&ws.st.lock()).2 as i64)),
            "parent" => Ok(wrap(Xl::Book(book_of(ws)?))),
            m @ ("cell" | "merge_cells" | "iter_rows" | "iter_cols" | "append") => Ok(V::native(Native::MethodOf(
                me(),
                match m {
                    "cell" => "cell",
                    "merge_cells" => "merge_cells",
                    "iter_rows" => "iter_rows",
                    "iter_cols" => "iter_cols",
                    _ => "append",
                },
            ))),
            _ => Err(unsupported(format!("Worksheet.{name}"))),
        },
        Xl::Cell(c) => cell_attr(c, name),
        Xl::Dim(ws, key) => match name {
            "width" => Ok(V::Float(ws.st.lock().cols.iter().find(|d| &d.index == key).map(|d| d.width).unwrap_or(DEFAULT_COLUMN_WIDTH))),
            _ => Err(unsupported(format!("ColumnDimension.{name}"))),
        },
        _ => Err(unsupported(format!("{}.{name}", type_name(x)))),
    }
}

pub fn setattr(x: &Arc<Xl>, name: &str, v: V) -> R<()> {
    match (&**x, name) {
        (Xl::Sheet(ws), "title") => set_title(&book_of(ws)?, ws, &v),
        (Xl::Cell(c), _) => cell_setattr(c, name, v),
        (Xl::Dim(ws, key), "width") => {
            let w = float_arg(Some(&v))?.ok_or_else(|| Exc::type_error("expected <class 'float'>"))?;
            let mut st = ws.st.lock();
            match st.cols.iter_mut().find(|d| &d.index == key) {
                Some(d) => d.width = w,
                None => st.cols.push(ColDim { index: key.clone(), width: w }),
            }
            Ok(())
        }
        _ => Err(unsupported(format!("setting {}.{name}", type_name(x)))),
    }
}

pub fn getitem(x: &Arc<Xl>, k: &V) -> R {
    match &**x {
        Xl::Sheet(ws) => sheet_getitem(ws, k),
        Xl::Book(b) => {
            let V::Str(name) = k else { return Err(unsupported("workbook[non-str]")) };
            match b.sheets.lock().iter().find(|s| s.st.lock().title == **name) {
                Some(s) => Ok(wrap(Xl::Sheet(s.clone()))),
                None => Err(Exc::new(&KEY_ERROR, vec![V::str(format!("Worksheet {name} does not exist."))])),
            }
        }
        Xl::Dims(ws) => {
            let V::Str(key) = k else { return Err(Exc::type_error("expected <class 'str'>")) };
            let key = key.to_string();
            column_index(&key)?;
            let mut st = ws.st.lock();
            if !st.cols.iter().any(|d| d.index == key) {
                // BoundDictionary: the default ColumnDimension (width 13) is created by the lookup itself
                st.cols.push(ColDim { index: key.clone(), width: DEFAULT_COLUMN_WIDTH });
            }
            Ok(wrap(Xl::Dim(ws.clone(), key)))
        }
        _ => Err(unsupported(format!("{}[...]", type_name(x)))),
    }
}

pub fn setitem(x: &Arc<Xl>, k: &V, v: V) -> R<()> {
    match &**x {
        Xl::Sheet(ws) => match sheet_getitem(ws, k)? {
            V::Native(n) => match &*n {
                Native::Xl(c) => match &**c {
                    Xl::Cell(c) => set_value(c, &v),
                    _ => Err(unsupported("assigning a range")),
                },
                _ => Err(unsupported("assigning a range")),
            },
            _ => Err(Exc::attr_error("'tuple' object has no attribute 'value'")),
        },
        _ => Err(unsupported(format!("{}[...] = ...", type_name(x)))),
    }
}

pub fn iter(x: &Arc<Xl>) -> R<Vec<V>> {
    match &**x {
        Xl::Sheet(ws) => match iter_rows(ws, &[], &[], false)? {
            V::List(l) => Ok(l.lock().clone()),
            _ => Ok(vec![]),
        },
        _ => Err(Exc::type_error(format!("'{}' object is not iterable", type_name(x)))),
    }
}

pub fn method(x: &Arc<Xl>, name: &str, args: &[V], kwargs: &[(String, V)]) -> R {
    match (&**x, name) {
        (Xl::Book(b), "create_sheet") => {
            if kw(args, kwargs, 1, "index").is_some_and(|v| !matches!(v, V::None)) {
                return Err(unsupported("create_sheet(index=)"));
            }
            let ws = new_sheet(b, kw(args, kwargs, 0, "title"))?;
            b.sheets.lock().push(ws.clone());
            Ok(wrap(Xl::Sheet(ws)))
        }
        (Xl::Book(b), "save") => save(b, kw(args, kwargs, 0, "filename")),
        (Xl::Sheet(ws), "cell") => {
            let row = kw(args, kwargs, 0, "row").ok_or_else(|| Exc::type_error("Worksheet.cell() missing 2 required positional arguments: 'row' and 'column'"))?;
            let col = kw(args, kwargs, 1, "column").ok_or_else(|| Exc::type_error("Worksheet.cell() missing 1 required positional argument: 'column'"))?;
            cell(ws, row, col, kw(args, kwargs, 2, "value"))
        }
        (Xl::Sheet(ws), "merge_cells") => merge_cells(ws, args, kwargs),
        (Xl::Sheet(ws), "iter_rows") => iter_rows(ws, args, kwargs, false),
        (Xl::Sheet(ws), "iter_cols") => iter_rows(ws, args, kwargs, true),
        (Xl::Sheet(ws), "append") => append(ws, args.first().ok_or_else(|| Exc::type_error("append() missing 1 required positional argument: 'iterable'"))?),
        _ => Err(unsupported(format!("{}.{name}()", type_name(x)))),
    }
}

// ---------------------------------------------------------------- writing

fn esc_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

fn esc_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#09;"),
            c => out.push(c),
        }
    }
    out
}

/// openpyxl's safe_string of a number: "%.16g"
fn g16(v: &V) -> R<String> {
    if let V::Decimal(d) = v {
        // `%` converts a Decimal with float()
        return super::ops::percent_format("%.16g", &V::Float(d.to_f64()));
    }
    super::ops::percent_format("%.16g", v)
}

fn color_xml(tag: &str, rgb: &str, out: &mut String) {
    out.push_str(&format!("<{tag} rgb=\"{}\" />", esc_attr(rgb)));
}

fn font_xml(f: &Font, out: &mut String) -> R<()> {
    if f.default {
        out.push_str("<font><name val=\"Calibri\" /><family val=\"2\" /><color theme=\"1\" /><sz val=\"11\" /><scheme val=\"minor\" /></font>");
        return Ok(());
    }
    if f.name.is_none() && f.color.is_none() && f.sz.is_none() && [f.b, f.i, f.strike].iter().all(|v| *v != Some(true)) {
        out.push_str("<font />");
        return Ok(());
    }
    out.push_str("<font>");
    if let Some(n) = &f.name {
        out.push_str(&format!("<name val=\"{}\" />", esc_attr(n)));
    }
    for (tag, v) in [("b", f.b), ("i", f.i), ("strike", f.strike)] {
        if v == Some(true) {
            out.push_str(&format!("<{tag} val=\"1\" />"));
        }
    }
    if let Some(c) = &f.color {
        color_xml("color", c, out);
    }
    if let Some(sz) = f.sz {
        out.push_str(&format!("<sz val=\"{}\" />", g16(&V::Float(f64::from_bits(sz)))?));
    }
    out.push_str("</font>");
    Ok(())
}

fn fill_xml(f: &Fill, out: &mut String) {
    out.push_str("<fill>");
    let mut open = String::from("<patternFill");
    if let Some(p) = &f.pattern {
        open.push_str(&format!(" patternType=\"{p}\""));
    }
    let mut inner = String::new();
    if f.fg != NO_COLOR {
        color_xml("fgColor", &f.fg, &mut inner);
    }
    if f.bg != NO_COLOR {
        color_xml("bgColor", &f.bg, &mut inner);
    }
    if inner.is_empty() {
        out.push_str(&open);
        out.push_str(" />");
    } else {
        out.push_str(&open);
        out.push('>');
        out.push_str(&inner);
        out.push_str("</patternFill>");
    }
    out.push_str("</fill>");
}

fn border_xml(b: &Border, out: &mut String) {
    if [&b.left, &b.right, &b.top, &b.bottom, &b.diagonal].iter().all(|s| s.is_none()) {
        out.push_str("<border />");
        return;
    }
    out.push_str("<border>");
    for (tag, s) in [("left", &b.left), ("right", &b.right), ("top", &b.top), ("bottom", &b.bottom), ("diagonal", &b.diagonal)] {
        let Some(s) = s else { continue };
        let style = s.style.as_ref().map(|x| format!(" style=\"{x}\"")).unwrap_or_default();
        match &s.color {
            Some(c) => {
                out.push_str(&format!("<{tag}{style}>"));
                color_xml("color", c, out);
                out.push_str(&format!("</{tag}>"));
            }
            None => out.push_str(&format!("<{tag}{style} />")),
        }
    }
    out.push_str("</border>");
}

fn align_xml(a: &Align) -> String {
    let mut s = String::from("<alignment");
    if let Some(h) = &a.horizontal {
        s.push_str(&format!(" horizontal=\"{h}\""));
    }
    if let Some(v) = &a.vertical {
        s.push_str(&format!(" vertical=\"{v}\""));
    }
    if a.wrap == Some(true) {
        s.push_str(" wrapText=\"1\"");
    }
    if a.shrink == Some(true) {
        s.push_str(" shrinkToFit=\"1\"");
    }
    s.push_str(" />");
    s
}

const INDEXED: [&str; 64] = [
    "00000000", "00FFFFFF", "00FF0000", "0000FF00", "000000FF", "00FFFF00", "00FF00FF", "0000FFFF", "00000000", "00FFFFFF", "00FF0000",
    "0000FF00", "000000FF", "00FFFF00", "00FF00FF", "0000FFFF", "00800000", "00008000", "00000080", "00808000", "00800080", "00008080",
    "00C0C0C0", "00808080", "009999FF", "00993366", "00FFFFCC", "00CCFFFF", "00660066", "00FF8080", "000066CC", "00CCCCFF", "00000080",
    "00FF00FF", "00FFFF00", "0000FFFF", "00800080", "00800000", "00008080", "000000FF", "0000CCFF", "00CCFFFF", "00CCFFCC", "00FFFF99",
    "0099CCFF", "00FF99CC", "00CC99FF", "00FFCC99", "003366FF", "0033CCCC", "0099CC00", "00FFCC00", "00FF9900", "00FF6600", "00666699",
    "00969696", "00003366", "00339966", "00003300", "00333300", "00993300", "00993366", "00333399", "00333333",
];

fn styles_xml(l: &Lists) -> R<String> {
    let mut s = String::from("<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><numFmts count=\"0\" />");
    s.push_str(&format!("<fonts count=\"{}\">", l.fonts.len()));
    for f in &l.fonts {
        font_xml(f, &mut s)?;
    }
    s.push_str(&format!("</fonts><fills count=\"{}\">", l.fills.len()));
    for f in &l.fills {
        fill_xml(f, &mut s);
    }
    s.push_str(&format!("</fills><borders count=\"{}\">", l.borders.len()));
    for b in &l.borders {
        border_xml(b, &mut s);
    }
    s.push_str("</borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" /></cellStyleXfs>");
    s.push_str(&format!("<cellXfs count=\"{}\">", l.xfs.len()));
    for x in &l.xfs {
        let al = (x[5] != 0).then(|| align_xml(&l.aligns[x[5] as usize]));
        s.push_str(&format!("<xf numFmtId=\"{}\" fontId=\"{}\" fillId=\"{}\" borderId=\"{}\"", x[3], x[0], x[1], x[2]));
        if al.is_some() {
            s.push_str(" applyAlignment=\"1\"");
        }
        s.push_str(&format!(" pivotButton=\"{}\" quotePrefix=\"{}\" xfId=\"{}\"", x[6], x[7], x[8]));
        match al {
            Some(a) => s.push_str(&format!(">{a}</xf>")),
            None => s.push_str(" />"),
        }
    }
    s.push_str("</cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\" hidden=\"0\" /></cellStyles>");
    s.push_str("<tableStyles count=\"0\" defaultTableStyle=\"TableStyleMedium9\" defaultPivotStyle=\"PivotStyleLight16\" /><colors><indexedColors>");
    for c in INDEXED {
        s.push_str(&format!("<rgbColor rgb=\"{c}\" />"));
    }
    s.push_str("</indexedColors></colors></styleSheet>");
    Ok(s)
}

fn sheet_xml(book: &Book, ws: &Sheet) -> R<String> {
    let st = ws.st.lock();
    let mut s = String::from(
        "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetPr><outlinePr summaryBelow=\"1\" summaryRight=\"1\" /><pageSetUpPr /></sheetPr>",
    );
    let dim = if st.cells.is_empty() {
        "A1:A1".to_string()
    } else {
        let (r0, r1, c0, c1) = extent(&st);
        format!("{}{}:{}{}", column_letter(c0 as i64)?, r0, column_letter(c1 as i64)?, r1)
    };
    s.push_str(&format!("<dimension ref=\"{dim}\" />"));
    s.push_str("<sheetViews><sheetView workbookViewId=\"0\"><selection activeCell=\"A1\" sqref=\"A1\" /></sheetView></sheetViews>");
    s.push_str("<sheetFormatPr baseColWidth=\"8\" defaultRowHeight=\"15\" />");
    let mut cols: Vec<(u32, &ColDim)> = st.cols.iter().map(|d| Ok((column_index(&d.index)?, d))).collect::<R<_>>()?;
    cols.sort_by_key(|(i, _)| *i);
    let written: Vec<_> = cols.iter().filter(|(_, d)| d.width != 0.0).collect();
    if !written.is_empty() {
        s.push_str("<cols>");
        for (i, d) in written {
            s.push_str(&format!("<col width=\"{}\" customWidth=\"1\" min=\"{i}\" max=\"{i}\" />", g16(&V::Float(d.width))?));
        }
        s.push_str("</cols>");
    }
    s.push_str("<sheetData>");
    let mut lists = book.lists.lock();
    let mut row = None;
    for ((r, c), cl) in st.cells.iter() {
        if row != Some(*r) {
            if row.is_some() {
                s.push_str("</row>");
            }
            s.push_str(&format!("<row r=\"{r}\">"));
            row = Some(*r);
        }
        let cs = cl.st.lock();
        let styled = cs.style.map(|x| x.iter().any(|v| *v != 0)).unwrap_or(false);
        if matches!(cs.value, V::None) && !styled {
            continue;
        }
        s.push_str(&format!("<c r=\"{}{r}\"", column_letter(*c as i64)?));
        if styled {
            let sa = cs.style.unwrap();
            let id = index_of(&mut lists.xfs, &sa);
            s.push_str(&format!(" s=\"{id}\""));
        }
        match cs.dtype {
            's' => s.push_str(" t=\"inlineStr\""),
            'f' => {}
            t => s.push_str(&format!(" t=\"{t}\"")),
        }
        let empty = match &cs.value {
            V::None => true,
            V::Str(x) => x.is_empty(),
            _ => false,
        };
        if empty {
            s.push_str(" />");
            continue;
        }
        s.push('>');
        match (&cs.value, cs.dtype) {
            (V::Str(x), 'f') => {
                s.push_str("<f>");
                esc_text(&x[1..], &mut s);
                s.push_str("</f><v /></c>");
                continue;
            }
            (V::Str(x), 's') => {
                let t = x.trim();
                if !t.is_empty() && t != &**x {
                    s.push_str("<is><t xml:space=\"preserve\">");
                } else {
                    s.push_str("<is><t>");
                }
                esc_text(x, &mut s);
                s.push_str("</t></is>");
            }
            (V::Str(x), _) => {
                s.push_str("<v>");
                esc_text(x, &mut s);
                s.push_str("</v>");
            }
            (v, _) => s.push_str(&format!("<v>{}</v>", g16(v)?)),
        }
        s.push_str("</c>");
    }
    if row.is_some() {
        s.push_str("</row>");
    }
    s.push_str("</sheetData>");
    if let Some((c0, r0, c1, r1)) = st.merged.first() {
        // CellRange.coord: one cell is written alone
        let mut coord = format!("{}{r0}", column_letter(*c0 as i64)?);
        if (c0, r0) != (c1, r1) {
            coord.push_str(&format!(":{}{r1}", column_letter(*c1 as i64)?));
        }
        s.push_str(&format!("<mergeCells count=\"1\"><mergeCell ref=\"{coord}\" /></mergeCells>"));
    }
    s.push_str("<pageMargins left=\"0.75\" right=\"0.75\" top=\"1\" bottom=\"1\" header=\"0.5\" footer=\"0.5\" /></worksheet>");
    Ok(s)
}

fn w3cdtf(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn save(b: &Arc<Book>, target: Option<&V>) -> R {
    let target = match target {
        Some(t @ V::Native(n)) if matches!(&**n, Native::BytesIO(_)) => t.clone(),
        Some(_) => return Err(unsupported("save() to anything but an io.BytesIO")),
        None => return Err(Exc::type_error("Workbook.save() missing 1 required positional argument: 'filename'")),
    };
    let modified = chrono::Utc::now();
    let z = super::zipw::writer(target);
    let put = |name: &str, data: String, external: u32| super::zipw::add(&z, name, data.into_bytes(), 8, -1, external).map(|_| ());
    let file = 0o600 << 16;
    put("docProps/app.xml", "<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>Microsoft Excel Compatible / Openpyxl 3.1.5</Application><AppVersion>3.1</AppVersion></Properties>".into(), file)?;
    put(
        "docProps/core.xml",
        format!(
            "<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><dc:creator>openpyxl</dc:creator><dcterms:created xsi:type=\"dcterms:W3CDTF\">{}</dcterms:created><dcterms:modified xsi:type=\"dcterms:W3CDTF\">{}</dcterms:modified></cp:coreProperties>",
            w3cdtf(b.created),
            w3cdtf(modified)
        ),
        file,
    )?;
    put("xl/theme/theme1.xml", THEME.into(), file)?;
    let sheets = b.sheets.lock().clone();
    for (i, ws) in sheets.iter().enumerate() {
        // written to a temporary file then `archive.write()`: the file's mode, 0o100600
        put(&format!("xl/worksheets/sheet{}.xml", i + 1), sheet_xml(b, ws)?, 0o100600 << 16)?;
    }
    let styles = styles_xml(&b.lists.lock())?;
    put("xl/styles.xml", styles, file)?;
    put("_rels/.rels", "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\" Id=\"rId1\" /><Relationship Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\" Id=\"rId2\" /><Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\" Id=\"rId3\" /></Relationships>".into(), file)?;
    let mut wbx = String::from("<workbook xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><workbookPr /><workbookProtection /><bookViews><workbookView visibility=\"visible\" minimized=\"0\" showHorizontalScroll=\"1\" showVerticalScroll=\"1\" showSheetTabs=\"1\" tabRatio=\"600\" firstSheet=\"0\" activeTab=\"0\" autoFilterDateGrouping=\"1\" /></bookViews><sheets>");
    for (i, ws) in sheets.iter().enumerate() {
        wbx.push_str(&format!("<sheet name=\"{}\" sheetId=\"{}\" state=\"visible\" r:id=\"rId{}\" />", esc_attr(&ws.st.lock().title), i + 1, i + 1));
    }
    wbx.push_str("</sheets><definedNames /><calcPr calcId=\"124519\" fullCalcOnLoad=\"1\" /></workbook>");
    put("xl/workbook.xml", wbx, file)?;
    let mut rels = String::from("<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">");
    for i in 1..=sheets.len() {
        rels.push_str(&format!("<Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"/xl/worksheets/sheet{i}.xml\" Id=\"rId{i}\" />"));
    }
    let n = sheets.len();
    rels.push_str(&format!("<Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\" Id=\"rId{}\" /><Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"theme/theme1.xml\" Id=\"rId{}\" /></Relationships>", n + 1, n + 2));
    put("xl/_rels/workbook.xml.rels", rels, file)?;
    let mut ct = String::from("<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\" /><Default Extension=\"xml\" ContentType=\"application/xml\" /><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\" /><Override PartName=\"/xl/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\" /><Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\" /><Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\" />");
    for i in 1..=n {
        ct.push_str(&format!("<Override PartName=\"/xl/worksheets/sheet{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\" />"));
    }
    ct.push_str("<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\" /></Types>");
    put("[Content_Types].xml", ct, file)?;
    super::zipw::close(&z)?;
    Ok(V::None)
}
