//! Markdown → corporate PDF. The Markdown is laid out as HTML and printed by a headless
//! Chromium (Brave/Chrome/Edge, whichever is installed; invisible, separate profile), then every
//! page is stamped with CoreGraphics: letterhead (logo, organization, subtitle, date), footer
//! ("Page X of Y") and an optional diagonal watermark. Branding is edited in the export dialog
//! and remembered per project.
#![allow(unexpected_cfgs)] // objc 0.2 macros

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use gpui::{ClickEvent, Context, Entity, EventEmitter, Focusable, PathPromptOptions, Render, Window, div, prelude::*, px};
use serde::{Deserialize, Serialize};

use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone, Serialize, Deserialize, Default, Debug)]
#[serde(default)]
pub struct Branding {
    pub name: String,
    pub subtitle: String,
    pub logo: String,
    pub accent: String,
    pub author: String,
    pub footer: String,
    pub show_date: bool,
    pub watermark: bool,
    pub watermark_text: String,
    /// Percent (1–60).
    pub watermark_opacity: u32,
}

fn settings_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/juan-terminal/pdf-branding.json")
}

/// Git root (or the file's folder): branding is remembered per project.
pub fn project_root(file: &Path) -> PathBuf {
    let start = file.parent().unwrap_or(Path::new("/"));
    start.ancestors().find(|d| d.join(".git").exists()).unwrap_or(start).to_path_buf()
}

