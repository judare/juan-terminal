//! Diff viewer in the style of VS Code: the whole file (single-file diffs carry full context),
//! unified (default) or side-by-side, syntax highlighted, with the changed words inside each modified line
//! highlighted. Navigation and stage/unstage work per change block.

use std::path::{Path, PathBuf};

use editor::{HIGHLIGHT_NAMES, HighlightSpan, Language, highlight};
use git_engine::{DiffLineKind, FileDiff, inline_changes, side_by_side_indices};
use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, HighlightStyle, KeyBinding,
    Render, ScrollStrategy, SharedString, StyledText, UniformListScrollHandle, Window, actions, div,
    prelude::*, px, uniform_list,
};

use crate::theme;

/// Last layout picked with the toggle; new diff views open with it (unified by default).
static SIDE_BY_SIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

actions!(diff_view, [NextHunk, PreviousHunk, ToggleSideBySide, AcceptProposal, RejectProposal]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("DiffView");
    cx.bind_keys([
        KeyBinding::new("alt-down", NextHunk, c),
        KeyBinding::new("alt-up", PreviousHunk, c),
        KeyBinding::new("n", NextHunk, c),
        KeyBinding::new("p", PreviousHunk, c),
        KeyBinding::new("s", ToggleSideBySide, c),
        KeyBinding::new("cmd-enter", AcceptProposal, c),
        KeyBinding::new("cmd-backspace", RejectProposal, c),
    ]);
}

/// What the diff represents; enables hunk staging for index/worktree diffs.
#[derive(Clone, Debug)]
pub enum DiffSource {
    /// Working tree vs index (unstaged) or index vs HEAD (staged) for one file.
    Changes { repo: PathBuf, path: String, staged: bool },
    /// Read-only (commit, branch comparison).
    ReadOnly,
    /// An edit Claude proposes (IDE `openDiff`): Accept / Reject.
    ClaudeProposal,
}

pub enum DiffViewEvent {
    /// Stage (`reverse == false`) or unstage a hunk patch.
    ApplyHunk { repo: PathBuf, patch: String, reverse: bool },
    OpenFile { path: PathBuf, line: usize },
    /// Accept (true) or reject a Claude proposal.
    Resolve(bool),
}

#[derive(Clone)]
enum Row {
    FileHeader(usize),
    HunkHeader(usize, usize),
    Unified(usize, usize, usize), // file, hunk, line
    Split(usize, usize, Option<usize>, Option<usize>),
    Binary,
}

/// Per-line highlight spans (byte ranges within the line) for old and new sides.
type LineSpans = Vec<(std::ops::Range<usize>, u16)>;

/// One contiguous run of changed lines.
struct Block {
    file: usize,
    hunk: usize,
    lines: std::ops::Range<usize>,
    /// First row showing this block.
    row: usize,
}

pub struct DiffView {
    pub title: SharedString,
    pub source: DiffSource,
    files: Vec<FileDiff>,
    /// Base directory for resolving file paths (repo root).
    root: Option<PathBuf>,
    side_by_side: bool,
    rows: Vec<Row>,
    blocks: Vec<Block>,
    /// Row index -> block whose first row it is (carries the block's actions).
    row_block: std::collections::HashMap<usize, usize>,
    /// [file][hunk][line] -> spans
    spans: Vec<Vec<Vec<LineSpans>>>,
    /// [file][hunk][line] -> changed byte ranges within a modified line.
    emphasis: Vec<Vec<Vec<Vec<std::ops::Range<usize>>>>>,
    scroll: UniformListScrollHandle,
    current_hunk: usize,
    focus_handle: FocusHandle,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    pub fn new(title: impl Into<SharedString>, source: DiffSource, root: Option<PathBuf>, files: Vec<FileDiff>, cx: &mut Context<Self>) -> Self {
        let spans = files.iter().map(highlight_file).collect();
        let emphasis = files.iter().map(emphasis_file).collect();
        let mut this = Self {
            title: title.into(),
            source,
            files,
            root,
            side_by_side: SIDE_BY_SIDE.load(std::sync::atomic::Ordering::Relaxed),
            rows: Vec::new(),
            blocks: Vec::new(),
            row_block: Default::default(),
            spans,
            emphasis,
            scroll: UniformListScrollHandle::new(),
            current_hunk: 0,
            focus_handle: cx.focus_handle(),
        };
        this.rebuild_rows();
        this
    }

