//! GPUI view for a terminal session: grid rendering, keyboard/IME input, selection, scrolling.

use std::ops::Range;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use gpui::{
    ClickEvent,
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, Rgba, ScrollDelta,
    ScrollWheelEvent, ShapedLine, SharedString, Style, Subscription, Task, TextRun,
    UTF16Selection, UnderlineStyle, StrikethroughStyle, Window, actions, div, fill, point,
    prelude::*, px, relative, size,
};
use std::collections::HashMap;
use terminal_engine::{
    CellFlags, CursorShape, KeyInput, KeyModes, Mark, Palette, Rgb, SessionEvent, parse_mark_uri,
    Snapshot, TermDims, TerminalSession, encode_key, encode_paste,
};

use crate::theme;

actions!(terminal, [Copy, Paste, Clear, ScrollPageUp, ScrollPageDown, ScrollToTop, ScrollToBottom]);

pub enum TerminalViewEvent {
    TitleChanged,
    Exited(Option<i32>),
    /// The user looked at this terminal: its attention badge was cleared.
    AttentionCleared,
    /// ⌘-click on a file path in the output.
    OpenPath { path: PathBuf, line: Option<usize>, column: Option<usize> },
}

/// What Claude Code's statusLine reports for the session in this terminal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeStatus {
    pub model: String,
    pub model_id: String,
    pub effort: Option<String>,
    pub ctx_pct: f32,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub cost_usd: f64,
}

