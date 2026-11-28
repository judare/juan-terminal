//! Multi-line, soft-wrapping text area (commit messages, notes). Grows with its content up to
//! `max_rows`, then scrolls to keep the caret visible.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId,
    InspectorElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, SharedString, Style, TextAlign, TextRun, UTF16Selection,
    Window, WrappedLine, actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::theme;

actions!(
    text_area,
    [Backspace, Delete, Left, Right, Up, Down, SelectLeft, SelectRight, SelectUp, SelectDown, SelectAll, Home, End, Paste, Cut, Copy, Newline, ForceNewline, Submit, Cancel, DeleteWordLeft, WordLeft, WordRight]
);

pub fn bind_keys(cx: &mut App) {
    let c = Some("TextArea");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("alt-left", WordLeft, c),
        KeyBinding::new("alt-right", WordRight, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-left", Home, c),
        KeyBinding::new("cmd-right", End, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("shift-enter", ForceNewline, c),
        KeyBinding::new("alt-enter", ForceNewline, c),
        KeyBinding::new("cmd-enter", Submit, c),
        KeyBinding::new("escape", Cancel, c),
    ]);
}

pub enum TextAreaEvent {
    Changed,
    Submit,
    Cancel,
    /// An image was pasted (chat composers): saved to this PNG/JPEG file.
    PastedImage(std::path::PathBuf),
}

pub struct TextArea {
    focus_handle: FocusHandle,
    pub content: String,
    placeholder: SharedString,
    /// Byte offsets; `anchor` may be after `head`.
    anchor: usize,
    head: usize,
    marked: Option<Range<usize>>,
    goal_x: Option<Pixels>,
    min_rows: usize,
    max_rows: usize,
    layout: Option<AreaLayout>,
    selecting: bool,
    scroll_y: Pixels,
    /// Chat composer: Enter sends, Shift/Option+Enter inserts a newline, pasted images are
    /// reported instead of ignored.
    pub submit_on_enter: bool,
}

#[derive(Clone)]
struct AreaLayout {
    origin: Point<Pixels>,
    line_height: Pixels,
    /// (paragraph byte start, wrapped line, y offset)
    paras: Vec<(usize, WrappedLine, Pixels)>,
}

impl EventEmitter<TextAreaEvent> for TextArea {}

