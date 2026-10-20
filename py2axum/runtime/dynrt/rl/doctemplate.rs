//! reportlab/platypus/doctemplate.py: SimpleDocTemplate (one frame, templates 'First' and 'Later')
//! and the BaseDocTemplate machinery it runs (handle_flowable, page begin/end, postponed flowables).

use std::collections::VecDeque;

use super::canvas::{Canvas, CanvasOpts};
use super::flowables::{FlowKind, Flowable};
use super::frames::Frame;
use super::RlError;

/// `SimpleDocTemplate(filename, **kw)` keyword arguments.
#[derive(Clone)]
pub struct SimpleDocTemplate {
    pub pagesize: (f64, f64),
    pub left_margin: f64,
    pub right_margin: f64,
    pub top_margin: f64,
    pub bottom_margin: f64,
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    /// `keywords` (a list is joined with ", " by the caller, like canvas.setKeywords).
    pub keywords: String,
    pub invariant: bool,
    pub page_compression: Option<bool>,
    pub allow_splitting: bool,
    pub rotation: f64,
    pub initial_font_name: Option<String>,
    pub initial_font_size: Option<f64>,
    pub initial_leading: Option<f64>,
}

impl Default for SimpleDocTemplate {
    fn default() -> Self {
        SimpleDocTemplate {
            pagesize: super::a4(),
            left_margin: super::INCH,
            right_margin: super::INCH,
            top_margin: super::INCH,
            bottom_margin: super::INCH,
            title: None,
            author: None,
            subject: None,
            creator: None,
            producer: None,
            keywords: String::new(),
            invariant: false,
            page_compression: None,
            allow_splitting: true,
            rotation: 0.0,
            initial_font_name: None,
            initial_font_size: None,
            initial_leading: None,
        }
    }
}

/// What a page callback sees of the doc template (`doc` in `onPage(canvas, doc)`).
#[derive(Clone, Debug)]
pub struct DocInfo {
    /// `doc.page` (1 on the first page).
    pub page: i64,
    pub pagesize: (f64, f64),
    pub left_margin: f64,
    pub right_margin: f64,
    pub top_margin: f64,
    pub bottom_margin: f64,
    /// `doc.width`, `doc.height` (the frame's outer size).
    pub width: f64,
    pub height: f64,
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub creator: Option<String>,
}

/// A page callback (`onFirstPage` / `onLaterPages`); its error stops the build and is returned as is.
pub type PageCallback<'a, E> = &'a mut dyn FnMut(&mut Canvas, &DocInfo) -> Result<(), E>;

enum Item {
    PageBegin,
    F { f: Box<dyn Flowable>, postponed: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum Which {
    Story,
    Hanging,
}

const EMPTY_PAGES_ALLOWED: i64 = 10;

struct Build<'a, 'c, E> {
    t: &'a SimpleDocTemplate,
    canv: Canvas,
    frame: Frame,
    story: VecDeque<Item>,
    hanging: VecDeque<Item>,
    /// 0 = 'First', 1 = 'Later'
    template: usize,
    next_template: Option<usize>,
    page: i64,
    cur_page_flowables: i64,
    empty_pages: i64,
    width: f64,
    height: f64,
    on_first: Option<PageCallback<'c, E>>,
    on_later: Option<PageCallback<'c, E>>,
}

impl SimpleDocTemplate {
    /// `(width, height)` of the frame, as `_calc`.
    pub fn calc(&self) -> (f64, f64) {
        let rm = self.pagesize.0 - self.right_margin;
        let tm = self.pagesize.1 - self.top_margin;
        (rm - self.left_margin, tm - self.bottom_margin)
    }

    /// `doc.build(story, onFirstPage=, onLaterPages=)` → the PDF bytes.
    pub fn build<'c, E: From<RlError>>(
        &self,
        story: Vec<Box<dyn Flowable>>,
        on_first: Option<PageCallback<'c, E>>,
        on_later: Option<PageCallback<'c, E>>,
    ) -> Result<Vec<u8>, E> {
        let (width, height) = self.calc();
        let frame = Frame::new(self.left_margin, self.bottom_margin, width, height, Some("normal"));
        let mut canv = Canvas::new(
            self.pagesize,
            &CanvasOpts {
                page_compression: self.page_compression,
                invariant: self.invariant,
                initial_font_name: self.initial_font_name.clone(),
                initial_font_size: self.initial_font_size,
                initial_leading: self.initial_leading,
            },
        )?;
        canv.set_author(self.author.as_deref());
        canv.set_title(self.title.as_deref());
        canv.set_subject(self.subject.as_deref());
        canv.set_creator(self.creator.as_deref());
        canv.set_producer(self.producer.as_deref());
        canv.set_keywords(Some(&self.keywords));
        let mut b = Build {
            t: self,
            canv,
            frame,
            story: story.into_iter().map(|f| Item::F { f, postponed: false }).collect(),
            hanging: VecDeque::from([Item::PageBegin]),
            template: 0,
            next_template: None,
            page: 0,
            cur_page_flowables: 0,
            empty_pages: 0,
            width,
            height,
            on_first,
            on_later,
        };
        while !b.story.is_empty() {
            b.clean_hanging()?;
            b.handle_flowable(Which::Story)?;
        }
        // _endBuild
        if matches!(b.hanging.back(), Some(Item::PageBegin)) {
            b.hanging.pop_back();
            b.clean_hanging()?;
        } else {
            b.clean_hanging()?;
            b.handle_page_end()?;
        }
        Ok(b.canv.get_pdf_data())
    }
}

impl<'a, 'c, E: From<RlError>> Build<'a, 'c, E> {
    fn list(&mut self, w: Which) -> &mut VecDeque<Item> {
        match w {
            Which::Story => &mut self.story,
            Which::Hanging => &mut self.hanging,
        }
    }

