//! GPUI code editor view: rendering, keyboard/IME/mouse input, find bar, async highlighting.

use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use editor::{Document, HIGHLIGHT_NAMES, HighlightSpan, Language, Selection, highlight};
use gpui::{
    App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, ShapedLine, SharedString, Style, Subscription, Task, TextRun, Timer,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px, relative,
    size,
};

use crate::text_input::{TextInput, TextInputEvent};

mod insights;
use crate::theme;

actions!(
    editor,
    [
        MoveLeft, MoveRight, MoveUp, MoveDown, SelectLeft, SelectRight, SelectUp, SelectDown,
        MoveWordLeft, MoveWordRight, SelectWordLeft, SelectWordRight, MoveLineStart, MoveLineEnd,
        SelectLineStart, SelectLineEnd, MoveDocStart, MoveDocEnd, SelectDocStart, SelectDocEnd,
        PageUp, PageDown, Backspace, Delete, DeleteWordLeft, DeleteToLineStart, Newline, Indent,
        Outdent, SelectAll, Copy, Cut, Paste, Undo, Redo, Save, ToggleComment, Find, FindNext,
        FindPrevious, SelectLine, GoToLine, ToggleMarkdownLive, GoToDefinition, InsertTable,
        ToggleWordWrap, ExportPdf, DismissMenu, SelectNextOccurrence, SelectAllOccurrences, AddCursorAbove,
        AddCursorBelow,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let c = Some("Editor");
    cx.bind_keys([
        KeyBinding::new("left", MoveLeft, c),
        KeyBinding::new("right", MoveRight, c),
        KeyBinding::new("up", MoveUp, c),
        KeyBinding::new("down", MoveDown, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("alt-left", MoveWordLeft, c),
        KeyBinding::new("alt-right", MoveWordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("cmd-left", MoveLineStart, c),
        KeyBinding::new("home", MoveLineStart, c),
        KeyBinding::new("cmd-right", MoveLineEnd, c),
        KeyBinding::new("end", MoveLineEnd, c),
        KeyBinding::new("cmd-shift-left", SelectLineStart, c),
        KeyBinding::new("shift-home", SelectLineStart, c),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, c),
        KeyBinding::new("shift-end", SelectLineEnd, c),
        KeyBinding::new("cmd-up", MoveDocStart, c),
        KeyBinding::new("cmd-down", MoveDocEnd, c),
        KeyBinding::new("cmd-shift-up", SelectDocStart, c),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("shift-backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("tab", Indent, c),
        KeyBinding::new("shift-tab", Outdent, c),
        KeyBinding::new("cmd-]", Indent, c),
        KeyBinding::new("cmd-[", Outdent, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        KeyBinding::new("cmd-s", Save, c),
        KeyBinding::new("cmd-/", ToggleComment, c),
        KeyBinding::new("cmd-f", Find, c),
        KeyBinding::new("cmd-g", FindNext, c),
        KeyBinding::new("cmd-shift-g", FindPrevious, c),
        KeyBinding::new("cmd-l", SelectLine, c),
        KeyBinding::new("ctrl-g", GoToLine, c),
        KeyBinding::new("cmd-shift-v", ToggleMarkdownLive, c),
        KeyBinding::new("f12", GoToDefinition, c),
        KeyBinding::new("alt-z", ToggleWordWrap, c),
        KeyBinding::new("escape", DismissMenu, c),
        KeyBinding::new("cmd-d", SelectNextOccurrence, c),
        KeyBinding::new("cmd-ctrl-g", SelectAllOccurrences, c),
        KeyBinding::new("cmd-alt-up", AddCursorAbove, c),
        KeyBinding::new("cmd-alt-down", AddCursorBelow, c),
    ]);
}

pub enum EditorEvent {
    /// Dirty flag or title changed.
    Changed,
    Error(String),
    OpenLocation { path: PathBuf, line: usize, character: usize },
    /// Written to disk (Git status may have changed).
    Saved(PathBuf),
    /// Caret/selection or diagnostics changed (Claude IDE sync).
    SelectionChanged,
    /// Save requested on an untitled buffer: the workspace asks where to put it.
    SaveAs,
    /// Open a file in its own tab (WhatsApp media).
    OpenFile(PathBuf),
    /// "Pedir a Claude…" on a WhatsApp chat tab.
    AskClaude { jid: String, instruction: String },
    /// Open another WhatsApp chat (a group member from the info sheet).
    OpenChat { jid: String, name: String },
}

/// A view shown as an editor tab instead of a file (plugin details, HTTP client, agenda…).
/// `key` identifies it, so opening the same page again focuses the existing tab.
pub struct Page {
    pub key: String,
    pub title: SharedString,
    pub icon: &'static str,
    pub color: u32,
    pub view: gpui::AnyView,
    /// Focused when the tab is selected.
    pub focus: Option<FocusHandle>,
}

/// Oxfmt/Oxlint found for the repository containing this file.
#[derive(Clone, Default)]
pub struct FileTools {
    pub oxfmt: Option<code_quality::Located>,
    pub oxlint: Option<code_quality::Located>,
}

const MAX_RENDERED_LINE_CHARS: usize = 4000;

struct FindState {
    input: Entity<TextInput>,
    matches: Vec<Range<usize>>,
    current: Option<usize>,
    go_to_line: bool,
    _sub: Subscription,
}

#[derive(Clone)]
struct RenderedLine {
    line: usize,
    /// Visual row index (across the whole document, after soft wrap).
    vrow: usize,
    /// Doc char columns covered by this row: `start..end`.
    start: usize,
    end: usize,
    last_row: bool,
    shaped: ShapedLine,
    /// Byte offset in `shaped` for each char column `start..=end`.
    col_bytes: Vec<usize>,
}

impl RenderedLine {
    fn x_of(&self, col: usize) -> Pixels {
        let i = col.clamp(self.start, self.end) - self.start;
        self.shaped.x_for_index(self.col_bytes[i.min(self.col_bytes.len() - 1)])
    }
    fn contains_cursor(&self, col: usize) -> bool {
        col >= self.start && (col < self.end || (col == self.end && (self.last_row || self.start == self.end)))
    }
}

/// Soft-wrap cache: visual row where each doc line starts (prefix sums, len = lines + 1).
struct WrapCache {
    version: u64,
    cols: usize,
    tab: usize,
    row_start: Vec<usize>,
}

/// Files bigger than this are never soft-wrapped (keeps layout O(visible)).
const MAX_WRAP_LINES: usize = 50_000;

/// Char indices (doc columns) where wrapped rows after the first start. Breaks after the last
/// whitespace that fits, or mid-word when a word is longer than the row.
pub(crate) fn wrap_breaks(text: &str, tab: usize, cols: usize) -> Vec<usize> {
    let mut breaks = Vec::new();
    if cols == usize::MAX || cols == 0 {
        return breaks;
    }
    let (mut row_char, mut row_disp, mut disp) = (0usize, 0usize, 0usize);
    let mut last_space: Option<(usize, usize)> = None;
    let mut seen_text = false;
    for (i, ch) in text.chars().enumerate().take(MAX_RENDERED_LINE_CHARS) {
        let w = if ch == '\t' { tab - disp % tab } else { 1 };
        let space = ch == ' ' || ch == '\t';
        // Whitespace may hang past the edge; only visible chars force a break.
        while !space && disp + w - row_disp > cols && i > row_char {
            let (bi, bd) = match last_space {
                Some(sp) if sp.0 > row_char && sp.0 <= i => sp,
                _ => (i, disp),
            };
            breaks.push(bi);
            row_char = bi;
            row_disp = bd;
            last_space = None;
            seen_text = false;
        }
        disp += w;
        if space {
            // Never break right after leading indentation (would leave an empty row).
            if seen_text {
                last_space = Some((i + 1, disp));
            }
        } else {
            seen_text = true;
        }
    }
    breaks
}

/// Display column (tabs expanded) of each char column `0..=n` of a line.
fn display_cols(text: &str, tab: usize) -> Vec<usize> {
    let mut v = Vec::with_capacity(text.len() + 1);
    let mut d = 0;
    v.push(0);
    for ch in text.chars().take(MAX_RENDERED_LINE_CHARS) {
        d += if ch == '\t' { tab - d % tab } else { 1 };
        v.push(d);
    }
    v
}

fn slice_runs(runs: &[TextRun], start: usize, end: usize) -> Vec<TextRun> {
    let mut out = Vec::new();
    let mut at = 0;
    for r in runs {
        let (a, b) = (at.max(start), (at + r.len).min(end));
        if a < b {
            let mut r2 = r.clone();
            r2.len = b - a;
            out.push(r2);
        }
        at += r.len;
        if at >= end {
            break;
        }
    }
    out
}

#[derive(Clone)]
struct Layout {
    text_origin: Point<Pixels>,
    line_height: Pixels,
    char_width: Pixels,
    viewport: Bounds<Pixels>,
    lines: Vec<RenderedLine>,
    total_rows: usize,
    /// Inline blame annotation: (bounds, line).
    blame: Option<(Bounds<Pixels>, usize)>,
    /// Conflict actions painted on `<<<<<<<` lines: (bounds, conflict index, resolution).
    conflict_buttons: Vec<(Bounds<Pixels>, usize, editor::conflicts::Resolution)>,
}

pub struct EditorView {
    pub doc: Document,
    /// Non-text files (images, CSV, media) are shown by an asset viewer instead.
    pub asset: Option<Entity<crate::asset_view::AssetView>>,
    /// WhatsApp conversation shown in this tab instead of a file.
    pub chat: Option<Entity<crate::chat_view::ChatView>>,
    chat_title: Option<SharedString>,
    _chat_sub: Option<Subscription>,
    /// CSV/TSV: `asset` is the table view of `doc` (Table/Edit toggle).
    csv_source: bool,
    /// CSV showing the table (vs. the text editor).
    table_mode: bool,
    _asset_sub: Option<Subscription>,
    _asset_obs: Option<Subscription>,
    /// Cached dirty state of an editable asset (xlsx cells, PDF, PPTX).
    asset_dirty: bool,
    /// .docx edited as Markdown: the Word model that saves the text back into the file.
    pub docx: Option<crate::office::docx::DocxModel>,
    /// A plugin page (plugin details, HTTP client, agenda, clipboard) shown instead of a file.
    pub page: Option<Page>,
    /// Markdown/Word → corporate PDF dialog.
    export: Option<Entity<crate::md_export::ExportDialog>>,
    focus_handle: FocusHandle,
    spans: Vec<HighlightSpan>,
    spans_version: u64,
    highlight_task: Option<Task<()>>,
    scroll: Point<Pixels>,
    layout: Option<Layout>,
    marked_text: Option<String>,
    selecting: bool,
    /// Drag-select: last pointer position, editor viewport and whether the edge-scroll ticker runs.
    drag_pos: Option<Point<Pixels>>,
    drag_viewport: Option<Bounds<Pixels>>,
    drag_ticking: bool,
    drag_task: Option<Task<()>>,
    find: Option<FindState>,
    pub disk_conflict: bool,
    font: Font,
    font_size: Pixels,
    autoscroll: bool,
    /// Markdown files render live (markers hidden except on the edited line).
    pub markdown_live: bool,
    md_blocks: Vec<editor::markdown::MdBlock>,
    md_blocks_version: u64,
    pub(crate) md_layout: Option<crate::markdown_live::MdLayout>,
    /// Notion-style block picker opened by `/` on a blank line (live Markdown).
    pub(crate) slash: Option<crate::slash_menu::SlashMenu>,
    /// Last (anchor, head, version, diagnostics) reported to Claude.
    ide_fingerprint: (usize, usize, u64, usize),
    pub tools: Option<FileTools>,
    /// SVG files show the rendered image by default (Preview/Source toggle).
    svg_preview: bool,
    /// Zoom/pan of the SVG preview (natural size = the SVG's intrinsic size).
    svg_zoom: crate::zoom_view::ZoomPan,
    /// Doc version `svg_zoom.natural` was measured for.
    svg_size_ver: Option<u64>,
    /// Rasterized preview: (doc version, raster scale, image). Re-rendered sharp as the zoom changes.
    svg_raster: Option<(u64, f32, std::sync::Arc<gpui::RenderImage>)>,
    /// In-flight rasterization: (doc version, raster scale, task).
    svg_task: Option<(u64, f32, Task<()>)>,
    diagnostics: Vec<code_quality::Diagnostic>,
    hover: Option<(usize, Point<Pixels>)>,
    save_task: Option<Task<()>>,
    lint_task: Option<Task<()>>,
    /// Language servers attached to this file (tsgo, oxlint --lsp).
    lsp: Vec<std::sync::Arc<lsp_client::LspServer>>,
    lsp_task: Option<Task<()>>,
    /// Soft wrap long lines to the editor width (⌥Z toggles).
    pub word_wrap: bool,
    /// VS Code-style preview tab: replaced by the next single-click open until it's edited,
    /// saved or double-clicked.
    pub preview: bool,
    /// Bumped whenever `spans` change (cache key for the markdown layout).
    pub(crate) spans_gen: u64,
    pub(crate) md_cache: Option<crate::markdown_live::MdCache>,
    wrap_cache: Option<WrapCache>,
    /// Columns per visual row in the last layout (`usize::MAX` = no wrap).
    wrap_cols: usize,
    /// Display-column goal for visual up/down: (cursor pos it applies to, goal).
    visual_goal: Option<(usize, usize)>,
    /// Inline blame, merge conflicts and LSP hover (see `insights`).
    ins: insights::Insights,
}

impl Drop for EditorView {
    fn drop(&mut self) {
        if let Some(path) = &self.doc.path {
            for s in &self.lsp {
                let _ = s.did_close(path);
            }
        }
    }
}

impl EventEmitter<EditorEvent> for EditorView {}

impl EditorView {
    pub fn new(doc: Document, font_size: f32, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            doc,
            asset: None,
            chat: None,
            chat_title: None,
            _chat_sub: None,
            csv_source: false,
            table_mode: false,
            _asset_sub: None,
            _asset_obs: None,
            asset_dirty: false,
            docx: None,
            export: None,
            page: None,
            focus_handle: cx.focus_handle(),
            spans: Vec::new(),
            spans_version: u64::MAX,
            highlight_task: None,
            scroll: Point::default(),
            layout: None,
            marked_text: None,
            selecting: false,
            drag_pos: None,
            drag_viewport: None,
            drag_ticking: false,
            drag_task: None,
            find: None,
            disk_conflict: false,
            font: gpui::font(theme::MONO_FONT),
            font_size: px(font_size + theme::EDITOR_FONT_BONUS),
            autoscroll: false,
            markdown_live: false,
            md_blocks: Vec::new(),
            md_blocks_version: u64::MAX,
            md_layout: None,
            slash: None,
            ide_fingerprint: (usize::MAX, 0, 0, 0),
            tools: None,
            svg_preview: false,
            svg_zoom: crate::zoom_view::ZoomPan::new(None, true),
            svg_size_ver: None,
            svg_raster: None,
            svg_task: None,
            diagnostics: Vec::new(),
            hover: None,
            save_task: None,
            lint_task: None,
            lsp: Vec::new(),
            lsp_task: None,
            word_wrap: true,
            preview: false,
            spans_gen: 0,
            md_cache: None,
            wrap_cache: None,
            wrap_cols: usize::MAX,
            visual_goal: None,
            ins: Default::default(),
        };
        this.markdown_live = this.doc.language == Language::Markdown && this.doc.line_count() < 5000;
        this.svg_preview = this.is_svg();
        if this.svg_preview {
            crate::zoom_view::register_pinch(cx.weak_entity(), cx);
        }
        this.schedule_highlight(Duration::ZERO, cx);
        this
    }

    pub fn new_asset(path: PathBuf, kind: crate::asset_view::AssetKind, font_size: f32, cx: &mut Context<Self>) -> Self {
        let mut doc = Document::from_str("", Language::PlainText);
        doc.path = Some(path.clone());
        let mut this = Self::new(doc, font_size, cx);
        let focus = this.focus_handle.clone();
        let asset = cx.new(|cx| crate::asset_view::AssetView::new(path, kind, focus, cx));
        this._asset_obs = Some(cx.observe(&asset, |this, asset, cx| {
            let d = asset.read(cx).is_dirty(cx);
            if d != this.asset_dirty {
                this.asset_dirty = d;
                if !d {
                    this.preview = false;
                }
                cx.emit(EditorEvent::Changed);
            }
            cx.notify();
        }));
        this.asset = Some(asset);
        this
    }

    /// WhatsApp conversation tab.
    pub fn new_chat(jid: String, name: String, font_size: f32, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(Document::from_str("", Language::PlainText), font_size, cx);
        let chat = cx.new(|cx| crate::chat_view::ChatView::new(jid, name, cx));
        this._chat_sub = Some(cx.subscribe(&chat, |this, chat, e: &crate::chat_view::ChatEvent, cx| match e {
            crate::chat_view::ChatEvent::OpenFile(p) => cx.emit(EditorEvent::OpenFile(p.clone())),
            crate::chat_view::ChatEvent::AskClaude { jid, instruction } => cx.emit(EditorEvent::AskClaude { jid: jid.clone(), instruction: instruction.clone() }),
            crate::chat_view::ChatEvent::OpenChat { jid, name } => cx.emit(EditorEvent::OpenChat { jid: jid.clone(), name: name.clone() }),
            crate::chat_view::ChatEvent::TitleChanged => {
                this.chat_title = Some(chat.read(cx).name.clone().into());
                cx.emit(EditorEvent::Changed);
            }
            crate::chat_view::ChatEvent::Interacted => {
                if this.preview {
                    this.preview = false;
                    cx.emit(EditorEvent::Changed);
                }
            }
        }));
        this.chat_title = Some(chat.read(cx).name.clone().into());
        this.chat = Some(chat);
        this
    }

    /// A plugin page as a tab (see `Page`).
    pub fn new_page(page: Page, font_size: f32, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(Document::from_str("", Language::PlainText), font_size, cx);
        this.page = Some(page);
        this
    }

    pub fn page_key(&self) -> Option<&str> {
        self.page.as_ref().map(|p| p.key.as_str())
    }

    /// WhatsApp tab: (picture, unread count).
    pub fn chat_badge(&self, cx: &App) -> Option<(Option<PathBuf>, u32)> {
        self.chat.as_ref().map(|c| {
            let c = c.read(cx);
            (c.avatar.clone(), c.unread)
        })
    }

    pub fn chat_jid<'a>(&self, cx: &'a App) -> Option<&'a str> {
        self.chat.as_ref().map(|c| c.read(cx).jid.as_str())
    }

    /// Word document: shown and edited as live Markdown, saved back into the .docx (⌘S).
    pub fn new_docx(model: crate::office::docx::DocxModel, font_size: f32, cx: &mut Context<Self>) -> Self {
        let mut doc = Document::from_str(&model.markdown, Language::Markdown);
        doc.path = Some(model.path.clone());
        doc.mark_saved();
        let mut this = Self::new(doc, font_size, cx);
        this.markdown_live = true;
        this.docx = Some(model);
        this
    }

    /// CSV/TSV: a normal text document shown as a selectable table, with "Edit" to switch
    /// to the text editor and "Table" (or ⌘⇧V) to switch back.
    pub fn new_csv(doc: Document, font_size: f32, cx: &mut Context<Self>) -> Self {
        let path = doc.path.clone().unwrap_or_default();
        let mut this = Self::new(doc, font_size, cx);
        let focus = this.focus_handle.clone();
        let asset = cx.new(|cx| {
            let mut a = crate::asset_view::AssetView::new(path, crate::asset_view::AssetKind::Table, focus, cx);
            a.csv_source = true;
            a
        });
        this._asset_sub = Some(cx.subscribe(&asset, |this, _, e: &crate::asset_view::AssetEvent, cx| match e {
            crate::asset_view::AssetEvent::EditSource => {
                this.table_mode = false;
                this.autoscroll = true;
                cx.notify();
            }
            crate::asset_view::AssetEvent::Changed => {}
        }));
        this.asset = Some(asset);
        this.csv_source = true;
        this.table_mode = true;
        this
    }

    /// Non-text file with no editable document behind it.
    fn pure_asset(&self) -> bool {
        self.asset.is_some() && !self.csv_source
    }

    fn show_table(&mut self, cx: &mut Context<Self>) {
        self.table_mode = true;
        if let Some(a) = self.asset.clone() {
            let text = self.doc.text();
            a.update(cx, |a, cx| a.set_csv_text(text, cx));
        }
        cx.notify();
    }

    // ---- accessors for the markdown renderer -------------------------------------------

    pub(crate) fn diagnostics(&self) -> &[code_quality::Diagnostic] {
        &self.diagnostics
    }
    pub(crate) fn focus_handle_ref(&self) -> &FocusHandle {
        &self.focus_handle
    }
    pub(crate) fn font_scale(&self) -> f32 {
        (f32::from(self.font_size) - theme::EDITOR_FONT_BONUS) / theme::DEFAULT_FONT_SIZE
    }
    pub(crate) fn refresh_md_blocks(&mut self) {
        if self.md_blocks_version != self.doc.version() || self.md_blocks.len() != self.doc.line_count() {
            let lines: Vec<String> = (0..self.doc.line_count()).map(|l| self.doc.line(l).to_string()).collect();
            self.md_blocks = editor::markdown::classify(lines.iter().map(|s| s.as_str()));
            self.md_blocks_version = self.doc.version();
        }
    }
    pub(crate) fn md_blocks(&self) -> &[editor::markdown::MdBlock] {
        &self.md_blocks
    }
    pub(crate) fn current_spans(&self) -> &[HighlightSpan] {
        if self.spans_version == self.doc.version() { &self.spans } else { &[] }
    }
    pub(crate) fn take_autoscroll(&mut self) -> bool {
        std::mem::take(&mut self.autoscroll)
    }
    pub(crate) fn scroll_y(&self) -> Pixels {
        self.scroll.y
    }
    pub(crate) fn set_scroll_y(&mut self, y: Pixels) {
        self.scroll.y = y.max(px(0.));
    }
    pub(crate) fn find_ranges(&self) -> Vec<(Range<usize>, bool)> {
        self.find
            .as_ref()
            .filter(|f| !f.go_to_line)
            .map(|f| f.matches.iter().enumerate().map(|(i, m)| (m.clone(), Some(i) == f.current)).collect())
            .unwrap_or_default()
    }
    pub(crate) fn marked(&self) -> Option<String> {
        self.marked_text.clone()
    }
    pub(crate) fn set_md_layout(&mut self, l: crate::markdown_live::MdLayout) {
        self.md_layout = Some(l);
    }

    fn is_svg(&self) -> bool {
        self.doc.path.as_ref().and_then(|p| p.extension()).is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    }

    /// Rasterized SVG for the preview at the current zoom (`dpr` = window scale factor).
    /// Returns the latest finished raster (maybe stale) while a sharper one renders.
    fn svg_raster(&mut self, dpr: f32, cx: &mut Context<Self>) -> Option<std::sync::Arc<gpui::RenderImage>> {
        let v = self.doc.version();
        if self.svg_size_ver != Some(v) {
            self.svg_size_ver = Some(v);
            self.svg_zoom.natural = cx.svg_renderer().svg_size(self.doc.text().as_bytes()).ok().filter(|(w, h)| *w > 0. && *h > 0.);
        }
        let (w, h) = self.svg_zoom.natural?;
        // Next power of two ≥ on-screen pixels per SVG unit, capped at ~16 MP.
        let want = (self.svg_zoom.current_scale() * dpr).max(1. / 64.);
        let scale = 2f32.powf(want.log2().ceil()).min((16_000_000. / (w * h)).sqrt());
        let current = self.svg_raster.as_ref().is_some_and(|(rv, rs, _)| *rv == v && *rs == scale);
        let pending = self.svg_task.as_ref().is_some_and(|(tv, ts, _)| *tv == v && *ts == scale);
        if !current && !pending {
            let bytes = self.doc.text().into_bytes();
            let renderer = cx.svg_renderer();
            let task = cx.spawn(async move |this, cx| {
                let image = cx.background_executor().spawn(async move { renderer.render_image(&bytes, scale).ok() }).await;
                let _ = this.update(cx, |this, cx| {
                    this.svg_task = None;
                    if let Some(image) = image {
                        if let Some((_, _, old)) = this.svg_raster.replace((v, scale, image)) {
                            cx.drop_image(old, None);
                        }
                    }
                    cx.notify();
                });
            });
            self.svg_task = Some((v, scale, task));
        }
        self.svg_raster.as_ref().map(|(_, _, i)| i.clone())
    }

    // ---- markdown tables ------------------------------------------------------------------

    /// Line range of the table containing the caret, if any.
    fn current_table(&mut self) -> Option<std::ops::Range<usize>> {
        if self.doc.language != Language::Markdown {
            return None;
        }
        self.refresh_md_blocks();
        let (line, _) = self.doc.line_col(self.doc.selection.head);
        let is_table = |l: usize| self.md_blocks.get(l) == Some(&editor::markdown::MdBlock::Table);
        if !is_table(line) {
            return None;
        }
        let mut a = line;
        while a > 0 && is_table(a - 1) {
            a -= 1;
        }
        let mut b = line + 1;
        while is_table(b) {
            b += 1;
        }
        Some(a..b)
    }

    /// Apply a structural edit to the table under the caret and put the caret in `target`
    /// (data row, column) afterwards.
    fn edit_table(&mut self, cx: &mut Context<Self>, op: impl FnOnce(&mut editor::markdown::Table, usize, usize) -> (usize, usize)) {
        use editor::markdown::{is_separator_row, parse_table, row_cells};
        let Some(range) = self.current_table() else { return };
        let lines: Vec<String> = range.clone().map(|l| self.doc.line(l).to_string()).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let sep = refs.iter().position(|l| is_separator_row(l));
        let (line, col) = self.doc.line_col(self.doc.selection.head);
        let in_table = line - range.start;
        let row = editor::markdown::data_row(in_table, sep).min(refs.len().saturating_sub(2));
        let cell = row_cells(&lines[in_table]).iter().position(|r| col <= r.end).unwrap_or(0);
        let mut table = parse_table(&refs);
        let (trow, tcol) = op(&mut table, row, cell);
        let new_lines = table.to_markdown();
        let start = self.doc.pos(range.start, 0);
        let end = self.doc.pos(range.end - 1, usize::MAX);
        let text = new_lines.join("\n");
        let target_line = range.start + if trow == 0 { 0 } else { trow + 1 };
        let target_col = new_lines
            .get(target_line - range.start)
            .and_then(|l| row_cells(l).get(tcol.min(table.cols() - 1)).map(|r| r.start))
            .unwrap_or(2);
        self.run(cx, move |d| {
            d.selection = editor::Selection { anchor: start, head: end };
            d.insert(&text);
            let p = d.pos(target_line, target_col);
            d.set_cursor(p, false);
        });
    }

    /// Tab / Shift-Tab inside a table: move between cells (adds a row at the end).
    fn table_tab(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        use editor::markdown::{is_separator_row, row_cells};
        let Some(range) = self.current_table() else { return false };
        let (line, col) = self.doc.line_col(self.doc.selection.head);
        let text = self.doc.line(line).to_string();
        let cells = row_cells(&text);
        let idx = cells.iter().position(|r| col <= r.end).unwrap_or(0);
        let target = if forward {
            if idx + 1 < cells.len() {
                Some((line, cells[idx + 1].clone()))
            } else {
                let mut next = line + 1;
                while next < range.end && is_separator_row(&self.doc.line(next)) {
                    next += 1;
                }
                (next < range.end).then(|| (next, row_cells(&self.doc.line(next)).first().cloned().unwrap_or(2..2)))
            }
        } else if idx > 0 {
            Some((line, cells[idx - 1].clone()))
        } else {
            let mut prev = line;
            loop {
                if prev == range.start {
                    break None;
                }
                prev -= 1;
                if !is_separator_row(&self.doc.line(prev)) {
                    break Some((prev, row_cells(&self.doc.line(prev)).last().cloned().unwrap_or(2..2)));
                }
            }
        };
        match target {
            Some((l, r)) => {
                let (a, b) = (self.doc.pos(l, r.start), self.doc.pos(l, r.end));
                self.doc.selection = editor::Selection { anchor: a, head: b };
                self.autoscroll = true;
                cx.notify();
            }
            None if forward => {
                let rows = range.len();
                self.edit_table(cx, |t, _, _| {
                    t.insert_row(t.rows.len());
                    let _ = rows;
                    (t.rows.len() - 1, 0)
                });
            }
            None => {}
        }
        true
    }

    /// Enter inside a table: same column, next row (creating it at the end).
    fn table_enter(&mut self, cx: &mut Context<Self>) -> bool {
        use editor::markdown::{is_separator_row, row_cells};
        let Some(range) = self.current_table() else { return false };
        let (line, col) = self.doc.line_col(self.doc.selection.head);
        let idx = row_cells(&self.doc.line(line)).iter().position(|r| col <= r.end).unwrap_or(0);
        let mut next = line + 1;
        while next < range.end && is_separator_row(&self.doc.line(next)) {
            next += 1;
        }
        if next < range.end {
            let cells = row_cells(&self.doc.line(next));
            let r = cells.get(idx).or(cells.last()).cloned().unwrap_or(2..2);
            let p = self.doc.pos(next, r.end);
            self.doc.set_cursor(p, false);
            self.autoscroll = true;
            cx.notify();
        } else {
            self.edit_table(cx, |t, _, c| {
                t.insert_row(t.rows.len());
                (t.rows.len() - 1, c)
            });
        }
        true
    }

    fn insert_table(&mut self, _: &InsertTable, _: &mut Window, cx: &mut Context<Self>) {
        if self.doc.language != Language::Markdown {
            return;
        }
        let (line, _) = self.doc.line_col(self.doc.selection.head);
        let at = self.doc.pos(line, usize::MAX);
        let prefix = if self.doc.line_len(line) == 0 { "" } else { "\n\n" };
        let text = format!("{prefix}{}", editor::markdown::NEW_TABLE.trim_end());
        let first_cell_line = line + if prefix.is_empty() { 0 } else { 2 };
        self.run(cx, move |d| {
            d.set_cursor(at, false);
            d.insert(&text);
            let p = d.pos(first_cell_line, 2);
            d.set_cursor(p, false);
        });
    }

    fn render_table_toolbar(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.markdown_live {
            return None;
        }
        self.current_table()?;
        let (line, _) = self.doc.line_col(self.doc.selection.head);
        let (top, left, _) = self.md_layout.as_ref()?.table_top(line)?;
        let btn = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(7.))
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(11.5))
                .text_color(theme::rgb(theme::TEXT_MUTED))
                .hover(|s| s.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
                .child(label)
        };
        Some(
            div()
                .absolute()
                .top((top - px(30.)).max(px(2.)))
                .left(left)
                .occlude()
                // Don't let the editor's mouse-down move the caret out of the table first.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .flex()
                .gap(px(2.))
                .p(px(2.))
                .rounded(px(6.))
                .bg(theme::rgb(0x202227))
                .border_1()
                .border_color(theme::rgb(0x33363d))
                .shadow_md()
                .child(btn("tbl-row-above", "+ Row above").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.edit_table(cx, |t, r, c| {
                    let at = r.max(1);
                    t.insert_row(at);
                    (at, c)
                }))))
                .child(btn("tbl-row-below", "+ Row below").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.edit_table(cx, |t, r, c| {
                    t.insert_row(r + 1);
                    (r + 1, c)
                }))))
                .child(btn("tbl-col-left", "+ Col left").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.edit_table(cx, |t, r, c| {
                    t.insert_col(c);
                    (r, c)
                }))))
                .child(btn("tbl-col-right", "+ Col right").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.edit_table(cx, |t, r, c| {
                    t.insert_col(c + 1);
                    (r, c + 1)
                }))))
                .child(btn("tbl-del-row", "− Row").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.edit_table(cx, |t, r, c| {
                    t.delete_row(r);
                    (r.min(t.rows.len() - 1), c)
                }))))
                .child(btn("tbl-del-col", "− Col").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.edit_table(cx, |t, r, c| {
                    t.delete_col(c);
                    (r, c.min(t.cols() - 1))
                }))))
                .child(btn("tbl-format", "Align").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.edit_table(cx, |_, r, c| (r, c)))))
                .into_any_element(),
        )
    }

    fn toggle_markdown_live(&mut self, _: &ToggleMarkdownLive, _: &mut Window, cx: &mut Context<Self>) {
        if self.csv_source {
            self.show_table(cx);
            return;
        }
        if self.is_svg() {
            self.svg_preview = !self.svg_preview;
            cx.notify();
            return;
        }
        if self.doc.language == Language::Markdown {
            self.doc.clear_extra();
            self.markdown_live = !self.markdown_live;
            self.autoscroll = true;
            cx.notify();
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.doc.path.as_ref()
    }

    pub fn title(&self) -> SharedString {
        if let Some(name) = &self.chat_title {
            return name.clone();
        }
        if let Some(p) = &self.page {
            return p.title.clone();
        }
        self.doc
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".into())
            .into()
    }

    pub fn is_dirty(&self) -> bool {
        self.doc.is_dirty() || self.asset_dirty
    }

    /// New buffer (⌘N) that has never been saved.
    pub fn is_untitled(&self) -> bool {
        self.doc.path.is_none() && self.asset.is_none() && self.chat.is_none() && self.page.is_none()
    }

    /// Give an untitled buffer its file: the language follows the new extension.
    pub fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.doc.language = Language::from_path(&path);
        self.doc.path = Some(path);
        self.spans.clear();
        self.spans_version = u64::MAX;
        self.md_blocks_version = u64::MAX;
        self.md_cache = None;
        self.markdown_live = self.doc.language == Language::Markdown && self.doc.line_count() < 5000;
        self.svg_preview = false;
        self.schedule_highlight(Duration::ZERO, cx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.font_size = px(size + theme::EDITOR_FONT_BONUS);
        cx.notify();
    }

    pub fn go_to(&mut self, line: usize, col: usize, cx: &mut Context<Self>) {
        self.doc.clear_extra();
        let pos = self.doc.pos(line, col);
        self.doc.set_cursor(pos, false);
        self.autoscroll = true;
        cx.notify();
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_in(None, cx)
    }

    fn save_in(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) -> bool {
        if self.pure_asset() {
            let Some(asset) = self.asset.clone() else { return true };
            let r = asset.update(cx, |a, cx| a.save(window, cx));
            return match r {
                Ok(()) => {
                    self.asset_dirty = asset.read(cx).is_dirty(cx);
                    self.preview = false;
                    cx.emit(EditorEvent::Changed);
                    if let Some(p) = self.doc.path.clone() {
                        cx.emit(EditorEvent::Saved(p));
                    }
                    cx.notify();
                    true
                }
                Err(e) => {
                    cx.emit(EditorEvent::Error(e));
                    false
                }
            };
        }
        if let Some(model) = self.docx.as_mut() {
            let text = self.doc.text();
            return match model.save(&text) {
                Ok(()) => {
                    self.doc.mark_saved();
                    self.disk_conflict = false;
                    self.preview = false;
                    cx.emit(EditorEvent::Changed);
                    if let Some(p) = self.doc.path.clone() {
                        cx.emit(EditorEvent::Saved(p));
                    }
                    cx.notify();
                    true
                }
                Err(e) => {
                    cx.emit(EditorEvent::Error(format!("Couldn't save the Word document: {e}")));
                    false
                }
            };
        }
        if self.is_untitled() {
            cx.emit(EditorEvent::SaveAs);
            return false;
        }
        if let Some(path) = &self.doc.path {
            for s in &self.lsp {
                let _ = s.did_save(path);
            }
        }
        match self.doc.save() {
            Ok(()) => {
                self.disk_conflict = false;
                self.preview = false;
                self.schedule_blame(Duration::from_millis(300), cx);
                cx.emit(EditorEvent::Changed);
                if let Some(p) = self.doc.path.clone() {
                    cx.emit(EditorEvent::Saved(p));
                }
                cx.notify();
                true
            }
            Err(e) => {
                cx.emit(EditorEvent::Error(format!("{e:#}")));
                false
            }
        }
    }

    /// Called when the window regains focus: pick up external modifications.
    pub fn check_disk(&mut self, cx: &mut Context<Self>) {
        if self.pure_asset() || !self.doc.changed_on_disk() {
            return;
        }
        if self.docx.is_some() {
            if self.doc.is_dirty() {
                self.disk_conflict = true;
            } else if let Some(path) = self.doc.path.clone() {
                match crate::office::docx::DocxModel::load(&path) {
                    Ok(m) => {
                        self.doc.replace_all_keep_cursor(&m.markdown);
                        self.doc.mark_saved();
                        self.docx = Some(m);
                    }
                    Err(e) => cx.emit(EditorEvent::Error(e)),
                }
            }
            cx.notify();
            return;
        }
        if self.doc.is_dirty() {
            self.disk_conflict = true;
        } else if let Err(e) = self.doc.reload() {
            cx.emit(EditorEvent::Error(format!("{e:#}")));
        } else {
            self.schedule_highlight(Duration::ZERO, cx);
            if self.csv_source && self.table_mode {
                self.show_table(cx);
            }
        }
        cx.notify();
    }

    fn reload_from_disk(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.doc.reload() {
            Ok(()) => {
                self.disk_conflict = false;
                self.schedule_highlight(Duration::ZERO, cx);
                self.schedule_blame(Duration::ZERO, cx);
                cx.emit(EditorEvent::Changed);
            }
            Err(e) => cx.emit(EditorEvent::Error(format!("{e:#}"))),
        }
        cx.notify();
    }

    fn schedule_highlight(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let version = self.doc.version();
        let language = self.doc.language;
        if language == Language::PlainText {
            return;
        }
        let text = self.doc.text();
        self.highlight_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                Timer::after(delay).await;
            }
            let spans = cx.background_executor().spawn(async move { highlight(language, &text) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.doc.version() == version {
                    this.spans = spans;
                    this.spans_version = version;
                    this.spans_gen += 1;
                    cx.notify();
                }
            });
        }));
    }

    /// Run a document command, then autoscroll, re-highlight and notify as needed.
    pub(crate) fn run(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Document)) {
        let before = (self.doc.version(), self.doc.is_dirty());
        let sel_before = self.doc.selection.range().start;
        let len_before = self.doc.rope().len_bytes();
        f(&mut self.doc);
        self.autoscroll = true;
        if self.slash.is_some() {
            self.update_slash_menu(None);
        }
        if self.doc.version() != before.0 {
            if self.preview {
                self.preview = false;
                cx.emit(EditorEvent::Changed);
            }
            // Keep the previous highlighting, shifted past the edit, until the fresh parse
            // arrives. Avoids the text flashing white on every keystroke.
            if self.spans_version == before.0 {
                let pos_char = sel_before.min(self.doc.selection.range().start).min(self.doc.len_chars());
                let edit = self.doc.rope().char_to_byte(pos_char);
                let delta = self.doc.rope().len_bytes() as isize - len_before as isize;
                shift_spans(&mut self.spans, edit, delta);
                self.spans_version = self.doc.version();
                self.spans_gen += 1;
            }
            self.schedule_highlight(Duration::from_millis(30), cx);
            self.schedule_lsp_sync(Duration::from_millis(250), cx);
            self.schedule_blame(Duration::from_millis(1200), cx);
            self.clear_lsp_hover(cx);
            if self.find.is_some() {
                self.update_find_matches(false, cx);
            }
        }
        if self.doc.is_dirty() != before.1 {
            cx.emit(EditorEvent::Changed);
        }
        cx.notify();
    }

    /// Rebuild the visual-row table if the text, width or tab size changed.
    fn ensure_wrap(&mut self, cols: usize) {
        let tab = self.doc.tab_size;
        let ok = self.wrap_cache.as_ref().is_some_and(|c| c.version == self.doc.version() && c.cols == cols && c.tab == tab);
        if ok {
            return;
        }
        let n = self.doc.line_count();
        let mut row_start = Vec::with_capacity(n + 1);
        let mut acc = 0usize;
        for l in 0..n {
            row_start.push(acc);
            acc += if cols == usize::MAX { 1 } else { wrap_breaks(&self.doc.line(l), tab, cols).len() + 1 };
        }
        row_start.push(acc);
        self.wrap_cache = Some(WrapCache { version: self.doc.version(), cols, tab, row_start });
    }

    fn row_starts(&self) -> &[usize] {
        self.wrap_cache.as_ref().map(|c| c.row_start.as_slice()).unwrap_or(&[])
    }

    /// Doc line containing visual row `vrow`.
    fn line_of_row(&self, vrow: usize) -> usize {
        let rs = self.row_starts();
        if rs.len() < 2 {
            return vrow.min(self.doc.line_count().saturating_sub(1));
        }
        rs.partition_point(|&s| s <= vrow).saturating_sub(1).min(rs.len() - 2)
    }

    /// Row starts (doc columns) of one line, including 0.
    fn line_rows(&self, line: usize) -> Vec<usize> {
        let mut v = vec![0];
        v.extend(wrap_breaks(&self.doc.line(line), self.doc.tab_size, self.wrap_cols));
        v
    }

    fn toggle_word_wrap(&mut self, _: &ToggleWordWrap, _: &mut Window, cx: &mut Context<Self>) {
        self.word_wrap = !self.word_wrap;
        self.scroll.x = px(0.);
        self.autoscroll = true;
        cx.notify();
    }

    /// Markdown live view draws a single caret: keep only the primary one there.
    fn single_cursor_in_live(&mut self, cx: &mut Context<Self>) {
        if self.markdown_live && self.doc.has_multi() {
            self.doc.clear_extra();
            cx.notify();
        }
    }

    /// Up/down by visual row when soft wrap is active (like VS Code).
    fn move_visual(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if !extend && self.slash_key(if delta < 0 { crate::slash_menu::SlashKey::Up } else { crate::slash_menu::SlashKey::Down }, cx) {
            return;
        }
        if self.wrap_cols == usize::MAX || self.markdown_live || self.doc.has_multi() {
            self.run(cx, |d| d.each_cursor(|d| d.move_vertical(delta, extend)));
            return;
        }
        let head = self.doc.selection.head;
        let (line, col) = self.doc.line_col(head);
        let tab = self.doc.tab_size;
        let rows = self.line_rows(line);
        let r = rows.partition_point(|&s| s <= col).saturating_sub(1);
        let dc = display_cols(&self.doc.line(line), tab);
        let goal = match self.visual_goal {
            Some((p, g)) if p == head => g,
            _ => dc[col.min(dc.len() - 1)] - dc[rows[r].min(dc.len() - 1)],
        };
        let (tline, trow) = if delta < 0 {
            if r > 0 {
                (line, r - 1)
            } else if line > 0 {
                (line - 1, self.line_rows(line - 1).len() - 1)
            } else {
                self.visual_goal = None;
                self.run(cx, |d| d.move_vertical(delta, extend));
                return;
            }
        } else if r + 1 < rows.len() {
            (line, r + 1)
        } else if line + 1 < self.doc.line_count() {
            (line + 1, 0)
        } else {
            self.visual_goal = None;
            self.run(cx, |d| d.move_vertical(delta, extend));
            return;
        };
        let trows = self.line_rows(tline);
        let tdc = display_cols(&self.doc.line(tline), tab);
        let len = self.doc.line_len(tline).min(tdc.len() - 1);
        let start = trows[trow].min(len);
        let last = trow + 1 == trows.len();
        let end = if last { len } else { trows[trow + 1].saturating_sub(1).max(start) };
        let mut tcol = start;
        while tcol < end && tdc[tcol + 1] - tdc[start] <= goal {
            tcol += 1;
        }
        // Snap to the nearer side of the char the goal falls in.
        if tcol < end && goal > tdc[tcol] - tdc[start] && (tdc[tcol + 1] - tdc[start]) - goal < goal - (tdc[tcol] - tdc[start]) {
            tcol += 1;
        }
        let pos = self.doc.pos(tline, tcol);
        self.run(cx, |d| d.set_cursor(pos, extend));
        self.visual_goal = Some((self.doc.selection.head, goal));
    }

    fn page_lines(&self) -> isize {
        self.layout
            .as_ref()
            .map(|l| ((l.viewport.size.height / l.line_height) as isize - 2).max(1))
            .unwrap_or(20)
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.doc.has_multi() {
            // One line per cursor; pasting with the same number of cursors splits it back.
            cx.write_to_clipboard(ClipboardItem::new_string(self.doc.selections_text().join("\n")));
            return;
        }
        let text = if self.doc.selection.is_empty() {
            // Copy the whole line when nothing is selected (common editor behavior).
            let (line, _) = self.doc.line_col(self.doc.selection.head);
            format!("{}\n", self.doc.line(line))
        } else {
            self.doc.selected_text()
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn cut(&mut self, _: &Cut, w: &mut Window, cx: &mut Context<Self>) {
        if self.doc.has_multi() {
            self.copy(&Copy, w, cx);
            self.run(cx, |d| d.each_cursor(|d| d.insert("")));
            return;
        }
        if self.doc.selection.is_empty() {
            let head = self.doc.selection.head;
            self.doc.select_line_at(head);
        }
        self.copy(&Copy, w, cx);
        self.run(cx, |d| d.insert(""));
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
            let pieces: Vec<String> = text.split('\n').map(str::to_string).collect();
            self.run(cx, |d| {
                if !d.insert_per_cursor(&pieces) {
                    d.each_cursor(|d| d.insert(&text));
                }
            });
        }
    }

    /// ⌘S: format with oxfmt (if the repo has it) as an undoable edit, save, then lint.
    fn export_pdf(&mut self, _: &ExportPdf, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.language != Language::Markdown {
            return;
        }
        let source = self.doc.path.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("untitled.md"));
        let text = self.doc.text();
        let dialog = cx.new(|cx| crate::md_export::ExportDialog::new(source, text, window, cx));
        cx.subscribe_in(&dialog, window, |this, _, e: &crate::md_export::ExportEvent, window, cx| match e {
            crate::md_export::ExportEvent::Close => {
                this.export = None;
                window.focus(&this.focus_handle);
                cx.notify();
            }
            crate::md_export::ExportEvent::Done(p) => {
                this.export = None;
                window.focus(&this.focus_handle);
                cx.open_with_system(p);
                cx.notify();
            }
            crate::md_export::ExportEvent::Error(_) => {}
        })
        .detach();
        self.export = Some(dialog);
        cx.notify();
    }

    fn save_action(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        if self.pure_asset() || self.docx.is_some() {
            self.save_in(Some(window), cx);
            return;
        }
        let fmt = self.tools.as_ref().and_then(|t| t.oxfmt.clone());
        let (Some(fmt), Some(path)) = (fmt, self.doc.path.clone()) else {
            if self.save(cx) {
                self.lint(cx);
            }
            return;
        };
        if self.pure_asset() {
            return;
        }
        let text = self.doc.text();
        let version = self.doc.version();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { code_quality::format_stdin(&fmt, &path, &text) }).await;
            let _ = this.update(cx, |this, cx| {
                // Only apply if the user didn't type meanwhile.
                if this.doc.version() == version {
                    if let Ok(Some(formatted)) = &result {
                        this.run(cx, |d| d.replace_all_keep_cursor(formatted));
                    }
                }
                if this.save(cx) {
                    this.lint(cx);
                }
            });
        }));
    }

    pub fn attach_lsp(&mut self, server: std::sync::Arc<lsp_client::LspServer>, cx: &mut Context<Self>) {
        let Some(path) = self.doc.path.clone() else { return };
        if self.lsp.iter().any(|s| std::sync::Arc::ptr_eq(s, &server)) {
            return;
        }
        let _ = server.did_open(&path, crate::language_servers::language_id(&path), self.doc.version() as i64, &self.doc.text());
        self.lsp.push(server);
        self.schedule_lsp_sync(Duration::ZERO, cx);
    }

    fn has_oxlint_lsp(&self) -> bool {
        self.lsp.iter().any(|s| s.name == "oxlint")
    }

    /// Send the buffer to the language servers (debounced) and collect fresh diagnostics.
    fn schedule_lsp_sync(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if self.lsp.is_empty() {
            return;
        }
        let Some(path) = self.doc.path.clone() else { return };
        let servers = self.lsp.clone();
        let version = self.doc.version();
        self.lsp_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                Timer::after(delay).await;
            }
            let Ok(text) = this.update(cx, |this, _| this.doc.text()) else { return };
            for s in &servers {
                let _ = s.did_change(&path, version as i64, &text);
            }
            let mut all = Vec::new();
            for s in &servers {
                if let Ok(d) = s.diagnostics(&path).await {
                    all.extend(d.into_iter().map(|d| (s.name.clone(), d)));
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.doc.version() != version {
                    return; // a newer sync is on its way
                }
                let lsp_diags: Vec<code_quality::Diagnostic> = all
                    .into_iter()
                    .filter(|(_, d)| d.severity <= 2)
                    .map(|(name, d)| code_quality::Diagnostic {
                        file: path.clone(),
                        line: d.line + 1,
                        column: d.character + 1,
                        length: if d.end_line == d.line { d.end_character.saturating_sub(d.character) } else { this.doc.line_len(d.line).saturating_sub(d.character) },
                        severity: if d.severity == 1 { code_quality::Severity::Error } else { code_quality::Severity::Warning },
                        message: d.message,
                        code: Some(match d.code {
                            Some(c) if name == "tsgo" => format!("ts({c})"),
                            Some(c) => c,
                            None => name.clone(),
                        }),
                        help: None,
                    })
                    .collect();
                // Keep CLI-lint results only if oxlint isn't running as a server.
                let keep_cli = !this.has_oxlint_lsp();
                let mut merged: Vec<code_quality::Diagnostic> = if keep_cli {
                    this.diagnostics.iter().filter(|d| !d.code.as_deref().is_some_and(|c| c.starts_with("ts("))).cloned().collect()
                } else {
                    Vec::new()
                };
                merged.extend(lsp_diags);
                this.diagnostics = merged;
                this.hover = None;
                cx.emit(EditorEvent::Changed);
                cx.notify();
            });
        }));
    }

    /// ⌘-click on a URL opens it in the browser; on a file path (relative to this file's folder
    /// or any folder above it), opens the file.
    fn open_link_at(&mut self, pos: usize, cx: &mut Context<Self>) -> bool {
        let (line, col) = self.doc.line_col(pos);
        let text = self.doc.line(line).to_string();
        let text = text.trim_end_matches(['\n', '\r']).to_string();
        if let Some(url) = crate::terminal_view::url_at(std::slice::from_ref(&text), &[false], 0, col) {
            crate::music::open_url(&url, cx);
            return true;
        }
        let chars: Vec<char> = text.chars().collect();
        let Some((a, b)) = crate::terminal_view::token_at(&chars, col) else { return false };
        let tok: String = chars[a..b].iter().collect();
        if !tok.contains('/') && !tok.contains('.') {
            return false;
        }
        let dir = self.doc.path.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let found = match dir {
            Some(d) => d.ancestors().find_map(|anc| crate::terminal_view::resolve_path(&tok, Some(anc))),
            None => crate::terminal_view::resolve_path(&tok, None),
        };
        let Some((path, l, c)) = found else { return false };
        if self.doc.path.as_ref() == Some(&path) && l.is_none() {
            return false;
        }
        if path.is_dir() {
            cx.reveal_path(&path);
        } else {
            cx.emit(EditorEvent::OpenLocation { path, line: l.unwrap_or(1).saturating_sub(1), character: c.unwrap_or(1).saturating_sub(1) });
        }
        true
    }

    fn go_to_definition(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.lsp.iter().find(|s| s.name == "tsgo").cloned() else { return };
        let Some(path) = self.doc.path.clone() else { return };
        let (line, col) = self.doc.line_col(self.doc.selection.head);
        // LSP positions are UTF-16 code units.
        let character: usize = self.doc.line(line).chars().take(col).map(|c| c.len_utf16()).sum();
        let text = self.doc.text();
        let version = self.doc.version() as i64;
        cx.spawn(async move |this, cx| {
            let _ = server.did_change(&path, version, &text);
            if let Ok(locs) = server.definition(&path, line, character).await {
                if let Some(l) = locs.into_iter().next() {
                    let _ = this.update(cx, |_, cx| cx.emit(EditorEvent::OpenLocation { path: l.path, line: l.line, character: l.character }));
                }
            }
        })
        .detach();
    }

    /// Lint this file with oxlint and show the results inline.
    pub fn lint(&mut self, cx: &mut Context<Self>) {
        if self.has_oxlint_lsp() {
            return; // live diagnostics already come from the oxlint language server
        }
        let Some(tool) = self.tools.as_ref().and_then(|t| t.oxlint.clone()) else { return };
        let Some(path) = self.doc.path.clone().filter(|p| code_quality::lintable(p)) else { return };
        self.lint_task = Some(cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx.background_executor().spawn(async move { code_quality::lint(&tool, &[p]) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(diags) = result {
                    let mut merged: Vec<code_quality::Diagnostic> = this.diagnostics.iter().filter(|d| d.code.as_deref().is_some_and(|c| c.starts_with("ts("))).cloned().collect();
                    merged.extend(diags.into_iter().filter(|d| d.file == path));
                    this.diagnostics = merged;
                    this.hover = None;
                    cx.emit(EditorEvent::Changed);
                    cx.notify();
                }
            });
        }));
    }

    pub fn set_tools(&mut self, tools: FileTools, cx: &mut Context<Self>) {
        self.tools = Some(tools);
        // Show existing problems as soon as the file is opened.
        self.lint(cx);
    }

    pub fn problem_counts(&self) -> (usize, usize) {
        let e = self.diagnostics.iter().filter(|d| d.severity == code_quality::Severity::Error).count();
        (e, self.diagnostics.len() - e)
    }

    fn diagnostic_at(&self, pos: usize) -> Option<usize> {
        let (line, col) = self.doc.line_col(pos);
        self.diagnostics.iter().position(|d| {
            let start = d.column.saturating_sub(1);
            d.line.saturating_sub(1) == line && col >= start && col <= start + d.length.max(1)
        })
    }

    // ---- find -----------------------------------------------------------------------------

    fn open_find(&mut self, go_to_line: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.as_ref().is_some_and(|f| f.go_to_line != go_to_line) {
            self.find = None;
        }
        if self.find.is_none() {
            let placeholder = if go_to_line { "Go to line…" } else { "Find" };
            let input = cx.new(|cx| TextInput::new(placeholder, cx));
            let sub = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                TextInputEvent::Changed => this.update_find_matches(true, cx),
                TextInputEvent::Submit { shift } => {
                    if this.find.as_ref().is_some_and(|f| f.go_to_line) {
                        this.close_find(window, cx);
                    } else {
                        this.find_step(!*shift, cx);
                    }
                }
                TextInputEvent::Cancel => this.close_find(window, cx),
            });
            self.find = Some(FindState { input, matches: Vec::new(), current: None, go_to_line, _sub: sub });
        }
        let find = self.find.as_ref().unwrap();
        let input = find.input.clone();
        if !go_to_line && !self.doc.selection.is_empty() {
            let sel = self.doc.selected_text();
            if !sel.contains('\n') {
                input.update(cx, |i, cx| i.set_text(&sel, cx));
            }
        } else {
            input.update(cx, |i, cx| i.select_all_text(cx));
        }
        window.focus(&input.focus_handle(cx));
        self.update_find_matches(true, cx);
        cx.notify();
    }

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    fn update_find_matches(&mut self, jump: bool, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else { return };
        let query = find.input.read(cx).content.clone();
        if find.go_to_line {
            let mut parts = query.trim().splitn(2, ':');
            let line = parts.next().and_then(|s| s.parse::<usize>().ok());
            let col = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
            if let Some(line) = line {
                let pos = self.doc.pos(line.saturating_sub(1), col.saturating_sub(1));
                self.doc.set_cursor(pos, false);
                self.autoscroll = true;
            }
            cx.notify();
            return;
        }
        find.matches = self.doc.find_all(&query);
        let head = self.doc.selection.range().start;
        find.current = find.matches.iter().position(|m| m.start >= head).or((!find.matches.is_empty()).then_some(0));
        if jump {
            if let Some(m) = find.current.map(|i| find.matches[i].clone()) {
                self.doc.clear_extra();
                self.doc.selection = Selection { anchor: m.start, head: m.end };
                self.autoscroll = true;
            }
        }
        cx.notify();
    }

    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else { return };
        if find.matches.is_empty() {
            return;
        }
        let n = find.matches.len();
        let next = match find.current {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        };
        find.current = Some(next);
        let m = find.matches[next].clone();
        self.doc.clear_extra();
        self.doc.selection = Selection { anchor: m.start, head: m.end };
        self.autoscroll = true;
        cx.notify();
    }

    // ---- mouse ----------------------------------------------------------------------------

    fn pos_at(&self, position: Point<Pixels>) -> Option<usize> {
        if self.markdown_live {
            let md = self.md_layout.as_ref()?;
            let l = md.line_at_y(position.y)?;
            let text = self.doc.line(l.line).to_string();
            let col = md.col_at(l, position, &text).min(self.doc.line_len(l.line));
            return Some(self.doc.pos(l.line, col));
        }
        let layout = self.layout.as_ref()?;
        let y = position.y - layout.text_origin.y;
        let vrow = ((y / layout.line_height).floor().max(0.)) as usize;
        let vrow = vrow.min(layout.total_rows.saturating_sub(1));
        let x = position.x - layout.text_origin.x;
        if let Some(rl) = layout.lines.iter().find(|l| l.vrow == vrow) {
            let byte = rl.shaped.closest_index_for_x(x);
            let mut col = rl.start + rl.col_bytes.partition_point(|&b| b < byte);
            if !rl.last_row && col >= rl.end && rl.end > rl.start {
                col = rl.end - 1; // clicking past a wrapped row stays on that row
            }
            return Some(self.doc.pos(rl.line, col.min(self.doc.line_len(rl.line))));
        }
        let line = self.line_of_row(vrow);
        let col = ((x / layout.char_width).round().max(0.)) as usize;
        Some(self.doc.pos(line, col.min(self.doc.line_len(line))))
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.clear_lsp_hover(cx);
        if e.click_count == 1 && !self.markdown_live {
            if let Some(layout) = self.layout.as_ref() {
                if let Some(&(_, ix, how)) = layout.conflict_buttons.iter().find(|(b, _, _)| b.contains(&e.position)) {
                    self.resolve_conflict(ix, how, cx);
                    return;
                }
                if let Some((_, line)) = layout.blame.filter(|(b, _)| b.contains(&e.position)) {
                    self.copy_blame_hash(line, cx);
                    return;
                }
            }
        }
        let Some(pos) = self.pos_at(e.position) else { return };
        // ⌥-click: add (or remove) a caret.
        if e.modifiers.alt && e.click_count == 1 && !self.markdown_live {
            self.doc.toggle_cursor_at(pos);
            self.selecting = false;
            cx.notify();
            return;
        }
        self.doc.clear_extra();
        if e.modifiers.platform && e.click_count == 1 && self.open_link_at(pos, cx) {
            return;
        }
        if e.modifiers.platform && e.click_count == 1 && !self.markdown_live {
            self.doc.set_cursor(pos, false);
            self.go_to_definition(cx);
            cx.notify();
            return;
        }
        // Clicking a rendered checkbox toggles the task (only when that line isn't being edited).
        if self.markdown_live && e.click_count == 1 && !e.modifiers.shift {
            let (line, col) = self.doc.line_col(pos);
            let (cur, _) = self.doc.line_col(self.doc.selection.head);
            if line != cur {
                self.refresh_md_blocks();
                let text = self.doc.line(line).to_string();
                if let Some(block) = self.md_blocks.get(line).copied() {
                    if let Some(tc) = editor::markdown::task_toggle_col(&text, block) {
                        if col + 1 >= tc.saturating_sub(2) && col <= tc + 2 {
                            let done = text.chars().nth(tc).is_some_and(|c| c != ' ');
                            let at = self.doc.pos(line, tc);
                            let keep = self.doc.selection;
                            self.run(cx, |d| {
                                d.selection = editor::Selection { anchor: at, head: at + 1 };
                                d.insert(if done { " " } else { "x" });
                                d.selection = keep;
                            });
                            return;
                        }
                    }
                }
            }
        }
        match e.click_count {
            2 => self.doc.select_word_at(pos),
            3.. => self.doc.select_line_at(pos),
            _ => self.doc.set_cursor(pos, e.modifiers.shift),
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || e.pressed_button != Some(MouseButton::Left) {
            if self.markdown_live {
                return;
            }
            // Conflict actions and the blame annotation sit over the text.
            let (btn, on_blame) = match self.layout.as_ref() {
                Some(l) => (
                    l.conflict_buttons.iter().find(|(b, _, _)| b.contains(&e.position)).map(|&(_, ix, how)| (ix, how)),
                    l.blame.is_some_and(|(b, _)| b.contains(&e.position)),
                ),
                None => (None, false),
            };
            if btn != self.ins.conflict_btn_hover {
                self.ins.conflict_btn_hover = btn;
                cx.notify();
            }
            let blame_hover = on_blame.then_some(self.ins.blame_hover.unwrap_or(e.position));
            if blame_hover != self.ins.blame_hover {
                self.ins.blame_hover = blame_hover;
                cx.notify();
            }
            // Hover: show the diagnostic under the mouse, else ask the language server.
            let pos = if btn.is_some() || on_blame { None } else { self.pos_at(e.position) };
            let hit = pos.and_then(|p| self.diagnostic_at(p));
            let new = hit.map(|i| (i, e.position));
            if new.map(|n| n.0) != self.hover.map(|h| h.0) {
                self.hover = new;
                cx.notify();
            }
            self.hover_lsp_at(if hit.is_some() { None } else { pos }, e.position, cx);
            return;
        }
        // Dragging is handled by the window-level listener registered in paint (`register_drag`),
        // which also sees the pointer once it leaves the editor.
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
        self.drag_pos = None;
    }

    /// How far the drag pointer is above (negative) or below (positive) the viewport.
    fn drag_overshoot(&self) -> Pixels {
        let (Some(p), Some(vp)) = (self.drag_pos, self.drag_viewport) else { return px(0.) };
        let top = vp.origin.y;
        let bottom = vp.origin.y + vp.size.height;
        if p.y < top {
            p.y - top
        } else if p.y > bottom {
            p.y - bottom
        } else {
            px(0.)
        }
    }

    /// Extend the selection to the drag pointer, clamped inside the viewport.
    fn drag_select(&mut self, cx: &mut Context<Self>) {
        let (Some(mut p), Some(vp)) = (self.drag_pos, self.drag_viewport) else { return };
        p.y = p.y.clamp(vp.origin.y + px(1.), vp.origin.y + vp.size.height - px(2.));
        if let Some(pos) = self.pos_at(p) {
            if pos != self.doc.selection.head {
                self.doc.set_cursor(pos, true);
                cx.notify();
            }
        }
    }

    pub(crate) fn drag_moved(&mut self, position: Point<Pixels>, pressed: Option<MouseButton>, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        if pressed != Some(MouseButton::Left) {
            self.selecting = false;
            self.drag_pos = None;
            return;
        }
        self.drag_pos = Some(position);
        self.drag_select(cx);
        if self.drag_overshoot() != px(0.) && !self.drag_ticking {
            self.drag_ticking = true;
            self.drag_task = Some(cx.spawn(async move |this, cx| loop {
                Timer::after(Duration::from_millis(16)).await;
                let keep = this.update(cx, |v, cx| v.drag_tick(cx)).unwrap_or(false);
                if !keep {
                    break;
                }
            }));
        }
    }

    /// One edge-scroll step while the drag pointer is outside the viewport.
    fn drag_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let over = self.drag_overshoot();
        if !self.selecting || over == px(0.) {
            self.drag_ticking = false;
            return false;
        }
        // Faster the further the pointer is from the edge.
        let step = (px(4.) + over.abs() * 0.5).min(px(160.));
        let y = if over < px(0.) { self.scroll.y - step } else { self.scroll.y + step };
        let mut y = y.max(px(0.));
        if !self.markdown_live {
            if let Some(l) = self.layout.as_ref() {
                let max_y = (l.line_height * l.total_rows as f32 - l.line_height * 3.).max(px(0.));
                y = y.min(max_y);
            }
        }
        self.drag_select(cx);
        if y != self.scroll.y {
            self.scroll.y = y;
            cx.notify();
        }
        true
    }

    /// Track the pointer window-wide while drag-selecting (called from the elements' paint).
    pub(crate) fn register_drag(view: &Entity<Self>, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let selecting = view.update(cx, |v, _| {
            v.drag_viewport = Some(bounds);
            v.selecting
        });
        if !selecting {
            return;
        }
        let view = view.clone();
        window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
            if phase == gpui::DispatchPhase::Bubble {
                view.update(cx, |v, cx| v.drag_moved(e.position, e.pressed_button, cx));
            }
        });
    }

    fn scroll_wheel(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_lsp_hover(cx);
        if self.markdown_live {
            let dy = match e.delta {
                ScrollDelta::Pixels(p) => p.y,
                ScrollDelta::Lines(p) => px(24.) * p.y,
            };
            self.scroll.y = (self.scroll.y - dy).max(px(0.));
            cx.notify();
            return;
        }
        let Some(layout) = self.layout.as_ref() else { return };
        let delta = match e.delta {
            ScrollDelta::Pixels(p) => p,
            ScrollDelta::Lines(p) => point(layout.char_width * p.x * 3., layout.line_height * p.y),
        };
        let max_y = (layout.line_height * layout.total_rows as f32 - layout.line_height * 3.).max(px(0.));
        self.scroll.y = (self.scroll.y - delta.y).clamp(px(0.), max_y);
        if self.wrap_cols == usize::MAX {
            self.scroll.x = (self.scroll.x - delta.x).max(px(0.));
        }
        cx.notify();
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.page.as_ref().and_then(|p| p.focus.clone()).unwrap_or_else(|| self.focus_handle.clone())
    }
}

