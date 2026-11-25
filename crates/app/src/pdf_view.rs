//! PDF viewer/editor backed by macOS PDFKit. Pages are rasterized lazily (only the ones on
//! screen, at the current zoom × display scale) on a background thread and evicted when far
//! away, so a big PDF costs little RAM. Tools add real PDF annotations (text, highlight, box,
//! freehand ink, eraser) and page operations (rotate, delete, reorder); ⌘S writes the file.
#![allow(unexpected_cfgs)] // objc 0.2 macros

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gpui::{
    Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point,
    Render, RenderImage, ScrollHandle, ScrollWheelEvent, Task, Window, canvas, div, img, point, prelude::*, px,
};

use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

// ---- PDFKit / CoreGraphics FFI ----------------------------------------------------------------

mod ffi {
    #![allow(non_upper_case_globals, non_snake_case)]
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::c_void;

    pub type Id = *mut Object;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CGSize {
        pub width: f64,
        pub height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CGRect {
        pub origin: CGPoint,
        pub size: CGSize,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(cs: *mut c_void);
        fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bpc: usize, bpr: usize, cs: *mut c_void, info: u32) -> *mut c_void;
        fn CGContextRelease(c: *mut c_void);
        fn CGContextScaleCTM(c: *mut c_void, sx: f64, sy: f64);
        fn CGContextSetRGBFillColor(c: *mut c_void, r: f64, g: f64, b: f64, a: f64);
        fn CGContextFillRect(c: *mut c_void, rect: CGRect);
    }

    #[link(name = "PDFKit", kind = "framework")]
    unsafe extern "C" {
        static PDFAnnotationSubtypeFreeText: Id;
        static PDFAnnotationSubtypeHighlight: Id;
        static PDFAnnotationSubtypeInk: Id;
        static PDFAnnotationSubtypeSquare: Id;
    }

    /// kPDFDisplayBoxCropBox: what viewers show.
    const CROP_BOX: i64 = 1;