    pub fn set_files(&mut self, files: Vec<FileDiff>, cx: &mut Context<Self>) {
        self.spans = files.iter().map(highlight_file).collect();
        self.emphasis = files.iter().map(emphasis_file).collect();
        self.files = files;
        self.rebuild_rows();
        cx.notify();
    }

    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|f| f.hunks.is_empty() && !f.binary)
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.blocks.clear();
        self.row_block.clear();
        let multi = self.files.len() > 1;
        for (fi, file) in self.files.iter().enumerate() {
            if multi {
                self.rows.push(Row::FileHeader(fi));
            }
            if file.binary {
                self.rows.push(Row::Binary);
                continue;
            }
            // A single hunk starting at the top is the whole file: no `@@` separator.
            let whole_file = file.hunks.len() == 1 && file.hunks[0].old_start <= 1 && file.hunks[0].new_start <= 1;
            for (hi, hunk) in file.hunks.iter().enumerate() {
                if !whole_file {
                    self.rows.push(Row::HunkHeader(fi, hi));
                }
                let mut blocks = hunk.change_blocks().into_iter().peekable();
                let first_row = self.rows.len();
                if self.side_by_side {
                    for (l, r) in side_by_side_indices(hunk) {
                        self.rows.push(Row::Split(fi, hi, l, r));
                    }
                } else {
                    for li in 0..hunk.lines.len() {
                        self.rows.push(Row::Unified(fi, hi, li));
                    }
                }
                // Map each block's first line to the row that shows it.
                for (ri, row) in self.rows[first_row..].iter().enumerate() {
                    let Some(b) = blocks.peek() else { break };
                    let shows = match row {
                        Row::Unified(_, _, li) => *li == b.start,
                        Row::Split(_, _, l, r) => l.is_some_and(|x| x >= b.start) || r.is_some_and(|x| x >= b.start),
                        _ => false,
                    };
                    if shows {
                        let lines = blocks.next().unwrap();
                        self.row_block.insert(first_row + ri, self.blocks.len());
                        self.blocks.push(Block { file: fi, hunk: hi, lines, row: first_row + ri });
                    }
                }
            }
        }
        self.current_hunk = self.current_hunk.min(self.blocks.len().saturating_sub(1));
    }

    fn toggle_layout(&mut self, cx: &mut Context<Self>) {
        self.side_by_side = !self.side_by_side;
        SIDE_BY_SIDE.store(self.side_by_side, std::sync::atomic::Ordering::Relaxed);
        self.rebuild_rows();
        cx.notify();
    }

    fn jump_hunk(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.blocks.is_empty() {
            return;
        }
        let n = self.blocks.len() as isize;
        self.current_hunk = (self.current_hunk as isize + delta).rem_euclid(n) as usize;
        self.scroll.scroll_to_item(self.blocks[self.current_hunk].row, ScrollStrategy::Center);
        cx.notify();
    }

    /// Open / Stage buttons for the change block starting at row `ix`.
    fn block_actions(&self, ix: usize, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let b = &self.blocks[*self.row_block.get(&ix)?];
        let lines = &self.files[b.file].hunks[b.hunk].lines;
        let line = lines[b.lines.clone()].iter().chain(&lines[b.lines.end..]).find_map(|l| l.new_no).unwrap_or(1).saturating_sub(1);
        let mut row = div().flex().flex_none().gap(px(6.)).px(px(8.));
        if let Some(path) = self.file_path(b.file).filter(|p| p.is_file()) {
            row = row.child(toolbar_button(("open-block", ix), "Open").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cx.emit(DiffViewEvent::OpenFile { path: path.clone(), line })
            })));
        }
        if let DiffSource::Changes { repo, staged, .. } = &self.source {
            let (repo, staged) = (repo.clone(), *staged);
            let patch = self.files[b.file].block_patch(b.hunk, b.lines.clone());
            row = row.child(toolbar_button(("stage-block", ix), if staged { "Unstage" } else { "Stage" }).on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cx.emit(DiffViewEvent::ApplyHunk { repo: repo.clone(), patch: patch.clone(), reverse: staged })
            })));
        }
        Some(row)
    }

    fn file_path(&self, fi: usize) -> Option<PathBuf> {
        let rel = self.files.get(fi)?.display_path();
        Some(self.root.as_ref().map(|r| r.join(rel)).unwrap_or_else(|| PathBuf::from(rel)))
    }

    fn render_code(&self, fi: usize, hi: usize, li: usize) -> StyledText {
        let line = &self.files[fi].hunks[hi].lines[li];
        // Expand tabs, keeping a map from source byte offsets to display offsets.
        let mut text = String::with_capacity(line.text.len());
        let mut map = Vec::with_capacity(line.text.len() + 1);
        for (i, c) in line.text.char_indices() {
            map.resize(i, text.len());
            map.push(text.len());
            if c == '\t' {
                text.push_str("    ");
            } else {
                text.push(c);
            }
        }
        map.resize(line.text.len() + 1, text.len());
        let at = |r: &std::ops::Range<usize>| map[r.start.min(line.text.len())]..map[r.end.min(line.text.len())];
        let syntax = self.spans[fi][hi][li]
            .iter()
            .map(|(r, k)| (at(r), HighlightStyle { color: Some(theme::rgb(theme::syntax_color(HIGHLIGHT_NAMES[*k as usize]))), ..Default::default() }))
            .filter(|(r, _)| !r.is_empty())
            .collect::<Vec<_>>();
        let emph_bg = match line.kind {
            DiffLineKind::Added => WORD_ADDED_BG,
            _ => WORD_REMOVED_BG,
        };
        let emphasis = self.emphasis[fi][hi][li]
            .iter()
            .map(|r| (at(r), HighlightStyle { background_color: Some(theme::rgb(emph_bg)), ..Default::default() }))
            .filter(|(r, _)| !r.is_empty())
            .collect::<Vec<_>>();
        let highlights: Vec<_> = if emphasis.is_empty() { syntax } else { gpui::combine_highlights(syntax, emphasis).collect() };
        StyledText::new(SharedString::from(text)).with_highlights(highlights)
    }
}