macro_rules! cmd {
    ($this:ident, $cx:ident, $body:expr) => {
        $cx.listener(move |$this: &mut EditorView, _, _, cx| {
            let f: fn(&mut Document, isize) = $body;
            let page = $this.page_lines();
            $this.run(cx, |d| d.each_cursor(|d| f(d, page)));
        })
    };
}

impl Render for EditorView {
    fn render(&mut self, win: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let svg_mode = self.svg_preview && self.is_svg();
        let svg_view = svg_mode.then(|| {
            let raster = self.svg_raster(win.scale_factor(), cx);
            let toolbar = self.svg_zoom.natural.is_some().then(|| self.svg_zoom.toolbar(cx));
            let body = match raster {
                Some(r) => self.svg_zoom.viewport(r, cx),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::rgb(0x1d1f23))
                    .text_size(px(13.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child(if self.svg_zoom.natural.is_none() { "Invalid SVG" } else { "" })
                    .into_any_element(),
            };
            (toolbar, body)
        });
        let (svg_toolbar, svg_body) = match svg_view {
            Some((t, b)) => (t, Some(b)),
            None => (None, None),
        };
        let fp = (self.doc.selection.anchor, self.doc.selection.head, self.doc.version(), self.diagnostics.len());
        if fp != self.ide_fingerprint {
            self.ide_fingerprint = fp;
            cx.emit(EditorEvent::SelectionChanged);
        }
        let table_toolbar = self.render_table_toolbar(cx);
        let slash_menu = if self.markdown_live { self.render_slash_menu(cx) } else { None };
        if let Some(p) = &self.page {
            return div().id("editor").size_full().child(p.view.clone());
        }
        if let Some(chat) = self.chat.clone() {
            let name: SharedString = chat.read(cx).name.clone().into();
            self.chat_title = Some(name);
            return div().id("editor").size_full().child(chat);
        }
        if let Some(asset) = self.asset.clone().filter(|_| !self.csv_source || self.table_mode) {
            // The asset view tracks this editor's focus handle itself (keyboard selection).
            return div()
                .id("editor")
                .key_context("Editor")
                .size_full()
                .on_action(cx.listener(Self::save_action))
                .child(asset);
        }
        if self.ins.blame.is_none() && self.ins.blame_task.is_none() {
            self.schedule_blame(Duration::ZERO, cx);
        }
        let conflict_bar = if self.markdown_live { None } else { self.render_conflict_bar(cx) };
        let blame_card = self.ins.blame_hover.and_then(|at| self.layout.as_ref().and_then(|l| l.blame).and_then(|(_, line)| self.render_blame_card(line, at)));
        let lsp_hover = if self.hover.is_none() && !self.markdown_live { self.render_lsp_hover() } else { None };
        let focused_editor = cx.entity();
        let find_bar = self.find.as_ref().map(|f| {
            let count = if f.go_to_line {
                String::new()
            } else if f.matches.is_empty() {
                if f.input.read(cx).content.is_empty() { String::new() } else { "No results".into() }
            } else {
                format!("{} of {}", f.current.map(|c| c + 1).unwrap_or(0), f.matches.len())
            };
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(34.))
                .px(px(12.))
                .bg(theme::rgb(theme::SIDEBAR_BG))
                .border_b_1()
                .border_color(theme::rgb(theme::BORDER))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .max_w(px(360.))
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(5.))
                        .bg(theme::rgb(theme::EDITOR_BG))
                        .border_1()
                        .border_color(theme::rgb(theme::BORDER))
                        .child(f.input.clone()),
                )
                .child(div().min_w(px(64.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(count))
                .when(!f.go_to_line, |bar| {
                    let button = |id: &'static str, label: &'static str, tip: &'static str| {
                        div()
                            .id(id)
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(22.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(theme::rgb(theme::TEXT_MUTED))
                            .hover(|s| s.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
                            .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
                            .child(label)
                    };
                    bar.child(
                        button("find-prev", "↑", "Previous match (⇧Enter / ⌘⇧G)")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.find_step(false, cx))),
                    )
                    .child(
                        button("find-next", "↓", "Next match (Enter / ⌘G)")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.find_step(true, cx))),
                    )
                    .child(
                        button("find-close", "✕", "Close (Esc)")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.close_find(window, cx))),
                    )
                })
        });

        let conflict = self.disk_conflict.then(|| {
            div()
                .id("disk-conflict")
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(6.))
                .bg(theme::rgb(0x3a3220))
                .text_size(px(12.))
                .text_color(theme::rgb(theme::MODIFIED))
                .child("File changed on disk and you have unsaved changes.")
                .child(
                    div()
                        .id("reload-disk")
                        .px(px(8.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .bg(theme::rgb(theme::SELECTED))
                        .child("Reload (discard mine)")
                        .on_click(cx.listener(Self::reload_from_disk)),
                )
                .child(
                    div()
                        .id("keep-mine")
                        .px(px(8.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .bg(theme::rgb(theme::SELECTED))
                        .child("Keep mine")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.disk_conflict = false;
                            this.doc.disk_mtime = this.doc.path.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
                            cx.notify();
                        })),
                )
        });

        div()
            .id("editor")
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::rgb(theme::EDITOR_BG))
            .on_action::<MoveLeft>(cmd!(this, cx, |d, _| d.move_left(false)))
            .on_action::<MoveRight>(cmd!(this, cx, |d, _| d.move_right(false)))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_visual(-1, false, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_visual(1, false, cx)))
            .on_action::<SelectLeft>(cmd!(this, cx, |d, _| d.move_left(true)))
            .on_action::<SelectRight>(cmd!(this, cx, |d, _| d.move_right(true)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.move_visual(-1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.move_visual(1, true, cx)))
            .on_action::<MoveWordLeft>(cmd!(this, cx, |d, _| d.move_word(false, false)))
            .on_action::<MoveWordRight>(cmd!(this, cx, |d, _| d.move_word(true, false)))
            .on_action::<SelectWordLeft>(cmd!(this, cx, |d, _| d.move_word(false, true)))
            .on_action::<SelectWordRight>(cmd!(this, cx, |d, _| d.move_word(true, true)))
            .on_action::<MoveLineStart>(cmd!(this, cx, |d, _| d.move_line_start(false)))
            .on_action::<MoveLineEnd>(cmd!(this, cx, |d, _| d.move_line_end(false)))
            .on_action::<SelectLineStart>(cmd!(this, cx, |d, _| d.move_line_start(true)))
            .on_action::<SelectLineEnd>(cmd!(this, cx, |d, _| d.move_line_end(true)))
            .on_action::<MoveDocStart>(cmd!(this, cx, |d, _| d.move_doc_start(false)))
            .on_action::<MoveDocEnd>(cmd!(this, cx, |d, _| d.move_doc_end(false)))
            .on_action::<SelectDocStart>(cmd!(this, cx, |d, _| d.move_doc_start(true)))
            .on_action::<SelectDocEnd>(cmd!(this, cx, |d, _| d.move_doc_end(true)))
            .on_action::<PageUp>(cmd!(this, cx, |d, p| d.move_vertical(-p, false)))
            .on_action::<PageDown>(cmd!(this, cx, |d, p| d.move_vertical(p, false)))
            .on_action::<Backspace>(cmd!(this, cx, |d, _| d.backspace()))
            .on_action::<Delete>(cmd!(this, cx, |d, _| d.delete_forward()))
            .on_action::<DeleteWordLeft>(cmd!(this, cx, |d, _| d.delete_word_backward()))
            .on_action::<DeleteToLineStart>(cmd!(this, cx, |d, _| d.delete_to_line_start()))
            .on_action(cx.listener(|this, _: &DismissMenu, _, cx| {
                if this.doc.has_multi() {
                    this.doc.clear_extra();
                    cx.notify();
                    return;
                }
                if !this.slash_key(crate::slash_menu::SlashKey::Escape, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &Newline, _, cx| {
                if this.slash_key(crate::slash_menu::SlashKey::Enter, cx) {
                    return;
                }
                if !(this.markdown_live && !this.doc.has_multi() && this.table_enter(cx)) {
                    this.run(cx, |d| d.each_cursor(|d| d.newline()));
                }
            }))
            .on_action(cx.listener(|this, _: &Indent, _, cx| {
                if !(this.markdown_live && !this.doc.has_multi() && this.table_tab(true, cx)) {
                    this.run(cx, |d| d.each_cursor(|d| d.indent()));
                }
            }))
            .on_action(cx.listener(|this, _: &Outdent, _, cx| {
                if !(this.markdown_live && !this.doc.has_multi() && this.table_tab(false, cx)) {
                    this.run(cx, |d| d.each_cursor(|d| d.outdent()));
                }
            }))
            .on_action(cx.listener(Self::insert_table))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.run(cx, |d| d.select_all())))
            .on_action::<SelectLine>(cmd!(this, cx, |d, _| {
                let head = d.selection.range().start;
                d.select_line_at(head)
            }))
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                this.run(cx, |d| {
                    d.undo();
                })
            }))
            .on_action(cx.listener(|this, _: &Redo, _, cx| {
                this.run(cx, |d| {
                    d.redo();
                })
            }))
            .on_action(cx.listener(|this, _: &SelectNextOccurrence, _, cx| {
                this.run(cx, |d| d.select_next_occurrence());
                this.single_cursor_in_live(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllOccurrences, _, cx| {
                if !this.markdown_live {
                    this.run(cx, |d| d.select_all_occurrences());
                }
            }))
            .on_action(cx.listener(|this, _: &AddCursorAbove, _, cx| {
                if !this.markdown_live {
                    this.run(cx, |d| d.add_cursor_vertical(-1));
                }
            }))
            .on_action(cx.listener(|this, _: &AddCursorBelow, _, cx| {
                if !this.markdown_live {
                    this.run(cx, |d| d.add_cursor_vertical(1));
                }
            }))
            .on_action::<ToggleComment>(cmd!(this, cx, |d, _| d.toggle_comment()))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::export_pdf))
            .on_action(cx.listener(|this, _: &Find, w, cx| this.open_find(false, w, cx)))
            .on_action(cx.listener(|this, _: &GoToLine, w, cx| this.open_find(true, w, cx)))
            .on_action(cx.listener(Self::toggle_markdown_live))
            .on_action(cx.listener(Self::toggle_word_wrap))
            .on_action(cx.listener(|this, _: &GoToDefinition, _, cx| this.go_to_definition(cx)))
            .on_action(cx.listener(|this, _: &FindNext, _, cx| this.find_step(true, cx)))
            .on_action(cx.listener(|this, _: &FindPrevious, _, cx| this.find_step(false, cx)))
            .children(self.hover.and_then(|(i, pos)| self.diagnostics.get(i).map(|d| (d.clone(), pos))).map(|(d, pos)| {
                let (c, label) = if d.severity == code_quality::Severity::Error { (theme::DELETED, "error") } else { (theme::MODIFIED, "warning") };
                gpui::deferred(
                    gpui::anchored().position(point(pos.x, pos.y + px(14.))).snap_to_window_with_margin(px(8.)).child(
                        div()
                            .max_w(px(520.))
                            .p(px(10.))
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .rounded(px(6.))
                            .bg(theme::rgb(0x202227))
                            .border_1()
                            .border_color(theme::rgb(c))
                            .shadow_lg()
                            .child(div().flex().gap(px(6.)).text_size(px(11.)).text_color(theme::rgb(c)).child(label).child(div().text_color(theme::rgb(theme::TEXT_FAINT)).child(d.code.clone().unwrap_or_default())))
                            .child(div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(d.message.clone()))
                            .when_some(d.help.clone(), |el, h| el.child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(h))),
                    ),
                )
                .with_priority(2)
            }))
            .children(blame_card)
            .children(lsp_hover)
            .children(conflict)
            .children(conflict_bar)
            .children(self.export.clone().map(|d| gpui::deferred(d).with_priority(2)))
            .when(self.doc.language == Language::Markdown || self.is_svg() || self.csv_source, |d| {
                let svg = self.is_svg();
                let csv = self.csv_source;
                let on = if csv { false } else if svg { self.svg_preview } else { self.markdown_live };
                d.child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(2.))
                        .px(px(10.))
                        .pt(px(6.))
                        .when_some(svg_toolbar, |d, t| d.child(t).child(div().flex_1()))
                        .child(md_toggle("md-live", if csv { "Table" } else if svg { "Preview" } else { "Live" }, on).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                            if !on {
                                this.toggle_markdown_live(&ToggleMarkdownLive, w, cx)
                            }
                        })))
                        .child(md_toggle("md-source", if csv { "Edit" } else { "Source" }, !on).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                            if on {
                                this.toggle_markdown_live(&ToggleMarkdownLive, w, cx)
                            }
                        })))
                        .when(!csv && !svg, |d| {
                            d.child(div().w(px(8.))).child(md_toggle("md-export", "Export PDF", false).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.export_pdf(&ExportPdf, w, cx))))
                        }),
                )
            })
            .children(find_bar)
            .child(
                div()
                    .id("editor-text")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .when(!svg_mode, |d| {
                        d.cursor(CursorStyle::IBeam)
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                            .on_mouse_move(cx.listener(Self::mouse_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
                            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
                            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                    })
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.clear_lsp_hover(cx);
                            if this.ins.conflict_btn_hover.take().is_some() {
                                cx.notify();
                            }
                        }
                    }))
                    .child(if let Some(body) = svg_body {
                        div().size_full().flex().child(body).into_any_element()
                    } else if self.markdown_live {
                        crate::markdown_live::MarkdownElement { view: focused_editor }.into_any_element()
                    } else {
                        EditorElement { view: focused_editor }.into_any_element()
                    })
                    // After the content so it paints (and hit-tests) on top.
                    .children(table_toolbar)
                    .children(slash_menu),
            )
    }
}