    pub fn nsstring(s: &str) -> Id {
        let c = std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    fn file_url(path: &std::path::Path) -> Id {
        unsafe { msg_send![class!(NSURL), fileURLWithPath: nsstring(&path.to_string_lossy())] }
    }

    fn color(rgba: (f64, f64, f64, f64)) -> Id {
        unsafe { msg_send![class!(NSColor), colorWithSRGBRed: rgba.0 green: rgba.1 blue: rgba.2 alpha: rgba.3] }
    }

    /// Retained PDFDocument. PDFKit objects are only touched while holding the owning Mutex.
    pub struct Doc(Id);
    unsafe impl Send for Doc {}

    impl Drop for Doc {
        fn drop(&mut self) {
            unsafe {
                let _: () = msg_send![self.0, release];
            }
        }
    }

    /// Retained object kept for undo (a removed page or annotation).
    pub struct Retained(pub Id);
    unsafe impl Send for Retained {}
    impl Drop for Retained {
        fn drop(&mut self) {
            unsafe {
                let _: () = msg_send![self.0, release];
            }
        }
    }
    impl Retained {
        pub fn new(o: Id) -> Self {
            unsafe {
                let _: Id = msg_send![o, retain];
            }
            Self(o)
        }
    }

    pub fn pool<T>(f: impl FnOnce() -> T) -> T {
        objc::rc::autoreleasepool(f)
    }

    impl Doc {
        pub fn open(path: &std::path::Path) -> Option<Doc> {
            pool(|| unsafe {
                let d: Id = msg_send![class!(PDFDocument), alloc];
                let d: Id = msg_send![d, initWithURL: file_url(path)];
                (!d.is_null()).then(|| Doc(d))
            })
        }

        pub fn locked(&self) -> bool {
            unsafe {
                let b: bool = msg_send![self.0, isLocked];
                b
            }
        }

        pub fn count(&self) -> usize {
            unsafe {
                let n: usize = msg_send![self.0, pageCount];
                n
            }
        }

        pub fn page(&self, i: usize) -> Id {
            unsafe { msg_send![self.0, pageAtIndex: i] }
        }

        pub fn rotation(&self, i: usize) -> i64 {
            unsafe {
                let r: i64 = msg_send![self.page(i), rotation];
                r.rem_euclid(360)
            }
        }

        pub fn set_rotation(&self, i: usize, r: i64) {
            unsafe {
                let _: () = msg_send![self.page(i), setRotation: r.rem_euclid(360)];
            }
        }

        pub fn crop(&self, i: usize) -> CGRect {
            unsafe {
                let r: CGRect = msg_send![self.page(i), boundsForBox: CROP_BOX];
                r
            }
        }

        /// Displayed size (rotation applied), in points.
        pub fn display_size(&self, i: usize) -> (f64, f64) {
            let b = self.crop(i);
            if self.rotation(i) % 180 == 90 { (b.size.height, b.size.width) } else { (b.size.width, b.size.height) }
        }

        /// Rasterize page `i` at `scale` px per point: (w, h, BGRA premultiplied).
        pub fn render(&self, i: usize, scale: f64) -> Option<(u32, u32, Vec<u8>)> {
            pool(|| unsafe {
                let (pw, ph) = self.display_size(i);
                let (w, h) = ((pw * scale).round().max(1.) as usize, (ph * scale).round().max(1.) as usize);
                if w * h > 64_000_000 {
                    return None;
                }
                let mut buf = vec![0u8; w * h * 4];
                let cs = CGColorSpaceCreateDeviceRGB();
                // kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little → BGRA in memory.
                let ctx = CGBitmapContextCreate(buf.as_mut_ptr() as *mut c_void, w, h, 8, w * 4, cs, 2 | (2 << 12));
                CGColorSpaceRelease(cs);
                if ctx.is_null() {
                    return None;
                }
                CGContextSetRGBFillColor(ctx, 1., 1., 1., 1.);
                CGContextFillRect(ctx, CGRect { origin: CGPoint::default(), size: CGSize { width: w as f64, height: h as f64 } });
                CGContextScaleCTM(ctx, scale, scale);
                let page = self.page(i);
                let _: () = msg_send![page, drawWithBox: CROP_BOX toContext: ctx];
                CGContextRelease(ctx);
                Some((w as u32, h as u32, buf))
            })
        }

        /// View point (points from the displayed page's top-left) → page space.
        pub fn to_page(&self, i: usize, x: f64, y: f64) -> CGPoint {
            let b = self.crop(i);
            let (w, h) = (b.size.width, b.size.height);
            let (dw, dh) = self.display_size(i);
            let (dx, dy) = (x.clamp(0., dw), dh - y.clamp(0., dh));
            let (u, v) = match self.rotation(i) {
                90 => (w - dy, dx),
                180 => (w - dx, h - dy),
                270 => (dy, h - dx),
                _ => (dx, dy),
            };
            CGPoint { x: b.origin.x + u, y: b.origin.y + v }
        }

        fn rect_from(&self, i: usize, a: (f64, f64), b: (f64, f64)) -> CGRect {
            let p = self.to_page(i, a.0, a.1);
            let q = self.to_page(i, b.0, b.1);
            CGRect { origin: CGPoint { x: p.x.min(q.x), y: p.y.min(q.y) }, size: CGSize { width: (p.x - q.x).abs(), height: (p.y - q.y).abs() } }
        }

        fn add(&self, i: usize, annot: Id) -> Retained {
            unsafe {
                let _: () = msg_send![self.page(i), addAnnotation: annot];
            }
            let r = Retained::new(annot);
            unsafe {
                let _: () = msg_send![annot, release];
            }
            r
        }

        fn new_annot(rect: CGRect, kind: Id) -> Id {
            unsafe {
                let a: Id = msg_send![class!(PDFAnnotation), alloc];
                let nil: Id = std::ptr::null_mut();
                msg_send![a, initWithBounds: rect forType: kind withProperties: nil]
            }
        }

        fn border(annot: Id, width: f64) {
            unsafe {
                let b: Id = msg_send![class!(PDFBorder), new];
                let _: () = msg_send![b, setLineWidth: width];
                let _: () = msg_send![annot, setBorder: b];
                let _: () = msg_send![b, release];
            }
        }

        pub fn add_text(&self, i: usize, at: (f64, f64), text: &str, size: f64, rgba: (f64, f64, f64, f64)) -> Retained {
            pool(|| unsafe {
                let lines: Vec<&str> = text.lines().collect();
                let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f64;
                let (w, h) = (longest * size * 0.58 + size, lines.len().max(1) as f64 * size * 1.25 + size * 0.5);
                let p = self.to_page(i, at.0, at.1);
                // The click is the text's top-left; PDF rects grow upward.
                let rect = match self.rotation(i) {
                    0 => CGRect { origin: CGPoint { x: p.x, y: p.y - h }, size: CGSize { width: w, height: h } },
                    _ => self.rect_from(i, at, (at.0 + w, at.1 + h)),
                };
                let a = Self::new_annot(rect, PDFAnnotationSubtypeFreeText);
                let _: () = msg_send![a, setContents: nsstring(text)];
                let font: Id = msg_send![class!(NSFont), systemFontOfSize: size];
                let _: () = msg_send![a, setFont: font];
                let _: () = msg_send![a, setFontColor: color(rgba)];
                let clear: Id = msg_send![class!(NSColor), clearColor];
                let _: () = msg_send![a, setColor: clear];
                Self::border(a, 0.);
                self.add(i, a)
            })
        }

        pub fn add_box(&self, i: usize, a: (f64, f64), b: (f64, f64), highlight: bool, rgba: (f64, f64, f64, f64)) -> Retained {
            pool(|| unsafe {
                let rect = self.rect_from(i, a, b);
                if highlight {
                    let an = Self::new_annot(rect, PDFAnnotationSubtypeHighlight);
                    let (w, h) = (rect.size.width, rect.size.height);
                    let pts: Vec<Id> = [(0., h), (w, h), (0., 0.), (w, 0.)]
                        .iter()
                        .map(|&(x, y)| msg_send![class!(NSValue), valueWithPoint: CGPoint { x, y }])
                        .collect();
                    let arr: Id = msg_send![class!(NSArray), arrayWithObjects: pts.as_ptr() count: pts.len()];
                    let _: () = msg_send![an, setQuadrilateralPoints: arr];
                    let _: () = msg_send![an, setColor: color(rgba)];
                    self.add(i, an)
                } else {
                    let an = Self::new_annot(rect, PDFAnnotationSubtypeSquare);
                    let _: () = msg_send![an, setColor: color(rgba)];
                    Self::border(an, 2.);
                    self.add(i, an)
                }
            })
        }

        pub fn add_ink(&self, i: usize, pts: &[(f64, f64)], rgba: (f64, f64, f64, f64)) -> Option<Retained> {
            if pts.len() < 2 {
                return None;
            }
            pool(|| unsafe {
                let page_pts: Vec<CGPoint> = pts.iter().map(|p| self.to_page(i, p.0, p.1)).collect();
                let pad = 3.;
                let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for p in &page_pts {
                    x0 = x0.min(p.x);
                    y0 = y0.min(p.y);
                    x1 = x1.max(p.x);
                    y1 = y1.max(p.y);
                }
                let rect = CGRect { origin: CGPoint { x: x0 - pad, y: y0 - pad }, size: CGSize { width: x1 - x0 + 2. * pad, height: y1 - y0 + 2. * pad } };
                let an = Self::new_annot(rect, PDFAnnotationSubtypeInk);
                let path: Id = msg_send![class!(NSBezierPath), bezierPath];
                for (k, p) in page_pts.iter().enumerate() {
                    // Ink paths are relative to the annotation's origin.
                    let q = CGPoint { x: p.x - rect.origin.x, y: p.y - rect.origin.y };
                    if k == 0 {
                        let _: () = msg_send![path, moveToPoint: q];
                    } else {
                        let _: () = msg_send![path, lineToPoint: q];
                    }
                }
                let _: () = msg_send![an, addBezierPath: path];
                let _: () = msg_send![an, setColor: color(rgba)];
                Self::border(an, 2.);
                Some(self.add(i, an))
            })
        }

        pub fn annotation_at(&self, i: usize, at: (f64, f64)) -> Option<Retained> {
            pool(|| unsafe {
                let p = self.to_page(i, at.0, at.1);
                let a: Id = msg_send![self.page(i), annotationAtPoint: p];
                (!a.is_null()).then(|| Retained::new(a))
            })
        }

        pub fn remove_annotation(&self, i: usize, a: &Retained) {
            unsafe {
                let _: () = msg_send![self.page(i), removeAnnotation: a.0];
            }
        }

        pub fn add_annotation(&self, i: usize, a: &Retained) {
            unsafe {
                let _: () = msg_send![self.page(i), addAnnotation: a.0];
            }
        }

        pub fn remove_page(&self, i: usize) -> Retained {
            let p = Retained::new(self.page(i));
            unsafe {
                let _: () = msg_send![self.0, removePageAtIndex: i];
            }
            p
        }

        pub fn insert_page(&self, p: &Retained, i: usize) {
            unsafe {
                let _: () = msg_send![self.0, insertPage: p.0 atIndex: i];
            }
        }

        pub fn swap(&self, a: usize, b: usize) {
            unsafe {
                let _: () = msg_send![self.0, exchangePageAtIndex: a withPageAtIndex: b];
            }
        }

        /// Page space → displayed page points (top-left origin, rotation applied).
        pub fn from_page(&self, i: usize, x: f64, y: f64) -> (f64, f64) {
            let b = self.crop(i);
            let (w, h) = (b.size.width, b.size.height);
            let (dw, dh) = self.display_size(i);
            let (u, v) = (x - b.origin.x, y - b.origin.y);
            let (dx, dy) = match self.rotation(i) {
                90 => (v, w - u),
                180 => (w - u, h - v),
                270 => (h - v, u),
                _ => (u, v),
            };
            let _ = dw;
            (dx, dh - dy)
        }

        /// Case-insensitive text search: (page, rect in displayed points x, y, w, h).
        pub fn find(&self, q: &str) -> Vec<(usize, (f64, f64, f64, f64))> {
            pool(|| unsafe {
                let mut out = Vec::new();
                let sels: Id = msg_send![self.0, findString: nsstring(q) withOptions: 1usize];
                if sels.is_null() {
                    return out;
                }
                let n: usize = msg_send![sels, count];
                for k in 0..n.min(5000) {
                    let sel: Id = msg_send![sels, objectAtIndex: k];
                    let pages: Id = msg_send![sel, pages];
                    let pc: usize = msg_send![pages, count];
                    for j in 0..pc {
                        let page: Id = msg_send![pages, objectAtIndex: j];
                        let i: usize = msg_send![self.0, indexForPage: page];
                        let r: CGRect = msg_send![sel, boundsForPage: page];
                        let a = self.from_page(i, r.origin.x, r.origin.y);
                        let b = self.from_page(i, r.origin.x + r.size.width, r.origin.y + r.size.height);
                        out.push((i, (a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())));
                    }
                }
                out
            })
        }

        pub fn write(&self, path: &std::path::Path) -> bool {
            pool(|| unsafe {
                let ok: bool = msg_send![self.0, writeToURL: file_url(path)];
                ok
            })
        }
    }
}

use ffi::{Doc, Retained};

/// Top of page 1 rendered `width` px wide and cropped to `aspect` (h / w), saved as JPEG at
/// `dest`. For chat previews; call off the main thread.
pub fn first_page_jpeg(src: &std::path::Path, dest: &std::path::Path, width: u32, aspect: f32) -> Option<()> {
    let doc = Doc::open(src)?;
    if doc.locked() || doc.count() == 0 {
        return None;
    }
    let (pw, _) = doc.display_size(0);
    let (w, h, bgra) = doc.render(0, width as f64 / pw.max(1.))?;
    let crop_h = h.min((w as f32 * aspect).round() as u32).max(1);
    let mut rgb = image::RgbImage::new(w, crop_h);
    for (x, y, p) in rgb.enumerate_pixels_mut() {
        let i = ((y * w + x) * 4) as usize;
        *p = image::Rgb([bgra[i + 2], bgra[i + 1], bgra[i]]);
    }
    std::fs::create_dir_all(dest.parent()?).ok()?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(dest).ok()?);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&rgb).ok()
}

