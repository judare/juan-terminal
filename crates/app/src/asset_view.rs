//! Viewers for non-text files: images (rendered), CSV/TSV (table), and media/PDF/binary
//! (file info + open with the macOS default app; GPUI has no video playback).

use std::path::{Path, PathBuf};

use gpui::{
    App, ClickEvent, Context, Global, ListHorizontalSizingBehavior, MouseMoveEvent, ObjectFit, Pixels, Point, Render,
    ScrollHandle, ScrollWheelEvent, SharedString, Task, WeakEntity, Window, div, img, point, prelude::*, px, uniform_list,
};

use crate::theme;

/// Files above this size are refused (text and assets).
pub const MAX_OPEN_BYTES: u64 = 100 * 1024 * 1024;
const MAX_CSV_ROWS: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Image,
    Table,
    Video,
    Audio,
    Pdf,
    Binary,
}

pub fn asset_kind(path: &Path) -> Option<AssetKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico" => AssetKind::Image,
        "csv" | "tsv" => AssetKind::Table,
        "mp4" | "mov" | "m4v" | "webm" | "avi" | "mkv" => AssetKind::Video,
        "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" => AssetKind::Audio,
        "pdf" => AssetKind::Pdf,
        "zip" | "gz" | "tgz" | "dmg" | "woff" | "woff2" | "ttf" | "otf" | "exe" | "so" | "dylib" | "wasm" | "sqlite" | "db" => AssetKind::Binary,
        _ => return None,
    })
}

pub struct AssetView {
    pub path: PathBuf,
    pub kind: AssetKind,
    bytes: u64,
    table: Option<Table>,
    loading: bool,
    error: Option<String>,
    /// `None` = fit (natural size, scaled down if needed); `Some(z)` = explicit zoom factor.
    zoom: Option<f32>,
    /// Natural pixel size of the image (from its header).
    natural: Option<(f32, f32)>,
    scroll: ScrollHandle,
    hovered: bool,
    /// Last mouse position in window coordinates (zoom anchor for pinch).
    mouse: Option<Point<Pixels>>,
    _task: Option<Task<()>>,
}

struct Table {
    rows: Vec<Vec<String>>,
    widths: Vec<f32>,
    truncated: bool,
}

impl AssetView {
    pub fn new(path: PathBuf, kind: AssetKind, cx: &mut Context<Self>) -> Self {
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let natural = if kind == AssetKind::Image {
            imagesize::size(&path).ok().map(|s| (s.width as f32, s.height as f32)).filter(|(w, h)| *w > 0. && *h > 0.)
        } else {
            None
        };
        let mut this = Self {
            path,
            kind,
            bytes,
            table: None,
            loading: false,
            error: None,
            zoom: None,
            natural,
            scroll: ScrollHandle::new(),
            hovered: false,
            mouse: None,
            _task: None,
        };
        if kind == AssetKind::Image {
            pinch::register(cx.weak_entity(), cx);
        }
        if kind == AssetKind::Table {
            this.loading = true;
            let p = this.path.clone();
            this._task = Some(cx.spawn(async move |this, cx| {
                let result = cx.background_executor().spawn(async move { load_table(&p) }).await;
                let _ = this.update(cx, |this, cx| {
                    this.loading = false;
                    match result {
                        Ok(t) => this.table = Some(t),
                        Err(e) => this.error = Some(e),
                    }
                    cx.notify();
                });
            }));
        }
        this
    }
}

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 32.;
const PAD: f32 = 16.;

impl AssetView {
    /// Scale currently on screen: the explicit zoom, or the fit scale.
    fn current_scale(&self) -> f32 {
        if let Some(z) = self.zoom {
            return z;
        }
        let Some((w, h)) = self.natural else { return 1. };
        let b = self.scroll.bounds();
        let (vw, vh) = (f32::from(b.size.width) - 2. * PAD, f32::from(b.size.height) - 2. * PAD);
        if vw <= 0. || vh <= 0. {
            return 1.;
        }
        (vw / w).min(vh / h).min(1.)
    }

