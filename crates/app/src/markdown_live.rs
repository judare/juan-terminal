//! Live Markdown editing: the document stays plain Markdown, but every line except the one
//! being edited is rendered with its markers hidden (headings sized, emphasis applied, bullets,
//! checkboxes, code blocks, quotes). Tables render as an editable grid (Notion-style).

use std::ops::Range;

use editor::markdown::{CharStyle, MdBlock, is_separator_row, line_styles, row_cells};
use editor::{HIGHLIGHT_NAMES, HighlightSpan};
use gpui::{
    App, Bounds, Element, ElementId, ElementInputHandler, Entity, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, Pixels, Point, ShapedLine,
    SharedString, StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle, Window,
    WrappedLine, fill, point, px, relative, size,
};

use crate::editor_view::EditorView;
use crate::theme;

const MAX_WIDTH: f32 = 780.;
const BASE_SIZE: f32 = 15.;
const CELL_PAD: f32 = 10.;

#[derive(Clone)]
pub struct Cell {
    /// Char columns of the cell content in the raw line.
    pub range: Range<usize>,
    pub x: Pixels,
    pub w: Pixels,
    pub shaped: ShapedLine,
}

#[derive(Clone)]
pub struct TableRow {
    pub cells: Vec<Cell>,
    pub header: bool,
    pub separator: bool,
    /// First line of the table this row belongs to.
    pub table_start: usize,
}

#[derive(Clone)]
pub struct MdLine {
    pub line: usize,
    pub y: Pixels,
    pub height: Pixels,
    pub line_height: Pixels,
    pub text_top: Pixels,
    pub wrapped: Option<WrappedLine>,
    /// Display byte offset per char column (len = chars + 1).
    pub col_bytes: Vec<usize>,
    pub block: MdBlock,
    pub table: Option<TableRow>,
}

/// Line layout reused across frames while only the scroll offset changes.
pub struct MdCache {
    key: (u64, u64, u32, u32, usize, usize, usize),
    lines: std::rc::Rc<Vec<MdLine>>,
    total_height: Pixels,
}

#[derive(Clone)]
pub struct MdLayout {
    /// Top-left of the content column in window coordinates, already scrolled.
    pub origin: Point<Pixels>,
    /// Origin of the element bounds (for overlays positioned relative to the editor).
    pub bounds_origin: Point<Pixels>,
    pub width: Pixels,
    pub lines: std::rc::Rc<Vec<MdLine>>,
}

fn cell_caret(cell: &Cell, col: usize, line: &str) -> Pixels {
    let local = col.clamp(cell.range.start, cell.range.end) - cell.range.start;
    let text: String = line.chars().skip(cell.range.start).take(local).collect();
    cell.x + px(CELL_PAD) + cell.shaped.x_for_index(text.len())
}

impl MdLayout {
    pub fn line_at_y(&self, y: Pixels) -> Option<&MdLine> {
        let rel = y - self.origin.y;
        self.lines.iter().find(|l| l.height > px(0.) && rel < l.y + l.height).or(self.lines.last())
    }

    /// Char column at a window position within `l` (`text` is the raw line).
    pub fn col_at(&self, l: &MdLine, pos: Point<Pixels>, text: &str) -> usize {
        if let Some(t) = &l.table {
            let x = pos.x - self.origin.x;
            let cell = t.cells.iter().find(|c| x < c.x + c.w).or(t.cells.last());
            return match cell {
                Some(c) => {
                    let byte = c.shaped.closest_index_for_x(x - c.x - px(CELL_PAD));
                    let content: String = text.chars().skip(c.range.start).take(c.range.len()).collect();
                    let byte = byte.min(content.len());
                    let byte = (0..=byte).rev().find(|b| content.is_char_boundary(*b)).unwrap_or(0);
                    c.range.start + content[..byte].chars().count()
                }
                None => 0,
            };
        }
        let Some(w) = &l.wrapped else { return 0 };
        let local = point(pos.x - self.origin.x, (pos.y - self.origin.y - l.y - l.text_top).max(px(0.)));
        let byte = match w.closest_index_for_position(local, l.line_height) {
            Ok(b) | Err(b) => b,
        };
        l.col_bytes.partition_point(|&b| b < byte).min(l.col_bytes.len().saturating_sub(1))
    }