// ---- view -------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Hand,
    Text,
    Highlight,
    Rect,
    Draw,
    Erase,
}

enum Undo {
    Added(usize, Retained),
    Removed(usize, Retained),
    Rotated(usize, i64),
    DeletedPage(usize, Retained),
    Swapped(usize, usize),
}

const GAP: f32 = 14.;
const COLORS: [(&str, (f64, f64, f64, f64)); 5] =
    [("Black", (0.1, 0.1, 0.1, 1.)), ("Red", (0.86, 0.16, 0.16, 1.)), ("Blue", (0.1, 0.36, 0.9, 1.)), ("Green", (0.1, 0.6, 0.3, 1.)), ("Yellow", (1., 0.85, 0.1, 1.))];

pub struct PdfView {
    pub path: PathBuf,
    doc: Option<Arc<Mutex<Doc>>>,
    error: Option<String>,
    /// Displayed page sizes in points.
    sizes: Vec<(f32, f32)>,
    /// Points → pixels. `None` = fit width.
    zoom: Option<f32>,
    images: HashMap<usize, (f32, Arc<RenderImage>)>,
    rendering: HashSet<usize>,
    /// Pages whose cached image is stale (re-render but keep showing the old one).
    stale: HashSet<usize>,
    page_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    scroll: ScrollHandle,
    tool: Tool,
    color: usize,
    font_size: f32,
    /// Drag in progress: (page, start, current) in page points (display space).
    drag: Option<(usize, (f64, f64), (f64, f64))>,
    ink: Vec<(f64, f64)>,
    text: Option<(usize, (f64, f64), Entity<TextInput>)>,
    undo: Vec<Undo>,
    pub dirty: bool,
    current: usize,
    focus: FocusHandle,
    _tasks: Vec<Task<()>>,
    status: Option<String>,
    /// Search hits (page, displayed rect) and the current one.
    hits: Vec<(usize, (f64, f64, f64, f64))>,
    hit: usize,
}

