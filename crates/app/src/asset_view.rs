//! Viewers for non-text files: images (rendered), CSV/TSV and spreadsheets (selectable grid),
//! and media/PDF/binary (file info + open with the macOS default app; GPUI has no video playback).

use std::path::{Path, PathBuf};

use gpui::{
    App, ClickEvent, ClipboardItem, Context, EventEmitter, FocusHandle, Global, ListHorizontalSizingBehavior, MouseButton,
    MouseDownEvent, MouseMoveEvent, ObjectFit, Pixels, Point, Render, ScrollHandle, ScrollStrategy, ScrollWheelEvent,
    SharedString, Task, UniformListScrollHandle, WeakEntity, Window, div, img, point, prelude::*, px, uniform_list,
};

use crate::editor_view as ev;
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
        "csv" | "tsv" | "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => AssetKind::Table,
        "mp4" | "mov" | "m4v" | "webm" | "avi" | "mkv" => AssetKind::Video,
        "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" => AssetKind::Audio,
        "pdf" => AssetKind::Pdf,
        "zip" | "gz" | "tgz" | "dmg" | "woff" | "woff2" | "ttf" | "otf" | "exe" | "so" | "dylib" | "wasm" | "sqlite" | "db" => AssetKind::Binary,
        _ => return None,
    })
}

/// CSV/TSV: plain text, so it can also be edited in the text editor (Table/Edit toggle).
pub fn is_text_table(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("csv") || e.eq_ignore_ascii_case("tsv"))
}

pub enum AssetEvent {
    /// "Edit" clicked on a CSV table: show the text editor.
    EditSource,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragKind {
    Cells,
    Rows,
    Cols,
}

/// Rectangular cell selection (anchor = where it started, head = active cell).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CellSel {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl CellSel {
    /// (first row, last row, first col, last col), inclusive.
    fn rect(&self) -> (usize, usize, usize, usize) {
        let (a, h) = (self.anchor, self.head);
        (a.0.min(h.0), a.0.max(h.0), a.1.min(h.1), a.1.max(h.1))
    }
    fn contains(&self, r: usize, c: usize) -> bool {
        let (r0, r1, c0, c1) = self.rect();
        (r0..=r1).contains(&r) && (c0..=c1).contains(&c)
    }
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
    /// Shared with the owning editor so the tab keeps keyboard focus.
    focus: FocusHandle,
    /// Spreadsheet sheet names (empty for CSV) and the one shown.
    sheets: Vec<String>,
    sheet: usize,
    sel: Option<CellSel>,
    drag: Option<DragKind>,
    sel_info: String,
    list_scroll: UniformListScrollHandle,
    /// CSV backed by an editable text document: shows the Table/Edit toggle.
    pub csv_source: bool,
}

impl EventEmitter<AssetEvent> for AssetView {}

struct Table {
    rows: Vec<Vec<String>>,
    widths: Vec<f32>,
    truncated: bool,
}