impl ClaudeStatus {
    pub fn from_json(v: &serde_json::Value) -> Self {
        let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        ClaudeStatus {
            model: s("/model/display_name"),
            model_id: s("/model/id"),
            effort: v.pointer("/effort/level").and_then(|x| x.as_str()).map(str::to_string),
            ctx_pct: v.pointer("/context_window/used_percentage").and_then(|x| x.as_f64()).unwrap_or(0.) as f32,
            session_id: s("/session_id"),
            cwd: v.pointer("/workspace/current_dir").and_then(|x| x.as_str()).or_else(|| v.get("cwd").and_then(|x| x.as_str())).map(PathBuf::from),
            cost_usd: v.pointer("/cost/total_cost_usd").and_then(|x| x.as_f64()).unwrap_or(0.),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum ClaudeMenu {
    Model,
    Effort,
    History(Vec<crate::claude_sessions::SessionInfo>),
}

const MODELS: &[(&str, &str)] = &[("Opus 5.5", "claude-opus-5-5"), ("Sonnet 5.5", "claude-sonnet-5-5"), ("Haiku 5.5", "claude-haiku-5-5"), ("Fable 5.2", "claude-fable-5-2")];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Rows of Claude Code's TUI that juan-terminal shows natively (welcome banner, mode and
/// effort hints): hidden while Claude is in the foreground.
pub fn is_claude_chrome_row(text: &str) -> bool {
    let t = text.trim();
    t.contains("▐▛███▛█") || t.contains("▝▜██████▀") || t.starts_with("▝▝   ▝▝") || (t.contains("auto mode on") && t.contains("shift+tab")) || t.ends_with("· /effort")
}

/// Claude (via hooks) says this terminal needs a look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    /// Claude finished its turn.
    Done,
    /// Claude is waiting for a permission/answer.
    NeedsInput,
}

pub struct TerminalView {
    session: TerminalSession,
    focus_handle: FocusHandle,
    snapshot: Snapshot,
    title: Option<String>,
    pub initial_cwd: Option<PathBuf>,
    pub repository: Option<String>,
    /// Fixed label (e.g. "Claude") shown when the app hasn't set a title.
    pub label: Option<String>,
    exit_code: Option<Option<i32>>,
    marked_text: Option<String>,
    font: Font,
    font_size: Pixels,
    /// Last painted grid origin and cell size, for mouse → cell mapping.
    layout: Option<GridLayout>,
    scroll_remainder: f32,
    selecting: bool,
    /// Stable id passed to the shell as `JT_TERMINAL_ID` (hooks report it back).
    pub jt_id: String,
    pub attention: Option<Attention>,
    pub claude_status: Option<ClaudeStatus>,
    claude_menu: Option<ClaudeMenu>,
    /// Claude Code is the foreground process (updated each render).
    claude_fg: bool,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy)]
struct GridLayout {
    origin: Point<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl TerminalView {
    pub fn new(
        jt_id: String,
        session: TerminalSession,
        mut rx: futures::channel::mpsc::UnboundedReceiver<terminal_engine::RawEvent>,
        initial_cwd: Option<PathBuf>,
        repository: Option<String>,
        font_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                let alive = this.update(cx, |view, cx| view.handle_session_event(event, cx));
                if alive.is_err() {
                    break;
                }
            }
        });

        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |view, _, cx| {
                view.session.set_focused(true);
                if view.attention.take().is_some() {
                    cx.emit(TerminalViewEvent::AttentionCleared);
                }
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |view, _, cx| {
                view.session.set_focused(false);
                cx.notify();
            }),
        ];

        Self {
            session,
            focus_handle,
            snapshot: Snapshot::default(),
            title: None,
            initial_cwd,
            repository,
            label: None,
            exit_code: None,
            marked_text: None,
            font: gpui::font(theme::MONO_FONT),
            font_size: px(font_size),
            layout: None,
            scroll_remainder: 0.,
            selecting: false,
            jt_id,
            attention: None,
            claude_status: None,
            claude_menu: None,
            claude_fg: false,
            _events: events,
            _subscriptions: subscriptions,
        }
    }

    pub fn title(&self) -> SharedString {
        if let Some(t) = self.title.as_ref().filter(|t| !t.is_empty()) {
            return t.clone().into();
        }
        if let Some(label) = &self.label {
            return label.clone().into();
        }
        if let Some(repo) = &self.repository {
            return repo.clone().into();
        }
        self.initial_cwd
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned().into())
            .unwrap_or_else(|| "shell".into())
    }

    /// Type text into the terminal as if the user did (used to auto-start commands).
    pub fn is_claude_running(&self) -> bool {
        self.session.foreground_exe().is_some_and(|p| p.to_string_lossy().contains("claude"))
    }

    pub fn set_claude_status(&mut self, st: ClaudeStatus, cx: &mut Context<Self>) {
        if self.claude_status.as_ref() != Some(&st) {
            self.claude_status = Some(st);
            cx.notify();
        }
    }

    fn claude_command(&mut self, cmd: String, window: &mut Window, cx: &mut Context<Self>) {
        self.claude_menu = None;
        self.send_input(&format!("{cmd}\r"));
        window.focus(&self.focus_handle);
        cx.notify();
    }

    fn toggle_menu(&mut self, menu: ClaudeMenu, cx: &mut Context<Self>) {
        let same = std::mem::discriminant(&menu) == self.claude_menu.as_ref().map(std::mem::discriminant).unwrap_or(std::mem::discriminant(&ClaudeMenu::Model)) && self.claude_menu.is_some();
        self.claude_menu = if same { None } else { Some(menu) };
        cx.notify();
    }

    fn open_history(&mut self, cx: &mut Context<Self>) {
        if matches!(self.claude_menu, Some(ClaudeMenu::History(_))) {
            self.claude_menu = None;
            cx.notify();
            return;
        }
        let cwd = self.claude_status.as_ref().and_then(|s| s.cwd.clone()).or_else(|| self.current_cwd());
        let Some(cwd) = cwd else { return };
        cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async move { crate::claude_sessions::list_sessions(&cwd, 40) }).await;
            let _ = this.update(cx, |this, cx| {
                this.claude_menu = Some(ClaudeMenu::History(list));
                cx.notify();
            });
        })
        .detach();
    }

    fn render_claude_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let st = self.claude_status.clone().unwrap_or_default();
        let pct = st.ctx_pct.clamp(0., 100.);
        let steps = ((pct / 100. * 12.).round() as usize).min(12);
        let ring_color = if pct >= 85. { 0xe06c75 } else if pct >= 60. { 0xe5c07b } else { 0x7aa2f7 };
        let chip = |id: &'static str, label: String, active: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(4.))
                .px(px(7.))
                .h(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(11.5))
                .text_color(theme::rgb(theme::TEXT_MUTED))
                .when(active, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
                .hover(|s| s.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT)))
                .child(label)
                .child(gpui::svg().path("icons/chevron-down.svg").size(px(8.)).text_color(theme::rgb(theme::TEXT_FAINT)))
        };
        let menu = self.claude_menu.clone();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(28.))
            .px(px(10.))
            .border_t_1()
            .border_color(theme::rgb(theme::BORDER))
            .bg(theme::rgb(0x17181c))
            .child(
                div()
                    .id("ctx-ring")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip("Context window used")).into())
                    .child(
                        div()
                            .relative()
                            .size(px(14.))
                            .child(gpui::svg().absolute().path("icons/ring-track.svg").size(px(14.)).text_color(theme::rgb(0x2e3138)))
                            .when(steps > 0, |d| d.child(gpui::svg().absolute().path(SharedString::from(format!("icons/ring-{steps}.svg"))).size(px(14.)).text_color(theme::rgb(ring_color)))),
                    )
                    .child(div().text_size(px(11.5)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{:.0}%", pct))),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(theme::rgb(theme::BORDER)))
            .child(chip("claude-model", if st.model.is_empty() { "model".into() } else { st.model.clone() }, matches!(menu, Some(ClaudeMenu::Model))).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.toggle_menu(ClaudeMenu::Model, cx)
            })))
            .child(chip("claude-effort", st.effort.clone().unwrap_or_else(|| "effort".into()), matches!(menu, Some(ClaudeMenu::Effort))).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.toggle_menu(ClaudeMenu::Effort, cx)
            })))
            .child(div().flex_1())
            .when(st.cost_usd > 0., |d| d.child(div().text_size(px(11.)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("${:.2}", st.cost_usd))))
            .into_any_element()
    }

    fn render_claude_popover(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let menu = self.claude_menu.clone()?;
        let st = self.claude_status.clone().unwrap_or_default();
        let panel = || {
            div()
                .absolute()
                .flex()
                .flex_col()
                .p(px(4.))
                .rounded(px(8.))
                .bg(theme::rgb(0x202227))
                .border_1()
                .border_color(theme::rgb(0x33363d))
                .shadow_lg()
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };
        let item = |id: SharedString, label: String, hint: String, active: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .min_h(px(28.))
                .rounded(px(5.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                .child(div().w(px(10.)).text_size(px(11.)).text_color(theme::rgb(theme::ACCENT)).child(if active { "●" } else { "" }))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(label))
                .child(div().flex_none().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(hint))
        };
        Some(match menu {
            ClaudeMenu::Model => {
                let mut p = panel().bottom(px(32.)).left(px(40.)).w(px(220.));
                for (label, id) in MODELS {
                    let cmd = format!("/model {id}");
                    let active = st.model_id.starts_with(id);
                    p = p.child(item(SharedString::from(format!("m-{id}")), label.to_string(), String::new(), active).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.claude_command(cmd.clone(), w, cx))));
                }
                p.into_any_element()
            }
            ClaudeMenu::Effort => {
                let mut p = panel().bottom(px(32.)).left(px(120.)).w(px(160.));
                for lvl in EFFORTS {
                    let cmd = format!("/effort {lvl}");
                    let active = st.effort.as_deref() == Some(*lvl);
                    p = p.child(item(SharedString::from(format!("e-{lvl}")), lvl.to_string(), String::new(), active).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.claude_command(cmd.clone(), w, cx))));
                }
                p.into_any_element()
            }
            ClaudeMenu::History(list) => {
                let mut p = panel().id("claude-history").top(px(34.)).right(px(10.)).w(px(420.)).max_h(px(380.)).overflow_y_scroll();
                p = p.child(div().px(px(8.)).pt(px(4.)).pb(px(2.)).text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child("SESSIONS IN THIS FOLDER"));
                if list.is_empty() {
                    p = p.child(div().px(px(8.)).py(px(6.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("No saved sessions here yet."));
                }
                for sess in list {
                    let cmd = format!("/resume {}", sess.id);
                    let active = sess.id == st.session_id;
                    let hint = format!("{} · {} msg", crate::claude_sessions::ago(sess.modified), sess.user_messages);
                    p = p.child(item(SharedString::from(format!("s-{}", sess.id)), sess.title.clone(), hint, active).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.claude_command(cmd.clone(), w, cx))));
                }
                p.into_any_element()
            }
        })
    }

    pub fn send_input(&self, text: &str) {
        self.session.write(text.as_bytes().to_vec());
    }

    pub fn has_exited(&self) -> bool {
        self.exit_code.is_some()
    }

    pub fn current_cwd(&self) -> Option<PathBuf> {
        self.session.foreground_cwd().or_else(|| self.initial_cwd.clone())
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.font_size = px(size);
        cx.notify();
    }

    fn handle_session_event(&mut self, event: terminal_engine::RawEvent, cx: &mut Context<Self>) {
        let Some(event) = self.session.process_event(event) else { return };
        match event {
            SessionEvent::Redraw => cx.notify(),
            SessionEvent::Title(t) => {
                self.title = Some(t);
                cx.emit(TerminalViewEvent::TitleChanged);
            }
            SessionEvent::ResetTitle => {
                self.title = None;
                cx.emit(TerminalViewEvent::TitleChanged);
            }
            SessionEvent::Bell => {}
            SessionEvent::ClipboardStore(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            SessionEvent::Exited(code) => {
                self.exit_code = Some(code);
                cx.emit(TerminalViewEvent::Exited(code));
                cx.notify();
            }
        }
    }

    fn write_user_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.snapshot.display_offset > 0 {
            self.session.scroll_to_bottom();
        }
        self.session.clear_selection();
        self.session.write(bytes);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.is_some() {
            return; // IME composition in progress: let the input handler process it.
        }
        let ks = &event.keystroke;
        let input = KeyInput {
            key: &ks.key,
            ctrl: ks.modifiers.control,
            alt: ks.modifiers.alt,
            shift: ks.modifiers.shift,
            cmd: ks.modifiers.platform,
        };
        let modes = KeyModes { app_cursor: self.snapshot.mode.app_cursor };
        if let Some(bytes) = encode_key(&input, modes) {
            self.write_user_input(bytes.into_owned(), cx);
            cx.stop_propagation();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.session.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let bytes = encode_paste(&text, self.snapshot.mode.bracketed_paste);
            self.write_user_input(bytes, cx);
        }
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        // Same as Terminal.app's Cmd-K: clear screen and scrollback, then ask the shell to redraw
        // its prompt (form feed).
        self.session.write(&b"\x0c"[..]);
        self.session.clear_history();
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.session.scroll_page(true);
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.session.scroll_page(false);
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.session.scroll_to_top();
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.session.scroll_to_bottom();
        cx.notify();
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize, bool)> {
        let layout = self.layout?;
        let x = (position.x - layout.origin.x).max(px(0.)) / layout.cell_width;
        let y = (position.y - layout.origin.y).max(px(0.)) / layout.cell_height;
        let col = x.floor() as usize;
        let row = y.floor() as usize;
        let right_half = x.fract() > 0.5;
        Some((
            row.min(self.snapshot.rows.saturating_sub(1)),
            col.min(self.snapshot.cols.saturating_sub(1)),
            right_half,
        ))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let Some((row, col, right)) = self.cell_at(event.position) else { return };
        if event.modifiers.platform {
            let rows: Vec<String> = (0..self.snapshot.rows).map(|r| self.snapshot.row_text(r)).collect();
            let wrapped: Vec<bool> = (0..self.snapshot.rows)
                .map(|r| self.snapshot.row(r).last().is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE)))
                .collect();
            let cwd = self.current_cwd();
            if let Some((path, line, column)) = path_at(&rows, &wrapped, row, col, cwd.as_deref()) {
                cx.emit(TerminalViewEvent::OpenPath { path, line, column });
                return;
            }
        }
        if event.modifiers.shift {
            self.session.update_selection(row, col, right);
        } else {
            self.session.start_selection(row, col, right, event.click_count);
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some((row, col, right)) = self.cell_at(event.position) {
            self.session.update_selection(row, col, right);
            cx.notify();
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            // A plain click without drag leaves an empty selection; drop it.
            if self.session.selection_text().is_none() {
                self.session.clear_selection();
            }
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.layout else { return };
        let lines = match event.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y / layout.cell_height,
        } + self.scroll_remainder;
        let whole = lines.trunc() as i32;
        self.scroll_remainder = lines.fract();
        if whole == 0 {
            return;
        }
        let mode = self.snapshot.mode;
        if mode.alt_screen && mode.alternate_scroll {
            // Full-screen apps (less, vim, ...): translate wheel into arrow keys.
            let seq: &[u8] = match (whole > 0, mode.app_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let bytes = seq.repeat(whole.unsigned_abs() as usize);
            self.session.write(bytes);
        } else {
            self.session.scroll(whole);
        }
        cx.notify();
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let exited = self.exit_code;
        let claude = self.exit_code.is_none() && self.is_claude_running();
        if claude != self.claude_fg {
            self.claude_fg = claude;
            if !claude {
                self.claude_menu = None;
                self.claude_status = None;
            }
        }
        let bar = claude.then(|| self.render_claude_bar(cx));
        let popover = if claude { self.render_claude_popover(cx) } else { None };
        let history_btn = claude.then(|| {
            div()
                .absolute()
                .top(px(6.))
                .right(px(10.))
                .child(
                    crate::git_panel::icon_action("claude-history-btn", "icons/history.svg", "Sessions (resume)")
                        .bg(theme::rgb(0x1c1e23))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.open_history(cx)
                        })),
                )
        });
        div()
            .id("terminal")
            .relative()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::rgb(theme::TERM_BG))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::scroll_to_top))
            .on_action(cx.listener(Self::scroll_to_bottom))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(10.))
                    .pt(px(6.))
                    .child(TerminalElement { view: cx.entity(), focused }),
            )
            .children(bar)
            .children(history_btn)
            .children(popover)
            .when_some(exited, |el, code| {
                let msg = match code {
                    Some(c) => format!("Process exited with code {c}. ⌘W to close."),
                    None => "Process exited. ⌘W to close.".to_string(),
                };
                el.child(
                    div()
                        .px(px(12.))
                        .py(px(6.))
                        .text_size(px(12.))
                        .text_color(theme::rgb(theme::TEXT_MUTED))
                        .child(msg),
                )
            })
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
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
        // The macOS text system reports Return as "\n"; terminals expect CR.
        let text = text.replace('\n', "\r");
        self.write_user_input(text.into_bytes(), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!text.is_empty()).then(|| text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout?;
        let cursor = self.snapshot.cursor?;
        Some(Bounds::new(
            point(
                layout.origin.x + layout.cell_width * cursor.col as f32,
                layout.origin.y + layout.cell_height * cursor.row as f32,
            ),
            size(layout.cell_width, layout.cell_height),
        ))
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Grid element
// ---------------------------------------------------------------------------------------------

struct TerminalElement {
    view: Entity<TerminalView>,
    focused: bool,
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

struct TextFragment {
    origin: Point<Pixels>,
    line: ShapedLine,
}

pub struct PrepaintState {
    /// Prompt chips: rounded boxes with a border.
    chips: Vec<(Bounds<Pixels>, Hsla, Hsla)>,
    icons: Vec<(Bounds<Pixels>, SharedString, Hsla)>,
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    text: Vec<TextFragment>,
    cursor: Option<(Bounds<Pixels>, Hsla, bool)>,
    marked: Option<TextFragment>,
    cursor_text: Option<TextFragment>,
    line_height: Pixels,
}

fn hsla(c: Rgb) -> Hsla {
    Rgba { r: c.r as f32 / 255., g: c.g as f32 / 255., b: c.b as f32 / 255., a: 1. }.into()
}

const PATH_STOP: &[char] = &['"', '\'', '`', '(', ')', '<', '>', '[', ']', '{', '}', '|', ',', ';'];

/// Token (non-space, non-quote run) around char index `col` of `line`, with its char range.
fn token_at(line: &[char], col: usize) -> Option<(usize, usize)> {
    let ok = |c: char| !c.is_whitespace() && !PATH_STOP.contains(&c);
    if col >= line.len() || !ok(line[col]) {
        return None;
    }
    let mut a = col;
    while a > 0 && ok(line[a - 1]) {
        a -= 1;
    }
    let mut b = col + 1;
    while b < line.len() && ok(line[b]) {
        b += 1;
    }
    Some((a, b))
}

/// Split "path:12:3" / "path:12" into parts (only when the suffixes are numbers).
fn split_line_col(t: &str) -> (&str, Option<usize>, Option<usize>) {
    let mut parts = t.rsplitn(3, ':').collect::<Vec<_>>();
    parts.reverse();
    match parts.as_slice() {
        [p, l, c] if l.parse::<usize>().is_ok() && c.parse::<usize>().is_ok() => (p, l.parse().ok(), c.parse().ok()),
        [p, l] if l.parse::<usize>().is_ok() => (p, l.parse().ok(), None),
        [p, rest @ ..] if rest.len() == 1 && rest[0].parse::<usize>().is_ok() => (p, rest[0].parse().ok(), None),
        _ => {
            // "a:b:12" → path may itself contain ':'
            if let Some((p, l)) = t.rsplit_once(':') {
                if let Ok(n) = l.parse::<usize>() {
                    return (p, Some(n), None);
                }
            }
            (t, None, None)
        }
    }
}

fn resolve_path(t: &str, cwd: Option<&Path>) -> Option<(PathBuf, Option<usize>, Option<usize>)> {
    let t = t.strip_prefix("file://").unwrap_or(t);
    // Trailing sentence punctuation is not part of the path ("…/a.png.").
    for cand in [t, t.trim_end_matches(['.', ':', '!', '?'])] {
        let (p, line, col) = split_line_col(cand);
        if p.is_empty() {
            continue;
        }
        let path = if let Some(rest) = p.strip_prefix("~/") {
            PathBuf::from(std::env::var_os("HOME")?).join(rest)
        } else if p.starts_with('/') {
            PathBuf::from(p)
        } else {
            cwd?.join(p)
        };
        if path.exists() {
            return Some((path, line, col));
        }
    }
    None
}

/// Find an existing file path under (row, col). Handles soft-wrapped rows (terminal wrap
/// flag) and hard-wrapped ones (TUIs like Claude Code break long paths onto the next line,
/// usually indented): pieces are joined and the longest existing path wins.
pub fn path_at(rows: &[String], wrapped: &[bool], row: usize, col: usize, cwd: Option<&Path>) -> Option<(PathBuf, Option<usize>, Option<usize>)> {
    let chars: Vec<Vec<char>> = rows.iter().map(|r| r.chars().collect()).collect();
    let (a, b) = token_at(chars.get(row)?, col)?;
    let tok = |r: usize, a: usize, b: usize| chars[r][a..b].iter().collect::<String>();
    let ends_row = |r: usize, b: usize| chars[r][b..].iter().all(|c| c.is_whitespace());
    let starts_row = |r: usize, a: usize| chars[r][..a].iter().all(|c| c.is_whitespace());
    // Walk up while this piece starts its row and the previous row ends with a token.
    let mut pieces = vec![tok(row, a, b)];
    let mut first_row = row;
    let mut first_a = a;
    while first_row > 0 && (starts_row(first_row, first_a) || wrapped[first_row - 1]) {
        let prev = &chars[first_row - 1];
        let end = prev.iter().rposition(|c| !c.is_whitespace()).map(|i| i + 1).unwrap_or(0);
        if end == 0 {
            break;
        }
        let Some((pa, pb)) = token_at(prev, end - 1) else { break };
        if pb != end {
            break;
        }
        pieces.insert(0, tok(first_row - 1, pa, pb));
        first_row -= 1;
        first_a = pa;
        if pieces.len() > 6 {
            break;
        }
    }
    let center = pieces.len() - 1;
    let mut last_row = row;
    let mut last_b = b;
    while last_row + 1 < chars.len() && ends_row(last_row, last_b) && pieces.len() < 12 {
        let next = &chars[last_row + 1];
        let Some(start) = next.iter().position(|c| !c.is_whitespace()) else { break };
        let Some((na, nb)) = token_at(next, start) else { break };
        pieces.push(tok(last_row + 1, na, nb));
        last_row += 1;
        last_b = nb;
    }
    // Try every contiguous run that includes the clicked piece, longest first.
    let mut best: Option<(usize, (PathBuf, Option<usize>, Option<usize>))> = None;
    for i in 0..=center {
        for j in center..pieces.len() {
            let joined: String = pieces[i..=j].concat();
            if let Some(found) = resolve_path(&joined, cwd) {
                if best.as_ref().is_none_or(|(len, _)| joined.len() > *len) {
                    best = Some((joined.len(), found));
                }
            }
        }
    }
    best.map(|(_, f)| f)
}

/// How a visible row is drawn, from the shell-integration markers.
#[derive(Clone, Debug, PartialEq)]
enum RowKind {
    Normal,
    /// The prompt being typed at: drawn as chips.
    LivePrompt(HashMap<String, String>),
    /// Prompt line of a finished command: "cwd (1.2s)" header.
    Header(HashMap<String, String>, Option<(i32, u64)>),
    /// Block end marker: an empty spacer row.
    End,
}

/// Classify visible rows into blocks. Returns per-row kind and whether the row belongs to a
/// failed block (non-zero exit), for the red tint.
fn analyze_blocks(rows: usize, marks: &[(usize, Mark, String)]) -> (Vec<RowKind>, Vec<bool>) {
    let mut kinds = vec![RowKind::Normal; rows];
    let mut failed = vec![false; rows];
    let end_info = |uri: &str| {
        let q = parse_mark_uri(uri)?.1;
        Some((q.get("code")?.parse::<i32>().ok()?, q.get("ms")?.parse::<u64>().ok()?))
    };
    let mut block_start = 0usize; // first row of the current block (0 if its prompt is above)
    let mut open_prompt: Option<(usize, HashMap<String, String>)> = None;
    for (row, mark, uri) in marks {
        let row = *row;
        if row >= rows {
            continue;
        }
        match mark {
            Mark::Prompt => {
                // A prompt without an end before the next one (empty Enter edge cases): header only.
                if let Some((p, q)) = open_prompt.take() {
                    kinds[p] = RowKind::Header(q, None);
                }
                let q = parse_mark_uri(uri).map(|x| x.1).unwrap_or_default();
                open_prompt = Some((row, q));
                block_start = row;
            }
            Mark::End => {
                let info = end_info(uri);
                if let Some((p, q)) = open_prompt.take() {
                    kinds[p] = RowKind::Header(q, info);
                }
                kinds[row] = RowKind::End;
                if info.is_some_and(|(code, _)| code != 0) {
                    for f in &mut failed[block_start..=row] {
                        *f = true;
                    }
                }
                block_start = row + 1;
            }
            _ => {}
        }
    }
    if let Some((p, q)) = open_prompt {
        kinds[p] = RowKind::LivePrompt(q);
    }
    (kinds, failed)
}

fn fmt_ms(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1000..=59_999 => format!("{:.2}s", ms as f64 / 1000.),
        60_000..=3_599_999 => format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60),
        _ => format!("{}h {:02}m", ms / 3_600_000, (ms / 60_000) % 60),
    }
}