    /// Caret point (relative to the document top) for a char column.
    pub fn caret(&self, l: &MdLine, col: usize, text: &str) -> Point<Pixels> {
        if let Some(t) = &l.table {
            let cell = t.cells.iter().find(|c| col <= c.range.end).or(t.cells.last());
            let x = cell.map(|c| cell_caret(c, col, text)).unwrap_or(px(0.));
            return point(x, l.y + l.text_top);
        }
        let Some(w) = &l.wrapped else { return point(px(0.), l.y) };
        let byte = l.col_bytes[col.min(l.col_bytes.len() - 1)];
        let p = w.position_for_index(byte, l.line_height).unwrap_or_default();
        point(p.x, l.y + l.text_top + p.y)
    }

    /// Top-left of the table containing `line`, relative to the element (for its toolbar).
    pub fn table_top(&self, line: usize) -> Option<(Pixels, Pixels, usize)> {
        let l = self.lines.iter().find(|l| l.line == line)?;
        let t = l.table.as_ref()?;
        let first = self.lines.iter().find(|x| x.line == t.table_start)?;
        Some((self.origin.y + first.y - self.bounds_origin.y, self.origin.x - self.bounds_origin.x, t.table_start))
    }
}

fn block_metrics(block: MdBlock) -> (f32, f32, f32) {
    // (font size, line-height factor, space above)
    match block {
        MdBlock::Heading(1) => (27., 1.3, 18.),
        MdBlock::Heading(2) => (22., 1.3, 14.),
        MdBlock::Heading(3) => (18., 1.35, 10.),
        MdBlock::Heading(_) => (16., 1.4, 8.),
        MdBlock::Code | MdBlock::Fence => (13., 1.5, 0.),
        MdBlock::Table => (14., 1.5, 0.),
        _ => (BASE_SIZE, 1.6, 0.),
    }
}

fn hsla(hex: u32) -> Hsla {
    theme::rgb(hex)
}

pub struct MarkdownElement {
    pub view: Entity<EditorView>,
}

impl IntoElement for MarkdownElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

enum Paint {
    Wrapped(Point<Pixels>, Pixels, WrappedLine),
    Shaped(Point<Pixels>, Pixels, ShapedLine),
}

pub struct Prepaint {
    quads: Vec<(Bounds<Pixels>, Hsla)>,
    texts: Vec<Paint>,
    cursor: Option<Bounds<Pixels>>,
    marked: Option<(Point<Pixels>, Pixels, WrappedLine)>,
}

#[allow(clippy::too_many_arguments)]
fn build_line(
    text: &str,
    styles: &[CharStyle],
    block: MdBlock,
    hide: bool,
    line_start_byte: usize,
    spans: &[HighlightSpan],
    prose: &Font,
    mono: &Font,
) -> (String, Vec<TextRun>, Vec<usize>) {
    let mut display = String::with_capacity(text.len());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut col_bytes = Vec::with_capacity(text.len() + 1);
    let mut si = spans.partition_point(|s| s.end <= line_start_byte);
    let mut byte = line_start_byte;
    let heading = matches!(block, MdBlock::Heading(_));
    for (ch, st) in text.chars().zip(styles.iter().copied()) {
        col_bytes.push(display.len());
        let ch_len = ch.len_utf8();
        if hide && st.marker && st.replace.is_none() {
            byte += ch_len;
            continue;
        }
        let piece: std::borrow::Cow<str> = match (hide, st.replace) {
            (true, Some(r)) => r.into(),
            _ if ch == '\t' => "    ".into(),
            _ => ch.to_string().into(),
        };
        while si < spans.len() && spans[si].end <= byte {
            si += 1;
        }
        let syntax = spans.get(si).filter(|s| s.start <= byte && byte < s.end).map(|s| HIGHLIGHT_NAMES[s.kind as usize]);
        let color = if st.marker {
            theme::TEXT_FAINT
        } else if st.code && matches!(block, MdBlock::Code) {
            syntax.map(theme::syntax_color).unwrap_or(theme::TEXT)
        } else if st.link {
            theme::ACCENT
        } else if st.replace.is_some() && hide {
            theme::TEXT_MUTED
        } else if heading {
            0xeef0f4
        } else if matches!(block, MdBlock::Quote) {
            theme::TEXT_MUTED
        } else if st.code {
            0xe6b673
        } else {
            0xd5d8de
        };
        let mut font = if st.code { mono.clone() } else { prose.clone() };
        if st.bold || heading {
            font.weight = FontWeight::BOLD;
        }
        if st.italic {
            font.style = FontStyle::Italic;
        }
        let c = hsla(color);
        let run = TextRun {
            len: piece.len(),
            font,
            color: c,
            background_color: (st.code && !matches!(block, MdBlock::Code | MdBlock::Table)).then(|| hsla(0x24272d)),
            underline: st.link.then(|| UnderlineStyle { color: Some(c), thickness: px(1.), wavy: false }),
            strikethrough: st.strike.then(|| StrikethroughStyle { color: Some(c), thickness: px(1.) }),
        };
        match runs.last_mut() {
            Some(r) if r.font == run.font && r.color == run.color && r.background_color == run.background_color && r.underline == run.underline && r.strikethrough == run.strikethrough => r.len += run.len,
            _ => runs.push(run),
        }
        display.push_str(&piece);
        byte += ch_len;
    }
    col_bytes.push(display.len());
    if display.is_empty() {
        display.push(' ');
        runs.push(TextRun { len: 1, font: prose.clone(), color: hsla(theme::TEXT), background_color: None, underline: None, strikethrough: None });
    }
    (display, runs, col_bytes)
}