impl EntityInputHandler for EditorView {
    fn text_for_range(&mut self, _: Range<usize>, _: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        None
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text.as_ref().map(|t| 0..t.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        if text.is_empty() {
            cx.notify();
            return;
        }
        let text = text.to_string();
        self.run(cx, |d| d.each_cursor(|d| d.insert(&text)));
        self.update_slash_menu(Some(&text));
    }

    fn replace_and_mark_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: Option<Range<usize>>, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = (!text.is_empty()).then(|| text.to_string());
        cx.notify();
    }

    fn bounds_for_range(&mut self, _: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        if self.markdown_live {
            let md = self.md_layout.as_ref()?;
            let (line, col) = self.doc.line_col(self.doc.selection.head);
            let l = md.lines.iter().find(|l| l.line == line)?;
            let c = md.caret(l, col, &self.doc.line(line));
            return Some(Bounds::new(point(md.origin.x + c.x, md.origin.y + c.y), size(px(8.), l.line_height)));
        }
        let layout = self.layout.as_ref()?;
        let (line, col) = self.doc.line_col(self.doc.selection.head);
        let rl = layout.lines.iter().find(|l| l.line == line && l.contains_cursor(col))?;
        Some(Bounds::new(
            point(layout.text_origin.x + rl.x_of(col), layout.text_origin.y + layout.line_height * rl.vrow as f32),
            size(layout.char_width, layout.line_height),
        ))
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

/// Move highlight spans after an edit at byte `at` that changed the length by `delta`.
fn shift_spans(spans: &mut Vec<HighlightSpan>, at: usize, delta: isize) {
    let shift = |x: usize| (x as isize + delta).max(at as isize) as usize;
    for s in spans.iter_mut() {
        if s.start >= at {
            s.start = shift(s.start);
            s.end = shift(s.end);
        } else if s.end > at {
            s.end = shift(s.end).max(s.start);
        }
    }
    spans.retain(|s| s.end > s.start);
}

pub(crate) fn md_toggle(id: &'static str, label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(8.))
        .py(px(2.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(11.5))
        .when(active, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
        .when(!active, |d| d.text_color(theme::rgb(theme::TEXT_FAINT)).hover(|s| s.text_color(theme::rgb(theme::TEXT_MUTED))))
        .child(label)
}

// ---------------------------------------------------------------------------------------------

struct EditorElement {
    view: Entity<EditorView>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

struct Prepaint {
    quads: Vec<(Bounds<Pixels>, Hsla)>,
    gutter: Vec<(Point<Pixels>, ShapedLine)>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursor: Option<Bounds<Pixels>>,
    /// Secondary carets (multi-cursor).
    extra_cursors: Vec<Bounds<Pixels>>,
    marked: Option<(Point<Pixels>, ShapedLine)>,
    line_height: Pixels,
    text_clip: Bounds<Pixels>,
    /// Clickable spots (conflict actions, blame): pointing-hand cursor.
    hand: Vec<gpui::Hitbox>,
}

fn color(hex: u32) -> Hsla {
    theme::rgb(hex)
}

/// Build the display string for a line (tabs expanded) and its styled runs.
fn layout_line(
    text: &str,
    line_start_byte: usize,
    spans: &[HighlightSpan],
    tab_size: usize,
    font: &Font,
    underlines: &[(Range<usize>, Hsla)],
) -> (String, Vec<TextRun>, Vec<usize>) {
    let mut display = String::with_capacity(text.len());
    let mut col_bytes = Vec::with_capacity(text.len() + 1);
    let mut runs: Vec<TextRun> = Vec::new();
    // First span that may intersect this line.
    let mut si = spans.partition_point(|s| s.end <= line_start_byte);
    let mut byte = line_start_byte;
    let mut display_col = 0usize;
    let default = theme::TEXT;
    for (i, ch) in text.chars().enumerate() {
        if i >= MAX_RENDERED_LINE_CHARS {
            break;
        }
        while si < spans.len() && spans[si].end <= byte {
            si += 1;
        }
        let hex = match spans.get(si) {
            Some(s) if s.start <= byte && byte < s.end => theme::syntax_color(HIGHLIGHT_NAMES[s.kind as usize]),
            _ => default,
        };
        col_bytes.push(display.len());
        let before = display.len();
        if ch == '\t' {
            let n = tab_size - (display_col % tab_size);
            display.extend(std::iter::repeat_n(' ', n));
            display_col += n;
        } else {
            display.push(ch);
            display_col += 1;
        }
        let added = display.len() - before;
        let c = color(hex);
        let underline = underlines.iter().find(|(r, _)| r.contains(&i)).map(|(_, uc)| UnderlineStyle { color: Some(*uc), thickness: px(1.), wavy: true });
        match runs.last_mut() {
            Some(r) if r.color == c && r.underline == underline => r.len += added,
            _ => runs.push(TextRun { len: added, font: font.clone(), color: c, background_color: None, underline, strikethrough: None }),
        }
        byte += ch.len_utf8();
    }
    col_bytes.push(display.len());
    (display, runs, col_bytes)
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) -> Prepaint {
        let focused = self.view.read(cx).focus_handle.is_focused(window);
        self.view.update(cx, |view, _| {
            let ts = window.text_system().clone();
            let font_size = view.font_size;
            let font_id = ts.resolve_font(&view.font);
            let char_width = ts.advance(font_id, font_size, 'm').map(|s| s.width).unwrap_or(font_size * 0.6);
            let line_height = (font_size * 1.55).round();
            let line_count = view.doc.line_count();
            let digits = line_count.max(1).to_string().len().max(3);
            let gutter_width = char_width * (digits as f32 + 3.);
            let text_left = bounds.origin.x + gutter_width;
            let text_width = (bounds.size.width - gutter_width).max(px(0.));
            let pad_top = px(6.);

            // Soft wrap: columns that fit the text area (monospace).
            let cols = if view.word_wrap && line_count <= MAX_WRAP_LINES {
                (((text_width - px(4.) - char_width) / char_width).floor().max(20.)) as usize
            } else {
                usize::MAX
            };
            if cols != usize::MAX {
                view.scroll.x = px(0.);
            }
            view.wrap_cols = cols;
            view.ensure_wrap(cols);
            let total_rows = *view.row_starts().last().unwrap_or(&line_count);

            // Autoscroll so the cursor is visible.
            let (cur_line, cur_col) = view.doc.line_col(view.doc.selection.head);
            if view.autoscroll {
                view.autoscroll = false;
                let rows = view.line_rows(cur_line);
                let r = rows.partition_point(|&s| s <= cur_col).saturating_sub(1);
                let cur_vrow = view.row_starts().get(cur_line).copied().unwrap_or(cur_line) + r;
                let y = line_height * cur_vrow as f32;
                let h = bounds.size.height - pad_top;
                if y < view.scroll.y {
                    view.scroll.y = (y - line_height * 2.).max(px(0.));
                } else if y + line_height > view.scroll.y + h {
                    view.scroll.y = y + line_height * 3. - h;
                }
                if cols == usize::MAX {
                    let dc = display_cols(&view.doc.line(cur_line), view.doc.tab_size);
                    let x = char_width * dc[cur_col.min(dc.len() - 1)] as f32;
                    let margin = char_width * 4.;
                    if x < view.scroll.x {
                        view.scroll.x = (x - margin).max(px(0.));
                    } else if x + margin > view.scroll.x + text_width {
                        view.scroll.x = x + margin - text_width;
                    }
                }
            }
            let max_y = (line_height * total_rows as f32 - line_height * 3.).max(px(0.));
            view.scroll.y = view.scroll.y.clamp(px(0.), max_y);

            let text_origin = point(text_left + px(4.) - view.scroll.x, bounds.origin.y + pad_top - view.scroll.y);
            let first_row = ((view.scroll.y / line_height).floor() as usize).min(total_rows.saturating_sub(1));
            let visible = ((bounds.size.height / line_height).ceil() as usize) + 2;
            let last_row = (first_row + visible).min(total_rows);
            let first = view.line_of_row(first_row);

            let conflicts: Vec<editor::conflicts::Conflict> = view.conflicts().to_vec();
            // Spans are only valid for the version they were computed from.
            let spans: &[HighlightSpan] = if view.spans_version == view.doc.version() { &view.spans } else { &[] };
            let sel = view.doc.selection.range();
            // Secondary cursors: (selection range, caret line, caret column).
            let extra_sels: Vec<(Range<usize>, usize, usize)> = view
                .doc
                .all_selections()
                .into_iter()
                .skip(1)
                .map(|s| {
                    let (l, c) = view.doc.line_col(s.head);
                    (s.range(), l, c)
                })
                .collect();
            let mut extra_cursors = Vec::new();
            let find_ranges: Vec<(Range<usize>, bool)> = view
                .find
                .as_ref()
                .filter(|f| !f.go_to_line)
                .map(|f| f.matches.iter().enumerate().map(|(i, m)| (m.clone(), Some(i) == f.current)).collect())
                .unwrap_or_default();

            let mut quads = Vec::new();
            let mut gutter = Vec::new();
            let mut lines = Vec::new();
            let mut rendered = Vec::new();
            let mut cursor = None;

            // Merge conflicts: tinted blocks and "Accept …" actions on the `<<<<<<<` line.
            use editor::conflicts::Resolution;
            let (green, blue) = (color(0x3fb950), color(0x58a6ff));
            let conflict_bg = |line: usize| -> Option<Hsla> {
                let c = conflicts.iter().find(|c| (c.start..=c.end).contains(&line))?;
                Some(if line == c.start {
                    green.opacity(0.32)
                } else if line == c.end {
                    blue.opacity(0.32)
                } else if line == c.sep || Some(line) == c.base {
                    color(theme::TEXT_FAINT).opacity(0.18)
                } else if line < c.base.unwrap_or(c.sep) {
                    green.opacity(0.12)
                } else if line < c.sep {
                    color(theme::TEXT_FAINT).opacity(0.08)
                } else {
                    blue.opacity(0.12)
                })
            };
            let mut conflict_buttons = Vec::new();
            let mut blame_hit = None;
            let blame_line = (sel.is_empty() && !view.doc.has_multi() && !conflicts.iter().any(|c| (c.start..=c.end).contains(&cur_line)))
                .then(|| view.blame_annotation(cur_line))
                .flatten();
            let small = (font_size * 0.86).round();

            let gutter_font = view.font.clone();
            let mut line = first;
            while line < line_count {
                let line_vrow = view.row_starts().get(line).copied().unwrap_or(line);
                if line_vrow >= last_row {
                    break;
                }
                let text = view.doc.line(line);
                let line_start_char = view.doc.pos(line, 0);
                let line_len = view.doc.line_len(line);
                let line_end_char = line_start_char + line_len;
                let line_diags: Vec<(Range<usize>, Hsla)> = view
                    .diagnostics
                    .iter()
                    .filter(|d| d.line.saturating_sub(1) == line)
                    .map(|d| {
                        let start = d.column.saturating_sub(1);
                        // Byte length approximates chars for ASCII; at least one char wide.
                        let len = d.length.max(1);
                        let c = if d.severity == code_quality::Severity::Error { theme::DELETED } else { theme::MODIFIED };
                        (start..start + len, color(c))
                    })
                    .collect();
                let (display, runs, col_bytes) = layout_line(&text, view.doc.line_start_byte(line), spans, view.doc.tab_size, &view.font, &line_diags);
                let ncols = col_bytes.len() - 1;
                let mut row_cols = vec![0];
                row_cols.extend(wrap_breaks(&text, view.doc.tab_size, cols));
                let nrows = row_cols.len();

                let number = SharedString::from((line + 1).to_string());
                let worst = line_diags.iter().map(|(_, c)| *c).find(|c| *c == color(theme::DELETED)).or(line_diags.first().map(|(_, c)| *c));
                let num_color = worst.unwrap_or_else(|| color(if line == cur_line { theme::GUTTER_TEXT_ACTIVE } else { theme::GUTTER_TEXT }));
                let num_run = TextRun { len: number.len(), font: gutter_font.clone(), color: num_color, background_color: None, underline: None, strikethrough: None };
                let num_shaped = ts.shape_line(number, font_size, &[num_run], None);
                let num_x = bounds.origin.x + gutter_width - char_width * 1.5 - num_shaped.width;
                gutter.push((point(num_x, text_origin.y + line_height * line_vrow as f32), num_shaped));

                for r in 0..nrows {
                    let vrow = line_vrow + r;
                    if vrow < first_row {
                        continue;
                    }
                    if vrow >= last_row {
                        break;
                    }
                    let y = text_origin.y + line_height * vrow as f32;
                    let start = row_cols[r].min(ncols);
                    let end = row_cols.get(r + 1).copied().unwrap_or(ncols).min(ncols);
                    let last = r + 1 == nrows;
                    let (b0, b1) = (col_bytes[start], col_bytes[end]);
                    let shaped = ts.shape_line(SharedString::from(display[b0..b1].to_string()), font_size, &slice_runs(&runs, b0, b1), None);
                    let rl = RenderedLine { line, vrow, start, end, last_row: last, shaped, col_bytes: col_bytes[start..=end].iter().map(|b| b - b0).collect() };

                    if let Some(bg) = conflict_bg(line) {
                        quads.push((Bounds::new(point(text_left, y), size(text_width, line_height)), bg));
                    }
                    if line == cur_line && sel.is_empty() {
                        quads.push((Bounds::new(point(text_left, y), size(text_width, line_height)), color(theme::CURRENT_LINE)));
                    }
                    if r == 0 {
                        if let Some(ix) = conflicts.iter().position(|c| c.start == line) {
                            let mut x = text_origin.x + rl.shaped.width + char_width * 3.;
                            for (how, label) in [(Resolution::Current, "Accept Current"), (Resolution::Incoming, "Accept Incoming"), (Resolution::Both, "Accept Both")] {
                                let hot = view.ins.conflict_btn_hover == Some((ix, how));
                                let c = if hot { color(theme::ACCENT) } else { color(theme::TEXT_MUTED) };
                                let run = TextRun {
                                    len: label.len(),
                                    font: view.font.clone(),
                                    color: c,
                                    background_color: None,
                                    underline: hot.then_some(UnderlineStyle { color: Some(c), thickness: px(1.), wavy: false }),
                                    strikethrough: None,
                                };
                                let shaped = ts.shape_line(SharedString::from(label), small, &[run], None);
                                let b = Bounds::new(point(x, y), size(shaped.width, line_height));
                                x += shaped.width + char_width * 2.;
                                lines.push((point(b.origin.x, y), shaped));
                                conflict_buttons.push((b, ix, how));
                            }
                        }
                    }
                    if last && line == cur_line {
                        if let Some(text) = blame_line.as_ref() {
                            let run = TextRun { len: text.len(), font: view.font.clone(), color: color(theme::TEXT_FAINT).opacity(0.75), background_color: None, underline: None, strikethrough: None };
                            let shaped = ts.shape_line(SharedString::from(text.clone()), small, &[run], None);
                            let x = text_origin.x + rl.shaped.width + char_width * 4.;
                            blame_hit = Some((Bounds::new(point(x, y), size(shaped.width, line_height)), line));
                            lines.push((point(x, y), shaped));
                        }
                    }
                    // Doc char range (relative to line) clipped to this row → quad.
                    let band = |a: usize, b: usize, extra: Pixels, c: Hsla, quads: &mut Vec<(Bounds<Pixels>, Hsla)>| {
                        let (a, b) = (a.max(start), b.min(end));
                        if a > b {
                            return;
                        }
                        let x0 = text_origin.x + rl.x_of(a);
                        let x1 = text_origin.x + rl.x_of(b) + extra;
                        if x1 > x0 {
                            quads.push((Bounds::new(point(x0, y), size(x1 - x0, line_height)), c));
                        }
                    };
                    for (m, current) in &find_ranges {
                        if m.end < line_start_char || m.start > line_end_char || m.start == m.end {
                            continue;
                        }
                        let a = m.start.max(line_start_char) - line_start_char;
                        let b = m.end.min(line_end_char) - line_start_char;
                        band(a, b, px(0.), color(if *current { theme::FIND_MATCH_CURRENT } else { theme::FIND_MATCH }), &mut quads);
                    }
                    if !sel.is_empty() && sel.start <= line_end_char && sel.end >= line_start_char {
                        let a = sel.start.max(line_start_char) - line_start_char;
                        let b = sel.end.min(line_end_char) - line_start_char;
                        // Selection continues past this row (wrap) or past the newline.
                        let extra = if (last && sel.end > line_end_char) || (!last && b > end) { char_width * 0.6 } else { px(0.) };
                        band(a, b, extra, color(theme::SELECTION), &mut quads);
                    }
                    if line == cur_line && rl.contains_cursor(cur_col.min(ncols)) {
                        let x = text_origin.x + rl.x_of(cur_col);
                        cursor = Some(Bounds::new(point(x, y), size(px(2.), line_height)));
                    }
                    for (r, cl, cc) in &extra_sels {
                        if !r.is_empty() && r.start <= line_end_char && r.end >= line_start_char {
                            let a = r.start.max(line_start_char) - line_start_char;
                            let b = r.end.min(line_end_char) - line_start_char;
                            let extra = if (last && r.end > line_end_char) || (!last && b > end) { char_width * 0.6 } else { px(0.) };
                            band(a, b, extra, color(theme::SELECTION), &mut quads);
                        }
                        if *cl == line && rl.contains_cursor((*cc).min(ncols)) {
                            let x = text_origin.x + rl.x_of(*cc);
                            extra_cursors.push(Bounds::new(point(x, y), size(px(2.), line_height)));
                        }
                    }
                    lines.push((point(text_origin.x, y), rl.shaped.clone()));
                    rendered.push(rl);
                }
                line += 1;
            }

            let marked = view.marked_text.as_ref().zip(cursor).map(|(t, c)| {
                let col = color(theme::TEXT);
                let run = TextRun {
                    len: t.len(),
                    font: view.font.clone(),
                    color: col,
                    background_color: Some(color(theme::EDITOR_BG)),
                    underline: Some(UnderlineStyle { color: Some(col), thickness: px(1.), wavy: false }),
                    strikethrough: None,
                };
                (c.origin, ts.shape_line(SharedString::from(t.clone()), font_size, &[run], None))
            });

            let text_clip = Bounds::new(point(text_left, bounds.origin.y), size(text_width, bounds.size.height));
            let hand = conflict_buttons
                .iter()
                .map(|(b, _, _)| *b)
                .chain(blame_hit.map(|(b, _)| b))
                .filter_map(|b: Bounds<Pixels>| {
                    let b = b.intersect(&text_clip);
                    (b.size.width > px(0.)).then(|| window.insert_hitbox(b, gpui::HitboxBehavior::Normal))
                })
                .collect();
            view.layout = Some(Layout { text_origin, line_height, char_width, viewport: bounds, lines: rendered, total_rows, blame: blame_hit, conflict_buttons });

            Prepaint {
                quads,
                gutter,
                lines,
                cursor: if focused { cursor } else { None },
                extra_cursors: if focused { extra_cursors } else { Vec::new() },
                marked,
                line_height,
                text_clip,
                hand,
            }
        })
    }

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), state: &mut Prepaint, window: &mut Window, cx: &mut App) {
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.view.clone()), cx);
        EditorView::register_drag(&self.view, bounds, window, cx);
        let lh = state.line_height;
        for hb in &state.hand {
            window.set_cursor_style(CursorStyle::PointingHand, hb);
        }
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for (origin, line) in &state.gutter {
                let _ = line.paint(*origin, lh, window, cx);
            }
        });
        window.with_content_mask(Some(gpui::ContentMask { bounds: state.text_clip }), |window| {
            for (b, c) in state.quads.drain(..) {
                window.paint_quad(fill(b, c));
            }
            for (origin, line) in &state.lines {
                let _ = line.paint(*origin, lh, window, cx);
            }
            if let Some((origin, line)) = state.marked.as_ref() {
                let _ = line.paint(*origin, lh, window, cx);
            }
            if let Some(c) = state.cursor {
                window.paint_quad(fill(c, color(theme::ACCENT)));
            }
            for c in &state.extra_cursors {
                window.paint_quad(fill(*c, color(theme::ACCENT)));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_shift_after_edits() {
        let mut spans = vec![HighlightSpan { start: 0, end: 3, kind: 1 }, HighlightSpan { start: 5, end: 8, kind: 2 }];
        shift_spans(&mut spans, 4, 2); // insert 2 bytes at 4
        assert_eq!((spans[0].start, spans[0].end, spans[1].start, spans[1].end), (0, 3, 7, 10));
        shift_spans(&mut spans, 1, -1); // delete 1 byte inside first span
        assert_eq!((spans[0].start, spans[0].end, spans[1].start), (0, 2, 6));
    }

    #[test]
    fn wrap_breaks_at_spaces_and_hard_breaks_long_words() {
        assert!(wrap_breaks("short", 4, 10).is_empty());
        // "hello world foo" at 11 cols: row 1 "hello world" won't fit the trailing space → "hello " | "world foo"
        assert_eq!(wrap_breaks("hello world foo", 4, 11), vec![12]);
        assert_eq!(wrap_breaks("hello world foo", 4, 8), vec![6, 12]);
        assert_eq!(wrap_breaks("abcdefghij", 4, 4), vec![4, 8]);
        // Tabs count by display width.
        assert_eq!(wrap_breaks("\tabcdef", 4, 6), vec![3]);
        assert!(wrap_breaks("anything long", 4, usize::MAX).is_empty());
    }

    #[test]
    fn slice_runs_splits_by_byte() {
        let font = gpui::font("Menlo");
        let (_, runs, _) = layout_line("abcdef", 0, &[], 4, &font, &[(2..4, theme::rgb(0xff0000))]);
        let mid = slice_runs(&runs, 1, 5);
        assert_eq!(mid.iter().map(|r| r.len).sum::<usize>(), 4);
        assert_eq!(mid.len(), 3);
    }

    #[test]
    fn layout_line_expands_tabs_and_maps_columns() {
        let font = gpui::font("Menlo");
        let (display, runs, cols) = layout_line("\tab", 0, &[], 4, &font, &[]);
        assert_eq!(display, "    ab");
        assert_eq!(cols, vec![0, 4, 5, 6]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), display.len());
    }

    #[test]
    fn layout_line_applies_spans_by_byte() {
        let font = gpui::font("Menlo");
        // "let x" with "let" as keyword (kind index of "keyword").
        let kw = HIGHLIGHT_NAMES.iter().position(|n| *n == "keyword").unwrap() as u16;
        let spans = [HighlightSpan { start: 10, end: 13, kind: kw }];
        let (_, runs, _) = layout_line("let x", 10, &spans, 4, &font, &[]);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len, 3);
        assert_eq!(runs[0].color, theme::rgb(theme::syntax_color("keyword")));
    }
}

impl crate::zoom_view::Zoomable for EditorView {
    fn zoom_pan(&mut self) -> &mut crate::zoom_view::ZoomPan {
        &mut self.svg_zoom
    }
}