impl PdfView {
    pub fn new(path: PathBuf, focus: FocusHandle, _cx: &mut Context<Self>) -> Self {
        let doc = Doc::open(&path);
        let (error, sizes) = match &doc {
            None => (Some("Couldn't open this PDF.".to_string()), Vec::new()),
            Some(d) if d.locked() => (Some("This PDF is password-protected.".to_string()), Vec::new()),
            Some(d) => (None, (0..d.count()).map(|i| d.display_size(i)).map(|(w, h)| (w as f32, h as f32)).collect()),
        };
        Self {
            path,
            doc: doc.map(|d| Arc::new(Mutex::new(d))),
            error,
            sizes,
            zoom: None,
            images: HashMap::new(),
            rendering: HashSet::new(),
            stale: HashSet::new(),
            page_bounds: Rc::new(RefCell::new(HashMap::new())),
            scroll: ScrollHandle::new(),
            tool: Tool::Hand,
            color: 1,
            font_size: 14.,
            drag: None,
            ink: Vec::new(),
            text: None,
            undo: Vec::new(),
            dirty: false,
            current: 0,
            focus,
            _tasks: Vec::new(),
            status: None,
            hits: Vec::new(),
            hit: 0,
        }
    }

    /// ⌘F: search (same query → next/previous hit). Returns "n of m".
    pub fn find(&mut self, q: &str, back: bool, fresh: bool, cx: &mut Context<Self>) -> String {
        if q.trim().is_empty() {
            self.hits.clear();
            cx.notify();
            return String::new();
        }
        if fresh {
            self.hits = self.with_doc(|d| d.find(q)).unwrap_or_default();
            self.hit = self.hits.iter().position(|h| h.0 >= self.current).unwrap_or(0);
        } else if !self.hits.is_empty() {
            let n = self.hits.len();
            self.hit = if back { (self.hit + n - 1) % n } else { (self.hit + 1) % n };
        }
        let Some(&(page, r)) = self.hits.get(self.hit) else {
            cx.notify();
            return "No results".into();
        };
        self.current = page;
        let scale = self.scale();
        let y: f32 = GAP + self.sizes[..page].iter().map(|s| s.1 * scale + GAP).sum::<f32>() + r.1 as f32 * scale;
        let view_h = f32::from(self.scroll.bounds().size.height);
        let off = self.scroll.offset();
        self.scroll.set_offset(point(off.x, px(-(y - view_h / 3.).max(0.))));
        cx.notify();
        format!("{} of {}", self.hit + 1, self.hits.len())
    }

    pub fn clear_find(&mut self, cx: &mut Context<Self>) {
        self.hits.clear();
        cx.notify();
    }

    fn with_doc<T>(&self, f: impl FnOnce(&Doc) -> T) -> Option<T> {
        let d = self.doc.as_ref()?;
        let g = d.lock().ok()?;
        Some(f(&g))
    }