/// Output lines that read as an error or a warning get tinted (only text the program left
/// in the default color, so real colors from tools are never overridden).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Error,
    Warning,
}

pub fn classify_line(line: &str) -> Option<Tone> {
    let t = line.trim_start();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // Markers that start the line: "error:", "error[E0308]:", "npm ERR!", "[WARN]", "fatal:"…
    let head = |words: &[&str]| {
        words.iter().any(|w| {
            lower.strip_prefix(w).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with([':', ' ', '!', '[', ']', '(']) || rest.starts_with(|c: char| c.is_ascii_digit())
            })
        })
    };
    let anywhere = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    // Logger levels near the start ("12:00:01 ERROR x", "[2024-…] WARN x").
    let level = |lvl: &str| t.char_indices().take_while(|(i, _)| *i < 40).any(|(i, _)| t[i..].starts_with(lvl) && t[i + lvl.len()..].starts_with([' ', ']', ':']) && (i == 0 || t[..i].ends_with([' ', '['])));
    if head(&["error", "fatal", "panic", "uncaught", "traceback", "exception", "npm err!", "[error]", "✗", "×", "✖", "fail", "failed"])
        || level("ERROR")
        || level("FATAL")
        || anywhere(&[": error:", ": error ts", " error ts", "command not found", "no such file or directory", "permission denied", "segmentation fault", "thread 'main' panicked", "err_", "eaddrinuse", "cannot find module"])
        || t.starts_with("zsh: ")
        || t.starts_with("bash: ")
    {
        return Some(Tone::Error);
    }
    if head(&["warning", "warn", "npm warn", "[warn]", "[warning]", "deprecated", "⚠"]) || level("WARN") || level("WARNING") || anywhere(&[": warning:", " deprecationwarning"]) {
        return Some(Tone::Warning);
    }
    None
}