/// Highlight each hunk's old and new side as continuous text, then split spans per line.
fn highlight_file(file: &FileDiff) -> Vec<Vec<LineSpans>> {
    let lang = Language::from_path(Path::new(file.display_path()));
    file.hunks
        .iter()
        .map(|hunk| {
            let mut result: Vec<LineSpans> = vec![Vec::new(); hunk.lines.len()];
            for side_new in [false, true] {
                let mut text = String::new();
                let mut starts = Vec::new(); // (line index, byte start)
                for (i, l) in hunk.lines.iter().enumerate() {
                    let include = match l.kind {
                        DiffLineKind::Context => true,
                        DiffLineKind::Added => side_new,
                        DiffLineKind::Removed => !side_new,
                    };
                    if include {
                        starts.push((i, text.len()));
                        text.push_str(&l.text);
                        text.push('\n');
                    }
                }
                if text.is_empty() {
                    continue;
                }
                let spans: Vec<HighlightSpan> = highlight(lang, &text);
                for (k, &(li, start)) in starts.iter().enumerate() {
                    // Context lines are taken from the new side only.
                    if hunk.lines[li].kind == DiffLineKind::Context && !side_new {
                        continue;
                    }
                    let end = starts.get(k + 1).map(|s| s.1 - 1).unwrap_or(text.len() - 1);
                    let first = spans.partition_point(|s| s.end <= start);
                    for s in &spans[first..] {
                        if s.start >= end {
                            break;
                        }
                        let a = s.start.max(start) - start;
                        let b = s.end.min(end) - start;
                        if b > a {
                            result[li].push((a..b, s.kind));
                        }
                    }
                }
            }
            result
        })
        .collect()
}