    fn scale(&self) -> f32 {
        if let Some(z) = self.zoom {
            return z;
        }
        let vw = f32::from(self.scroll.bounds().size.width);
        let maxw = self.sizes.iter().map(|s| s.0).fold(0., f32::max);
        if vw <= 0. || maxw <= 0. {
            return 1.;
        }
        ((vw - 2. * GAP - 14.) / maxw).clamp(0.1, 3.)
    }

    fn set_zoom(&mut self, z: Option<f32>, cx: &mut Context<Self>) {
        self.zoom = z.map(|z| z.clamp(0.1, 6.));
        cx.notify();
    }

    fn refresh_sizes(&mut self) {
        if let Some(s) = self.with_doc(|d| (0..d.count()).map(|i| d.display_size(i)).map(|(w, h)| (w as f32, h as f32)).collect::<Vec<_>>()) {
            self.sizes = s;
        }
    }

    /// Visible page range from the scroll offset (cheap layout math).
    fn visible(&self, scale: f32) -> std::ops::Range<usize> {
        let b = self.scroll.bounds();
        let top = -f32::from(self.scroll.offset().y);
        let bottom = top + f32::from(b.size.height).max(400.);
        let mut y = GAP;
        let (mut first, mut last) = (usize::MAX, 0);
        for (i, (_, h)) in self.sizes.iter().enumerate() {
            let ph = h * scale;
            if y + ph >= top && y <= bottom {
                first = first.min(i);
                last = i;
            }
            y += ph + GAP;
        }
        if first == usize::MAX { 0..self.sizes.len().min(1) } else { first..last + 1 }
    }