#[derive(Clone, Copy, PartialEq)]
struct RunStyle {
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let focused = self.focused;
        self.view.update(cx, |view, _cx| {
            let text_system = window.text_system().clone();
            let font_size = view.font_size;
            let font_id = text_system.resolve_font(&view.font);
            let cell_width = text_system
                .advance(font_id, font_size, 'm')
                .map(|s| s.width)
                .unwrap_or(font_size * 0.6);
            let cell_height = (font_size * theme::TERM_LINE_HEIGHT).round();

            let cols = ((bounds.size.width / cell_width).floor() as u16).max(2);
            let rows = ((bounds.size.height / cell_height).floor() as u16).max(1);
            let dims = TermDims::new(cols, rows, f32::from(cell_width) as u16, f32::from(cell_height) as u16);
            view.session.resize(dims);
            view.session.snapshot(&mut view.snapshot);
            view.layout = Some(GridLayout { origin: bounds.origin, cell_width, cell_height });

            let snap = &view.snapshot;
            let palette = Palette::dark();
            let default_bg = palette.background;
            let selection_bg = palette.selection;
            let mut backgrounds: Vec<(Bounds<Pixels>, Hsla)> = Vec::new();
            let mut text = Vec::new();
            let mut run_text = String::new();

            let cell_origin = |row: usize, col: usize| {
                point(bounds.origin.x + cell_width * col as f32, bounds.origin.y + cell_height * row as f32)
            };

            let mut chips: Vec<(Bounds<Pixels>, Hsla, Hsla)> = Vec::new();
            let mut icons: Vec<(Bounds<Pixels>, SharedString, Hsla)> = Vec::new();
            let (kinds, failed) = if snap.mode.alt_screen { (vec![RowKind::Normal; snap.rows], vec![false; snap.rows]) } else { analyze_blocks(snap.rows, &snap.marks) };
            let shape = |t: String, color: Hsla, bold: bool, size: Pixels| {
                let mut f = view.font.clone();
                if bold {
                    f.weight = FontWeight::BOLD;
                }
                let run = TextRun { len: t.len(), font: f, color, background_color: None, underline: None, strikethrough: None };
                text_system.shape_line(SharedString::from(t), size, &[run], None)
            };
            let error_fg = Rgb { r: 0xf0, g: 0x7a, b: 0x82 };
            let warn_fg = Rgb { r: 0xe5, g: 0xc0, b: 0x7b };
            for row in 0..snap.rows {
                let cells = snap.row(row);
                // Full-screen apps (vim, htop) are left alone.
                if view.claude_fg && is_claude_chrome_row(&snap.row_text(row)) {
                    continue;
                }
                let kind = &kinds[row];
                let row_y = cell_origin(row, 0).y;
                let row_w = cell_width * snap.cols as f32;
                if failed[row] {
                    let red = hsla(error_fg);
                    backgrounds.push((Bounds::new(cell_origin(row, 0), size(row_w, cell_height)), Hsla { a: 0.06, ..red }));
                    backgrounds.push((Bounds::new(point(bounds.origin.x - px(6.), row_y), size(px(2.), cell_height)), Hsla { a: 0.9, ..red }));
                }
                match kind {
                    RowKind::End => continue,
                    RowKind::Header(q, info) => {
                        // Block separator above the header (not for the very top row).
                        if row > 0 {
                            backgrounds.push((Bounds::new(point(bounds.origin.x - px(8.), row_y - px(1.)), size(row_w + px(16.), px(1.))), hsla(Rgb { r: 0x2a, g: 0x2c, b: 0x32 })));
                        }
                        let mut label = q.get("cwd").cloned().unwrap_or_default();
                        if let Some((_, ms)) = info {
                            label.push_str(&format!(" ({})", fmt_ms(*ms)));
                        }
                        let line = shape(label, hsla(Rgb { r: 0x8a, g: 0x8f, b: 0x98 }), false, font_size * 0.92);
                        let x = bounds.origin.x + line.width;
                        text.push(TextFragment { origin: point(bounds.origin.x, row_y), line });
                        if let Some((code, _)) = info.filter(|(c, _)| *c != 0) {
                            let l = shape(format!("  exit {code}"), hsla(error_fg), true, font_size * 0.92);
                            text.push(TextFragment { origin: point(x, row_y), line: l });
                        }
                        continue;
                    }
                    RowKind::LivePrompt(q) => {
                        if row > 0 {
                            backgrounds.push((Bounds::new(point(bounds.origin.x - px(8.), row_y - px(1.)), size(row_w + px(16.), px(1.))), hsla(Rgb { r: 0x2a, g: 0x2c, b: 0x32 })));
                        }
                        let green = hsla(Rgb { r: 0xc3, g: 0xe8, b: 0x8d });
                        let plain = hsla(Rgb { r: 0xd7, g: 0xda, b: 0xe0 });
                        let muted = hsla(Rgb { r: 0x8a, g: 0x8f, b: 0x98 });
                        let mut items: Vec<(Option<&'static str>, String, Hsla)> = Vec::new();
                        if let Some(n) = q.get("node").filter(|n| !n.is_empty()) {
                            items.push((Some("icons/js.svg"), n.clone(), green));
                        }
                        if let Some(c) = q.get("cwd") {
                            items.push((Some("icons/folder.svg"), c.clone(), plain));
                        }
                        if let Some(b) = q.get("branch").filter(|b| !b.is_empty()) {
                            items.push((Some("icons/git.svg"), b.clone(), green));
                            let d = q.get("dirty").cloned().unwrap_or_else(|| "0".into());
                            let dirty = d != "0";
                            items.push((None, format!("± {d}"), if dirty { hsla(Rgb { r: 0xe5, g: 0xc0, b: 0x7b }) } else { muted }));
                        }
                        let chip_h = cell_height - px(2.);
                        let tsize = font_size * 0.88;
                        let mut x = bounds.origin.x;
                        for (icon, label, color) in items {
                            let line = shape(label, color, true, tsize);
                            let icon_w = if icon.is_some() { px(12.) + px(5.) } else { px(0.) };
                            let w = px(7.) + icon_w + line.width + px(7.);
                            let b = Bounds::new(point(x, row_y + px(1.)), size(w, chip_h));
                            chips.push((b, hsla(Rgb { r: 0x1c, g: 0x1e, b: 0x23 }), hsla(Rgb { r: 0x34, g: 0x37, b: 0x3e })));
                            if let Some(i) = icon {
                                icons.push((Bounds::new(point(x + px(7.), row_y + (cell_height - px(12.)) / 2.), size(px(12.), px(12.))), SharedString::from(i), muted));
                            }
                            // Center the label vertically in the row.
                            text.push(TextFragment { origin: point(x + px(7.) + icon_w, row_y + (cell_height - cell_height * 0.88) / 2.), line });
                            x += w + px(6.);
                        }
                        continue;
                    }
                    RowKind::Normal => {}
                }
                let tone = if snap.mode.alt_screen { None } else { classify_line(&snap.row_text(row)) };
                let cmd_fg = Rgb { r: 0xff, g: 0xff, b: 0xff };
                let fg_of = |c: &terminal_engine::SnapCell| match tone {
                    _ if c.mark == Mark::Cmd && c.fg == palette.foreground => cmd_fg,
                    Some(Tone::Error) if c.fg == palette.foreground => error_fg,
                    Some(Tone::Warning) if c.fg == palette.foreground => warn_fg,
                    _ => c.fg,
                };
                if let Some(tone) = tone {
                    let base = hsla(if tone == Tone::Error { error_fg } else { warn_fg });
                    let row_w = cell_width * snap.cols as f32;
                    backgrounds.push((Bounds::new(cell_origin(row, 0), size(row_w, cell_height)), Hsla { a: 0.09, ..base }));
                    backgrounds.push((Bounds::new(point(bounds.origin.x - px(6.), cell_origin(row, 0).y), size(px(2.), cell_height)), Hsla { a: 0.85, ..base }));
                }

                // Backgrounds: merge adjacent cells with equal non-default color.
                let mut col = 0;
                while col < cells.len() {
                    let bg = if cells[col].selected { selection_bg } else { cells[col].bg };
                    let start = col;
                    while col < cells.len()
                        && (if cells[col].selected { selection_bg } else { cells[col].bg }) == bg
                    {
                        col += 1;
                    }
                    if bg != default_bg {
                        backgrounds.push((
                            Bounds::new(cell_origin(row, start), size(cell_width * (col - start) as f32, cell_height)),
                            hsla(bg),
                        ));
                    }
                }

                // Text runs: contiguous cells with the same style, positioned on the grid so
                // alignment never depends on shaping. Wide chars get their own run.
                let mut col = 0;
                while col < cells.len() {
                    let cell = cells[col];
                    if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER)
                        || (cell.c == ' ' && !cell.flags.intersects(CellFlags::ALL_UNDERLINES | CellFlags::STRIKEOUT))
                    {
                        col += 1;
                        continue;
                    }
                    let style = RunStyle {
                        fg: fg_of(&cell),
                        bold: cell.flags.contains(CellFlags::BOLD) || cell.mark == Mark::Cmd,
                        italic: cell.flags.contains(CellFlags::ITALIC),
                        underline: cell.flags.intersects(CellFlags::ALL_UNDERLINES),
                        strike: cell.flags.contains(CellFlags::STRIKEOUT),
                    };
                    let start = col;
                    run_text.clear();
                    let wide = cell.flags.contains(CellFlags::WIDE_CHAR);
                    loop {
                        let c = cells[col];
                        run_text.push(c.c);
                        let idx = row * snap.cols + col;
                        if let Some((_, zw)) = snap.zerowidth.iter().find(|(i, _)| *i == idx) {
                            run_text.extend(zw.iter());
                        }
                        col += 1;
                        if wide || col >= cells.len() {
                            break;
                        }
                        let n = cells[col];
                        let next_style = RunStyle {
                            fg: fg_of(&n),
                            bold: n.flags.contains(CellFlags::BOLD) || n.mark == Mark::Cmd,
                            italic: n.flags.contains(CellFlags::ITALIC),
                            underline: n.flags.intersects(CellFlags::ALL_UNDERLINES),
                            strike: n.flags.contains(CellFlags::STRIKEOUT),
                        };
                        if next_style != style
                            || n.flags.intersects(CellFlags::WIDE_CHAR | CellFlags::WIDE_CHAR_SPACER)
                            || snap.zerowidth.iter().any(|(i, _)| *i == row * snap.cols + col)
                        {
                            break;
                        }
                    }
                    let mut font = view.font.clone();
                    if style.bold {
                        font.weight = FontWeight::BOLD;
                    }
                    if style.italic {
                        font.style = FontStyle::Italic;
                    }
                    let color = hsla(style.fg);
                    let run = TextRun {
                        len: run_text.len(),
                        font,
                        color,
                        background_color: None,
                        underline: style.underline.then(|| UnderlineStyle {
                            color: Some(color),
                            thickness: px(1.),
                            wavy: false,
                        }),
                        strikethrough: style.strike.then(|| StrikethroughStyle {
                            color: Some(color),
                            thickness: px(1.),
                        }),
                    };
                    let line = text_system.shape_line(SharedString::from(run_text.clone()), font_size, &[run], None);
                    text.push(TextFragment { origin: cell_origin(row, start), line });
                }
            }

            let cursor = snap.cursor.map(|c| {
                let wide = snap
                    .row(c.row)
                    .get(c.col)
                    .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR));
                let w = if wide { cell_width * 2. } else { cell_width };
                let origin = cell_origin(c.row, c.col);
                let color = hsla(palette.cursor);
                let b = match (c.shape, focused) {
                    (CursorShape::Beam, true) => Bounds::new(origin, size(px(2.), cell_height)),
                    (CursorShape::Underline, true) => {
                        Bounds::new(point(origin.x, origin.y + cell_height - px(2.)), size(w, px(2.)))
                    }
                    _ => Bounds::new(origin, size(w, cell_height)),
                };
                let hollow = !focused || c.shape == CursorShape::HollowBlock;
                (b, color, hollow)
            });