    /// Zoom by `factor`, keeping the image point under `anchor` (window coords) fixed.
    fn zoom_by(&mut self, factor: f32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let Some((w, h)) = self.natural else { return };
        let z0 = self.current_scale();
        let z1 = (z0 * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (z1 - z0).abs() < f32::EPSILON {
            return;
        }
        let b = self.scroll.bounds();
        let (vw, vh) = (f32::from(b.size.width), f32::from(b.size.height));
        let anchor = anchor.filter(|p| b.contains(p)).unwrap_or_else(|| b.center());
        let (ax, ay) = (f32::from(anchor.x - b.origin.x), f32::from(anchor.y - b.origin.y));
        let off = if self.zoom.is_some() { self.scroll.offset() } else { point(px(0.), px(0.)) };
        // Image origin inside the scroll content (centered when smaller than the viewport).
        let origin = |z: f32, view: f32, natural: f32| ((view - natural * z) / 2.).max(PAD);
        let (ix0, iy0) = (origin(z0, vw, w), origin(z0, vh, h));
        // Image-pixel coordinate under the anchor.
        let cx_img = (ax - f32::from(off.x) - ix0) / z0;
        let cy_img = (ay - f32::from(off.y) - iy0) / z0;
        let (ix1, iy1) = (origin(z1, vw, w), origin(z1, vh, h));
        let max_x = (w * z1 + 2. * PAD - vw).max(0.);
        let max_y = (h * z1 + 2. * PAD - vh).max(0.);
        let nx = (ax - ix1 - cx_img * z1).clamp(-max_x, 0.);
        let ny = (ay - iy1 - cy_img * z1).clamp(-max_y, 0.);
        self.zoom = Some(z1);
        self.scroll.set_offset(point(px(nx), px(ny)));
        cx.notify();
    }

    fn set_zoom(&mut self, zoom: Option<f32>, cx: &mut Context<Self>) {
        self.zoom = zoom;
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
}

/// Trackpad pinch (NSEvent magnify). GPUI doesn't surface it, so we add
/// `magnifyWithEvent:` to GPUI's NSView class and forward the deltas to the hovered image view.
mod pinch {
    use super::*;
    use futures::{StreamExt, channel::mpsc};
    use std::sync::OnceLock;

    static TX: OnceLock<mpsc::UnboundedSender<f64>> = OnceLock::new();

    #[derive(Default)]
    struct Targets(Vec<WeakEntity<AssetView>>);
    impl Global for Targets {}

    pub fn register(view: WeakEntity<AssetView>, cx: &mut App) {
        if !cx.has_global::<Targets>() {
            cx.set_global(Targets::default());
            let (tx, mut rx) = mpsc::unbounded::<f64>();
            if TX.set(tx).is_ok() {
                install_magnify_hook();
            }
            cx.spawn(async move |cx| {
                while let Some(mut m) = rx.next().await {
                    // Coalesce queued deltas into one zoom step.
                    while let Ok(more) = rx.try_recv() {
                        m += more;
                    }
                    let _ = cx.update(|cx| {
                        let targets = cx.global::<Targets>().0.clone();
                        if let Some(t) = targets.iter().rev().filter_map(|w| w.upgrade()).find(|e| e.read(cx).hovered) {
                            t.update(cx, |v, cx| {
                                let anchor = v.mouse;
                                v.zoom_by((1. + m as f32).max(0.1), anchor, cx);
                            });
                        }
                    });
                }
            })
            .detach();
        }
        let t = cx.global_mut::<Targets>();
        t.0.retain(|w| w.upgrade().is_some());
        t.0.push(view);
    }

    #[cfg(target_os = "macos")]
    #[allow(unexpected_cfgs)]
    fn install_magnify_hook() {
        use objc::runtime::{Class, Object, Sel, class_addMethod};
        use objc::{msg_send, sel, sel_impl};

        extern "C" fn magnify(_this: &Object, _sel: Sel, event: *mut Object) {
            let m: f64 = unsafe { msg_send![event, magnification] };
            if let Some(tx) = TX.get() {
                let _ = tx.unbounded_send(m);
            }
        }

        let Some(cls) = Class::get("GPUIView") else { return };
        unsafe {
            let imp: extern "C" fn(&Object, Sel, *mut Object) = magnify;
            class_addMethod(cls as *const Class as *mut Class, sel!(magnifyWithEvent:), std::mem::transmute(imp), c"v@:@".as_ptr());
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn install_magnify_hook() {}
}

fn load_table(path: &Path) -> Result<Table, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let first = text.lines().next().unwrap_or("");
    let delim = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv")) || first.matches('\t').count() > first.matches(',').count() {
        '\t'
    } else if first.matches(';').count() > first.matches(',').count() {
        ';'
    } else {
        ','
    };
    let (rows, truncated) = parse_csv(&text, delim, MAX_CSV_ROWS);
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![4usize; cols];
    for r in rows.iter().take(500) {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count().min(48));
        }
    }
    Ok(Table { rows, widths: widths.into_iter().map(|w| w as f32 * 7.4 + 20.).collect(), truncated })
}