    fn ensure_rendered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.clone() else { return };
        let scale = self.scale();
        let px_scale = scale * window.scale_factor();
        let vis = self.visible(scale);
        let want = vis.start.saturating_sub(1)..(vis.end + 1).min(self.sizes.len());
        // Evict far pages (frees the bitmap and its GPU texture).
        let keep = vis.start.saturating_sub(4)..vis.end + 4;
        let drop: Vec<usize> = self.images.keys().copied().filter(|i| !keep.contains(i)).collect();
        for i in drop {
            if let Some((_, im)) = self.images.remove(&i) {
                let _ = window.drop_image(im);
            }
        }
        for i in want {
            let fresh = self.images.get(&i).is_some_and(|(s, _)| (s / px_scale - 1.).abs() < 0.15) && !self.stale.contains(&i);
            if fresh || self.rendering.contains(&i) {
                continue;
            }
            self.rendering.insert(i);
            let doc = doc.clone();
            let task = cx.spawn(async move |this, cx| {
                let out = cx
                    .background_executor()
                    .spawn(async move {
                        let g = doc.lock().ok()?;
                        if i >= g.count() {
                            return None;
                        }
                        g.render(i, px_scale as f64)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.rendering.remove(&i);
                    this.stale.remove(&i);
                    if let Some((w, h, buf)) = out {
                        if let Some(rgba) = image::RgbaImage::from_raw(w, h, buf) {
                            let im = Arc::new(RenderImage::new([image::Frame::new(rgba)]));
                            if let Some((_, old)) = this.images.insert(i, (px_scale, im)) {
                                cx.drop_image(old, None);
                            }
                        }
                    }
                    cx.notify();
                });
            });
            self._tasks.retain(|_| true);
            self._tasks.push(task);
            if self._tasks.len() > 32 {
                self._tasks.drain(..16);
            }
        }
    }

    fn invalidate(&mut self, page: usize, cx: &mut Context<Self>) {
        self.stale.insert(page);
        self.dirty = true;
        cx.notify();
    }

    fn invalidate_all(&mut self, cx: &mut Context<Self>) {
        self.refresh_sizes();
        self.images.clear();
        self.stale.clear();
        self.dirty = true;
        self.current = self.current.min(self.sizes.len().saturating_sub(1));
        cx.notify();
    }

    fn rgba(&self) -> (f64, f64, f64, f64) {
        COLORS[self.color].1
    }

    /// Window position → (page, point in displayed page points).
    fn hit(&self, pos: Point<Pixels>) -> Option<(usize, (f64, f64))> {
        let scale = self.scale() as f64;
        let b = self.page_bounds.borrow();
        b.iter().find(|(_, r)| r.contains(&pos)).map(|(i, r)| (*i, (f64::from(pos.x - r.origin.x) / scale, f64::from(pos.y - r.origin.y) / scale)))
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((page, p)) = self.hit(e.position) else { return };
        self.current = page;
        if self.text.is_some() {
            self.commit_text(window, cx);
        }
        match self.tool {
            Tool::Hand => {}
            Tool::Text => {
                let input = cx.new(|cx| TextInput::new("Type, Enter to place", cx));
                cx.subscribe_in(&input, window, |this, _, ev: &TextInputEvent, window, cx| match ev {
                    TextInputEvent::Submit { .. } => this.commit_text(window, cx),
                    TextInputEvent::Cancel => {
                        this.text = None;
                        window.focus(&this.focus);
                        cx.notify();
                    }
                    TextInputEvent::Changed => {}
                })
                .detach();
                window.focus(&input.focus_handle(cx));
                self.text = Some((page, p, input));
            }
            Tool::Erase => {
                let removed = self.with_doc(|d| {
                    let a = d.annotation_at(page, p)?;
                    d.remove_annotation(page, &a);
                    Some(a)
                });
                if let Some(Some(a)) = removed {
                    self.undo.push(Undo::Removed(page, a));
                    self.invalidate(page, cx);
                }
            }
            Tool::Draw => {
                self.ink = vec![p];
                self.drag = Some((page, p, p));
            }
            Tool::Highlight | Tool::Rect => self.drag = Some((page, p, p)),
        }
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((page, start, _)) = self.drag else { return };
        if e.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(r) = self.page_bounds.borrow().get(&page).copied() else { return };
        let scale = self.scale() as f64;
        let (w, h) = self.sizes[page];
        let p = (
            (f64::from(e.position.x - r.origin.x) / scale).clamp(0., w as f64),
            (f64::from(e.position.y - r.origin.y) / scale).clamp(0., h as f64),
        );
        self.drag = Some((page, start, p));
        if self.tool == Tool::Draw {
            if self.ink.last().is_none_or(|l| (l.0 - p.0).abs() + (l.1 - p.1).abs() > 0.8) {
                self.ink.push(p);
            }
        }
        cx.notify();
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((page, a, b)) = self.drag.take() else { return };
        let rgba = self.rgba();
        let added = match self.tool {
            Tool::Draw => {
                let pts = std::mem::take(&mut self.ink);
                self.with_doc(|d| d.add_ink(page, &pts, rgba)).flatten()
            }
            Tool::Highlight | Tool::Rect if (a.0 - b.0).abs() > 2. && (a.1 - b.1).abs() > 2. => {
                let hl = self.tool == Tool::Highlight;
                let c = if hl { (rgba.0, rgba.1, rgba.2, 0.45) } else { rgba };
                self.with_doc(|d| d.add_box(page, a, b, hl, c))
            }
            _ => None,
        };
        if let Some(r) = added {
            self.undo.push(Undo::Added(page, r));
            self.invalidate(page, cx);
        }
        cx.notify();
    }

    fn commit_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((page, at, input)) = self.text.take() else { return };
        let text = input.read(cx).content.clone();
        window.focus(&self.focus);
        if text.trim().is_empty() {
            cx.notify();
            return;
        }
        let (size, rgba) = (self.font_size as f64, self.rgba());
        if let Some(r) = self.with_doc(|d| d.add_text(page, at, &text, size, rgba)) {
            self.undo.push(Undo::Added(page, r));
            self.invalidate(page, cx);
        }
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(u) = self.undo.pop() else { return };
        let all = matches!(u, Undo::Rotated(..) | Undo::DeletedPage(..) | Undo::Swapped(..));
        let page = self.with_doc(|d| match &u {
            Undo::Added(p, a) => {
                d.remove_annotation(*p, a);
                *p
            }
            Undo::Removed(p, a) => {
                d.add_annotation(*p, a);
                *p
            }
            Undo::Rotated(p, r) => {
                d.set_rotation(*p, *r);
                *p
            }
            Undo::DeletedPage(i, pg) => {
                d.insert_page(pg, *i);
                *i
            }
            Undo::Swapped(a, b) => {
                d.swap(*a, *b);
                *a
            }
        });
        if let Some(p) = page {
            self.current = p;
            if all { self.invalidate_all(cx) } else { self.invalidate(p, cx) }
        }
    }

    fn rotate(&mut self, cx: &mut Context<Self>) {
        let i = self.current;
        if let Some(old) = self.with_doc(|d| {
            let r = d.rotation(i);
            d.set_rotation(i, r + 90);
            r
        }) {
            self.undo.push(Undo::Rotated(i, old));
            self.invalidate_all(cx);
        }
    }

    fn delete_page(&mut self, cx: &mut Context<Self>) {
        if self.sizes.len() <= 1 {
            self.status = Some("A PDF needs at least one page.".into());
            cx.notify();
            return;
        }
        let i = self.current;
        if let Some(p) = self.with_doc(|d| d.remove_page(i)) {
            self.undo.push(Undo::DeletedPage(i, p));
            self.invalidate_all(cx);
        }
    }

    fn move_page(&mut self, down: bool, cx: &mut Context<Self>) {
        let i = self.current;
        let j = if down { i + 1 } else { i.wrapping_sub(1) };
        if j >= self.sizes.len() {
            return;
        }
        self.with_doc(|d| d.swap(i, j));
        self.undo.push(Undo::Swapped(i, j));
        self.current = j;
        self.invalidate_all(cx);
    }

    pub fn save(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) -> Result<(), String> {
        if let Some(w) = window {
            self.commit_text(w, cx);
        }
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = self.path.with_file_name(format!(".{name}.jt-save"));
        let ok = self.with_doc(|d| d.write(&tmp)).unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("Couldn't write {}", self.path.display()));
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        self.dirty = false;
        self.status = Some("Saved".into());
        cx.notify();
        Ok(())
    }

    fn tool_btn(&self, id: &'static str, label: &'static str, tip: &'static str, t: Tool, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.tool == t;
        btn(id, label, tip)
            .when(on, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.commit_text(window, cx);
                this.tool = t;
                cx.notify();
            }))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sep = || div().w(px(1.)).h(px(16.)).mx(px(4.)).bg(theme::rgb(theme::BORDER));
        let pct = format!("{:.0}%", self.scale() * 100.);
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(2.))
            .px(px(10.))
            .py(px(4.))
            .flex_none()
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .child(self.tool_btn("pdf-hand", "Select", "Scroll / select page", Tool::Hand, cx))
            .child(self.tool_btn("pdf-text", "Text", "Click to add text", Tool::Text, cx))
            .child(self.tool_btn("pdf-hl", "Highlight", "Drag to highlight", Tool::Highlight, cx))
            .child(self.tool_btn("pdf-rect", "Box", "Drag to draw a box", Tool::Rect, cx))
            .child(self.tool_btn("pdf-draw", "Draw", "Freehand / signature", Tool::Draw, cx))
            .child(self.tool_btn("pdf-erase", "Erase", "Click an annotation to remove it", Tool::Erase, cx))
            .child(sep())
            .children(COLORS.iter().enumerate().map(|(i, (name, c))| {
                let on = self.color == i;
                div()
                    .id(("pdf-color", i))
                    .size(px(16.))
                    .rounded_full()
                    .cursor_pointer()
                    .border_2()
                    .border_color(if on { theme::rgb(theme::TEXT) } else { theme::rgb(theme::BORDER) })
                    .bg(gpui::rgba(((c.0 * 255.) as u32) << 24 | ((c.1 * 255.) as u32) << 16 | ((c.2 * 255.) as u32) << 8 | 0xff))
                    .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(name)).into())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.color = i;
                        cx.notify();
                    }))
            }))
            .child(btn("pdf-fs-dn", "A−", "Smaller text").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.font_size = (this.font_size - 2.).max(6.);
                cx.notify();
            })))
            .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{:.0}pt", self.font_size)))
            .child(btn("pdf-fs-up", "A+", "Bigger text").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.font_size = (this.font_size + 2.).min(96.);
                cx.notify();
            })))
            .child(sep())
            .child(btn("pdf-rot", "Rotate", "Rotate the current page 90°").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.rotate(cx))))
            .child(btn("pdf-up", "↑", "Move the current page up").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.move_page(false, cx))))
            .child(btn("pdf-dn", "↓", "Move the current page down").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.move_page(true, cx))))
            .child(btn("pdf-del", "Delete page", "Delete the current page").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.delete_page(cx))))
            .child(btn("pdf-undo", "Undo", "Undo (⌘Z)").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.undo(cx))))
            .child(div().flex_1())
            .child(btn("pdf-zo", "−", "Zoom out").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                let z = this.scale() / 1.2;
                this.set_zoom(Some(z), cx)
            })))
            .child(div().w(px(40.)).flex().justify_center().text_size(px(11.)).text_color(theme::rgb(theme::TEXT)).child(pct))
            .child(btn("pdf-zi", "+", "Zoom in").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                let z = this.scale() * 1.2;
                this.set_zoom(Some(z), cx)
            })))
            .child(btn("pdf-fit", "Fit", "Fit width").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_zoom(None, cx))))
    }
}