            let cursor_text = snap.cursor.filter(|c| {
                focused && matches!(c.shape, CursorShape::Block) && view.marked_text.is_none()
            }).and_then(|c| {
                let cell = *snap.row(c.row).get(c.col)?;
                if cell.c == ' ' || cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    return None;
                }
                let mut font = view.font.clone();
                if cell.flags.contains(CellFlags::BOLD) {
                    font.weight = FontWeight::BOLD;
                }
                let s: SharedString = cell.c.to_string().into();
                let run = TextRun {
                    len: s.len(),
                    font,
                    color: hsla(if cell.selected { selection_bg } else { cell.bg }),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                Some(TextFragment {
                    origin: cell_origin(c.row, c.col),
                    line: text_system.shape_line(s, font_size, &[run], None),
                })
            });

            let marked = view.marked_text.as_ref().zip(snap.cursor).map(|(t, c)| {
                let color = hsla(palette.foreground);
                let run = TextRun {
                    len: t.len(),
                    font: view.font.clone(),
                    color,
                    background_color: Some(hsla(default_bg)),
                    underline: Some(UnderlineStyle { color: Some(color), thickness: px(1.), wavy: false }),
                    strikethrough: None,
                };
                TextFragment {
                    origin: cell_origin(c.row, c.col),
                    line: text_system.shape_line(SharedString::from(t.clone()), font_size, &[run], None),
                }
            });