fn find_logo(root: &Path) -> Option<PathBuf> {
    const NAMES: [&str; 8] = ["logo.png", "logo.jpg", "logo.svg", "icon.png", "favicon.png", "app-icon.png", "logo@2x.png", "brand.png"];
    const SUBDIRS: [&str; 10] = ["", "packaging", "assets", "public", "static", "images", "img", "docs", "src/assets", "resources"];
    for d in SUBDIRS {
        for n in NAMES {
            let p = root.join(d).join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub fn load_branding(root: &Path) -> Branding {
    let all: HashMap<String, Branding> = std::fs::read(settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if let Some(b) = all.get(&root.to_string_lossy().into_owned()) {
        return b.clone();
    }
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let pretty = name.replace(['-', '_'], " ");
    let pretty = pretty.split(' ').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    });
    Branding {
        name: pretty.collect::<Vec<_>>().join(" "),
        subtitle: String::new(),
        logo: find_logo(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        accent: "#1F4E99".into(),
        author: String::new(),
        footer: "Confidential — internal use".into(),
        show_date: true,
        watermark: false,
        watermark_text: "CONFIDENTIAL".into(),
        watermark_opacity: 10,
    }
}

pub fn save_branding(root: &Path, b: &Branding) {
    let p = settings_path();
    let mut all: HashMap<String, Branding> = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    all.insert(root.to_string_lossy().into_owned(), b.clone());
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(j) = serde_json::to_vec_pretty(&all) {
        let _ = std::fs::write(p, j);
    }
}

fn parse_hex(s: &str) -> (f64, f64, f64) {
    let h = s.trim().trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6).unwrap_or(0x1F4E99);
    (((v >> 16) & 0xff) as f64 / 255., ((v >> 8) & 0xff) as f64 / 255., (v & 0xff) as f64 / 255.)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Markdown → standalone HTML (content only; letterhead is stamped later).
pub fn to_html(md: &str, base: &Path, accent: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let mut body = String::new();
    html::push_html(&mut body, Parser::new_ext(md, opts));
    let accent = html_escape(accent);
    let base = html_escape(&format!("file://{}/", base.to_string_lossy()));
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><base href="{base}"><style>
@page {{ size: A4; margin: 27mm 18mm 21mm 18mm; }}
html {{ -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
body {{ font: 10.5pt/1.55 -apple-system, "Helvetica Neue", Helvetica, Arial, sans-serif; color: #1d2330; margin: 0; }}
h1, h2, h3, h4 {{ color: {accent}; line-height: 1.25; page-break-after: avoid; break-after: avoid; }}
h1 {{ font-size: 21pt; margin: 0 0 10pt; padding-bottom: 6pt; border-bottom: 2px solid {accent}; }}
h2 {{ font-size: 15pt; margin: 18pt 0 6pt; }}
h3 {{ font-size: 12.5pt; margin: 14pt 0 4pt; }}
h4 {{ font-size: 11pt; margin: 12pt 0 4pt; color: #333; }}
p {{ margin: 0 0 8pt; }}
a {{ color: {accent}; }}
ul, ol {{ margin: 0 0 8pt; padding-left: 18pt; }}
li {{ margin: 2pt 0; }}
blockquote {{ margin: 8pt 0; padding: 6pt 12pt; border-left: 3px solid {accent}; background: #f3f5f9; color: #3a4250; }}
code {{ font: 9pt "SF Mono", Menlo, monospace; background: #f1f3f6; padding: 1px 4px; border-radius: 3px; }}
pre {{ background: #f6f8fa; border: 1px solid #e3e7ee; border-radius: 6px; padding: 9pt 11pt; overflow: hidden; white-space: pre-wrap; page-break-inside: avoid; }}
pre code {{ background: none; padding: 0; }}
table {{ border-collapse: collapse; width: 100%; margin: 8pt 0 12pt; font-size: 9.5pt; page-break-inside: auto; }}
tr {{ page-break-inside: avoid; }}
th {{ background: {accent}; color: #fff; text-align: left; font-weight: 600; }}
th, td {{ padding: 5pt 7pt; border: 1px solid #d8dde6; vertical-align: top; }}
tbody tr:nth-child(even) td {{ background: #f7f9fc; }}
img {{ max-width: 100%; }}
hr {{ border: 0; border-top: 1px solid #d8dde6; margin: 14pt 0; }}
input[type=checkbox] {{ margin-right: 5pt; }}
</style></head><body>{body}</body></html>"#
    )
}

fn browser() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let apps = [
        // Brave's headless print hangs; it's the last resort.
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "Chromium.app/Contents/MacOS/Chromium",
        "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "Brave Browser.app/Contents/MacOS/Brave Browser",
    ];
    for base in [PathBuf::from("/Applications"), home.join("Applications")] {
        for a in apps {
            let p = base.join(a);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Full export; blocking (run it off the main thread).
pub fn export(md: &str, source: &Path, out: &Path, b: &Branding) -> Result<(), String> {
    let bin = browser().ok_or("Exporting needs a Chromium browser engine (Brave, Chrome or Edge); none was found in /Applications.")?;
    let tmp = std::env::temp_dir().join(format!("jt-pdf-{}-{}", std::process::id(), Instant::now().elapsed().as_nanos()));
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let result = (|| {
        let base = source.parent().unwrap_or(Path::new("/"));
        let html = tmp.join("doc.html");
        std::fs::write(&html, to_html(md, base, &b.accent)).map_err(|e| e.to_string())?;
        let raw = tmp.join("raw.pdf");
        let mut child = Command::new(&bin)
            .args(["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--disable-extensions", "--no-pdf-header-footer", "--hide-scrollbars"])
            .arg(format!("--user-data-dir={}", tmp.join("profile").display()))
            .arg(format!("--print-to-pdf={}", raw.display()))
            .arg(format!("file://{}", html.display()))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Couldn't start the PDF engine: {e}"))?;
        let start = Instant::now();
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            if start.elapsed() > Duration::from_secs(45) {
                let _ = child.kill();
                return Err("The PDF engine timed out.".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !raw.is_file() {
            return Err("The PDF engine didn't produce a file.".into());
        }
        stamp::stamp(&raw, out, b)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Letterhead/footer/watermark drawn on every page of an existing PDF.
mod stamp {
    use super::{Branding, parse_hex};
    use core_foundation::base::TCFType;
    use core_foundation::url::{CFURL, CFURLRef};
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::c_void;
    use std::path::Path;

    type Id = *mut Object;
    type Ctx = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct P {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct S {
        w: f64,
        h: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct R {
        o: P,
        s: S,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPDFDocumentCreateWithURL(url: CFURLRef) -> *mut c_void;
        fn CGPDFDocumentRelease(d: *mut c_void);
        fn CGPDFDocumentGetNumberOfPages(d: *mut c_void) -> usize;
        fn CGPDFDocumentGetPage(d: *mut c_void, n: usize) -> *mut c_void;
        fn CGPDFPageGetBoxRect(p: *mut c_void, b: i32) -> R;
        fn CGPDFContextCreateWithURL(url: CFURLRef, media: *const R, aux: *const c_void) -> Ctx;
        fn CGPDFContextClose(c: Ctx);
        fn CGContextBeginPage(c: Ctx, media: *const R);
        fn CGContextEndPage(c: Ctx);
        fn CGContextRelease(c: Ctx);
        fn CGContextDrawPDFPage(c: Ctx, p: *mut c_void);
        fn CGContextSaveGState(c: Ctx);
        fn CGContextRestoreGState(c: Ctx);
        fn CGContextTranslateCTM(c: Ctx, x: f64, y: f64);
        fn CGContextRotateCTM(c: Ctx, a: f64);
        fn CGContextSetRGBFillColor(c: Ctx, r: f64, g: f64, b: f64, a: f64);
        fn CGContextFillRect(c: Ctx, r: R);
    }

    fn ns(s: &str) -> Id {
        let c = std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    fn attrs(size: f64, weight: f64, rgba: (f64, f64, f64, f64)) -> Id {
        unsafe {
            let font: Id = msg_send![class!(NSFont), systemFontOfSize: size weight: weight];
            let color: Id = msg_send![class!(NSColor), colorWithSRGBRed: rgba.0 green: rgba.1 blue: rgba.2 alpha: rgba.3];
            let objs = [font, color];
            let keys = [ns("NSFont"), ns("NSColor")];
            msg_send![class!(NSDictionary), dictionaryWithObjects: objs.as_ptr() forKeys: keys.as_ptr() count: 2usize]
        }
    }

    fn width(text: Id, a: Id) -> f64 {
        unsafe {
            let s: S = msg_send![text, sizeWithAttributes: a];
            s.w
        }
    }

    fn draw(text: &str, x: f64, y: f64, a: Id) -> f64 {
        unsafe {
            let t = ns(text);
            let _: () = msg_send![t, drawAtPoint: P { x, y } withAttributes: a];
            width(t, a)
        }
    }

    fn draw_right(text: &str, right: f64, y: f64, a: Id) {
        let w = width(ns(text), a);
        draw(text, right - w, y, a);
    }

    pub fn stamp(src: &Path, out: &Path, b: &Branding) -> Result<(), String> {
        objc::rc::autoreleasepool(|| unsafe {
            let src_url = CFURL::from_path(src, false).ok_or("bad path")?;
            let out_url = CFURL::from_path(out, false).ok_or("bad output path")?;
            let doc = CGPDFDocumentCreateWithURL(src_url.as_concrete_TypeRef());
            if doc.is_null() {
                return Err("Couldn't read the generated PDF.".into());
            }
            let n = CGPDFDocumentGetNumberOfPages(doc);
            let first = CGPDFPageGetBoxRect(CGPDFDocumentGetPage(doc, 1), 0);
            let ctx = CGPDFContextCreateWithURL(out_url.as_concrete_TypeRef(), &first, std::ptr::null());
            if ctx.is_null() {
                CGPDFDocumentRelease(doc);
                return Err(format!("Couldn't write {}", out.display()));
            }
            let nsctx: Id = msg_send![class!(NSGraphicsContext), graphicsContextWithCGContext: ctx flipped: false];
            let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: nsctx];
            let accent = parse_hex(&b.accent);
            let logo: Id = if b.logo.trim().is_empty() {
                std::ptr::null_mut()
            } else {
                let i: Id = msg_send![class!(NSImage), alloc];
                msg_send![i, initWithContentsOfFile: ns(b.logo.trim())]
            };
            let date = if b.show_date { today() } else { String::new() };
            let gray = (0.42, 0.45, 0.5, 1.);
            for i in 1..=n {
                let page = CGPDFDocumentGetPage(doc, i);
                let r = CGPDFPageGetBoxRect(page, 0);
                let (w, h) = (r.s.w, r.s.h);
                let m = 51.; // 18 mm
                CGContextBeginPage(ctx, &r);
                CGContextDrawPDFPage(ctx, page);

                // Letterhead.
                let mut x = m;
                if !logo.is_null() {
                    let sz: S = msg_send![logo, size];
                    if sz.w > 0. && sz.h > 0. {
                        let lh = 30.;
                        let lw = (sz.w / sz.h * lh).min(130.);
                        let lh = lw * sz.h / sz.w;
                        let rect = R { o: P { x: m, y: h - 22. - lh - (30. - lh) / 2. }, s: S { w: lw, h: lh } };
                        let zero = R::default();
                        let _: () = msg_send![logo, drawInRect: rect fromRect: zero operation: 2usize fraction: 1.0f64];
                        x = m + lw + 10.;
                    }
                }
                let has_sub = !b.subtitle.trim().is_empty();
                if !b.name.trim().is_empty() {
                    draw(b.name.trim(), x, h - (if has_sub { 36. } else { 42. }), attrs(13., 0.4, (accent.0, accent.1, accent.2, 1.)));
                }
                if has_sub {
                    draw(b.subtitle.trim(), x, h - 50., attrs(8.5, 0., gray));
                }
                let right_top: Vec<&str> = [date.as_str(), b.author.trim()].into_iter().filter(|s| !s.is_empty()).collect();
                for (k, t) in right_top.iter().enumerate() {
                    draw_right(t, w - m, h - 36. - k as f64 * 12., attrs(8.5, 0., gray));
                }
                CGContextSetRGBFillColor(ctx, accent.0, accent.1, accent.2, 1.);
                CGContextFillRect(ctx, R { o: P { x: m, y: h - 60. }, s: S { w: w - 2. * m, h: 1.6 } });

                // Footer.
                CGContextSetRGBFillColor(ctx, 0.82, 0.84, 0.88, 1.);
                CGContextFillRect(ctx, R { o: P { x: m, y: 40. }, s: S { w: w - 2. * m, h: 0.6 } });
                if !b.footer.trim().is_empty() {
                    draw(b.footer.trim(), m, 26., attrs(8., 0., gray));
                }
                draw_right(&format!("{i} / {n}"), w - m, 26., attrs(8., 0.3, gray));

                // Watermark.
                if b.watermark && !b.watermark_text.trim().is_empty() {
                    let alpha = (b.watermark_opacity.clamp(1, 60) as f64) / 100.;
                    let text = b.watermark_text.trim();
                    let mut size = 96.;
                    let mut a = attrs(size, 0.6, (0.5, 0.5, 0.55, alpha));
                    let diag = (w * w + h * h).sqrt() * 0.8;
                    let tw = width(ns(text), a);
                    if tw > diag {
                        size *= diag / tw;
                        a = attrs(size, 0.6, (0.5, 0.5, 0.55, alpha));
                    }
                    let tw = width(ns(text), a);
                    CGContextSaveGState(ctx);
                    CGContextTranslateCTM(ctx, w / 2., h / 2.);
                    CGContextRotateCTM(ctx, (h / w).atan());
                    draw(text, -tw / 2., -size * 0.35, a);
                    CGContextRestoreGState(ctx);
                }
                CGContextEndPage(ctx);
            }
            let nil: Id = std::ptr::null_mut();
            let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: nil];
            if !logo.is_null() {
                let _: () = msg_send![logo, release];
            }
            CGPDFContextClose(ctx);
            CGContextRelease(ctx);
            CGPDFDocumentRelease(doc);
            Ok(())
        })
    }

    fn today() -> String {
        let out = std::process::Command::new("date").arg("+%Y-%m-%d").output().ok();
        out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
    }
}

// ---- dialog -----------------------------------------------------------------------------------

pub enum ExportEvent {
    Close,
    Done(PathBuf),
    Error(String),
}

pub struct ExportDialog {
    source: PathBuf,
    markdown: String,
    root: PathBuf,
    name: Entity<TextInput>,
    subtitle: Entity<TextInput>,
    logo: Entity<TextInput>,
    accent: Entity<TextInput>,
    author: Entity<TextInput>,
    footer: Entity<TextInput>,
    wm_text: Entity<TextInput>,
    wm_opacity: Entity<TextInput>,
    show_date: bool,
    watermark: bool,
    busy: bool,
    error: Option<String>,
}

impl EventEmitter<ExportEvent> for ExportDialog {}

impl ExportDialog {
    pub fn new(source: PathBuf, markdown: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let root = project_root(&source);
        let b = load_branding(&root);
        let field = |v: &str, ph: &str, cx: &mut Context<Self>| {
            let ph = ph.to_string();
            let v = v.to_string();
            cx.new(|cx| {
                let mut t = TextInput::new(ph, cx);
                t.set_text(&v, cx);
                t.move_end(cx);
                t
            })
        };
        let this = Self {
            name: field(&b.name, "Company / project name", cx),
            subtitle: field(&b.subtitle, "Department, tagline or document type", cx),
            logo: field(&b.logo, "Path to a logo image (PNG, JPG, SVG)", cx),
            accent: field(&b.accent, "#1F4E99", cx),
            author: field(&b.author, "Author (optional)", cx),
            footer: field(&b.footer, "Footer text", cx),
            wm_text: field(&b.watermark_text, "CONFIDENTIAL", cx),
            wm_opacity: field(&b.watermark_opacity.max(1).to_string(), "10", cx),
            show_date: b.show_date,
            watermark: b.watermark,
            busy: false,
            error: None,
            source,
            markdown,
            root,
        };
        for input in [&this.name, &this.subtitle, &this.logo, &this.accent, &this.author, &this.footer, &this.wm_text, &this.wm_opacity] {
            cx.subscribe(input, |this, _, e: &crate::text_input::TextInputEvent, cx| match e {
                crate::text_input::TextInputEvent::Cancel => cx.emit(ExportEvent::Close),
                crate::text_input::TextInputEvent::Submit { .. } => this.export(cx),
                _ => {}
            })
            .detach();
        }
        window.focus(&this.name.focus_handle(cx));
        this
    }

    fn branding(&self, cx: &App) -> Branding {
        let t = |e: &Entity<TextInput>| e.read(cx).content.clone();
        Branding {
            name: t(&self.name),
            subtitle: t(&self.subtitle),
            logo: t(&self.logo),
            accent: t(&self.accent),
            author: t(&self.author),
            footer: t(&self.footer),
            show_date: self.show_date,
            watermark: self.watermark,
            watermark_text: t(&self.wm_text),
            watermark_opacity: t(&self.wm_opacity).trim().trim_end_matches('%').parse().unwrap_or(10),
        }
    }

    fn pick_logo(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose logo".into()) });
        let logo = self.logo.clone();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let _ = logo.update(cx, |t, cx| t.set_text(&p.to_string_lossy(), cx));
        })
        .detach();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let b = self.branding(cx);
        save_branding(&self.root, &b);
        let dir = self.source.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let stem = self.source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("{stem}.pdf")));
        let (md, src) = (self.markdown.clone(), self.source.clone());
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let out = match rx.await {
                Ok(Ok(Some(p))) => p,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.busy = false;
                        cx.notify();
                    });
                    return;
                }
            };
            let out = if out.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) { out } else { out.with_extension("pdf") };
            let o = out.clone();
            let res = cx.background_executor().spawn(async move { export(&md, &src, &o, &b) }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match res {
                    Ok(()) => cx.emit(ExportEvent::Done(out)),
                    Err(e) => {
                        this.error = Some(e.clone());
                        cx.emit(ExportEvent::Error(e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

use gpui::App;

fn row(label: &'static str, input: Entity<TextInput>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(div().w(px(110.)).flex_none().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(label))
        .child(
            div()
                .flex_1()
                .px(px(8.))
                .py(px(4.))
                .rounded(px(5.))
                .border_1()
                .border_color(theme::rgb(theme::BORDER))
                .bg(theme::rgb(theme::EDITOR_BG))
                .text_color(theme::rgb(theme::TEXT))
                .child(input),
        )
}

fn check(id: &'static str, on: bool, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme::rgb(theme::TEXT))
        .child(
            div()
                .size(px(14.))
                .rounded(px(3.))
                .border_1()
                .border_color(theme::rgb(if on { theme::ACCENT } else { theme::BORDER }))
                .when(on, |d| d.bg(theme::rgb(theme::ACCENT)))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .text_color(gpui::white())
                .when(on, |d| d.child("✓")),
        )
        .child(label)
}

fn button(id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(14.))
        .py(px(6.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(12.5))
        .when(primary, |d| d.bg(theme::rgb(theme::ACCENT)).text_color(gpui::white()))
        .when(!primary, |d| d.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT)))
        .child(label)
}

impl Render for ExportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (date, wm) = (self.show_date, self.watermark);
        div()
            .id("pdf-export-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000088))
            .occlude()
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.stop_propagation()))
            .child(
                div()
                    .id("pdf-export")
                    .w(px(560.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(18.))
                    .rounded(px(10.))
                    .bg(theme::rgb(theme::SIDEBAR_BG))
                    .border_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .shadow_lg()
                    .child(div().text_size(px(15.)).text_color(theme::rgb(theme::TEXT)).child("Export PDF"))
                    .child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Letterhead on every page. Saved for this project."))
                    .child(row("Name", self.name.clone()))
                    .child(row("Subtitle", self.subtitle.clone()))
                    .child(
                        div().flex().items_center().gap(px(6.)).child(div().flex_1().child(row("Logo", self.logo.clone()))).child(
                            button("pdf-logo-pick", "Choose…", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pick_logo(cx))),
                        ),
                    )
                    .child(row("Accent color", self.accent.clone()))
                    .child(row("Author", self.author.clone()))
                    .child(row("Footer", self.footer.clone()))
                    .child(check("pdf-date", date, "Show date in the letterhead").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.show_date = !this.show_date;
                        cx.notify();
                    })))
                    .child(check("pdf-wm", wm, "Watermark").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.watermark = !this.watermark;
                        cx.notify();
                    })))
                    .when(wm, |d| d.child(row("Watermark text", self.wm_text.clone())).child(row("Opacity %", self.wm_opacity.clone())))
                    .when_some(self.error.clone(), |d, e| d.child(div().text_size(px(12.)).text_color(theme::rgb(0xe06c75)).child(e)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .pt(px(6.))
                            .child(button("pdf-cancel", "Cancel", false).on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(ExportEvent::Close))))
                            .child(button("pdf-go", if self.busy { "Exporting…" } else { "Export…" }, true).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.export(cx)))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `JT_EXPORT_OUT=/tmp/x.pdf cargo test -- --ignored export_sample`
    #[test]
    #[ignore]
    fn export_sample() {
        let out = PathBuf::from(std::env::var("JT_EXPORT_OUT").unwrap());
        let md = "# Informe trimestral\n\nTexto **importante** con *énfasis*.\n\n## Tabla\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n- uno\n- dos\n\n```rust\nfn main() {}\n```\n\n".repeat(6);
        let b = Branding { name: "Juan Terminal".into(), subtitle: "Engineering".into(), logo: std::env::var("JT_LOGO").unwrap_or_default(), accent: "#1F4E99".into(), author: "Juan".into(), footer: "Confidential".into(), show_date: true, watermark: true, watermark_text: "BORRADOR".into(), watermark_opacity: 10 };
        export(&md, &out, &out, &b).unwrap();
        assert!(out.is_file());
    }
}