impl TextArea {
    pub fn new(placeholder: impl Into<SharedString>, min_rows: usize, max_rows: usize, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            anchor: 0,
            head: 0,
            marked: None,
            goal_x: None,
            min_rows,
            max_rows,
            layout: None,
            selecting: false,
            scroll_y: px(0.),
            submit_on_enter: false,
        }
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content = text.to_string();
        self.anchor = self.content.len();
        self.head = self.content.len();
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    /// Insert at the caret (replacing the selection).
    pub fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        self.replace(self.sel(), text, cx);
    }

    fn sel(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    fn prev_boundary(&self, i: usize) -> usize {
        let mut i = i.saturating_sub(1);
        while i > 0 && !self.content.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next_boundary(&self, i: usize) -> usize {
        let mut i = (i + 1).min(self.content.len());
        while i < self.content.len() && !self.content.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    fn word_start(&self, from: usize) -> usize {
        let b = self.content.as_bytes();
        let mut i = from;
        while i > 0 && !b[i - 1].is_ascii_alphanumeric() {
            i -= 1;
        }
        while i > 0 && b[i - 1].is_ascii_alphanumeric() {
            i -= 1;
        }
        while !self.content.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn word_end(&self, from: usize) -> usize {
        let b = self.content.as_bytes();
        let mut i = from;
        while i < b.len() && !b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        while i < b.len() && !self.content.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    fn move_to(&mut self, pos: usize, extend: bool, cx: &mut Context<Self>) {
        self.head = pos;
        if !extend {
            self.anchor = pos;
        }
        self.goal_x = None;
        cx.notify();
    }

    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.content.replace_range(range.clone(), text);
        let pos = range.start + text.len();
        self.anchor = pos;
        self.head = pos;
        self.marked = None;
        self.goal_x = None;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    /// Caret position (relative to the text origin) for a byte offset.
    fn point_for(&self, offset: usize) -> Option<Point<Pixels>> {
        let l = self.layout.as_ref()?;
        let (start, line, y) = l.paras.iter().rev().find(|(s, _, _)| *s <= offset)?;
        let local = (offset - start).min(line.len());
        let p = line.position_for_index(local, l.line_height)?;
        Some(point(p.x, p.y + *y))
    }

    fn offset_for(&self, pos: Point<Pixels>) -> usize {
        let Some(l) = self.layout.as_ref() else { return self.content.len() };
        let rel = point(pos.x - l.origin.x, pos.y - l.origin.y + self.scroll_y);
        let Some((start, line, y)) = l
            .paras
            .iter()
            .find(|(_, line, y)| rel.y < *y + line.size(l.line_height).height)
            .or(l.paras.last())
        else {
            return 0;
        };
        let local = match line.closest_index_for_position(point(rel.x, (rel.y - *y).max(px(0.))), l.line_height) {
            Ok(i) | Err(i) => i,
        };
        start + local.min(line.len())
    }

    fn vertical(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let Some(l) = self.layout.clone() else { return };
        let Some(p) = self.point_for(self.head) else { return };
        let x = *self.goal_x.get_or_insert(p.x);
        let y = if down { p.y + l.line_height } else { p.y - l.line_height };
        let target = if y < px(0.) {
            0
        } else {
            self.offset_for(point(l.origin.x + x, l.origin.y + y - self.scroll_y + l.line_height * 0.5))
        };
        let goal = self.goal_x;
        self.head = target;
        if !extend {
            self.anchor = target;
        }
        self.goal_x = goal;
        cx.notify();
    }
}

impl EntityInputHandler for TextArea {
    fn text_for_range(&mut self, r: Range<usize>, actual: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let r = utf16_to_utf8(&self.content, &r);
        actual.replace(utf8_to_utf16(&self.content, &r));
        Some(self.content[r].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: utf8_to_utf16(&self.content, &self.sel()), reversed: self.head < self.anchor })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| utf8_to_utf16(&self.content, r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(&mut self, r: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let range = r.map(|r| utf16_to_utf8(&self.content, &r)).or(self.marked.clone()).unwrap_or(self.sel());
        self.replace(range, text, cx);
    }

    fn replace_and_mark_text_in_range(&mut self, r: Option<Range<usize>>, text: &str, _: Option<Range<usize>>, _: &mut Window, cx: &mut Context<Self>) {
        let range = r.map(|r| utf16_to_utf8(&self.content, &r)).or(self.marked.clone()).unwrap_or(self.sel());
        self.content.replace_range(range.clone(), text);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        self.anchor = range.start + text.len();
        self.head = self.anchor;
        cx.notify();
    }

    fn bounds_for_range(&mut self, _: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let l = self.layout.as_ref()?;
        let p = self.point_for(self.head)?;
        Some(Bounds::new(point(l.origin.x + p.x, l.origin.y + p.y - self.scroll_y), size(px(2.), l.line_height)))
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

fn utf16_to_utf8(s: &str, r: &Range<usize>) -> Range<usize> {
    let conv = |target: usize| {
        let mut u16 = 0;
        for (i, c) in s.char_indices() {
            if u16 >= target {
                return i;
            }
            u16 += c.len_utf16();
        }
        s.len()
    };
    conv(r.start)..conv(r.end)
}

fn utf8_to_utf16(s: &str, r: &Range<usize>) -> Range<usize> {
    let conv = |b: usize| s[..b.min(s.len())].encode_utf16().count();
    conv(r.start)..conv(r.end)
}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextArea {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("TextArea")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .w_full()
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                let r = this.sel();
                let r = if r.is_empty() { this.prev_boundary(r.start)..r.start } else { r };
                this.replace(r, "", cx);
            }))
            .on_action(cx.listener(|this, _: &Delete, _, cx| {
                let r = this.sel();
                let r = if r.is_empty() { r.start..this.next_boundary(r.start) } else { r };
                this.replace(r, "", cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteWordLeft, _, cx| {
                let r = this.sel();
                let r = if r.is_empty() { this.word_start(r.start)..r.start } else { r };
                this.replace(r, "", cx);
            }))
            .on_action(cx.listener(|this, _: &Left, _, cx| {
                let r = this.sel();
                let p = if r.is_empty() { this.prev_boundary(this.head) } else { r.start };
                this.move_to(p, false, cx)
            }))
            .on_action(cx.listener(|this, _: &Right, _, cx| {
                let r = this.sel();
                let p = if r.is_empty() { this.next_boundary(this.head) } else { r.end };
                this.move_to(p, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.move_to(this.prev_boundary(this.head), true, cx)))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.move_to(this.next_boundary(this.head), true, cx)))
            .on_action(cx.listener(|this, _: &WordLeft, _, cx| this.move_to(this.word_start(this.head), false, cx)))
            .on_action(cx.listener(|this, _: &WordRight, _, cx| this.move_to(this.word_end(this.head), false, cx)))
            .on_action(cx.listener(|this, _: &Up, _, cx| this.vertical(false, false, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.vertical(true, false, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.vertical(false, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.vertical(true, true, cx)))
            .on_action(cx.listener(|this, _: &Home, _, cx| {
                let start = this.content[..this.head].rfind('\n').map(|i| i + 1).unwrap_or(0);
                this.move_to(start, false, cx)
            }))
            .on_action(cx.listener(|this, _: &End, _, cx| {
                let end = this.content[this.head..].find('\n').map(|i| this.head + i).unwrap_or(this.content.len());
                this.move_to(end, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.anchor = 0;
                this.head = this.content.len();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| {
                let r = this.sel();
                if !r.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(this.content[r].to_string()));
                }
            }))
            .on_action(cx.listener(|this, _: &Cut, _, cx| {
                let r = this.sel();
                if !r.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(this.content[r.clone()].to_string()));
                    this.replace(r, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                if this.submit_on_enter
                    && let Some(item) = cx.read_from_clipboard()
                    && item.text().is_none()
                    && let Some(img) = item.entries().iter().find_map(|e| match e {
                        gpui::ClipboardEntry::Image(i) => Some(i.clone()),
                        _ => None,
                    })
                {
                    let ext = match img.format {
                        gpui::ImageFormat::Jpeg => "jpg",
                        _ => "png",
                    };
                    let path = std::env::temp_dir().join(format!("pegado-{}.{ext}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)));
                    if std::fs::write(&path, &img.bytes).is_ok() {
                        cx.emit(TextAreaEvent::PastedImage(path));
                    }
                    return;
                }
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    let r = this.sel();
                    this.replace(r, &t.replace("\r\n", "\n"), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Newline, _, cx| {
                if this.submit_on_enter {
                    cx.emit(TextAreaEvent::Submit);
                    return;
                }
                let r = this.sel();
                this.replace(r, "\n", cx);
            }))
            .on_action(cx.listener(|this, _: &ForceNewline, _, cx| {
                let r = this.sel();
                this.replace(r, "\n", cx);
            }))
            .on_action(cx.listener(|_, _: &Submit, _, cx| cx.emit(TextAreaEvent::Submit)))
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(TextAreaEvent::Cancel)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle);
                    let o = this.offset_for(e.position);
                    if e.click_count >= 2 {
                        this.anchor = this.word_start(o);
                        this.head = this.word_end(o);
                    } else {
                        this.head = o;
                        if !e.modifiers.shift {
                            this.anchor = o;
                        }
                    }
                    this.selecting = true;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.selecting && e.pressed_button == Some(MouseButton::Left) {
                    this.head = this.offset_for(e.position);
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.selecting = false))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.selecting = false))
            .text_size(px(12.5))
            .line_height(px(18.))
            .child(AreaElement { area: cx.entity() })
    }
}

struct AreaElement {
    area: Entity<TextArea>,
}

impl IntoElement for AreaElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for AreaElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let area = self.area.read(cx);
        let lh = window.line_height();
        let rows = area
            .layout
            .as_ref()
            .map(|l| l.paras.iter().map(|(_, w, _)| w.size(lh).height / lh).sum::<f32>().round() as usize)
            .unwrap_or(1)
            .clamp(area.min_rows, area.max_rows);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (lh * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let lh = window.line_height();
        let ts = window.text_system().clone();
        let color = style.color;
        let font = style.font();
        self.area.update(cx, |area, _| {
            let mut paras = Vec::new();
            let mut y = px(0.);
            let mut start = 0;
            for para in area.content.split('\n') {
                let text: SharedString = para.to_string().into();
                let run = TextRun { len: text.len(), font: font.clone(), color, background_color: None, underline: None, strikethrough: None };
                let lines = ts.shape_text(text, font_size, &[run], Some(bounds.size.width), None).unwrap_or_default();
                if let Some(line) = lines.into_iter().next() {
                    let h = line.size(lh).height;
                    paras.push((start, line, y));
                    y += h;
                }
                start += para.len() + 1;
            }
            area.layout = Some(AreaLayout { origin: bounds.origin, line_height: lh, paras });
            // Keep the caret visible.
            if let Some(p) = area.point_for(area.head) {
                if p.y < area.scroll_y {
                    area.scroll_y = p.y;
                } else if p.y + lh > area.scroll_y + bounds.size.height {
                    area.scroll_y = p.y + lh - bounds.size.height;
                }
            }
            if y <= bounds.size.height {
                area.scroll_y = px(0.);
            }
        });
    }

    fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), _: &mut (), window: &mut Window, cx: &mut App) {
        let focus = self.area.read(cx).focus_handle.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.area.clone()), cx);
        let area = self.area.read(cx);
        let Some(layout) = area.layout.clone() else { return };
        let lh = layout.line_height;
        let scroll = area.scroll_y;
        let sel = area.sel();
        let head = area.head;
        let empty = area.content.is_empty();
        let placeholder = area.placeholder.clone();
        let sel_rects: Vec<Bounds<Pixels>> = if sel.is_empty() {
            Vec::new()
        } else {
            // One rect per visual row between start and end.
            let (a, b) = (area.point_for(sel.start), area.point_for(sel.end));
            match (a, b) {
                (Some(a), Some(b)) if a.y == b.y => vec![Bounds::new(point(a.x, a.y), size(b.x - a.x, lh))],
                (Some(a), Some(b)) => {
                    let mut v = vec![Bounds::new(point(a.x, a.y), size(bounds.size.width - a.x, lh))];
                    let mut y = a.y + lh;
                    while y < b.y {
                        v.push(Bounds::new(point(px(0.), y), size(bounds.size.width, lh)));
                        y += lh;
                    }
                    v.push(Bounds::new(point(px(0.), b.y), size(b.x, lh)));
                    v
                }
                _ => Vec::new(),
            }
        };
        let caret = area.point_for(head);
        let focused = focus.is_focused(window);
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            let origin = point(bounds.origin.x, bounds.origin.y - scroll);
            for r in sel_rects {
                window.paint_quad(fill(Bounds::new(point(origin.x + r.origin.x, origin.y + r.origin.y), r.size), theme::rgb(theme::SELECTION)));
            }
            if empty {
                let style = window.text_style();
                // One shaped line per `\n` (shape_line doesn't break lines).
                for (i, text) in placeholder.split('\n').enumerate() {
                    let run = TextRun { len: text.len(), font: style.font(), color: theme::rgb(theme::TEXT_FAINT), background_color: None, underline: None, strikethrough: None };
                    let line = window.text_system().shape_line(SharedString::from(text.to_string()), style.font_size.to_pixels(window.rem_size()), &[run], None);
                    let _ = line.paint(point(origin.x, origin.y + lh * i as f32), lh, window, cx);
                }
            }
            for (_, line, y) in &layout.paras {
                let _ = line.paint(point(origin.x, origin.y + *y), lh, TextAlign::Left, Some(bounds), window, cx);
            }
            if focused {
                if let Some(c) = caret {
                    window.paint_quad(fill(Bounds::new(point(origin.x + c.x, origin.y + c.y), size(px(1.5), lh)), theme::rgb(theme::ACCENT)));
                }
            }
        });
    }
}