/// Changed byte ranges within each modified line, pairing removed and added lines the same
/// way the side-by-side view does.
fn emphasis_file(file: &FileDiff) -> Vec<Vec<Vec<std::ops::Range<usize>>>> {
    file.hunks
        .iter()
        .map(|hunk| {
            let mut out = vec![Vec::new(); hunk.lines.len()];
            for (l, r) in side_by_side_indices(hunk) {
                if let (Some(l), Some(r)) = (l, r) {
                    if l != r {
                        let (a, b) = inline_changes(&hunk.lines[l].text, &hunk.lines[r].text);
                        out[l] = a;
                        out[r] = b;
                    }
                }
            }
            out
        })
        .collect()
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

const ROW_H: f32 = 20.;
/// Stronger tint behind the exact words that changed inside a modified line.
const WORD_ADDED_BG: u32 = 0x2d5a36;
const WORD_REMOVED_BG: u32 = 0x6a2a32;

fn line_bg(kind: DiffLineKind) -> Option<u32> {
    match kind {
        DiffLineKind::Added => Some(0x1c2b20),
        DiffLineKind::Removed => Some(0x2e1d20),
        DiffLineKind::Context => None,
    }
}

/// Stronger tint for the line-number gutter of a changed line.
fn gutter_bg(kind: DiffLineKind) -> Option<u32> {
    match kind {
        DiffLineKind::Added => Some(0x23402a),
        DiffLineKind::Removed => Some(0x4a2226),
        DiffLineKind::Context => None,
    }
}

fn gutter(n: Option<usize>) -> gpui::Div {
    div()
        .w(px(44.))
        .h_full()
        .flex_none()
        .pr(px(8.))
        .flex()
        .items_center()
        .justify_end()
        .text_color(theme::rgb(theme::GUTTER_TEXT))
        .child(n.map(|n| n.to_string()).unwrap_or_default())
}

/// Line-number gutter tinted for its line kind; changed lines get brighter numbers.
fn kind_gutter(n: Option<usize>, kind: DiffLineKind) -> gpui::Div {
    gutter(n)
        .when_some(gutter_bg(kind), |d, c| d.bg(theme::rgb(c)).text_color(theme::rgb(theme::GUTTER_TEXT_ACTIVE)))
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.rows.len();
        let (adds, dels) = self.files.iter().fold((0, 0), |acc, f| {
            let (a, d) = f.stats();
            (acc.0 + a, acc.1 + d)
        });
        let toolbar = div()
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
            .child(div().text_color(theme::rgb(theme::TEXT)).child(self.title.clone()))
            .child(div().text_color(theme::rgb(theme::ADDED)).child(format!("+{adds}")))
            .child(div().text_color(theme::rgb(theme::DELETED)).child(format!("−{dels}")))
            .child(div().flex_1())
            .when(matches!(self.source, DiffSource::ClaudeProposal), |d| {
                d.child(crate::assets::claude_logo(16.))
                    .child(
                        div()
                            .id("claude-reject")
                            .px(px(10.))
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .cursor_pointer()
                            .bg(theme::rgb(theme::HOVER))
                            .text_color(theme::rgb(0xf0b3b8))
                            .hover(|s| s.bg(theme::rgb(0x3a1f22)))
                            .child("Reject  ⌘⌫")
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(DiffViewEvent::Resolve(false)))),
                    )
                    .child(
                        div()
                            .id("claude-accept")
                            .px(px(10.))
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .cursor_pointer()
                            .bg(theme::rgb(0x2b4a7a))
                            .text_color(theme::rgb(theme::TEXT))
                            .hover(|s| s.bg(theme::rgb(0x355a92)))
                            .child("Accept  ⌘↵")
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(DiffViewEvent::Resolve(true)))),
                    )
            })
            .when(!self.blocks.is_empty(), |d| {
                d.child(div().text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("{} of {} changes", self.current_hunk + 1, self.blocks.len())))
            })
            .child(toolbar_button("prev-hunk", "↑ Change").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.jump_hunk(-1, cx))))
            .child(toolbar_button("next-hunk", "↓ Change").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.jump_hunk(1, cx))))
            .child(
                toolbar_button("toggle-sbs", if self.side_by_side { "Unified" } else { "Side by side" })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_layout(cx))),
            );

        let body = if self.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(theme::rgb(theme::TEXT_FAINT))
                .child("No changes")
                .into_any_element()
        } else {
            uniform_list(
                "diff-rows",
                count,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|ix| {
                            let row = this.rows[ix].clone();
                            let base = div().id(ix).flex().items_center().h(px(ROW_H)).w_full().text_size(px(12.5)).font_family(theme::MONO_FONT).whitespace_nowrap();
                            match row {
                                Row::FileHeader(fi) => {
                                    let f = &this.files[fi];
                                    let (a, d) = f.stats();
                                    let name = f.display_path().to_string();
                                    let (icon, color) = crate::assets::file_icon(name.rsplit('/').next().unwrap_or(""));
                                    base.gap(px(8.))
                                        .px(px(10.))
                                        .bg(theme::rgb(theme::SIDEBAR_BG))
                                        .font_family(theme::UI_FONT)
                                        .child(gpui::svg().path(icon).size(px(13.)).text_color(theme::rgb(color)))
                                        .child(div().text_color(theme::rgb(theme::TEXT)).child(name))
                                        .child(div().text_color(theme::rgb(theme::ADDED)).child(format!("+{a}")))
                                        .child(div().text_color(theme::rgb(theme::DELETED)).child(format!("−{d}")))
                                        .into_any_element()
                                }
                                Row::Binary => base.px(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Binary file").into_any_element(),
                                Row::HunkHeader(fi, hi) => {
                                    let hunk = &this.files[fi].hunks[hi];
                                    let header = hunk.header.clone();
                                    let line = hunk.new_start.saturating_sub(1);
                                    let path = this.file_path(fi);
                                    let mut row = base
                                        .gap(px(8.))
                                        .px(px(10.))
                                        .bg(theme::rgb(0x1b2230))
                                        .text_color(theme::rgb(0x7f8fa9))
                                        .child(div().flex_1().overflow_hidden().child(header));
                                    if let Some(path) = path.filter(|p| p.is_file()) {
                                        row = row.child(toolbar_button(("open-hunk", ix), "Open").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                            cx.emit(DiffViewEvent::OpenFile { path: path.clone(), line })
                                        })));
                                    }
                                    row.into_any_element()
                                }
                                Row::Unified(fi, hi, li) => {
                                    let l = &this.files[fi].hunks[hi].lines[li];
                                    let sign = match l.kind {
                                        DiffLineKind::Added => "+",
                                        DiffLineKind::Removed => "−",
                                        DiffLineKind::Context => " ",
                                    };
                                    let actions = this.block_actions(ix, cx);
                                    base.when_some(line_bg(l.kind), |d, c| d.bg(theme::rgb(c)))
                                        .child(kind_gutter(l.old_no, l.kind))
                                        .child(kind_gutter(l.new_no, l.kind))
                                        .child(div().w(px(24.)).pl(px(8.)).flex_none().text_color(theme::rgb(match l.kind {
                                            DiffLineKind::Added => theme::ADDED,
                                            DiffLineKind::Removed => theme::DELETED,
                                            DiffLineKind::Context => theme::TEXT_FAINT,
                                        })).child(sign))
                                        .child(div().flex_1().min_w_0().overflow_hidden().text_color(theme::rgb(theme::TEXT)).child(this.render_code(fi, hi, li)))
                                        .children(actions)
                                        .into_any_element()
                                }
                                Row::Split(fi, hi, left, right) => {
                                    let lines = &this.files[fi].hunks[hi].lines;
                                    let half = |idx: Option<usize>, old: bool| {
                                        let mut d = div().flex().flex_1().min_w_0().h_full().items_center().overflow_hidden();
                                        if let Some(i) = idx {
                                            let l = &lines[i];
                                            let kind = l.kind;
                                            d = d
                                                .when_some(line_bg(kind), |d, c| d.bg(theme::rgb(c)))
                                                .child(kind_gutter(if old { l.old_no } else { l.new_no }, kind))
                                                .child(div().flex_1().min_w_0().overflow_hidden().text_color(theme::rgb(theme::TEXT)).child(this.render_code(fi, hi, i)));
                                        } else {
                                            d = d.bg(theme::rgb(0x18191c));
                                        }
                                        d
                                    };
                                    let actions = this.block_actions(ix, cx);
                                    base.child(half(left, true))
                                        .child(div().w(px(1.)).h_full().bg(theme::rgb(theme::BORDER)))
                                        .child(half(right, false))
                                        .children(actions)
                                        .into_any_element()
                                }
                            }
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(self.scroll.clone())
            .flex_1()
            .into_any_element()
        };

        div()
            .key_context("DiffView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &AcceptProposal, _, cx| {
                if matches!(this.source, DiffSource::ClaudeProposal) {
                    cx.emit(DiffViewEvent::Resolve(true));
                }
            }))
            .on_action(cx.listener(|this, _: &RejectProposal, _, cx| {
                if matches!(this.source, DiffSource::ClaudeProposal) {
                    cx.emit(DiffViewEvent::Resolve(false));
                }
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| this.jump_hunk(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousHunk, _, cx| this.jump_hunk(-1, cx)))
            .on_action(cx.listener(|this, _: &ToggleSideBySide, _, cx| this.toggle_layout(cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::rgb(theme::EDITOR_BG))
            .child(toolbar)
            .child(body)
    }
}

pub fn toolbar_button(id: impl Into<gpui::ElementId>, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(8.))
        .py(px(2.))
        .rounded(px(4.))
        .cursor_pointer()
        .font_family(theme::UI_FONT)
        .text_size(px(11.5))
        .text_color(theme::rgb(theme::TEXT_MUTED))
        .bg(theme::rgb(theme::HOVER))
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
        .child(label)
}