fn btn(id: &'static str, label: &'static str, tip: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(22.))
        .px(px(7.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(11.5))
        .text_color(theme::rgb(theme::TEXT_MUTED))
        .hover(|s| s.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT)))
        .child(label)
        .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(e) = &self.error {
            return div().flex_1().flex().items_center().justify_center().text_color(theme::rgb(theme::TEXT_MUTED)).child(e.clone()).into_any_element();
        }
        self.ensure_rendered(window, cx);
        let scale = self.scale();
        let n = self.sizes.len();
        let bounds = self.page_bounds.clone();
        bounds.borrow_mut().clear();
        let accent = theme::rgb(theme::ACCENT);
        let pages = self.sizes.iter().enumerate().map(|(i, (w, h))| {
            let (pw, ph) = (w * scale, h * scale);
            let b = bounds.clone();
            let mut page = div()
                .id(("pdf-page", i))
                .relative()
                .flex_none()
                .w(px(pw))
                .h(px(ph))
                .bg(gpui::white())
                .shadow_md()
                .when(i == self.current && n > 1, |d| d.border_1().border_color(accent))
                .child(canvas(move |r, _, _| b.borrow_mut().insert(i, r), |_, _, _, _| {}).absolute().size_full());
            if let Some((_, im)) = self.images.get(&i) {
                page = page.child(img(im.clone()).absolute().top_0().left_0().size_full());
            }
            for (k, (_, r)) in self.hits.iter().enumerate().filter(|(_, h)| h.0 == i) {
                let cur = k == self.hit;
                page = page.child(
                    div()
                        .absolute()
                        .left(px(r.0 as f32 * scale - 1.))
                        .top(px(r.1 as f32 * scale - 1.))
                        .w(px(r.2 as f32 * scale + 2.))
                        .h(px(r.3 as f32 * scale + 2.))
                        .rounded(px(2.))
                        .bg(gpui::rgba(if cur { 0xff9a0070 } else { 0xffe60055 })),
                );
            }
            // Live preview of the drag.
            if let Some((dp, a, bb)) = self.drag.filter(|d| d.0 == i) {
                let s = scale as f64;
                match self.tool {
                    Tool::Highlight | Tool::Rect => {
                        let (x0, y0) = (a.0.min(bb.0) * s, a.1.min(bb.1) * s);
                        let (ww, hh) = ((a.0 - bb.0).abs() * s, (a.1 - bb.1).abs() * s);
                        let c = self.rgba();
                        let col = gpui::rgba(((c.0 * 255.) as u32) << 24 | ((c.1 * 255.) as u32) << 16 | ((c.2 * 255.) as u32) << 8 | 0x70);
                        page = page.child(
                            div()
                                .absolute()
                                .left(px(x0 as f32))
                                .top(px(y0 as f32))
                                .w(px(ww as f32))
                                .h(px(hh as f32))
                                .when(self.tool == Tool::Highlight, |d| d.bg(col))
                                .when(self.tool == Tool::Rect, |d| d.border_2().border_color(col)),
                        );
                    }
                    Tool::Draw => {
                        let pts: Vec<(f32, f32)> = self.ink.iter().map(|p| ((p.0 * s) as f32, (p.1 * s) as f32)).collect();
                        let c = self.rgba();
                        let col = gpui::rgba(((c.0 * 255.) as u32) << 24 | ((c.1 * 255.) as u32) << 16 | ((c.2 * 255.) as u32) << 8 | 0xff);
                        let _ = dp;
                        page = page.child(
                            canvas(
                                |_, _, _| {},
                                move |r, _, window, _| {
                                    if pts.len() < 2 {
                                        return;
                                    }
                                    let mut pb = PathBuilder::stroke(px(2.));
                                    pb.move_to(point(r.origin.x + px(pts[0].0), r.origin.y + px(pts[0].1)));
                                    for p in &pts[1..] {
                                        pb.line_to(point(r.origin.x + px(p.0), r.origin.y + px(p.1)));
                                    }
                                    if let Ok(path) = pb.build() {
                                        window.paint_path(path, col);
                                    }
                                },
                            )
                            .absolute()
                            .size_full(),
                        );
                    }
                    _ => {}
                }
            }
            if let Some((_, at, input)) = self.text.as_ref().filter(|t| t.0 == i) {
                page = page.child(
                    div()
                        .absolute()
                        .left(px(at.0 as f32 * scale))
                        .top(px(at.1 as f32 * scale - 4.))
                        .min_w(px(160.))
                        .px(px(4.))
                        .rounded(px(3.))
                        .bg(theme::rgb(theme::EDITOR_BG))
                        .border_1()
                        .border_color(accent)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(input.clone()),
                );
            }
            page
        });
        let cursor = match self.tool {
            Tool::Hand => gpui::CursorStyle::Arrow,
            Tool::Text => gpui::CursorStyle::IBeam,
            Tool::Erase => gpui::CursorStyle::PointingHand,
            _ => gpui::CursorStyle::Crosshair,
        };
        let footer = format!(
            "Page {} of {n}{}{}",
            self.current + 1,
            if self.dirty { " · unsaved changes (⌘S)" } else { "" },
            self.status.as_ref().filter(|_| !self.dirty).map(|s| format!(" · {s}")).unwrap_or_default()
        );
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .id("pdf-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .bg(theme::rgb(0x2a2d32))
                    .cursor(cursor)
                    .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                        if e.modifiers.platform || e.modifiers.control {
                            let dy = f32::from(e.delta.pixel_delta(px(20.)).y);
                            let z = this.scale() * (dy * 0.005).exp();
                            this.set_zoom(Some(z), cx);
                            cx.stop_propagation();
                        } else {
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_move(cx.listener(Self::mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
                    .child(div().min_w_full().flex().flex_col().items_center().gap(px(GAP)).py(px(GAP)).px(px(GAP)).children(pages)),
            )
            .child(
                div()
                    .flex()
                    .px(px(12.))
                    .py(px(4.))
                    .border_t_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .text_size(px(11.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child(footer),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::ffi::Doc;

    /// `JT_PDF=file.pdf cargo test -- --ignored pdfkit_roundtrip` (writes file.pdf.out.pdf).
    #[test]
    #[ignore]
    fn pdfkit_roundtrip() {
        let p = std::path::PathBuf::from(std::env::var("JT_PDF").unwrap());
        let d = Doc::open(&p).unwrap();
        assert!(d.count() >= 1);
        let (w, h, buf) = d.render(0, 2.).unwrap();
        assert_eq!(buf.len(), (w * h * 4) as usize);
        let _a = d.add_text(0, (10., 10.), "Hola PDF", 14., (1., 0., 0., 1.));
        let _b = d.add_box(0, (20., 40.), (120., 60.), true, (1., 0.9, 0., 0.45));
        let _c = d.add_box(0, (20., 70.), (120., 90.), false, (0., 0., 1., 1.));
        let _i = d.add_ink(0, &[(10., 100.), (50., 120.), (90., 100.)], (0., 0., 0., 1.));
        assert!(d.annotation_at(0, (60., 50.)).is_some());
        d.set_rotation(0, 90);
        let (w2, h2, _) = d.render(0, 1.).unwrap();
        println!("render {w}x{h} rotated {w2}x{h2}");
        if let Ok(q) = std::env::var("JT_PDF_FIND") {
            let hits = d.find(&q);
            println!("find {q}: {hits:?}");
            assert!(!hits.is_empty());
        }
        let out = p.with_extension("out.pdf");
        assert!(d.write(&out));
    }
}