/// RFC 4180-style CSV parsing: quoted fields, escaped quotes, newlines inside quotes.
pub fn parse_csv(text: &str, delim: char, max_rows: usize) -> (Vec<Vec<String>>, bool) {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == delim => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                if rows.len() >= max_rows {
                    return (rows, chars.peek().is_some());
                }
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    (rows, false)
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.),
        b => format!("{b} B"),
    }
}

fn button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(12.))
        .py(px(5.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(12.5))
        .bg(theme::rgb(theme::SELECTED))
        .text_color(theme::rgb(theme::TEXT))
        .hover(|s| s.bg(theme::rgb(0x2b4a7a)))
        .child(label)
}

impl Render for AssetView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let info = format!("{name} · {}", human_size(self.bytes));
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(32.))
            .px(px(12.))
            .flex_none()
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .text_size(px(12.))
            .text_color(theme::rgb(theme::TEXT_MUTED))
            .child(info)
            .child(div().flex_1())
            .when(self.kind == AssetKind::Image && self.natural.is_some(), |d| {
                let pct = format!("{:.0}%", self.current_scale() * 100.);
                d.child(button("asset-zoom-out", "−").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.zoom_by(1. / 1.25, None, cx))))
                    .child(div().w(px(44.)).flex().justify_center().text_color(theme::rgb(theme::TEXT)).child(pct))
                    .child(button("asset-zoom-in", "+").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.zoom_by(1.25, None, cx))))
                    .child(button("asset-zoom-100", "100%").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_zoom(Some(1.), cx))))
                    .child(button("asset-zoom-fit", "Fit").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_zoom(None, cx))))
            })
            .child(button("asset-open", "Open with default app").on_click(cx.listener(|this, _: &ClickEvent, _, cx| cx.open_with_system(&this.path))))
            .child(button("asset-reveal", "Reveal in Finder").on_click(cx.listener(|this, _: &ClickEvent, _, cx| cx.reveal_path(&this.path))));

        let body = match self.kind {
            AssetKind::Image => {
                let base = div()
                    .id("image-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    // Neutral background so transparent images are readable.
                    .bg(theme::rgb(0x1d1f23))
                    .on_hover(cx.listener(|this, h: &bool, _, _| this.hovered = *h))
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, _| {
                        this.mouse = Some(e.position);
                        this.hovered = true;
                    }))
                    // ⌘/Ctrl + wheel zooms (plain wheel scrolls the zoomed image).
                    .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                        if !(e.modifiers.platform || e.modifiers.control) {
                            return;
                        }
                        let dy = f32::from(e.delta.pixel_delta(px(20.)).y);
                        this.zoom_by((dy * 0.005).exp(), Some(e.position), cx);
                        cx.stop_propagation();
                    }));
                match (self.zoom, self.natural) {
                    (Some(z), Some((w, h))) => base
                        .child(
                            div()
                                .min_w_full()
                                .min_h_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .p(px(PAD))
                                .child(img(self.path.clone()).flex_none().w(px(w * z)).h(px(h * z)).object_fit(ObjectFit::Fill)),
                        )
                        .into_any_element(),
                    // Fit: natural size, scaled down only if it doesn't fit.
                    _ => base
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(PAD))
                        .child(img(self.path.clone()).size_full().object_fit(ObjectFit::ScaleDown))
                        .into_any_element(),
                }
            }
            AssetKind::Table => {
                if let Some(e) = &self.error {
                    centered(e.clone()).into_any_element()
                } else if self.loading {
                    centered("Loading…".into()).into_any_element()
                } else if let Some(t) = &self.table {
                    let count = t.rows.len();
                    let widths = t.widths.clone();
                    let total_w: f32 = widths.iter().sum::<f32>() + 56.;
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(
                            uniform_list(
                                "csv-rows",
                                count,
                                cx.processor(move |this, range: std::ops::Range<usize>, _, _| {
                                    let t = this.table.as_ref().unwrap();
                                    range
                                        .map(|ix| {
                                            let header = ix == 0;
                                            let mut row = div()
                                                .id(ix)
                                                .flex()
                                                .h(px(24.))
                                                .w(px(total_w))
                                                .border_b_1()
                                                .border_color(theme::rgb(0x1f2125))
                                                .text_size(px(12.))
                                                .when(header, |d| d.bg(theme::rgb(theme::SIDEBAR_BG)).font_weight(gpui::FontWeight::SEMIBOLD))
                                                .child(
                                                    div()
                                                        .w(px(56.))
                                                        .flex_none()
                                                        .px(px(8.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_end()
                                                        .text_size(px(11.))
                                                        .text_color(theme::rgb(theme::GUTTER_TEXT))
                                                        .child(if header { String::new() } else { ix.to_string() }),
                                                );
                                            for (ci, w) in t.widths.iter().enumerate() {
                                                let cell = t.rows[ix].get(ci).cloned().unwrap_or_default();
                                                row = row.child(
                                                    div()
                                                        .w(px(*w))
                                                        .flex_none()
                                                        .px(px(8.))
                                                        .flex()
                                                        .items_center()
                                                        .border_l_1()
                                                        .border_color(theme::rgb(0x1f2125))
                                                        .overflow_hidden()
                                                        .whitespace_nowrap()
                                                        .text_ellipsis()
                                                        .text_color(theme::rgb(if header { theme::TEXT } else { 0xc3c7ce }))
                                                        .child(SharedString::from(cell)),
                                                );
                                            }
                                            row
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                            .flex_1(),
                        )
                        .child(
                            div()
                                .px(px(12.))
                                .py(px(4.))
                                .text_size(px(11.))
                                .text_color(theme::rgb(theme::TEXT_FAINT))
                                .child(format!(
                                    "{} rows · {} columns{}",
                                    count.saturating_sub(1),
                                    widths.len(),
                                    if t.truncated { format!(" · showing first {MAX_CSV_ROWS}") } else { String::new() }
                                )),
                        )
                        .into_any_element()
                } else {
                    centered("Empty file".into()).into_any_element()
                }
            }
            kind => {
                let (title, hint) = match kind {
                    AssetKind::Video => ("Video file", "Video playback isn't available inside the app (GPUI has no media player). Opens in QuickTime Player."),
                    AssetKind::Audio => ("Audio file", "Opens in the default audio app."),
                    AssetKind::Pdf => ("PDF document", "Opens in Preview."),
                    _ => ("Binary file", "This file can't be shown as text."),
                };
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .items_center()
                    .justify_center()
                    .child(div().text_size(px(15.)).text_color(theme::rgb(theme::TEXT)).child(title))
                    .child(div().max_w(px(420.)).text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_MUTED)).child(hint))
                    .child(button("asset-open-big", "Open").on_click(cx.listener(|this, _: &ClickEvent, _, cx| cx.open_with_system(&this.path))))
                    .into_any_element()
            }
        };
        div().size_full().flex().flex_col().bg(theme::rgb(theme::EDITOR_BG)).child(header).child(body)
    }
}

fn centered(text: String) -> gpui::Div {
    div().flex_1().flex().items_center().justify_center().text_size(px(13.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_and_newlines() {
        let (rows, trunc) = parse_csv("a,b,c\n1,\"x, y\",\"he said \"\"hi\"\"\"\n2,\"multi\nline\",3\r\n", ',', 100);
        assert!(!trunc);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], vec!["1", "x, y", "he said \"hi\""]);
        assert_eq!(rows[2][1], "multi\nline");
        let (rows, trunc) = parse_csv("1\n2\n3\n", ',', 2);
        assert_eq!(rows.len(), 2);
        assert!(trunc);
    }

    #[test]
    fn kinds() {
        assert_eq!(asset_kind(Path::new("a.PNG")), Some(AssetKind::Image));
        assert_eq!(asset_kind(Path::new("a.csv")), Some(AssetKind::Table));
        assert_eq!(asset_kind(Path::new("a.mov")), Some(AssetKind::Video));
        assert_eq!(asset_kind(Path::new("a.ts")), None);
    }
}
