//! The outer structure of the file, as reportlab/pdfbase/pdfdoc.py writes it: objects numbered in the
//! order of their first reference (some during the final formatting pass), sorted dictionaries broken
//! every 6 entries, A85 + Flate page streams, xref, trailer with /ID, Info with PDFDate.

use super::accel::{ascii_base85_encode, escape_pdf, fp1, fp_str};
use super::fontdata::PDFDOC;
use super::fonts::{self, FontId};
use md5::{Digest, Md5};

/// One finished page (canvas.showPage).
pub struct PageRec {
    pub width: f64,
    pub height: f64,
    pub rotate: f64,
    pub stream: String,
    pub compression: bool,
    /// ExtGState resources: (name, key, value) in creation order.
    pub ext_gstate: Vec<(String, &'static str, f64)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Obj {
    BasicFonts,
    Font(usize),
    Page(usize),
    Catalog,
    Info,
    Pages,
    Contents(usize),
}

/// `TimeStamp`: the instant of the document (local time unless invariant or SOURCE_DATE_EPOCH).
pub struct TimeStamp {
    /// `ascii(t)`, fed to the signature.
    pub t_repr: String,
    pub ymdhms: (i32, u32, u32, u32, u32, u32),
    pub dhh: i64,
    pub dmm: i64,
}

impl TimeStamp {
    pub fn new(invariant: bool) -> TimeStamp {
        use chrono::{Datelike, Local, Offset, TimeZone, Timelike, Utc};
        let env = std::env::var("SOURCE_DATE_EPOCH").unwrap_or_default();
        let env = env.trim();
        if invariant || !env.is_empty() {
            let (secs, t_repr) = if env.is_empty() {
                (946684800i64, "946684800.0".to_string())
            } else {
                let v: i64 = env.parse().unwrap_or(0);
                (v, v.to_string())
            };
            let d = Utc.timestamp_opt(secs, 0).single().unwrap_or_default();
            return TimeStamp {
                t_repr,
                ymdhms: (d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second()),
                dhh: 0,
                dmm: 0,
            };
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let t = now.as_secs_f64();
        let d = Local.timestamp_opt(t.floor() as i64, 0).single().unwrap_or_else(|| Local::now());
        let off = d.offset().fix().local_minus_utc() as i64;
        let mut t_repr = format!("{:?}", t);
        if !t_repr.contains('.') && !t_repr.contains('e') {
            t_repr.push_str(".0");
        }
        TimeStamp {
            t_repr,
            ymdhms: (d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second()),
            dhh: (off as f64 / 3600.0) as i64,
            dmm: (off % 3600).rem_euclid(3600) % 60,
        }
    }

    /// `PDFDate.format` text: D:YYYYMMDDhhmmss+HH'MM'.
    pub fn pdf_date(&self) -> String {
        let (y, mo, d, h, mi, s) = self.ymdhms;
        let sign = if self.dhh < 0 { '-' } else { '+' };
        format!("D:{:04}{:02}{:02}{:02}{:02}{:02}{}{:02}'{:02}'", y, mo, d, h, mi, s, sign, self.dhh.abs(), self.dmm)
    }
}

/// Document information (PDFInfo); None-able setters follow PDFDocument.setTitle & co.
pub struct Info {
    pub title: String,
    pub author: String,
    pub subject: String,
    pub creator: String,
    pub producer: String,
    pub keywords: String,
}

pub const DEFAULT_PRODUCER: &str = "ReportLab PDF Library - www.reportlab.com";

impl Default for Info {
    fn default() -> Info {
        Info {
            title: "untitled".into(),
            author: "anonymous".into(),
            subject: "unspecified".into(),
            creator: "ReportLab PDF Library - www.reportlab.com".into(),
            producer: DEFAULT_PRODUCER.into(),
            keywords: String::new(),
        }
    }
}

pub struct PdfDoc {
    objs: Vec<Obj>,
    /// fontMapping in registration order: (font, "F<n>").
    pub fonts: Vec<(FontId, String)>,
    pages: Vec<PageRec>,
    page_num: Vec<usize>,
    catalog: Option<usize>,
    info_num: Option<usize>,
    pages_num: Option<usize>,
    contents_num: Vec<Option<usize>>,
    pub info: Info,
    pub pdf_version: (u8, u8),
    ts: TimeStamp,
    sig: Md5,
}

impl PdfDoc {
    pub fn new(invariant: bool) -> PdfDoc {
        let ts = TimeStamp::new(invariant);
        let mut sig = Md5::new();
        sig.update(b"a reportlab document");
        sig.update(ts.t_repr.as_bytes());
        PdfDoc {
            objs: vec![Obj::BasicFonts],
            fonts: Vec::new(),
            pages: Vec::new(),
            page_num: Vec::new(),
            catalog: None,
            info_num: None,
            pages_num: None,
            contents_num: Vec::new(),
            info: Info::default(),
            pdf_version: (1, 3),
            ts,
            sig,
        }
    }