            PrepaintState { chips, icons, backgrounds, text, cursor, marked, cursor_text, line_height: cell_height }
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.view.clone()), cx);

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for (b, color) in state.backgrounds.drain(..) {
                window.paint_quad(fill(b, color));
            }
            for (b, bg, border) in state.chips.drain(..) {
                window.paint_quad(gpui::quad(b, px(5.), bg, px(1.), border, gpui::BorderStyle::Solid));
            }
            for (b, path, color) in state.icons.drain(..) {
                let _ = window.paint_svg(b, path, gpui::TransformationMatrix::unit(), color, cx);
            }
            if let Some((b, color, hollow)) = state.cursor {
                if hollow {
                    window.paint_quad(gpui::outline(b, color, gpui::BorderStyle::Solid));
                } else {
                    window.paint_quad(fill(b, color));
                }
            }
            for frag in state.text.iter() {
                let _ = frag.line.paint(frag.origin, state.line_height, window, cx);
            }
            // Character under a solid block cursor, redrawn in the cell's background color.
            if let Some(frag) = state.cursor_text.as_ref() {
                let _ = frag.line.paint(frag.origin, state.line_height, window, cx);
            }
            if let Some(m) = state.marked.as_ref() {
                let _ = m.line.paint(m.origin, state.line_height, window, cx);
            }
        });
    }
}