    fn info(&self) -> DocInfo {
        let t = self.t;
        DocInfo {
            page: self.page,
            pagesize: t.pagesize,
            left_margin: t.left_margin,
            right_margin: t.right_margin,
            top_margin: t.top_margin,
            bottom_margin: t.bottom_margin,
            width: self.width,
            height: self.height,
            title: t.title.clone(),
            author: t.author.clone(),
            subject: t.subject.clone(),
            creator: t.creator.clone(),
        }
    }

    fn clean_hanging(&mut self) -> Result<(), E> {
        while !self.hanging.is_empty() {
            self.handle_flowable(Which::Hanging)?;
        }
        Ok(())
    }

    /// `SimpleDocTemplate.handle_pageBegin`.
    fn handle_page_begin(&mut self) -> Result<(), E> {
        self.page += 1;
        // checkPageSize: sizes compared as ints
        let cp = (self.canv.pagesize().0 as i64, self.canv.pagesize().1 as i64);
        let sp = (self.t.pagesize.0 as i64, self.t.pagesize.1 as i64);
        if cp != sp {
            self.canv.set_page_size(self.t.pagesize);
        }
        let info = self.info();
        let cb = if self.template == 0 { self.on_first.as_mut() } else { self.on_later.as_mut() };
        if let Some(cb) = cb {
            cb(&mut self.canv, &info)?;
        }
        self.frame.reset();
        self.cur_page_flowables = 0;
        // handle_nextPageTemplate('Later')
        self.next_template = Some(1);
        Ok(())
    }

    fn handle_page_end(&mut self) -> Result<(), E> {
        if self.cur_page_flowables == 0 {
            self.empty_pages += 1;
        } else {
            self.empty_pages = 0;
        }
        if self.empty_pages >= EMPTY_PAGES_ALLOWED {
            return Err(RlError::Layout(format!(
                "More than {EMPTY_PAGES_ALLOWED} pages generated without content - halting layout.  Likely that a flowable is too large for any frame."
            ))
            .into());
        }
        self.canv.set_page_rotation(self.t.rotation);
        self.canv.show_page();
        if let Some(n) = self.next_template.take() {
            self.template = n;
        }
        self.hanging.push_back(Item::PageBegin);
        Ok(())
    }

    /// `handle_flowable(flowables)` on the story or on `_hanging`.
    fn handle_flowable(&mut self, w: Which) -> Result<(), E> {
        let Some(item) = self.list(w).pop_front() else { return Ok(()) };
        let (mut f, postponed) = match item {
            Item::PageBegin => return self.handle_page_begin(),
            Item::F { f, postponed } => (f, postponed),
        };
        if f.keep_with_next() {
            return Err(RlError::Value("keepWithNext is not supported yet".into()).into());
        }
        if f.kind() == FlowKind::PageBreak {
            return self.handle_page_end();
        }
        let allow = self.t.allow_splitting;
        if self.frame.add(f.as_mut(), &mut self.canv, allow)? {
            self.cur_page_flowables += 1;
            return Ok(());
        }
        let s = if allow { self.frame.split(f.as_mut(), &self.canv)? } else { Vec::new() };
        if !s.is_empty() {
            let mut s: VecDeque<Box<dyn Flowable>> = s.into();
            let mut first = s.pop_front().unwrap();
            if first.kind() == FlowKind::PageBreak {
                // flowables[0:0] = S
                let l = self.list(w);
                for x in s.into_iter().rev() {
                    l.push_front(Item::F { f: x, postponed: false });
                }
                l.push_front(Item::F { f: first, postponed: false });
                return Ok(());
            }
            if !self.frame.add(first.as_mut(), &mut self.canv, false)? {
                return Err(RlError::Layout(format!(
                    "Splitting error(n=={}) on page {} in\n<{}>\nS[0]=<{}>",
                    s.len() + 1,
                    self.page,
                    f.class_name(),
                    first.class_name()
                ))
                .into());
            }
            self.cur_page_flowables += 1;
            let l = self.list(w);
            for x in s.into_iter().rev() {
                l.push_front(Item::F { f: x, postponed: false });
            }
            return Ok(());
        }
        if postponed {
            let frame = match self.frame.id.as_deref() {
                Some(id) if !id.is_empty() => format!(" frame={id}"),
                _ => String::new(),
            };
            return Err(RlError::Layout(format!(
                "Flowable {}{} too large on page {} in frame 'normal'{} of template '{}'",
                f.identity(60, &frame),
                f.size_string(),
                self.page,
                self.frame.a_space_string(),
                if self.template == 0 { "First" } else { "Later" }
            ))
            .into());
        }
        self.list(w).push_front(Item::F { f, postponed: true });
        // handle_frameEnd: one frame per page
        self.handle_page_end()
    }
}