    fn reference(&mut self, o: Obj) -> usize {
        self.objs.push(o);
        self.objs.len()
    }

    pub fn ensure_transparency(&mut self) {
        self.pdf_version = self.pdf_version.max((1, 4));
    }

    /// `getInternalFontName`: "/F<n>", registering the font object on first use.
    pub fn internal_font_name(&mut self, f: FontId) -> String {
        if let Some((_, n)) = self.fonts.iter().find(|(g, _)| *g == f) {
            return format!("/{n}");
        }
        let name = format!("F{}", self.fonts.len() + 1);
        self.fonts.push((f, name.clone()));
        self.reference(Obj::Font(self.fonts.len() - 1));
        format!("/{name}")
    }

    pub fn add_page(&mut self, p: PageRec) {
        self.pages.push(p);
        self.contents_num.push(None);
        let n = self.reference(Obj::Page(self.pages.len() - 1));
        self.page_num.push(n);
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// `GetPDFData`.
    pub fn get_pdf_data(&mut self) -> Vec<u8> {
        for x in [&self.info.title, &self.info.author, &self.info.subject, &self.info.keywords] {
            self.sig.update(x.as_bytes());
        }
        self.catalog = Some(self.reference(Obj::Catalog));
        self.info_num = Some(self.reference(Obj::Info));
        let mut file: Vec<u8> = format!("%PDF-{}.{}\n", self.pdf_version.0, self.pdf_version.1).into_bytes();
        file.extend_from_slice(b"%\x93\x8c\x8b\x9e ReportLab Generated PDF document http://www.reportlab.com\n");
        let mut offsets = Vec::new();
        let mut counter = 0;
        while counter < self.objs.len() {
            counter += 1;
            let body = self.format_obj(self.objs[counter - 1]);
            offsets.push(file.len());
            file.extend_from_slice(format!("{counter} 0 obj\n").as_bytes());
            file.extend_from_slice(&body);
            if !body.ends_with(b"\n") {
                file.push(b'\n');
            }
            file.extend_from_slice(b"endobj\n");
        }
        let xref_off = file.len();
        let mut x = format!("xref\n0 {}\n0000000000 65535 f ", offsets.len() + 1);
        for o in &offsets {
            x.push_str(&format!("\n{:010} 00000 n ", o));
        }
        x.push('\n');
        file.extend_from_slice(x.as_bytes());
        let digest = self.sig.clone().finalize();
        let hexd: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        let id = format!(
            "\n[<{hexd}><{hexd}>]\n% ReportLab generated PDF document -- digest (http://www.reportlab.com)\n"
        );
        let t = format!(
            "trailer\n<<\n/ID {id}\n/Info {} 0 R\n/Root {} 0 R\n/Size {}\n>>\nstartxref\n{xref_off}\n%%EOF\n",
            self.info_num.unwrap(),
            self.catalog.unwrap(),
            offsets.len() + 1
        );
        file.extend_from_slice(t.as_bytes());
        file
    }

    fn pages_ref(&mut self) -> usize {
        match self.pages_num {
            Some(n) => n,
            None => {
                let n = self.reference(Obj::Pages);
                self.pages_num = Some(n);
                n
            }
        }
    }

    fn format_obj(&mut self, o: Obj) -> Vec<u8> {
        match o {
            Obj::BasicFonts => {
                let mut fonts = Vec::new();
                for (i, (_, name)) in self.fonts.iter().enumerate() {
                    let num = self.objs.iter().position(|x| *x == Obj::Font(i)).unwrap() + 1;
                    fonts.push((name.clone(), format!("{num} 0 R").into_bytes()));
                }
                fmt_dict(fonts)
            }
            Obj::Font(i) => {
                let (f, name) = &self.fonts[i];
                let mut d = vec![
                    ("Type".to_string(), b"/Font".to_vec()),
                    ("Subtype".to_string(), b"/Type1".to_vec()),
                    ("BaseFont".to_string(), pdf_name(fonts::font(*f).name).into_bytes()),
                    ("Name".to_string(), pdf_name(name).into_bytes()),
                ];
                if let Some(e) = fonts::pdf_encoding(*f) {
                    d.push(("Encoding".to_string(), pdf_name(e).into_bytes()));
                }
                fmt_dict(d)
            }
            Obj::Page(i) => {
                let parent = self.pages_ref();
                let contents = self.reference(Obj::Contents(i));
                self.contents_num[i] = Some(contents);
                let p = &self.pages[i];
                let mb = if p.rotate == 90.0 || p.rotate == 270.0 {
                    [0.0, 0.0, p.height, p.width]
                } else {
                    [0.0, 0.0, p.width, p.height]
                };
                let mut res = vec![
                    ("Font".to_string(), b"1 0 R".to_vec()),
                    ("ProcSet".to_string(), b"[ /PDF /Text /ImageB /ImageC /ImageI ]".to_vec()),
                ];
                if !p.ext_gstate.is_empty() {
                    let inner: Vec<(String, Vec<u8>)> = p
                        .ext_gstate
                        .iter()
                        .map(|(n, k, v)| (n.clone(), fmt_dict(vec![(k.to_string(), fp1(*v).into_bytes())])))
                        .collect();
                    res.push(("ExtGState".to_string(), fmt_dict(inner)));
                }
                fmt_dict(vec![
                    ("Type".to_string(), b"/Page".to_vec()),
                    ("Parent".to_string(), format!("{parent} 0 R").into_bytes()),
                    ("MediaBox".to_string(), fmt_array(mb.iter().map(|&v| fp1(v).into_bytes()).collect())),
                    ("Resources".to_string(), fmt_dict(res)),
                    ("Contents".to_string(), format!("{contents} 0 R").into_bytes()),
                    ("Rotate".to_string(), fp1(p.rotate).into_bytes()),
                    ("Trans".to_string(), fmt_dict(vec![])),
                ])
            }
            Obj::Catalog => {
                let pages = self.pages_ref();
                fmt_dict(vec![
                    ("Type".to_string(), b"/Catalog".to_vec()),
                    ("PageMode".to_string(), b"/UseNone".to_vec()),
                    ("Pages".to_string(), format!("{pages} 0 R").into_bytes()),
                ])
            }
            Obj::Info => {
                let i = &self.info;
                let date = pdf_string(&self.ts.pdf_date()).into_bytes();
                fmt_dict(vec![
                    ("Title".to_string(), pdf_string(&i.title).into_bytes()),
                    ("Author".to_string(), pdf_string(&i.author).into_bytes()),
                    ("ModDate".to_string(), date.clone()),
                    ("CreationDate".to_string(), date),
                    ("Producer".to_string(), pdf_string(&i.producer).into_bytes()),
                    ("Creator".to_string(), pdf_string(&i.creator).into_bytes()),
                    ("Subject".to_string(), pdf_string(&i.subject).into_bytes()),
                    ("Keywords".to_string(), pdf_string(&i.keywords).into_bytes()),
                    ("Trapped".to_string(), b"/False".to_vec()),
                ])
            }
            Obj::Pages => {
                let kids: Vec<Vec<u8>> = self.page_num.iter().map(|n| format!("{n} 0 R").into_bytes()).collect();
                fmt_dict(vec![
                    ("Type".to_string(), b"/Pages".to_vec()),
                    ("Kids".to_string(), fmt_array(kids)),
                    ("Count".to_string(), fp1(self.pages.len() as f64).into_bytes()),
                ])
            }
            Obj::Contents(i) => {
                let p = &self.pages[i];
                let (dict, content) = if p.compression {
                    let z = zlib_compress(p.stream.as_bytes());
                    let a = ascii_base85_encode(&z);
                    (
                        vec![
                            ("Filter".to_string(), b"[ /ASCII85Decode /FlateDecode ]".to_vec()),
                            ("Length".to_string(), fp1(a.len() as f64).into_bytes()),
                        ],
                        a.into_bytes(),
                    )
                } else {
                    let c = p.stream.as_bytes().to_vec();
                    (vec![("Length".to_string(), fp1(c.len() as f64).into_bytes())], c)
                };
                let mut out = fmt_dict(dict);
                out.extend_from_slice(b"\nstream\n");
                out.extend_from_slice(&content);
                out.extend_from_slice(b"endstream\n");
                out
            }
        }
    }
}

/// `zlib.compress(data)` (level 6, window 15, memLevel 8).
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    use flate2::{Compress, Compression, FlushCompress, Status};
    let mut c = Compress::new(Compression::new(6), true);
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(64));
        }
        let consumed = c.total_in() as usize;
        match c.compress_vec(&data[consumed..], &mut out, FlushCompress::Finish) {
            Ok(Status::StreamEnd) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    out
}

/// PDFDictionary.format with pdfMultiLine off: sorted keys, a line break every 6 entries.
pub fn fmt_dict(mut entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut l: Vec<Vec<u8>> = entries
        .into_iter()
        .map(|(k, v)| {
            let mut e = pdf_name(&k).into_bytes();
            e.push(b' ');
            e.extend_from_slice(&v);
            e
        })
        .collect();
    let mut i = (l.len().max(1) - 1) / 6 * 6;
    while i >= 6 {
        l.insert(i, b"\n ".to_vec());
        i -= 6;
    }
    let mut out = b"<<\n".to_vec();
    out.extend_from_slice(&l.join(&b' '));
    out.extend_from_slice(b"\n>>");
    out
}

/// PDFArray.format with pdfMultiLine off: a line break every 10 elements beyond 10.
pub fn fmt_array(mut l: Vec<Vec<u8>>) -> Vec<u8> {
    let n = l.len();
    if n > 10 {
        let mut i = (n - 1) / 10 * 10;
        while i >= 10 {
            l.insert(i, b"\n ".to_vec());
            i -= 10;
        }
    }
    let mut out = b"[ ".to_vec();
    out.extend_from_slice(&l.join(&b' '));
    out.extend_from_slice(b" ]");
    out
}

/// `PDFName(data)`.
pub fn pdf_name(data: &str) -> String {
    let mut s = String::from("/");
    for c in data.chars() {
        if c < '\x21' || c > '\x7e' || "%()<>{}[]#".contains(c) {
            s.push_str(&format!("#{:x}", c as u32));
        } else {
            s.push(c);
        }
    }
    s
}

/// `PDFString(s).format` with enc='auto': pdfdoc bytes when possible, else UTF-16BE with a BOM.
pub fn pdf_string(s: &str) -> String {
    let mut b = Vec::with_capacity(s.len());
    let mut ok = true;
    for c in s.chars() {
        match PDFDOC.binary_search_by_key(&(c as u32), |&(u, _)| u) {
            Ok(i) => b.push(PDFDOC[i].1),
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    if !ok {
        b = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            b.extend_from_slice(&u.to_be_bytes());
        }
    }
    format!("({})", escape_pdf(&b))
}

/// Space separated numbers (fp_str) re-exported for the canvas.
pub fn nums(a: &[f64]) -> String {
    fp_str(a)
}