#[cfg(test)]
mod tone_tests {
    use super::*;

    #[test]
    fn claude_chrome_rows_and_status_parsing() {
        assert!(is_claude_chrome_row(" ▐▛███▛█   Claude Code v2.1.330"));
        assert!(is_claude_chrome_row("▝▜██████▀  Opus 5.5 with medium effort · Claude Team"));
        assert!(is_claude_chrome_row("  ⏵⏵ auto mode on (shift+tab to cycle)"));
        assert!(is_claude_chrome_row("                         ◐ medium · /effort"));
        assert!(!is_claude_chrome_row("auto mode is great"));
        let v: serde_json::Value = serde_json::from_str(r#"{"session_id":"s1","model":{"id":"claude-opus-5-5","display_name":"Opus 5.5"},"workspace":{"current_dir":"/x"},"context_window":{"used_percentage":37.5},"effort":{"level":"high"},"cost":{"total_cost_usd":0.42}}"#).unwrap();
        let st = ClaudeStatus::from_json(&v);
        assert_eq!((st.model.as_str(), st.effort.as_deref(), st.ctx_pct, st.session_id.as_str()), ("Opus 5.5", Some("high"), 37.5, "s1"));
        assert_eq!(st.cwd, Some(PathBuf::from("/x")));
    }

    #[test]
    fn finds_paths_including_hard_wrapped_ones() {
        let dir = std::env::temp_dir().join(format!("jt-path-{}", std::process::id()));
        let deep = dir.join("projects/-Users-x-Desktop-repositories-blokay/tool-results");
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("shot.png");
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(dir.join("main.rs"), b"fn main(){}").unwrap();
        let full = file.display().to_string();
        // Claude-style hard wrap: path split mid-word, continuation indented, trailing '.'.
        let cut = full.find("repositories").unwrap() + 4;
        let rows = vec![
            "  El PNG quedó en".to_string(),
            format!("  {}", &full[..cut]),
            format!("  {}.", &full[cut..]),
            String::new(),
        ];
        let wrapped = vec![false; rows.len()];
        let hit = path_at(&rows, &wrapped, 1, 5, None).unwrap();
        assert_eq!(hit.0, file);
        let hit = path_at(&rows, &wrapped, 2, 4, None).unwrap();
        assert_eq!(hit.0, file, "clicking the second half works too");
        // Relative path with line:col, resolved against the terminal cwd.
        let rows = vec!["error at main.rs:12:5 here".to_string()];
        let hit = path_at(&rows, &[false], 0, 10, Some(&dir)).unwrap();
        assert_eq!((hit.0, hit.1, hit.2), (dir.join("main.rs"), Some(12), Some(5)));
        // Plain words are not paths.
        assert!(path_at(&rows, &[false], 0, 1, Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn blocks_from_marks() {
        let marks = vec![
            (0, Mark::Prompt, "jt://prompt?cwd=~/a".to_string()),
            (3, Mark::End, "jt://end?code=1&ms=30".to_string()),
            (4, Mark::Prompt, "jt://prompt?cwd=~/b&branch=main&dirty=2".to_string()),
            (6, Mark::End, "jt://end?code=0&ms=1500".to_string()),
            (7, Mark::Prompt, "jt://prompt?cwd=~/c".to_string()),
        ];
        let (k, failed) = analyze_blocks(9, &marks);
        assert!(matches!(&k[0], RowKind::Header(q, Some((1, 30))) if q["cwd"] == "~/a"));
        assert_eq!(k[3], RowKind::End);
        assert!(matches!(&k[4], RowKind::Header(_, Some((0, 1500)))));
        assert!(matches!(&k[7], RowKind::LivePrompt(q) if q["cwd"] == "~/c"));
        assert_eq!(failed, vec![true, true, true, true, false, false, false, false, false]);
        // End whose prompt scrolled off the top: tint from row 0.
        let (_, f2) = analyze_blocks(3, &[(1, Mark::End, "jt://end?code=2&ms=1".into())]);
        assert_eq!(f2, vec![true, true, false]);
        assert_eq!(fmt_ms(30), "30ms");
        assert_eq!(fmt_ms(1500), "1.50s");
        assert_eq!(fmt_ms(125_000), "2m 05s");
    }

    #[test]
    fn classifies_common_tool_output() {
        let e = Some(Tone::Error);
        let w = Some(Tone::Warning);
        for l in ["zsh: command not found: la", "error[E0308]: mismatched types", "npm ERR! code ENOENT", "src/a.ts(3,1): error TS2304: Cannot find name 'x'.",
            "Error: listen EADDRINUSE: address already in use :::3000", "fatal: not a git repository", "2024-01-01 12:00:00 ERROR db down", "Traceback (most recent call last):",
            "  × Failed to compile", "cat: x: No such file or directory"] {
            assert_eq!(classify_line(l), e, "{l}");
        }
        for l in ["warning: unused variable: `x`", "npm WARN deprecated foo@1.0.0", "[WARN] slow query", "src/main.c:3:5: warning: implicit declaration", "⚠ Fast Refresh had to reload"] {
            assert_eq!(classify_line(l), w, "{l}");
        }
        for l in ["", "hello world", "Compiling foo v0.1.0", "  ✓ 12ms", "errors: 0 files", "the warnings are fine", "Terrorist", "information"] {
            assert_eq!(classify_line(l), None, "{l}");
        }
    }
}
