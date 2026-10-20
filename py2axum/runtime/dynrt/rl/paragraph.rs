//! reportlab/platypus/paragraph.py: Paragraph as a Flowable (breakLines for one fragment and for
//! several, split between frames, drawPara with the four alignments). No bullets, hyphenation,
//! soft hyphens, CJK/bidi, endDots, autoLeading, underline/strike/links or images: those are refused.

use std::collections::VecDeque;
use std::rc::Rc;

use super::canvas::{font_id, Canvas};
use super::colors::Color;
use super::flowables::{Flowable, HAlign};
use super::fonts;
use super::frames::FUZZ;
use super::paraparser::{self, same_frag, Frag, FK_BREAK, FK_TEXT};
use super::styles::{ParagraphStyle, TA_CENTER, TA_JUSTIFY, TA_LEFT, TA_RIGHT};
use super::textobject::TextObject;
use super::RlError;

/// `_wsc` of paragraph.py (Unicode whitespace without U+00A0, with U+200B).
const WSC: [char; 29] = [
    '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}', '\u{20}', '\u{85}', '\u{1680}', '\u{2000}',
    '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}',
    '\u{200b}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
];

fn is_wsc(c: char) -> bool {
    WSC.contains(&c)
}

/// `string.whitespace`.
fn is_ascii_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(is_wsc)
}

fn lstrip(s: &str) -> &str {
    s.trim_start_matches(is_wsc)
}

/// `split(text)`: `str.split()` unless U+00A0 is present, then a split on runs of `_wsc` (empty ends kept).
pub fn split(text: &str) -> Vec<String> {
    if text.contains('\u{a0}') {
        let mut out = vec![String::new()];
        let mut in_ws = false;
        for c in text.chars() {
            if is_wsc(c) {
                if !in_ws {
                    out.push(String::new());
                    in_ws = true;
                }
            } else {
                in_ws = false;
                out.last_mut().unwrap().push(c);
            }
        }
        out
    } else {
        text.split(paraparser::py_isspace).filter(|w| !w.is_empty()).map(str::to_string).collect()
    }
}

fn nbsp_count(s: &str) -> usize {
    s.chars().filter(|&c| c == '\u{a0}').count()
}

fn sw(text: &str, font: &str, size: f64) -> Result<f64, RlError> {
    Ok(fonts::string_width(text, font_id(font)?, size))
}

/// A word of a one-fragment paragraph: str, `_SplitWord` or `_SplitWordEnd`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WKind {
    Plain,
    Split,
    SplitEnd,
}

#[derive(Clone, Debug)]
pub struct SWord {
    pub s: String,
    pub kind: WKind,
}

impl SWord {
    fn plain(s: String) -> SWord {
        SWord { s, kind: WKind::Plain }
    }
}

/// The list subclasses of a frag word.
#[derive(Clone, Copy, Debug, PartialEq)]
enum FwKind {
    List,
    Hs,
    Split,
    SplitH,
    SplitHs,
    Injected,
}

impl FwKind {
    fn is_hs(self) -> bool {
        matches!(self, FwKind::Hs | FwKind::SplitHs)
    }
    fn is_split(self) -> bool {
        matches!(self, FwKind::Split | FwKind::SplitH | FwKind::SplitHs)
    }
}

/// `[width, (frag, text), ...]`; frags are shared objects (`is` matters when rejoining).
#[derive(Clone, Debug)]
struct FragWord {
    w: f64,
    kind: FwKind,
    items: Vec<(Rc<Frag>, String)>,
}

#[derive(Clone, Debug)]
enum Frags {
    Parsed(Vec<Rc<Frag>>),
    Processed(Vec<FragWord>),
}

#[derive(Clone, Debug)]
struct FragLine {
    extra_space: f64,
    word_count: i64,
    line_break: bool,
    words: Vec<Frag>,
    font_size: f64,
    s_fw: usize,
}

#[derive(Clone, Debug)]
enum BlPara {
    /// kind 0: the frag's font and colour, lines of (extraSpace, words).
    Simple { f: Frag, lines: Vec<(f64, Vec<SWord>)> },
    /// kind 1.
    Lines(Vec<FragLine>),
}