impl AssetView {
    pub fn new(path: PathBuf, kind: AssetKind, focus: FocusHandle, cx: &mut Context<Self>) -> Self {
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
            focus,
            sheets: Vec::new(),
            sheet: 0,
            sel: None,
            drag: None,
            sel_info: String::new(),
            list_scroll: UniformListScrollHandle::new(),
            csv_source: false,
        };
        if kind == AssetKind::Image {
            pinch::register(cx.weak_entity(), cx);
        }
        if kind == AssetKind::Table {
            this.load_sheet(0, cx);
        }
        this
    }

    /// (Re)load the table from disk: CSV/TSV text, or sheet `ix` of a workbook.
    fn load_sheet(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.loading = true;
        self.sheet = ix;
        let p = self.path.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if is_text_table(&p) {
                        let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
                        Ok((Vec::new(), table_from_text(&String::from_utf8_lossy(&bytes), &p)))
                    } else {
                        load_workbook_sheet(&p, ix)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| this.apply(result.map(|(sheets, t)| (Some(sheets), t)), cx));
        }));
        cx.notify();
    }

    /// Re-parse the table from (possibly unsaved) CSV text after editing it.
    pub fn set_csv_text(&mut self, text: String, cx: &mut Context<Self>) {
        let p = self.path.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let t = cx.background_executor().spawn(async move { table_from_text(&text, &p) }).await;
            let _ = this.update(cx, |this, cx| this.apply(Ok((None, t)), cx));
        }));
    }

    fn apply(&mut self, result: Result<(Option<Vec<String>>, Table), String>, cx: &mut Context<Self>) {
        self.loading = false;
        match result {
            Ok((sheets, t)) => {
                if let Some(s) = sheets {
                    self.sheets = s;
                }
                self.error = None;
                // Keep the selection when it still fits (re-parse after an edit).
                let (rows, cols) = (t.rows.len(), t.widths.len());
                self.sel = self.sel.filter(|s| s.anchor.0.max(s.head.0) < rows && s.anchor.1.max(s.head.1) < cols);
                self.table = Some(t);
                self.update_sel_info();
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
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

fn table_from_text(text: &str, path: &Path) -> Table {
    let first = text.lines().next().unwrap_or("");
    let delim = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv")) || first.matches('\t').count() > first.matches(',').count() {
        '\t'
    } else if first.matches(';').count() > first.matches(',').count() {
        ';'
    } else {
        ','
    };
    let (rows, truncated) = parse_csv(text, delim, MAX_CSV_ROWS);
    table_from_rows(rows, truncated)
}

fn table_from_rows(rows: Vec<Vec<String>>, truncated: bool) -> Table {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![4usize; cols];
    for r in rows.iter().take(500) {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count().min(48));
        }
    }
    Table { rows, widths: widths.into_iter().map(|w| w as f32 * 7.4 + 20.).collect(), truncated }
}

/// Sheet names plus sheet `ix` of an .xlsx/.xls/.ods workbook, placed at its real
/// cell positions (so row numbers and column letters match Excel).
fn load_workbook_sheet(path: &Path, ix: usize) -> Result<(Vec<String>, Table), String> {
    use calamine::Reader;
    let mut wb = calamine::open_workbook_auto(path).map_err(|e| e.to_string())?;
    let names: Vec<String> = wb.sheet_names().iter().map(|s| s.to_string()).collect();
    let Some(name) = names.get(ix).cloned() else { return Ok((names, table_from_rows(Vec::new(), false))) };
    let range = wb.worksheet_range(&name).map_err(|e| e.to_string())?;
    let (r0, c0) = range.start().map(|(r, c)| (r as usize, c as usize)).unwrap_or((0, 0));
    let mut rows: Vec<Vec<String>> = vec![Vec::new(); r0.min(MAX_CSV_ROWS)];
    let mut truncated = false;
    for r in range.rows() {
        if rows.len() >= MAX_CSV_ROWS {
            truncated = true;
            break;
        }
        let mut row = vec![String::new(); c0.min(16_384)];
        row.extend(r.iter().map(cell_text));
        while row.last().is_some_and(|s| s.is_empty()) {
            row.pop();
        }
        rows.push(row);
    }
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    Ok((names, table_from_rows(rows, truncated)))
}

fn cell_text(d: &calamine::Data) -> String {
    use calamine::Data;
    match d {
        Data::Empty => String::new(),
        Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => fmt_number(*f),
        Data::Bool(b) => (if *b { "TRUE" } else { "FALSE" }).into(),
        Data::DateTime(dt) => {
            let v = dt.as_f64();
            match dt.as_datetime() {
                Some(t) if !dt.is_duration() && v < 1. => t.time().to_string(),
                Some(t) if !dt.is_duration() && v.fract() == 0. => t.date().to_string(),
                Some(t) if !dt.is_duration() => format!("{} {}", t.date(), t.time()),
                _ => fmt_number(v),
            }
        }
        Data::Error(_) => d.to_string(),
    }
}

fn fmt_number(f: f64) -> String {
    if f.fract() == 0. && f.abs() < 1e15 { format!("{}", f as i64) } else { format!("{f}") }
}