/// Selection/match rectangles for char range `a..b` on a line (multi-row aware).
fn range_rects(layout: &MdLayout, l: &MdLine, text: &str, a: usize, b: usize, extend_eol: bool) -> Vec<Bounds<Pixels>> {
    let p0 = layout.caret(l, a, text);
    let p1 = layout.caret(l, b, text);
    let lh = l.line_height;
    let w = layout.width;
    let mut out = Vec::new();
    if p0.y == p1.y {
        let end = if extend_eol { p1.x + px(6.) } else { p1.x };
        if end > p0.x {
            out.push(Bounds::new(p0, size(end - p0.x, lh)));
        }
    } else {
        out.push(Bounds::new(p0, size(w - p0.x, lh)));
        let mut y = p0.y + lh;
        while y < p1.y {
            out.push(Bounds::new(point(px(0.), y), size(w, lh)));
            y += lh;
        }
        out.push(Bounds::new(point(px(0.), p1.y), size(p1.x + if extend_eol { px(6.) } else { px(0.) }, lh)));
    }
    out
}

impl Element for MarkdownElement {
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
        let focused = self.view.read(cx).focus_handle_ref().is_focused(window);
        self.view.update(cx, |view, _| {
            let ts = window.text_system().clone();
            let scale = view.font_scale();
            let width = (bounds.size.width - px(64.)).min(px(MAX_WIDTH)).max(px(200.));
            let left = bounds.origin.x + (bounds.size.width - width) / 2.;
            let prose = gpui::font(theme::UI_FONT);
            let mono = gpui::font(theme::MONO_FONT);
            let sel = view.doc.selection.range();
            let (cur_line, cur_col) = view.doc.line_col(view.doc.selection.head);
            let (sel_l0, _) = view.doc.line_col(sel.start);
            let (sel_l1, _) = view.doc.line_col(sel.end);
            let key = (view.doc.version(), view.spans_gen, f32::from(width).to_bits(), scale.to_bits(), cur_line, sel_l0, sel_l1);
            let cached = view.md_cache.as_ref().filter(|c| c.key == key).map(|c| (c.lines.clone(), c.total_height));
            let (lines, total_height) = match cached {
                Some(hit) => hit,
                None => {
            view.refresh_md_blocks();
            let blocks = view.md_blocks().to_vec();
            let spans = view.current_spans().to_vec();
            let line_count = view.doc.line_count();
            let texts: Vec<String> = (0..line_count).map(|i| view.doc.line(i).to_string()).collect();

            let mut lines: Vec<MdLine> = Vec::with_capacity(line_count);
            let mut y = px(16.);
            let mut i = 0;
            while i < line_count {
                let block = blocks.get(i).copied().unwrap_or(MdBlock::Paragraph);
                let (fs, lhf, above) = block_metrics(block);
                let font_size = px(fs * scale);
                let lh = (font_size * lhf).round();

                if block == MdBlock::Table {
                    // Lay out the whole table: shared column widths.
                    let start = i;
                    let mut end = i;
                    while end < line_count && blocks.get(end) == Some(&MdBlock::Table) {
                        end += 1;
                    }
                    let sep_line = (start..end).find(|&k| is_separator_row(&texts[k]));
                    let mut shaped_rows: Vec<Vec<(Range<usize>, ShapedLine)>> = Vec::new();
                    let mut col_w: Vec<Pixels> = Vec::new();
                    for k in start..end {
                        let chars: Vec<char> = texts[k].chars().collect();
                        let header = sep_line.is_some_and(|s| k < s);
                        let separator = Some(k) == sep_line;
                        let mut row = Vec::new();
                        for (ci, r) in row_cells(&texts[k]).into_iter().enumerate() {
                            let content: String = chars[r.clone()].iter().collect();
                            let mut font = prose.clone();
                            if header {
                                font.weight = FontWeight::SEMIBOLD;
                            }
                            let run = TextRun { len: content.len(), font, color: hsla(if header { 0xeef0f4 } else { 0xd5d8de }), background_color: None, underline: None, strikethrough: None };
                            let shaped = ts.shape_line(SharedString::from(content), font_size, &[run], None);
                            if col_w.len() <= ci {
                                col_w.push(px(60.));
                            }
                            if !separator {
                                col_w[ci] = col_w[ci].max((shaped.width + px(CELL_PAD * 2.)).min(px(360.)));
                            }
                            row.push((r, shaped));
                        }
                        shaped_rows.push(row);
                    }
                    for (k, row) in (start..end).zip(shaped_rows) {
                        let separator = Some(k) == sep_line;
                        let mut x = px(0.);
                        let cells = row
                            .into_iter()
                            .enumerate()
                            .map(|(ci, (range, shaped))| {
                                let w = col_w.get(ci).copied().unwrap_or(px(60.));
                                let c = Cell { range, x, w, shaped };
                                x += w;
                                c
                            })
                            .collect();
                        let height = if separator { px(0.) } else { lh + px(10.) };
                        lines.push(MdLine {
                            line: k,
                            y,
                            height,
                            line_height: lh,
                            text_top: px(5.),
                            wrapped: None,
                            col_bytes: vec![],
                            block,
                            table: Some(TableRow { cells, header: sep_line.is_some_and(|s| k < s), separator, table_start: start }),
                        });
                        y += height;
                    }
                    y += px(8.);
                    i = end;
                    continue;
                }

                let text = &texts[i];
                let styles = line_styles(text, block);
                let editing = i == cur_line || (i >= sel_l0 && i <= sel_l1);
                let (display, runs, col_bytes) = build_line(text, &styles, block, !editing, view.doc.line_start_byte(i), &spans, &prose, &mono);
                let wrapped = ts
                    .shape_text(SharedString::from(display), font_size, &runs, Some(width - px(if matches!(block, MdBlock::Code | MdBlock::Fence) { 24. } else { 0. })), None)
                    .ok()
                    .and_then(|mut v| (!v.is_empty()).then(|| v.remove(0)));
                let Some(wrapped) = wrapped else {
                    i += 1;
                    continue;
                };
                let text_top = px(above * scale);
                let height = text_top + wrapped.size(lh).height;
                lines.push(MdLine { line: i, y, height, line_height: lh, text_top, wrapped: Some(wrapped), col_bytes, block, table: None });
                y += height;
                i += 1;
            }
            let total_height = y + px(16.);
            let lines = std::rc::Rc::new(lines);
            view.md_cache = Some(MdCache { key, lines: lines.clone(), total_height });
            (lines, total_height)
                }
            };

            // Autoscroll to the caret.
            if view.take_autoscroll() {
                let tmp = MdLayout { origin: point(px(0.), px(0.)), bounds_origin: bounds.origin, width, lines: std::rc::Rc::new(vec![]) };
                if let Some(l) = lines.iter().find(|l| l.line == cur_line) {
                    let cy = tmp.caret(l, cur_col, &view.doc.line(cur_line)).y;
                    let lh = l.line_height;
                    if cy < view.scroll_y() {
                        view.set_scroll_y(cy - lh);
                    } else if cy + lh > view.scroll_y() + bounds.size.height {
                        view.set_scroll_y(cy + lh * 2. - bounds.size.height);
                    }
                }
            }
            let max_scroll = (total_height - bounds.size.height * 0.5).max(px(0.));
            view.set_scroll_y(view.scroll_y().clamp(px(0.), max_scroll));

            let origin = point(left, bounds.origin.y - view.scroll_y());
            let layout = MdLayout { origin, bounds_origin: bounds.origin, width, lines };

            let mut quads = Vec::new();
            let mut paints = Vec::new();
            let mut cursor = None;
            let find = view.find_ranges();
            let top = bounds.origin.y;
            let bottom = bounds.origin.y + bounds.size.height;
            let grid = hsla(0x343841);
            let lines_rc = layout.lines.clone();
            let first_vis = lines_rc.partition_point(|l| origin.y + l.y + l.height < top);
            for l in &lines_rc[first_vis..] {
                let ly = origin.y + l.y;
                if ly > bottom {
                    break;
                }
                if ly + l.height < top {
                    continue;
                }
                let text = &view.doc.line(l.line).to_string();
                let line_start = view.doc.pos(l.line, 0);
                let line_end = line_start + view.doc.line_len(l.line);
                match (&l.table, l.block) {
                    (Some(t), _) if !t.separator => {
                        let row_w = t.cells.last().map(|c| c.x + c.w).unwrap_or(px(0.));
                        if t.header {
                            quads.push((Bounds::new(point(left, ly), size(row_w, l.height)), hsla(0x1e2127)));
                        }
                        if t.table_start == l.line {
                            quads.push((Bounds::new(point(left, ly), size(row_w, px(1.))), grid));
                        }
                        quads.push((Bounds::new(point(left, ly + l.height - px(1.)), size(row_w, px(1.))), grid));
                        for c in &t.cells {
                            quads.push((Bounds::new(point(left + c.x, ly), size(px(1.), l.height)), grid));
                        }
                        quads.push((Bounds::new(point(left + row_w - px(1.), ly), size(px(1.), l.height)), grid));
                    }
                    (None, MdBlock::Code | MdBlock::Fence) => quads.push((Bounds::new(point(left - px(12.), ly), size(width + px(24.), l.height)), hsla(0x1b1d22))),
                    (None, MdBlock::Quote) => quads.push((Bounds::new(point(left - px(12.), ly + l.text_top), size(px(3.), l.height - l.text_top)), hsla(0x3b4252))),
                    (None, MdBlock::Rule) if l.line != cur_line => quads.push((Bounds::new(point(left, ly + l.height / 2.), size(width, px(1.))), hsla(theme::BORDER))),
                    _ => {}
                }
                for (m, current) in &find {
                    if m.end < line_start || m.start > line_end {
                        continue;
                    }
                    let a = m.start.max(line_start) - line_start;
                    let b = m.end.min(line_end) - line_start;
                    for r in range_rects(&layout, l, text, a, b, false) {
                        quads.push((Bounds::new(point(origin.x + r.origin.x, origin.y + r.origin.y), r.size), hsla(if *current { theme::FIND_MATCH_CURRENT } else { theme::FIND_MATCH })));
                    }
                }
                if !sel.is_empty() && sel.start <= line_end && sel.end >= line_start {
                    let a = sel.start.max(line_start) - line_start;
                    let b = sel.end.min(line_end) - line_start;
                    for r in range_rects(&layout, l, text, a, b, sel.end > line_end && l.table.is_none()) {
                        quads.push((Bounds::new(point(origin.x + r.origin.x, origin.y + r.origin.y), r.size), hsla(theme::SELECTION)));
                    }
                }
                if l.line == cur_line && l.height > px(0.) {
                    let c = layout.caret(l, cur_col, text);
                    cursor = Some(Bounds::new(point(origin.x + c.x, origin.y + c.y), size(px(2.), l.line_height)));
                }
                match (&l.table, &l.wrapped) {
                    (Some(t), _) => {
                        if !t.separator {
                            for c in &t.cells {
                                paints.push(Paint::Shaped(point(left + c.x + px(CELL_PAD), ly + l.text_top), l.line_height, c.shaped.clone()));
                            }
                        }
                    }
                    (None, Some(w)) => paints.push(Paint::Wrapped(point(origin.x, ly + l.text_top), l.line_height, w.clone())),
                    _ => {}
                }
            }
            let marked = view.marked().zip(cursor).and_then(|(t, c)| {
                let col = hsla(theme::TEXT);
                let run = TextRun { len: t.len(), font: prose.clone(), color: col, background_color: Some(hsla(theme::EDITOR_BG)), underline: Some(UnderlineStyle { color: Some(col), thickness: px(1.), wavy: false }), strikethrough: None };
                let w = ts.shape_text(SharedString::from(t), px(BASE_SIZE * scale), &[run], None, None).ok()?.into_iter().next()?;
                Some((c.origin, c.size.height, w))
            });
            view.set_md_layout(layout);
            Prepaint { quads, texts: paints, cursor: if focused { cursor } else { None }, marked }
        })
    }

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), state: &mut Prepaint, window: &mut Window, cx: &mut App) {
        let focus = self.view.read(cx).focus_handle_ref().clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for (b, c) in state.quads.drain(..) {
                window.paint_quad(fill(b, c));
            }
            for p in &state.texts {
                match p {
                    Paint::Wrapped(o, lh, w) => {
                        let _ = w.paint(*o, *lh, TextAlign::Left, None, window, cx);
                    }
                    Paint::Shaped(o, lh, s) => {
                        let _ = s.paint(*o, *lh, window, cx);
                    }
                }
            }
            if let Some((o, lh, m)) = state.marked.as_ref() {
                let _ = m.paint(*o, *lh, TextAlign::Left, None, window, cx);
            }
            if let Some(c) = state.cursor {
                window.paint_quad(fill(c, hsla(theme::ACCENT)));
            }
        });
    }
}