impl BlPara {
    fn n_lines(&self) -> usize {
        match self {
            BlPara::Simple { lines, .. } => lines.len(),
            BlPara::Lines(l) => l.len(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Paragraph {
    pub text: Option<String>,
    frags: Frags,
    pub style: ParagraphStyle,
    bl_para: Option<BlPara>,
    pub width: f64,
    pub height: f64,
    /// `_JustifyLast` (set on the first half of a split).
    justify_last: bool,
}

fn check_style(s: &ParagraphStyle) -> Result<(), RlError> {
    let unsup = |what: &str| Err(RlError::Unsupported(format!("ParagraphStyle {what}")));
    if s.word_wrap.is_some() {
        return unsup("wordWrap");
    }
    if s.end_dots.is_some() {
        return unsup("endDots");
    }
    if s.text_transform.as_deref().is_some_and(|t| t.to_lowercase() != "none") {
        return unsup("textTransform");
    }
    if s.auto_leading.as_deref().is_some_and(|a| a != "" && a != "off") {
        return unsup("autoLeading");
    }
    if s.bullet_text.is_some() {
        return unsup("bulletText");
    }
    if s.uri_waste_reduce != 0.0 || s.embedded_hyphenation != 0.0 {
        return unsup("hyphenation");
    }
    Ok(())
}

fn has_shy(frags: &Frags) -> bool {
    match frags {
        Frags::Parsed(v) => v.iter().any(|f| f.text().contains('\u{ad}') || f.words.as_ref().is_some_and(|w| w.iter().any(|x| x.s.contains('\u{ad}')))),
        Frags::Processed(v) => v.iter().any(|w| w.items.iter().any(|(_, t)| t.contains('\u{ad}'))),
    }
}

impl Paragraph {
    /// `Paragraph(text, style)`.
    pub fn new(text: &str, style: &ParagraphStyle) -> Result<Paragraph, RlError> {
        check_style(style)?;
        let text = paraparser::clean_block_quoted_text(text);
        let frags = paraparser::parse(&text, style)?;
        let frags = Frags::Parsed(frags.into_iter().map(Rc::new).collect());
        if has_shy(&frags) {
            return Err(RlError::Unsupported("soft hyphen in paragraph text".into()));
        }
        Ok(Paragraph { text: Some(text), frags, style: style.clone(), bl_para: None, width: 0.0, height: 0.0, justify_last: false })
    }

    fn from_frags(frags: Frags, style: ParagraphStyle) -> Paragraph {
        Paragraph { text: None, frags, style, bl_para: None, width: 0.0, height: 0.0, justify_last: false }
    }

    fn n_frags(&self) -> usize {
        match &self.frags {
            Frags::Parsed(v) => v.len(),
            Frags::Processed(v) => v.len(),
        }
    }

    /// `minWidth()`.
    pub fn min_width(&self) -> Result<f64, RlError> {
        match &self.frags {
            Frags::Parsed(v) if v.is_empty() => Ok(0.0),
            Frags::Parsed(v) if v.len() == 1 => {
                let f = &v[0];
                let words: Vec<String> = match &f.text {
                    Some(t) => t.split(' ').map(str::to_string).collect(),
                    None => f.words.as_ref().map(|w| w.iter().map(|x| x.s.clone()).collect()).unwrap_or_default(),
                };
                let mut m: Option<f64> = None;
                for w in &words {
                    let x = sw(w, &f.font_name, f.font_size)?;
                    m = Some(m.map_or(x, |m| m.max(x)));
                }
                m.ok_or_else(|| RlError::Value("max() iterable argument is empty".into()))
            }
            _ => {
                let fw = get_frag_words(&self.frags)?;
                Ok(fw.iter().map(|w| w.w).fold(f64::NEG_INFINITY, |a, b| if b > a { b } else { a }))
            }
        }
    }

    /// `breakLines(widths)`.
    fn break_lines(&mut self, widths: &[f64]) -> Result<BlPara, RlError> {
        let max_widths = widths.to_vec();
        let maxlineno = max_widths.len() - 1;
        let mut lineno = 0usize;
        let style = &self.style;
        let space_shrinkage = style.space_shrinkage;
        let split_long_words = style.split_long_words;
        let mut max_width = max_widths[0];
        let simple = match &self.frags {
            Frags::Parsed(v) if v.len() == 1 && v[0].back_color.is_none() => Some(v[0].clone()),
            _ => None,
        };
        if let Some(f) = simple {
            let font_size = f.font_size;
            let font_name = f.font_name.clone();
            let mut bl_f = (*f).clone();
            bl_f.text = None;
            bl_f.words = None;
            let mut words: VecDeque<SWord> = match &f.text {
                Some(t) => {
                    let t = strip(t);
                    if t.is_empty() {
                        return Ok(BlPara::Simple { f: bl_f, lines: Vec::new() });
                    }
                    split(t).into_iter().map(SWord::plain).collect()
                }
                None => {
                    let w = f.words.clone().unwrap_or_default();
                    if !w.iter().any(|x| !strip(&x.s).is_empty()) {
                        return Ok(BlPara::Simple { f: bl_f, lines: Vec::new() });
                    }
                    w.into()
                }
            };
            let space_width = sw(" ", &font_name, font_size)?;
            let d_space_shrink = space_shrinkage * space_width;
            let mut c_line: Vec<SWord> = Vec::new();
            let mut current_width = -space_width;
            let mut forced_split = false;
            let mut lines: Vec<(f64, Vec<SWord>)> = Vec::new();
            while let Some(word) = words.pop_front() {
                if word.s.is_empty() && word.kind != WKind::Plain {
                    forced_split = true;
                }
                let word_width = sw(&word.s, &font_name, font_size)?;
                let new_width = current_width + space_width + word_width;
                let lim_width = max_width + d_space_shrink * c_line.len() as f64;
                if new_width > lim_width && !forced_split && split_long_words && word.kind == WKind::Plain {
                    let nmw = lineno.min(maxlineno);
                    if word_width > max_widths[nmw] {
                        let pieces =
                            split_word(&word.s, current_width + space_width, &max_widths, lineno, &font_name, font_size)?;
                        for p in pieces.into_iter().rev() {
                            words.push_front(p);
                        }
                        forced_split = true;
                        continue;
                    }
                }
                if new_width <= lim_width || c_line.is_empty() || forced_split {
                    if !word.s.is_empty() {
                        c_line.push(word);
                    }
                    if forced_split {
                        forced_split = false;
                        lines.push((max_width - new_width, std::mem::take(&mut c_line)));
                        current_width = -space_width;
                        lineno += 1;
                        max_width = max_widths[lineno.min(maxlineno)];
                    } else {
                        current_width = new_width;
                    }
                } else {
                    lines.push((max_width - current_width, std::mem::replace(&mut c_line, vec![word])));
                    current_width = word_width;
                    lineno += 1;
                    max_width = max_widths[lineno.min(maxlineno)];
                }
            }
            if !c_line.is_empty() {
                lines.push((max_width - current_width, c_line));
            }
            return Ok(BlPara::Simple { f: bl_f, lines });
        }
        if self.n_frags() == 0 {
            let s = &self.style;
            let f = Frag {
                font_name: s.font_name.clone(),
                font_size: s.font_size,
                text_color: s.text_color.clone(),
                back_color: None,
                bold: 0,
                italic: 0,
                rise: 0.0,
                nobr: false,
                text: None,
                words: None,
                line_break: false,
                self_closing: None,
                tag: String::new(),
                fkind: std::cell::Cell::new(paraparser::FK_NONE),
            };
            return Ok(BlPara::Simple { f, lines: Vec::new() });
        }
        // several fragments
        let njlbv = !style.justify_breaks;
        let mut words: Vec<Frag> = Vec::new();
        let mut fw: Vec<FragWord> = Vec::new();
        let mut q: VecDeque<FragWord> = get_frag_words(&self.frags)?.into();
        let mut s_fw = 0usize;
        let mut lines: Vec<FragLine> = Vec::new();
        let (mut n, mut space_width, mut current_width, mut max_size) = (0i64, 0.0f64, 0.0f64, 0.0f64);
        while let Some(w) = q.pop_front() {
            fw.push(w.clone());
            let f_last = w.items.last().unwrap().0.clone();
            let font_name = f_last.font_name.clone();
            let mut font_size = f_last.font_size;
            if words.is_empty() {
                n = 0;
                space_width = 0.0;
                current_width = 0.0;
                max_size = font_size;
            }
            let word_width = w.w;
            let f = w.items[0].0.clone();
            let new_width = if word_width > 0.0 { current_width + space_width + word_width } else { current_width };
            let line_break = f.fkind.get() == FK_BREAK;
            let mut lim_width = max_width;
            if space_shrinkage != 0.0 {
                let mut space_shrink = space_width;
                for wi in &words {
                    if wi.fkind.get() == FK_TEXT {
                        let ns = wi.text().matches(' ').count();
                        if ns > 0 {
                            space_shrink += ns as f64 * sw(" ", &wi.font_name, wi.font_size)?;
                        }
                    }
                }
                space_shrink *= space_shrinkage;
                lim_width += space_shrink;
            }
            if !line_break && new_width > lim_width && w.kind != FwKind::SplitH && split_long_words && !w.kind.is_split() {
                let nmw = lineno.min(maxlineno);
                if word_width > max_widths[nmw] {
                    let pieces = split_frag_word(&w, max_width - space_width - current_width, &max_widths, lineno)?;
                    let mut inj = (*f).clone();
                    inj.fkind = std::cell::Cell::new(FK_BREAK);
                    inj.text = Some(String::new());
                    let mut pieces: VecDeque<FragWord> = pieces.into();
                    pieces.insert(1, FragWord { w: 0.0, kind: FwKind::Injected, items: vec![(Rc::new(inj), String::new())] });
                    for p in pieces.into_iter().rev() {
                        q.push_front(p);
                    }
                    fw.pop();
                    continue;
                }
            }
            let end_line = (new_width > lim_width && n > 0) || line_break;
            if !end_line {
                let n_text = w.items[0].1.clone();
                if !n_text.is_empty() {
                    n += 1;
                }
                font_size = f.font_size;
                max_size = max_size.max(font_size);
                if words.is_empty() {
                    let mut g = (*f).clone();
                    g.text = Some(n_text);
                    words.push(g);
                } else if !same_frag(words.last().unwrap(), &f) {
                    if space_width != 0.0 {
                        for wi in words.iter_mut().rev() {
                            if wi.fkind.get() == FK_TEXT {
                                if !wi.text().ends_with(' ') {
                                    wi.text = Some(format!("{} ", wi.text()));
                                }
                                break;
                            }
                        }
                    }
                    let mut g = (*f).clone();
                    g.text = Some(n_text);
                    words.push(g);
                } else {
                    let g = words.last_mut().unwrap();
                    let t = g.text().to_string();
                    g.text = Some(if space_width != 0.0 && !t.ends_with(' ') { format!("{t} {n_text}") } else { format!("{t}{n_text}") });
                }
                space_width = if w.kind.is_hs() { sw(" ", &font_name, font_size)? } else { 0.0 };
                let mut ni = false;
                for (fr, t) in &w.items[1..] {
                    let mut g = (**fr).clone();
                    g.text = Some(t.clone());
                    if !t.is_empty() {
                        ni = true;
                    }
                    max_size = max_size.max(g.font_size);
                    words.push(g);
                }
                if n_text_empty(&w) && ni {
                    n += 1;
                }
                current_width = new_width;
            } else {
                let llb = if line_break {
                    words.push((*f).clone());
                    njlbv && w.kind != FwKind::Injected
                } else {
                    false
                };
                lines.push(FragLine {
                    extra_space: max_width - current_width,
                    word_count: n,
                    line_break: llb,
                    words: std::mem::take(&mut words),
                    font_size: max_size,
                    s_fw,
                });
                s_fw = fw.len() - 1;
                lineno += 1;
                max_width = max_widths[lineno.min(maxlineno)];
                if line_break {
                    continue;
                }
                space_width = if w.kind.is_hs() { sw(" ", &font_name, font_size)? } else { 0.0 };
                current_width = word_width;
                n = 1;
                let mut g = (*f).clone();
                max_size = g.font_size;
                g.text = Some(w.items[0].1.clone());
                words.push(g);
                for (fr, t) in &w.items[1..] {
                    let mut g = (**fr).clone();
                    g.text = Some(t.clone());
                    max_size = max_size.max(g.font_size);
                    words.push(g);
                }
            }
        }
        if !words.is_empty() {
            lines.push(FragLine { extra_space: max_width - current_width, word_count: n, line_break: false, words, font_size: max_size, s_fw });
        }
        self.frags = Frags::Processed(fw);
        Ok(BlPara::Lines(lines))
    }

    /// `_split_blParaSimple` / `_split_blParaProcessed`: the frags of lines start..stop.
    fn split_frags(&self, start: usize, stop: usize) -> Result<Frags, RlError> {
        match self.bl_para.as_ref().unwrap() {
            BlPara::Simple { f, lines } => {
                let mut out: Vec<SWord> = Vec::new();
                let mut r: Vec<String> = Vec::new();
                for (_, ws) in &lines[start..stop] {
                    for w in ws {
                        if w.kind != WKind::Plain {
                            r.push(w.s.clone());
                            if w.kind == WKind::SplitEnd {
                                out.push(SWord::plain(r.concat()));
                                r.clear();
                            }
                            continue;
                        } else if !r.is_empty() {
                            out.push(SWord::plain(r.concat()));
                            r.clear();
                        }
                        out.push(w.clone());
                    }
                }
                if !r.is_empty() {
                    out.push(SWord::plain(r.concat()));
                }
                if out.is_empty() {
                    return Err(RlError::Index("list index out of range".into()));
                }
                let mut g = f.clone();
                g.text = None;
                g.words = Some(out);
                Ok(Frags::Parsed(vec![Rc::new(g)]))
            }
            BlPara::Lines(lines) => {
                let Frags::Processed(fw) = &self.frags else { unreachable!() };
                if stop == 0 {
                    return Ok(Frags::Processed(Vec::new()));
                }
                let a = lines[start].s_fw;
                let b = if stop != lines.len() { lines[stop].s_fw } else { fw.len() };
                let mut v: Vec<FragWord> = fw[a..b].to_vec();
                while v.last().is_some_and(|x| x.kind == FwKind::Injected) {
                    v.pop();
                }
                if v.is_empty() {
                    return Err(RlError::Index("list index out of range".into()));
                }
                Ok(Frags::Processed(v))
            }
        }
    }

    fn draw_para(&mut self, canv: &mut Canvas) -> Result<(), RlError> {
        let style = self.style.clone();
        let bl = self.bl_para.clone().ok_or_else(|| RlError::Attribute("'Paragraph' object has no attribute 'blPara'".into()))?;
        let leading = style.leading;
        let left_indent = style.left_indent;
        let cur_x = left_indent;
        let bw = style.border_width;
        let bc = style.border_color.clone();
        let bg = style.back_color.clone();
        if bg.is_some() || (bc.is_some() && bw != 0.0) {
            canv.save_state();
            let (mut stroke, mut fill) = (false, false);
            let mut radius = None;
            if let (Some(c), true) = (&bc, bw != 0.0) {
                canv.set_stroke_color(c, None)?;
                canv.set_line_width(bw);
                stroke = true;
                if let Some(r) = style.border_radius.filter(|&r| r != 0.0) {
                    radius = Some(r);
                }
            }
            if let Some(c) = &bg {
                canv.set_fill_color(c, None)?;
                fill = true;
            }
            let (tbp, rbp, bbp, lbp) = style.border_padding.trbl();
            let (x, y) = (left_indent - lbp, -bbp);
            let w = self.width - (left_indent + style.right_indent) + lbp + rbp;
            let h = self.height + tbp + bbp;
            match radius {
                Some(r) => canv.round_rect(x, y, w, h, r, stroke, fill),
                None => canv.rect(x, y, w, h, stroke, fill),
            }
            canv.restore_state()?;
        }
        let n_lines = bl.n_lines();
        if n_lines == 0 {
            return Ok(());
        }
        canv.save_state();
        let alignment = style.alignment;
        let offset = style.first_line_indent;
        let lim = n_lines - 1;
        let no_justify_last = !self.justify_last;
        let jllwc = style.justify_last_line;
        let mut tx;
        match &bl {
            BlPara::Simple { f, lines } => {
                let cur_y = self.height - f.font_size;
                match &f.text_color {
                    Some(c) => canv.set_fill_color(c, None)?,
                    None => return Err(RlError::Value("Unknown color None".into())),
                }
                tx = TextObject::new(canv, cur_x, cur_y);
                tx.set_font(canv, &f.font_name, f.font_size, Some(leading))?;
                for (i, (ws, words)) in lines.iter().enumerate() {
                    let mut last = no_justify_last && i == lim;
                    if last && jllwc != 0 && words.len() as i64 > jllwc {
                        last = false;
                    }
                    let off = if i == 0 { offset } else { 0.0 };
                    draw_line_simple(canv, &mut tx, alignment, off, *ws, words, last)?;
                }
            }
            BlPara::Lines(lines) => {
                let cur_y = self.height - lines[0].font_size;
                tx = TextObject::new(canv, cur_x, cur_y);
                let mut xs = XState {
                    text_color: None,
                    back_color: None,
                    back_color_x: 0.0,
                    rise: 0.0,
                    back_colors: Vec::new(),
                    leading: style.leading,
                    left_indent,
                    cur_y,
                    f_font_size: lines[0].font_size,
                };
                tx.leading = None;
                tx.font_name = None;
                tx.font_size = None;
                for (i, line) in lines.iter().enumerate() {
                    let mut last = no_justify_last && i == lim;
                    if last && jllwc != 0 && line.word_count > jllwc {
                        last = false;
                    }
                    let off = if i == 0 { offset } else { 0.0 };
                    draw_line_x(canv, &mut tx, &mut xs, alignment, off, line, last)?;
                    // _do_post_text
                    if !xs.back_colors.is_empty() {
                        let yl = xs.cur_y + xs.f_font_size;
                        let ydesc = yl - xs.leading;
                        for (x1, x2, c) in std::mem::take(&mut xs.back_colors) {
                            canv.set_fill_color(&c, None)?;
                            canv.rect(x1, ydesc, x2 - x1, xs.leading, false, true);
                        }
                        xs.back_color = None;
                    }
                    xs.cur_y -= xs.leading;
                }
            }
        }
        canv.push_code(tx.get_code());
        canv.restore_state()
    }
}

fn n_text_empty(w: &FragWord) -> bool {
    w.items[0].1.is_empty()
}

fn set_x_pos(tx: &mut TextObject, dx: f64) {
    if dx > 1e-6 || dx < -1e-6 {
        tx.set_x_pos(dx);
    }
}

/// `_leftDrawParaLine` & co. (one-fragment paragraphs).
fn draw_line_simple(
    canv: &mut Canvas,
    tx: &mut TextObject,
    alignment: i64,
    offset: f64,
    extraspace: f64,
    words: &[SWord],
    last: bool,
) -> Result<(), RlError> {
    let text = words.iter().map(|w| w.s.as_str()).collect::<Vec<_>>().join(" ");
    let n_spaces = || words.len() as i64 + nbsp_count(&text) as i64 - 1;
    match alignment {
        TA_LEFT | TA_JUSTIFY => {
            set_x_pos(tx, offset);
            let mut simple = if alignment == TA_LEFT {
                extraspace > -1e-8
            } else {
                (-1e-8 < extraspace && extraspace <= 1e-8) || (last && extraspace > -1e-8)
            };
            let mut ns = 0;
            if !simple {
                ns = n_spaces();
                simple = ns <= 0;
            }
            if simple {
                tx.text_out_raw(canv, &text, true)?;
            } else {
                tx.set_word_space(extraspace / ns as f64);
                tx.text_out_raw(canv, &text, true)?;
                tx.set_word_space(0.0);
            }
            set_x_pos(tx, -offset);
        }
        TA_CENTER | TA_RIGHT => {
            let mut simple = extraspace > -1e-8;
            let mut ns = 0;
            if !simple {
                ns = n_spaces();
                simple = ns <= 0;
            }
            let m;
            if simple {
                m = if alignment == TA_CENTER { offset + 0.5 * extraspace } else { offset + extraspace };
                set_x_pos(tx, m);
                tx.text_out_raw(canv, &text, true)?;
            } else {
                m = offset;
                tx.set_word_space(extraspace / ns as f64);
                set_x_pos(tx, m);
                tx.text_out_raw(canv, &text, true)?;
                tx.set_word_space(0.0);
            }
            set_x_pos(tx, -m);
        }
        _ => return Err(RlError::Value(format!("bad align {alignment}"))),
    }
    Ok(())
}

/// `tx.XtraState` of a multi-fragment paragraph.
struct XState {
    text_color: Option<Color>,
    back_color: Option<Color>,
    back_color_x: f64,
    rise: f64,
    back_colors: Vec<(f64, f64, Color)>,
    leading: f64,
    left_indent: f64,
    cur_y: f64,
    f_font_size: f64,
}

/// `_leftDrawParaLineX` & co.
fn draw_line_x(
    canv: &mut Canvas,
    tx: &mut TextObject,
    xs: &mut XState,
    alignment: i64,
    offset: f64,
    line: &FragLine,
    last: bool,
) -> Result<(), RlError> {
    let extra = line.extra_space;
    let n_spaces = || line.word_count + line.words.iter().map(|w| nbsp_count(w.text()) as i64).sum::<i64>() - 1;
    match alignment {
        TA_LEFT | TA_JUSTIFY => {
            set_x_pos(tx, offset);
            let mut simple = if alignment == TA_LEFT {
                extra > -1e-8
            } else {
                line.line_break || (-1e-8 < extra && extra <= 1e-8) || (last && extra > -1e-8)
            };
            let mut ns = 0;
            if !simple {
                ns = n_spaces();
                simple = ns <= 0;
            }
            if simple {
                put_frag_line(canv, tx, xs, offset, line)?;
            } else {
                tx.set_word_space(extra / ns as f64);
                put_frag_line(canv, tx, xs, offset, line)?;
                tx.set_word_space(0.0);
            }
            set_x_pos(tx, -offset);
        }
        TA_CENTER | TA_RIGHT => {
            let mut simple = extra > -1e-8;
            let mut ns = 0;
            if !simple {
                ns = n_spaces();
                simple = ns <= 0;
            }
            let m;
            if simple {
                m = if alignment == TA_CENTER { offset + 0.5 * extra } else { offset + extra };
                set_x_pos(tx, m);
                put_frag_line(canv, tx, xs, m, line)?;
            } else {
                m = offset;
                tx.set_word_space(extra / ns as f64);
                put_frag_line(canv, tx, xs, m, line)?;
                tx.set_word_space(0.0);
            }
            set_x_pos(tx, -m);
        }
        _ => return Err(RlError::Value(format!("bad align {alignment}"))),
    }
    Ok(())
}

/// `_trailingSpaceLength(text, tx)`.
fn trailing_space_length(text: &str, tx: &TextObject) -> Result<f64, RlError> {
    let t = text.trim_end_matches(is_wsc);
    if t.len() == text.len() {
        return Ok(0.0);
    }
    tx.string_width(&text[t.len()..])
}

/// `_putFragLine` for text fragments.
fn put_frag_line(canv: &mut Canvas, tx: &mut TextObject, xs: &mut XState, cur_x: f64, line: &FragLine) -> Result<(), RlError> {
    let x0 = tx.x0;
    let leading = xs.leading;
    let mut cur_x = cur_x + xs.left_indent;
    if tx.leading != Some(leading) {
        tx.set_leading(leading);
    }
    let ws = tx.word_space;
    let mut n_spaces = 0usize;
    let words = &line.words;
    let nw = words.len();
    for (i, f) in words.iter().enumerate() {
        let cur_x_s = cur_x + n_spaces as f64 * ws;
        let mut end_x = cur_x_s;
        let font_size = f.font_size;
        if i > 0 {
            end_x = cur_x_s - trailing_space_length(words[i - 1].text(), tx)?;
        }
        if tx.font_name.as_deref() != Some(f.font_name.as_str()) || tx.font_size != Some(font_size) {
            tx.set_font_only(canv, &f.font_name, font_size)?;
        }
        if xs.text_color != f.text_color {
            xs.text_color = f.text_color.clone();
            tx.set_fill_color(canv, f.text_color.as_ref())?;
        }
        if xs.rise != f.rise {
            xs.rise = f.rise;
            tx.set_rise(f.rise);
        }
        let text = f.text();
        tx.text_out_raw(canv, text, i == nw - 1)?;
        let bg = f.back_color.clone().flatten();
        match (&bg, &xs.back_color) {
            (Some(b), None) => {
                xs.back_color = Some(b.clone());
                xs.back_color_x = cur_x_s;
            }
            (None, Some(c)) => {
                xs.back_colors.push((xs.back_color_x, end_x, c.clone()));
                xs.back_color = None;
            }
            (Some(b), Some(c)) => {
                if b != c || xs.text_color.as_ref() != Some(c) {
                    xs.back_colors.push((xs.back_color_x, end_x, c.clone()));
                    xs.back_color = Some(b.clone());
                    xs.back_color_x = cur_x_s;
                }
            }
            (None, None) => {}
        }
        cur_x += tx.string_width(text)?;
        n_spaces += text.matches(' ').count() + nbsp_count(text);
    }
    let cur_x_s = cur_x + (n_spaces as f64 - 1.0) * ws;
    if let Some(c) = &xs.back_color {
        xs.back_colors.push((xs.back_color_x, cur_x_s, c.clone()));
    }
    if tx.x0 != x0 {
        set_x_pos(tx, x0 - tx.x0);
    }
    Ok(())
}

/// `_splitWord(w, lineWidth, maxWidths, lineno, fontName, fontSize)`.
fn split_word(w: &str, mut line_width: f64, max_widths: &[f64], mut lineno: usize, font: &str, size: f64) -> Result<Vec<SWord>, RlError> {
    let mut r = Vec::new();
    let maxlineno = max_widths.len() - 1;
    let mut word_text = String::new();
    let mut max_width = max_widths[lineno.min(maxlineno)];
    let max_width_next = max_widths[(lineno + 1).min(maxlineno)];
    for c in w.chars() {
        let cw = sw(&c.to_string(), font, size)?;
        let mut new_line_width = line_width + cw;
        if new_line_width > max_width && (!word_text.is_empty() || cw <= max_width_next) {
            r.push(SWord { s: std::mem::take(&mut word_text), kind: WKind::Split });
            lineno += 1;
            max_width = max_widths[lineno.min(maxlineno)];
            new_line_width = cw;
        }
        word_text.push(c);
        line_width = new_line_width;
    }
    r.push(SWord { s: word_text, kind: WKind::SplitEnd });
    Ok(r)
}

/// `_splitFragWord(w, maxWidth, maxWidths, lineno)`.
fn split_frag_word(w: &FragWord, mut max_width: f64, max_widths: &[f64], mut lineno: usize) -> Result<Vec<FragWord>, RlError> {
    let mut r = Vec::new();
    let maxlineno = max_widths.len() - 1;
    let mut max_width_next = max_widths[(lineno + 1).min(maxlineno)];
    let mut ww: Vec<(Rc<Frag>, String)> = Vec::new();
    let mut line_width = 0.0;
    let mut frag_text = String::new();
    let mut word_width = 0.0;
    let mut f = w.items[0].0.clone();
    // _fragWordIter
    let mut it: Vec<(Rc<Frag>, f64, String)> = Vec::new();
    for (fr, s) in &w.items {
        if s.is_empty() {
            it.push((fr.clone(), 0.0, String::new()));
        } else {
            for c in s.chars() {
                let cs = c.to_string();
                it.push((fr.clone(), sw(&cs, &fr.font_name, fr.font_size)?, cs));
            }
        }
    }
    for (g, cw, c) in it {
        let mut new_line_width = line_width + cw;
        let too_long = new_line_width > max_width && (word_width != 0.0 || cw <= max_width_next);
        if !Rc::ptr_eq(&g, &f) || too_long {
            let mut fc = (*f).clone();
            if fc.text.is_some() {
                fc.text = Some(frag_text.clone());
            }
            ww.push((Rc::new(fc), std::mem::take(&mut frag_text)));
            if too_long {
                r.push(FragWord { w: word_width, kind: FwKind::Split, items: std::mem::take(&mut ww) });
                lineno += 1;
                max_width = max_width_next;
                max_width_next = max_widths[(lineno + 1).min(maxlineno)];
                new_line_width = cw;
                word_width = 0.0;
            }
            f = g;
        }
        word_width += cw;
        frag_text.push_str(&c);
        line_width = new_line_width;
    }
    ww.push((f, frag_text));
    r.push(FragWord { w: word_width, kind: if w.kind.is_hs() { FwKind::SplitHs } else { FwKind::SplitH }, items: ww });
    Ok(r)
}

/// `_shyUnsplit(s, ss)` for plain strings.
fn shy_unsplit(s: &str, ss: &str) -> String {
    format!("{}{}", s.trim_end_matches('-'), ss)
}

/// `_rejoinSplitFragWords(F)`.
fn rejoin_split_frag_words(fs: &[FragWord]) -> FragWord {
    let mut items: Vec<(Rc<Frag>, String)> = Vec::new();
    let mut w_len = 0.0;
    let mut psty: Option<Rc<Frag>> = None;
    for f in fs {
        w_len += f.w;
        for (sty, t) in &f.items {
            if psty.as_ref().is_some_and(|p| Rc::ptr_eq(p, sty)) {
                let l = items.last_mut().unwrap();
                l.1 = shy_unsplit(&l.1, t);
            } else {
                items.push((sty.clone(), t.clone()));
                psty = Some(sty.clone());
            }
        }
    }
    let last = fs.last().unwrap();
    FragWord { w: w_len, kind: if last.kind.is_hs() { FwKind::Hs } else { FwKind::List }, items }
}

/// `_getFragWords(frags)`.
fn get_frag_words(frags: &Frags) -> Result<Vec<FragWord>, RlError> {
    let mut r: Vec<FragWord> = Vec::new();
    match frags {
        Frags::Processed(v) => {
            let mut w: Vec<FragWord> = Vec::new();
            for f in v {
                if f.kind == FwKind::Injected {
                    continue;
                }
                if f.kind.is_split() {
                    w.push(f.clone());
                    if f.kind.is_hs() {
                        r.push(rejoin_split_frag_words(&w));
                        w.clear();
                    }
                } else {
                    if !w.is_empty() {
                        r.push(rejoin_split_frag_words(&w));
                        w.clear();
                    }
                    r.push(f.clone());
                }
            }
            if !w.is_empty() {
                r.push(rejoin_split_frag_words(&w));
            }
        }
        Frags::Parsed(v) => {
            let mut hanging_space = false;
            let mut n = 0.0;
            let mut hanging_strip = true;
            let mut w: Vec<(Rc<Frag>, String)> = Vec::new();
            for f in v {
                let Some(text0) = f.text.as_deref() else {
                    return Err(RlError::Unsupported("paragraph fragment without text".into()));
                };
                if !text0.is_empty() {
                    f.fkind.set(FK_TEXT);
                    let mut text = text0;
                    if hanging_strip {
                        text = lstrip(text);
                        if text.is_empty() {
                            continue;
                        }
                        hanging_strip = false;
                    }
                    let mut s = split(text);
                    if is_ascii_ws(text.chars().next().unwrap()) || s.is_empty() {
                        let whs;
                        if !w.is_empty() {
                            r.push(FragWord { w: n, kind: FwKind::List, items: std::mem::take(&mut w) });
                            whs = hanging_space;
                            hanging_space = false;
                            n = 0.0;
                        } else {
                            whs = r.last().is_some_and(|x| x.kind.is_hs());
                        }
                        if !whs {
                            s.insert(0, String::new());
                        } else if s.is_empty() {
                            continue;
                        }
                    }
                    let last = s.pop().unwrap();
                    for x in s {
                        n += sw(&x, &f.font_name, f.font_size)?;
                        w.push((f.clone(), x));
                        r.push(FragWord { w: n, kind: FwKind::Hs, items: std::mem::take(&mut w) });
                        n = 0.0;
                    }
                    hanging_space = false;
                    n += sw(&last, &f.font_name, f.font_size)?;
                    w.push((f.clone(), last));
                    if is_ascii_ws(text.chars().last().unwrap()) {
                        r.push(FragWord { w: n, kind: FwKind::Hs, items: std::mem::take(&mut w) });
                        n = 0.0;
                    }
                } else if f.line_break {
                    if !w.is_empty() {
                        r.push(FragWord { w: n, kind: FwKind::List, items: std::mem::take(&mut w) });
                        n = 0.0;
                        hanging_space = false;
                    }
                    f.fkind.set(FK_BREAK);
                    r.push(FragWord { w: 0.0, kind: FwKind::List, items: vec![(f.clone(), String::new())] });
                    hanging_strip = true;
                }
            }
            if !w.is_empty() {
                r.push(FragWord { w: n, kind: FwKind::List, items: w });
            }
            let _ = hanging_space;
            if r.is_empty() {
                if let Some(f) = v.first() {
                    f.fkind.set(FK_TEXT);
                    r.push(FragWord { w: 0.0, kind: FwKind::List, items: vec![(f.clone(), String::new())] });
                }
            }
        }
    }
    Ok(r)
}

impl Flowable for Paragraph {
    fn wrap(&mut self, _canv: &Canvas, avail_width: f64, _avail_height: f64) -> Result<(f64, f64), RlError> {
        if avail_width < FUZZ {
            return Ok((0.0, 0x7fffffff as f64));
        }
        self.width = avail_width;
        let s = &self.style;
        let first = avail_width - (s.left_indent + s.first_line_indent) - s.right_indent;
        let later = avail_width - s.left_indent - s.right_indent;
        let bl = self.break_lines(&[first, later])?;
        self.height = bl.n_lines() as f64 * self.style.leading;
        self.bl_para = Some(bl);
        Ok((self.width, self.height))
    }

    fn split(&mut self, canv: &Canvas, avail_width: f64, avail_height: f64) -> Result<Vec<Box<dyn Flowable>>, RlError> {
        if self.n_frags() == 0 || avail_width < FUZZ || avail_height < FUZZ {
            return Ok(Vec::new());
        }
        if self.bl_para.is_none() {
            self.wrap(canv, avail_width, avail_height)?;
        }
        let leading = self.style.leading;
        let mut s = (avail_height / leading) as i64;
        let height = s as f64 * leading;
        let allow_orphans = self.style.allow_orphans;
        if (!allow_orphans && s <= 1) || s == 0 {
            self.bl_para = None;
            return Ok(Vec::new());
        }
        let n = self.bl_para.as_ref().unwrap().n_lines() as i64;
        if n <= s {
            return Ok(vec![Box::new(self.clone())]);
        }
        if !self.style.allow_widows && n == s + 1 {
            if (allow_orphans && n == 3) || n > 3 {
                s -= 1;
            } else {
                self.bl_para = None;
                return Ok(Vec::new());
            }
        }
        let (s, n) = (s as usize, n as usize);
        let mut p1 = Paragraph::from_frags(self.split_frags(0, s)?, self.style.clone());
        p1.justify_last = match self.bl_para.as_ref().unwrap() {
            BlPara::Lines(l) => !l[s - 1].line_break,
            BlPara::Simple { .. } => true,
        };
        p1.height = height;
        p1.width = avail_width;
        let mut st2 = self.style.clone();
        st2.first_line_indent = 0.0;
        let p2 = Paragraph::from_frags(self.split_frags(s, n)?, st2);
        Ok(vec![Box::new(p1), Box::new(p2)])
    }

    fn draw(&mut self, canv: &mut Canvas) -> Result<(), RlError> {
        self.draw_para(canv)
    }

    fn h_align(&self) -> Option<HAlign> {
        None
    }
    fn space_before(&self) -> f64 {
        self.style.space_before
    }
    fn space_after(&self) -> f64 {
        self.style.space_after
    }
    fn keep_with_next(&self) -> bool {
        self.style.keep_with_next.unwrap_or(false)
    }
    fn size(&self) -> Option<(f64, f64)> {
        Some((self.width, self.height))
    }
    /// `identity()`: the `getPlainText(identify=1)`, cut to maxLen characters.
    fn identity(&self, max_len: usize, frame: &str) -> String {
        let mut r = String::new();
        match &self.frags {
            Frags::Parsed(v) if v.is_empty() => r = self.text.clone().unwrap_or_default(),
            Frags::Parsed(v) => v.iter().filter_map(|f| f.text.as_deref()).for_each(|t| r.push_str(t)),
            Frags::Processed(v) if v.is_empty() => r = self.text.clone().unwrap_or_default(),
            Frags::Processed(v) => {
                for w in v {
                    w.items.iter().for_each(|(_, t)| r.push_str(t));
                    if w.kind.is_hs() {
                        r.push(' ');
                    }
                }
            }
        }
        if max_len > 0 {
            r = r.chars().take(max_len).collect();
        }
        format!("<Paragraph at 0x0{frame}>{r}")
    }
    fn class_name(&self) -> &'static str {
        "Paragraph"
    }
    fn min_width(&self) -> Result<f64, RlError> {
        Paragraph::min_width(self)
    }
}