/// Excel-style column name: 0 → A, 25 → Z, 26 → AA.
fn col_name(mut c: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Quote a cell for TSV clipboard text (what Excel/Sheets/Numbers paste as cells).
fn tsv_field(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(['\t', '\n', '\r', '"']) { format!("\"{}\"", s.replace('"', "\"\"")).into() } else { s.into() }
}

const ROW_H: f32 = 24.;
const GUTTER_W: f32 = 56.;

// ---- table selection & keyboard -------------------------------------------------------------

impl AssetView {
    fn dims(&self) -> (usize, usize) {
        self.table.as_ref().map(|t| (t.rows.len(), t.widths.len())).unwrap_or((0, 0))
    }

    fn cell(&self, r: usize, c: usize) -> &str {
        self.table.as_ref().and_then(|t| t.rows.get(r)).and_then(|row| row.get(c)).map(|s| s.as_str()).unwrap_or("")
    }

    fn set_sel(&mut self, sel: Option<CellSel>, cx: &mut Context<Self>) {
        if self.sel != sel {
            self.sel = sel;
            self.update_sel_info();
            cx.notify();
        }
    }

    fn update_sel_info(&mut self) {
        self.sel_info.clear();
        let Some(s) = self.sel else { return };
        let (r0, r1, c0, c1) = s.rect();
        let a = format!("{}{}", col_name(c0), r0 + 1);
        let n = (r1 - r0 + 1) * (c1 - c0 + 1);
        if n == 1 {
            self.sel_info = a;
            return;
        }
        self.sel_info = format!("{a}:{}{} · {} × {}", col_name(c1), r1 + 1, r1 - r0 + 1, c1 - c0 + 1);
        // Sum/average of numeric cells (skipped for huge ranges).
        if n <= 200_000 {
            let (mut count, mut sum, mut nums) = (0usize, 0f64, 0usize);
            for r in r0..=r1 {
                for c in c0..=c1 {
                    let v = self.cell(r, c).trim();
                    if v.is_empty() {
                        continue;
                    }
                    count += 1;
                    if let Ok(x) = v.replace(',', "").parse::<f64>() {
                        sum += x;
                        nums += 1;
                    }
                }
            }
            self.sel_info.push_str(&format!(" · Count {count}"));
            if nums > 0 {
                self.sel_info.push_str(&format!(" · Sum {} · Avg {}", fmt_number((sum * 1e6).round() / 1e6), fmt_number((sum / nums as f64 * 1e6).round() / 1e6)));
            }
        }
    }

    fn press(&mut self, cell: (usize, usize), kind: DragKind, shift: bool, cx: &mut Context<Self>) {
        let (rows, cols) = self.dims();
        if rows == 0 || cols == 0 {
            return;
        }
        let (last_r, last_c) = (rows - 1, cols - 1);
        let anchor = match (shift, self.sel) {
            (true, Some(s)) => s.anchor,
            _ => match kind {
                DragKind::Cells => cell,
                DragKind::Rows => (cell.0, 0),
                DragKind::Cols => (0, cell.1),
            },
        };
        let head = match kind {
            DragKind::Cells => cell,
            DragKind::Rows => (cell.0, last_c),
            DragKind::Cols => (last_r, cell.1),
        };
        let anchor = match kind {
            DragKind::Rows => (anchor.0, 0),
            DragKind::Cols => (0, anchor.1),
            DragKind::Cells => anchor,
        };
        self.drag = Some(kind);
        self.set_sel(Some(CellSel { anchor, head }), cx);
    }

    fn drag_to(&mut self, cell: (usize, usize), cx: &mut Context<Self>) {
        let (Some(kind), Some(s)) = (self.drag, self.sel) else { return };
        let (rows, cols) = self.dims();
        let head = match kind {
            DragKind::Cells => cell,
            DragKind::Rows => (cell.0, cols.saturating_sub(1)),
            DragKind::Cols => (rows.saturating_sub(1), cell.1),
        };
        self.set_sel(Some(CellSel { anchor: s.anchor, head }), cx);
    }

    fn move_to(&mut self, f: impl FnOnce((usize, usize), usize, usize) -> (usize, usize), extend: bool, cx: &mut Context<Self>) {
        let (rows, cols) = self.dims();
        if rows == 0 || cols == 0 {
            return;
        }
        let cur = self.sel.map(|s| s.head).unwrap_or((0, 0));
        let (r, c) = f(cur, rows, cols);
        let head = (r.min(rows - 1), c.min(cols - 1));
        let anchor = if extend { self.sel.map(|s| s.anchor).unwrap_or(cur) } else { head };
        self.set_sel(Some(CellSel { anchor, head }), cx);
        self.reveal(head);
    }

    /// Scroll so the cell is visible (rows via the list, columns via the horizontal offset).
    fn reveal(&self, (r, c): (usize, usize)) {
        self.list_scroll.scroll_to_item(r + 1, ScrollStrategy::Top);
        let Some(t) = &self.table else { return };
        let base = self.list_scroll.0.borrow().base_handle.clone();
        let view_w = f32::from(base.bounds().size.width);
        if view_w <= 0. {
            return;
        }
        let off = base.offset();
        let x0 = GUTTER_W + t.widths[..c].iter().sum::<f32>();
        let x1 = x0 + t.widths[c];
        let scrolled = -f32::from(off.x);
        let new = if x0 - GUTTER_W < scrolled {
            x0 - GUTTER_W
        } else if x1 > scrolled + view_w {
            x1 - view_w
        } else {
            return;
        };
        base.set_offset(point(px(-new.max(0.)), off.y));
    }

    fn page_rows(&self) -> usize {
        let h = f32::from(self.list_scroll.0.borrow().base_handle.bounds().size.height);
        ((h / ROW_H) as usize).saturating_sub(2).max(1)
    }

    fn copy(&mut self, _: &ev::Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.sel else { return };
        let (r0, r1, c0, c1) = s.rect();
        let mut out = String::new();
        for r in r0..=r1 {
            if r > r0 {
                out.push('\n');
            }
            for c in c0..=c1 {
                if c > c0 {
                    out.push('\t');
                }
                out.push_str(&tsv_field(self.cell(r, c)));
            }
        }
        cx.write_to_clipboard(ClipboardItem::new_string(out));
    }

    fn bind_table_actions(&self, d: gpui::Stateful<gpui::Div>, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        macro_rules! nav {
            ($d:expr, $action:ty, $extend:expr, $f:expr) => {
                $d.on_action(cx.listener(|this, _: &$action, _, cx| this.move_to($f, $extend, cx)))
            };
        }
        let d = d.on_action(cx.listener(Self::copy)).on_action(cx.listener(|this, _: &ev::SelectAll, _, cx| {
            let (rows, cols) = this.dims();
            if rows > 0 && cols > 0 {
                this.set_sel(Some(CellSel { anchor: (0, 0), head: (rows - 1, cols - 1) }), cx);
            }
        }));
        let d = d
            .on_action(cx.listener(|this, _: &ev::DismissMenu, _, cx| {
                if let Some(s) = this.sel {
                    this.set_sel(Some(CellSel { anchor: s.head, head: s.head }), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ev::ToggleMarkdownLive, _, cx| {
                if this.csv_source {
                    cx.emit(AssetEvent::EditSource);
                }
            }));
        let d = nav!(d, ev::MoveLeft, false, |(r, c), _, _| (r, c.saturating_sub(1)));
        let d = nav!(d, ev::MoveRight, false, |(r, c), _, _| (r, c + 1));
        let d = nav!(d, ev::MoveUp, false, |(r, c), _, _| (r.saturating_sub(1), c));
        let d = nav!(d, ev::MoveDown, false, |(r, c), _, _| (r + 1, c));
        let d = nav!(d, ev::SelectLeft, true, |(r, c), _, _| (r, c.saturating_sub(1)));
        let d = nav!(d, ev::SelectRight, true, |(r, c), _, _| (r, c + 1));
        let d = nav!(d, ev::SelectUp, true, |(r, c), _, _| (r.saturating_sub(1), c));
        let d = nav!(d, ev::SelectDown, true, |(r, c), _, _| (r + 1, c));
        let d = nav!(d, ev::Newline, false, |(r, c), _, _| (r + 1, c));
        let d = nav!(d, ev::Indent, false, |(r, c), _, _| (r, c + 1));
        let d = nav!(d, ev::Outdent, false, |(r, c), _, _| (r, c.saturating_sub(1)));
        let d = nav!(d, ev::MoveLineStart, false, |(r, _), _, _| (r, 0));
        let d = nav!(d, ev::MoveLineEnd, false, |(r, _), _, cols| (r, cols - 1));
        let d = nav!(d, ev::SelectLineStart, true, |(r, _), _, _| (r, 0));
        let d = nav!(d, ev::SelectLineEnd, true, |(r, _), _, cols| (r, cols - 1));
        let d = nav!(d, ev::MoveDocStart, false, |(_, c), _, _| (0, c));
        let d = nav!(d, ev::MoveDocEnd, false, |(_, c), rows, _| (rows - 1, c));
        let d = nav!(d, ev::SelectDocStart, true, |(_, c), _, _| (0, c));
        let d = nav!(d, ev::SelectDocEnd, true, |(_, c), rows, _| (rows - 1, c));
        d.on_action(cx.listener(|this, _: &ev::PageUp, _, cx| {
            let n = this.page_rows();
            this.move_to(|(r, c), _, _| (r.saturating_sub(n), c), false, cx)
        }))
        .on_action(cx.listener(|this, _: &ev::PageDown, _, cx| {
            let n = this.page_rows();
            this.move_to(|(r, c), _, _| (r + n, c), false, cx)
        }))
    }

    fn render_table(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(e) = &self.error {
            return centered(e.clone()).into_any_element();
        }
        let Some(t) = &self.table else {
            return centered(if self.loading { "Loading…".into() } else { "Empty file".into() }).into_any_element();
        };
        let count = t.rows.len();
        let ncols = t.widths.len();
        let total_w: f32 = t.widths.iter().sum::<f32>() + GUTTER_W;
        let truncated = t.truncated;
        let grid = theme::rgb(0x1f2125);
        let list = uniform_list(
            "csv-rows",
            count + 1,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let t = this.table.as_ref().unwrap();
                let sel = this.sel;
                let (sr0, sr1, sc0, sc1) = sel.map(|s| s.rect()).unwrap_or((usize::MAX, 0, usize::MAX, 0));
                range
                    .map(|ix| {
                        let mut row = div().id(ix).flex().h(px(ROW_H)).w(px(total_w)).border_b_1().border_color(grid).text_size(px(12.));
                        // Column letters (click selects the column, drag extends).
                        if ix == 0 {
                            row = row.bg(theme::rgb(theme::SIDEBAR_BG)).child(
                                div().w(px(GUTTER_W)).flex_none().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                        window.focus(&this.focus);
                                        let (rows, cols) = this.dims();
                                        if rows > 0 && cols > 0 {
                                            this.set_sel(Some(CellSel { anchor: (0, 0), head: (rows - 1, cols - 1) }), cx);
                                        }
                                    }),
                                ),
                            );
                            for (c, w) in t.widths.iter().enumerate() {
                                let on = (sc0..=sc1).contains(&c);
                                row = row.child(
                                    div()
                                        .w(px(*w))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .border_l_1()
                                        .border_color(grid)
                                        .text_size(px(11.))
                                        .text_color(theme::rgb(if on { theme::TEXT } else { theme::GUTTER_TEXT }))
                                        .when(on, |d| d.bg(theme::rgb(theme::SELECTED)))
                                        .child(col_name(c))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                                window.focus(&this.focus);
                                                this.press((0, c), DragKind::Cols, e.modifiers.shift, cx);
                                            }),
                                        )
                                        .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _, cx| {
                                            if e.pressed_button == Some(MouseButton::Left) {
                                                this.drag_to((0, c), cx);
                                            }
                                        })),
                                );
                            }
                            return row;
                        }
                        let r = ix - 1;
                        let header = r == 0;
                        let row_on = (sr0..=sr1).contains(&r);
                        row = row.when(header, |d| d.font_weight(gpui::FontWeight::SEMIBOLD)).child(
                            div()
                                .w(px(GUTTER_W))
                                .flex_none()
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .justify_end()
                                .text_size(px(11.))
                                .bg(theme::rgb(if row_on { theme::SELECTED } else { theme::SIDEBAR_BG }))
                                .text_color(theme::rgb(if row_on { theme::TEXT } else { theme::GUTTER_TEXT }))
                                .child((r + 1).to_string())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                        window.focus(&this.focus);
                                        this.press((r, 0), DragKind::Rows, e.modifiers.shift, cx);
                                    }),
                                )
                                .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _, cx| {
                                    if e.pressed_button == Some(MouseButton::Left) {
                                        this.drag_to((r, 0), cx);
                                    }
                                })),
                        );
                        for (c, w) in t.widths.iter().enumerate() {
                            let text = t.rows[r].get(c).cloned().unwrap_or_default();
                            let in_sel = sel.is_some_and(|s| s.contains(r, c));
                            let active = sel.is_some_and(|s| s.head == (r, c));
                            row = row.child(
                                div()
                                    .w(px(*w))
                                    .flex_none()
                                    .px(px(8.))
                                    .flex()
                                    .items_center()
                                    .border_l_1()
                                    .border_color(grid)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .cursor(gpui::CursorStyle::Arrow)
                                    .when(header, |d| d.bg(theme::rgb(theme::SIDEBAR_BG)))
                                    .when(in_sel, |d| d.bg(theme::rgb(theme::SELECTION)))
                                    .when(active, |d| d.border_1().border_color(theme::rgb(theme::ACCENT)))
                                    .text_color(theme::rgb(if header { theme::TEXT } else { 0xc3c7ce }))
                                    .child(SharedString::from(text))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                            window.focus(&this.focus);
                                            this.press((r, c), DragKind::Cells, e.modifiers.shift, cx);
                                        }),
                                    )
                                    .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _, cx| {
                                        if e.pressed_button == Some(MouseButton::Left) {
                                            this.drag_to((r, c), cx);
                                        }
                                    })),
                            );
                        }
                        row
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.list_scroll.clone())
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .flex_1();

        let sheets = (self.sheets.len() > 1).then(|| {
            div().flex().gap(px(2.)).children(self.sheets.iter().enumerate().map(|(i, name)| {
                let on = i == self.sheet;
                div()
                    .id(("sheet", i))
                    .px(px(8.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
                    .when(!on, |d| d.hover(|s| s.text_color(theme::rgb(theme::TEXT_MUTED))))
                    .child(name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if this.sheet != i {
                            this.sel = None;
                            this.update_sel_info();
                            this.list_scroll.0.borrow().base_handle.set_offset(point(px(0.), px(0.)));
                            this.load_sheet(i, cx);
                        }
                    }))
            }))
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(12.))
                    .py(px(4.))
                    .border_t_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .text_size(px(11.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .children(sheets)
                    .child(format!("{count} rows · {ncols} columns{}", if truncated { format!(" · showing first {MAX_CSV_ROWS}") } else { String::new() }))
                    .child(div().flex_1())
                    .child(div().text_color(theme::rgb(theme::TEXT_MUTED)).child(self.sel_info.clone())),
            )
            .into_any_element()
    }
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
            .when(self.csv_source, |d| {
                d.child(
                    div()
                        .flex()
                        .gap(px(2.))
                        .child(ev::md_toggle("csv-table", "Table", true))
                        .child(ev::md_toggle("csv-edit", "Edit", false).on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(AssetEvent::EditSource)))),
                )
            })
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
            AssetKind::Table => self.render_table(cx),
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
        let root = div()
            .id("asset-view")
            .key_context("AssetView")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::rgb(theme::EDITOR_BG))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.drag = None))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, _| this.drag = None))
            .child(header)
            .child(body);
        if self.kind == AssetKind::Table { self.bind_table_actions(root, cx) } else { root }
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
        assert_eq!(asset_kind(Path::new("a.XLSX")), Some(AssetKind::Table));
        assert_eq!(asset_kind(Path::new("a.xls")), Some(AssetKind::Table));
        assert!(is_text_table(Path::new("a.csv")) && !is_text_table(Path::new("a.xlsx")));
    }

    #[test]
    fn columns_and_tsv() {
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(701), "ZZ");
        assert_eq!(col_name(702), "AAA");
        assert_eq!(tsv_field("a\tb"), "\"a\tb\"");
        assert_eq!(tsv_field("x\"y"), "\"x\"\"y\"");
        assert_eq!(tsv_field("plain"), "plain");
    }
}

