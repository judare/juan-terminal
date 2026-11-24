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
    CellFlags, CursorShape, KeyInput, KeyModes, Mark, Rgb, SessionEvent, parse_mark_uri,
    SearchHit, Snapshot, TermDims, TerminalSession, encode_key, encode_paste,
};

use crate::theme;

actions!(terminal, [Copy, Paste, Clear, ScrollPageUp, ScrollPageDown, ScrollToTop, ScrollToBottom, Find, FindNext, FindPrevious]);

/// Max matches kept by the scrollback search (a 10k-line history rarely gets close).
const SEARCH_LIMIT: usize = 5000;

/// ⌘F bar: matches over scrollback + screen.
struct TermSearch {
    input: Entity<crate::text_input::TextInput>,
    hits: Vec<SearchHit>,
    /// History size when `hits` was computed (grid lines shift as output arrives).
    base: usize,
    current: Option<usize>,
    _sub: Subscription,
}

/// Lines of output kept per terminal across relaunches.
const SCROLLBACK_KEEP_LINES: usize = 5_000;

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
    /// Subscription windows: (used %, resets_at unix secs).
    pub five_hour: Option<(f32, i64)>,
    pub seven_day: Option<(f32, i64)>,
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
            five_hour: window(v, "five_hour"),
            seven_day: window(v, "seven_day"),
        }
    }
}

fn window(v: &serde_json::Value, name: &str) -> Option<(f32, i64)> {
    let w = v.pointer(&format!("/rate_limits/{name}"))?;
    Some((w.get("used_percentage")?.as_f64()? as f32, w.get("resets_at").and_then(|x| x.as_i64()).unwrap_or(0)))
}

/// "in 2h 10m" / "in 3d 4h" until a unix timestamp.
fn resets_in(ts: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let s = (ts - now).max(0);
    if s >= 86_400 {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    } else if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}m", s / 60)
    }
}

fn usage_meter(id: &'static str, label: &'static str, w: (f32, i64)) -> gpui::Stateful<gpui::Div> {
    let (pct, reset) = (w.0.clamp(0., 100.), w.1);
    let color = if pct >= 90. { 0xe06c75 } else if pct >= 70. { 0xe5c07b } else { 0x7aa2f7 };
    let tip = format!("{label}: {pct:.0}% used · resets in {}", resets_in(reset));
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(5.))
        .tooltip(move |_, cx| cx.new(|_| crate::git_panel::DynTooltip(tip.clone().into())).into())
        .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(label))
        .child(
            div()
                .w(px(44.))
                .h(px(5.))
                .rounded_full()
                .bg(theme::rgb(0x2e3138))
                .child(div().h_full().rounded_full().w(px(44. * pct / 100.)).bg(theme::rgb(color))),
        )
        .child(div().text_size(px(11.)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{pct:.0}%")))
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
/// Quote a path for the shell only when it needs it (`'` inside becomes `'\\''`).
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,@%:=".contains(c));
    if safe { s.to_string() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

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
    /// Set when this tab runs a project dev script (Servers → Dev servers).
    pub dev_dir: Option<PathBuf>,
    pub claude_status: Option<ClaudeStatus>,
    claude_menu: Option<ClaudeMenu>,
    /// Claude Code is the foreground process (updated each render).
    claude_fg: bool,
    hover_cell: Option<(usize, usize)>,
    /// A mouse press was forwarded to the app (mouse tracking): its drag/release go too.
    mouse_forwarding: bool,
    last_reported_cell: Option<(usize, usize)>,
    hover_task: Option<Task<()>>,
    /// Hover preview and where to draw it (pointer, relative to the grid origin).
    preview: Option<(Point<Pixels>, crate::hover_preview::Preview)>,
    search: Option<TermSearch>,
    /// Debounced re-search while output arrives with the find bar open.
    search_refresh: Option<Task<()>>,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy)]
struct GridLayout {
    origin: Point<Pixels>,
    size: gpui::Size<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl TerminalView {
    /// Recolor with the current theme and tell theme-aware programs (DEC 2031) right away,
    /// even when this terminal is in a hidden tab.
    pub fn apply_theme(&mut self, cx: &mut Context<Self>) {
        self.session.set_palette(theme::terminal_palette());
        cx.notify();
    }

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
            dev_dir: None,
            claude_status: None,
            claude_menu: None,
            claude_fg: false,
            hover_cell: None,
            mouse_forwarding: false,
            last_reported_cell: None,
            hover_task: None,
            preview: None,
            search: None,
            search_refresh: None,
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

    /// Drop the Claude badge (and tell the workspace so the sidebar / Dock repaint).
    pub fn clear_attention(&mut self, cx: &mut Context<Self>) {
        if self.attention.take().is_some() {
            cx.emit(TerminalViewEvent::AttentionCleared);
            cx.notify();
        }
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
            .when_some(st.five_hour, |d, w| d.child(usage_meter("rl-5h", "5h", w)))
            .when_some(st.seven_day, |d, w| d.child(usage_meter("rl-7d", "week", w)))
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

    /// Output so far (with colors), for showing it again after a relaunch.
    pub fn dump_scrollback(&self) -> Option<String> {
        self.session.dump_ansi(SCROLLBACK_KEEP_LINES)
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
            SessionEvent::Redraw => {
                if self.search.is_some() && self.search_refresh.is_none() {
                    self.search_refresh = Some(cx.spawn(async move |this, cx| {
                        gpui::Timer::after(std::time::Duration::from_millis(250)).await;
                        let _ = this.update(cx, |this, cx| {
                            this.search_refresh = None;
                            this.run_search(false, cx);
                        });
                    }));
                }
                // Claude quit (or was killed) while its badge was up: nothing waits anymore.
                if self.attention.is_some() && !self.is_claude_running() {
                    self.clear_attention(cx);
                }
                cx.notify()
            }
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
                self.clear_attention(cx);
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

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Keys typed in the find bar bubble up here: they're not for the shell.
        if !self.focus_handle.is_focused(window) {
            return;
        }
        self.clear_preview(cx);
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

    // ---- find (⌘F) ------------------------------------------------------------------------

    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_none() {
            let input = cx.new(|cx| crate::text_input::TextInput::new("Find in terminal", cx));
            let sub = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                crate::text_input::TextInputEvent::Changed => this.run_search(true, cx),
                crate::text_input::TextInputEvent::Submit { shift } => this.search_step(!*shift, cx),
                crate::text_input::TextInputEvent::Cancel => this.close_search(window, cx),
            });
            self.search = Some(TermSearch { input, hits: Vec::new(), base: 0, current: None, _sub: sub });
        }
        let input = self.search.as_ref().unwrap().input.clone();
        match self.session.selection_text().filter(|t| !t.contains('\n')) {
            Some(sel) => input.update(cx, |i, cx| i.set_text(&sel, cx)),
            None => input.update(cx, |i, cx| i.select_all_text(cx)),
        }
        window.focus(&input.focus_handle(cx));
        self.run_search(true, cx);
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.search_step(true, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.search_step(false, cx);
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        self.search_refresh = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Recompute matches. `jump`: the query changed, go to the newest match (bottom);
    /// otherwise keep the current one (new output arrived).
    fn run_search(&mut self, jump: bool, cx: &mut Context<Self>) {
        let Some(s) = self.search.as_mut() else { return };
        let query = s.input.read(cx).content.clone();
        let prev = s.current.and_then(|i| s.hits.get(i)).map(|h| (h.start.0 as i64 + s.base as i64, h.start.1));
        let (hits, base) = self.session.search(&query, SEARCH_LIMIT);
        s.current = if hits.is_empty() {
            None
        } else if jump {
            Some(hits.len() - 1)
        } else {
            prev.and_then(|p| hits.iter().position(|h| (h.start.0 as i64 + base as i64, h.start.1) == p)).or(s.current.map(|c| c.min(hits.len() - 1)))
        };
        s.hits = hits;
        s.base = base;
        if jump {
            self.reveal_current();
        }
        cx.notify();
    }

    /// `older`: towards the top of the scrollback (Enter), like iTerm/Terminal.app.
    fn search_step(&mut self, older: bool, cx: &mut Context<Self>) {
        let Some(s) = self.search.as_mut() else { return };
        let n = s.hits.len();
        if n == 0 {
            return;
        }
        s.current = Some(match s.current {
            None => n - 1,
            Some(c) if older => (c + n - 1) % n,
            Some(c) => (c + 1) % n,
        });
        self.reveal_current();
        cx.notify();
    }

    fn reveal_current(&mut self) {
        if let Some(s) = &self.search {
            if let Some(h) = s.current.and_then(|i| s.hits.get(i)) {
                self.session.reveal_line(h.start.0, s.base);
            }
        }
    }

    fn render_search_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let s = self.search.as_ref()?;
        let count = if s.hits.is_empty() {
            if s.input.read(cx).content.is_empty() { String::new() } else { "No results".into() }
        } else {
            let more = if s.hits.len() >= SEARCH_LIMIT { "+" } else { "" };
            format!("{} of {}{more}", s.current.map(|c| c + 1).unwrap_or(0), s.hits.len())
        };
        let button = |id: &'static str, icon: &'static str, tip: &'static str| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(22.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
                .child(gpui::svg().path(icon).size(px(11.)).text_color(theme::rgb(theme::TEXT_MUTED)))
        };
        Some(
            div()
                .id("term-find")
                .absolute()
                .top(px(6.))
                .right(px(10.))
                .flex()
                .items_center()
                .gap(px(4.))
                .pl(px(8.))
                .pr(px(4.))
                .h(px(32.))
                .w(px(340.))
                .rounded(px(6.))
                .bg(theme::rgb(theme::SIDEBAR_BG))
                .border_1()
                .border_color(theme::rgb(theme::BORDER))
                .shadow_md()
                .cursor(CursorStyle::Arrow)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(div().flex_1().min_w_0().px(px(6.)).py(px(3.)).rounded(px(4.)).bg(theme::rgb(theme::EDITOR_BG)).cursor(CursorStyle::IBeam).child(s.input.clone()))
                .child(div().flex_none().min_w(px(62.)).text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_MUTED)).child(count))
                .child(button("term-find-prev", "icons/chevron-up.svg", "Older match (Enter / ⌘G)").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.search_step(true, cx))))
                .child(button("term-find-next", "icons/chevron-down.svg", "Newer match (⇧Enter / ⌘⇧G)").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.search_step(false, cx))))
                .child(button("term-find-close", "icons/close.svg", "Close (Esc)").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.close_search(w, cx))))
                .into_any_element(),
        )
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.session.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let item = cx.read_from_clipboard();
        let has_image = item.as_ref().is_some_and(|i| i.entries().iter().any(|e| matches!(e, gpui::ClipboardEntry::Image(_))));
        let text = item.as_ref().and_then(|i| i.text());
        // Claude Code reads images from the clipboard itself on Ctrl-V (like in other
        // terminals): hand it an image when there is one (a copied image may also carry its
        // URL as text), or when there's nothing textual to paste.
        if has_image && (self.claude_fg || text.is_none()) {
            self.write_user_input(b"\x16".to_vec(), cx);
            return;
        }
        if let Some(text) = text {
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
        self.clear_preview(cx);
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
        let mode = self.snapshot.mode;
        if mode.mouse_report() && !event.modifiers.shift {
            let m = &event.modifiers;
            self.session.write(terminal_engine::encode_mouse(0, col, row, true, mode.sgr_mouse, false, m.alt, m.control));
            self.mouse_forwarding = true;
            self.last_reported_cell = Some((row, col));
            return;
        }
        if event.modifiers.shift {
            self.session.update_selection(row, col, right);
        } else {
            self.session.start_selection(row, col, right, event.click_count);
        }
        self.selecting = true;
        cx.notify();
    }

    fn clear_preview(&mut self, cx: &mut Context<Self>) {
        self.hover_task = None;
        self.hover_cell = None;
        if self.preview.take().is_some() {
            cx.notify();
        }
    }

    /// Hover over a path / `[Image #N]`: after a short pause, show its preview by the pointer.
    fn hover(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some((row, col, _)) = self.cell_at(event.position) else {
            self.clear_preview(cx);
            return;
        };
        if self.hover_cell == Some((row, col)) {
            return;
        }
        self.hover_cell = Some((row, col));
        let Some(layout) = self.layout else { return };
        let local = event.position - layout.origin;
        let line = self.snapshot.row_text(row);
        let image = crate::hover_preview::image_ref_at(&line, col)
            .and_then(|n| self.claude_status.as_ref().filter(|s| !s.session_id.is_empty()).map(|s| (s.session_id.clone(), s.cwd.clone(), n)));
        let rows: Vec<String> = (0..self.snapshot.rows).map(|r| self.snapshot.row_text(r)).collect();
        let wrapped: Vec<bool> = (0..self.snapshot.rows).map(|r| self.snapshot.row(r).last().is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE))).collect();
        let cwd = self.current_cwd();
        // Still over the same target: keep the preview where it is.
        self.hover_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(std::time::Duration::from_millis(250)).await;
            let built = cx
                .background_executor()
                .spawn(async move {
                    if let Some((sid, scwd, n)) = image {
                        let p = crate::hover_preview::session_image(&sid, scwd.as_deref(), n)?;
                        return crate::hover_preview::for_path(&p, None).map(|pv| match pv {
                            crate::hover_preview::Preview::Image { path, caption, size, .. } => crate::hover_preview::Preview::Image { path, title: format!("[Image #{n}]"), caption, size },
                            other => other,
                        });
                    }
                    let (path, line, _) = path_at(&rows, &wrapped, row, col, cwd.as_deref())?;
                    crate::hover_preview::for_path(&path, line)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.hover_cell != Some((row, col)) {
                    return;
                }
                let changed = built.is_some() || this.preview.is_some();
                this.preview = built.map(|b| (local, b));
                if changed {
                    cx.notify();
                }
            });
        }));
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let mode = self.snapshot.mode;
        if mode.mouse_report() {
            let pressed = event.pressed_button == Some(MouseButton::Left);
            if (self.mouse_forwarding && pressed && (mode.mouse_drag || mode.mouse_motion)) || (!pressed && mode.mouse_motion) {
                if let Some((row, col, _)) = self.cell_at(event.position) {
                    if self.last_reported_cell != Some((row, col)) {
                        self.last_reported_cell = Some((row, col));
                        let button = if pressed { 32 } else { 35 };
                        self.session.write(terminal_engine::encode_mouse(button, col, row, true, mode.sgr_mouse, false, event.modifiers.alt, event.modifiers.control));
                    }
                }
            }
            if self.mouse_forwarding {
                return;
            }
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            if event.pressed_button.is_none() {
                self.hover(event, cx);
            }
            return;
        }
        if let Some((row, col, right)) = self.cell_at(event.position) {
            self.session.update_selection(row, col, right);
            cx.notify();
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_forwarding {
            self.mouse_forwarding = false;
            let mode = self.snapshot.mode;
            let (row, col) = self.cell_at(event.position).map(|(r, c, _)| (r, c)).or(self.last_reported_cell).unwrap_or((0, 0));
            self.session.write(terminal_engine::encode_mouse(0, col, row, false, mode.sgr_mouse, false, event.modifiers.alt, event.modifiers.control));
            return;
        }
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
        self.clear_preview(cx);
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
        if mode.mouse_report() {
            // The app tracks the mouse (Claude Code fullscreen, htop…): send wheel events at
            // the pointer instead of arrow keys or scrolling our own buffer.
            let (row, col) = self.cell_at(event.position).map(|(r, c, _)| (r, c)).unwrap_or((0, 0));
            let button = if whole > 0 { 64 } else { 65 };
            let mut bytes = Vec::new();
            for _ in 0..whole.unsigned_abs().min(20) {
                bytes.extend(terminal_engine::encode_mouse(button, col, row, true, mode.sgr_mouse, false, false, false));
            }
            self.session.write(bytes);
        } else if mode.alt_screen && mode.alternate_scroll {
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
        // Preview by the pointer: right of it, above it in the lower half (where it fits).
        let preview = self.preview.as_ref().zip(self.layout).map(|((at, pv), l)| {
            // The grid sits inside the root's padding (px 10, pt 6).
            let (x, y) = (at.x + px(10.), at.y + px(6.));
            let lower = at.y > l.size.height / 2.;
            // Space it can use: the view minus an 8 px margin, on the side of the pointer it opens to.
            let (vw, vh) = (f32::from(l.size.width), f32::from(l.size.height));
            let max_w = (vw - 16.).max(120.);
            let max_h = if lower { f32::from(y) - 14. - 8. } else { vh - f32::from(y) - 18. - 8. }.max(80.);
            let card_w = crate::hover_preview::card_width(pv, max_w, max_h);
            let left = (f32::from(x) + 14.).min(vw - card_w - 8.).max(8.);
            let el = div().absolute().left(px(left)).child(crate::hover_preview::render(pv, max_w, max_h));
            if lower { el.bottom(l.size.height - y + px(14.)) } else { el.top(y + px(18.)) }
        });
        let search_bar = self.render_search_bar(cx);
        let history_btn = (claude && search_bar.is_none()).then(|| {
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
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .drag_over::<gpui::ExternalPaths>(|s, _, _, _| s.opacity(0.8))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                // Like Terminal.app/iTerm: dropping files types their shell-quoted paths.
                if this.exit_code.is_some() || paths.paths().is_empty() {
                    return;
                }
                let mut text = paths.paths().iter().map(|p| shell_quote(&p.to_string_lossy())).collect::<Vec<_>>().join(" ");
                text.push(' ');
                let bytes = encode_paste(&text, this.snapshot.mode.bracketed_paste);
                this.write_user_input(bytes, cx);
                window.focus(&this.focus_handle);
                cx.stop_propagation();
            }))
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
            .children(search_bar)
            .children(popover)
            .children(preview)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.clear_preview(cx);
                }
            }))
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

/// Terminal chrome color (block separators, chips, find highlights) for the current theme.
fn ui(hex: u32) -> Rgb {
    let h = theme::map(hex);
    Rgb { r: (h >> 16) as u8, g: (h >> 8) as u8, b: h as u8 }
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
    if best.is_none() {
        // Relative to a subfolder of the cwd (Claude prints paths relative to where it ran a
        // command, e.g. `06-me-conviene/logo.png` under `store/`): find a folder named like
        // the first component below the cwd and resolve from its parent.
        let cwd = cwd?;
        let mut index: Option<HashMap<String, Vec<PathBuf>>> = None;
        for i in 0..=center {
            for j in center..pieces.len() {
                let joined: String = pieces[i..=j].concat();
                let t = joined.trim_end_matches(['.', ':', '!', '?']);
                let (p, line, col) = split_line_col(t);
                if p.starts_with('/') || p.starts_with('~') || p.starts_with("file:") {
                    continue;
                }
                let Some((first, _)) = p.split_once('/') else { continue };
                if first.is_empty() || first == "." || first == ".." {
                    continue;
                }
                let idx = index.get_or_insert_with(|| subdir_index(cwd));
                for dir in idx.get(first).into_iter().flatten() {
                    let Some(base) = dir.parent() else { continue };
                    let path = base.join(p);
                    if path.exists() && best.as_ref().is_none_or(|(len, _)| joined.len() > *len) {
                        best = Some((joined.len(), (path, line, col)));
                    }
                }
            }
        }
    }
    best.map(|(_, f)| f)
}

/// Folder name -> folders with that name below `root` (bounded breadth-first walk, skipping
/// hidden and dependency/build folders).
fn subdir_index(root: &Path) -> HashMap<String, Vec<PathBuf>> {
    const SKIP: &[&str] = &["node_modules", "target", "vendor", "dist", "build", "Pods", "DerivedData", "__pycache__"];
    let mut out: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut seen = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            seen += 1;
            if seen > 5000 {
                return out;
            }
            let path = e.path();
            out.entry(name).or_default().push(path.clone());
            if depth < 4 {
                queue.push_back((path, depth + 1));
            }
        }
    }
    out
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
            view.session.set_palette(theme::terminal_palette());
            view.session.snapshot(&mut view.snapshot);
            view.layout = Some(GridLayout { origin: bounds.origin, size: bounds.size, cell_width, cell_height });

            let snap = &view.snapshot;
            let palette = view.session.palette().clone();
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
            let error_fg = ui(0xf07a82);
            let warn_fg = ui(0xe5c07b);
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
                            backgrounds.push((Bounds::new(point(bounds.origin.x - px(8.), row_y - px(1.)), size(row_w + px(16.), px(1.))), hsla(ui(0x2a2c32))));
                        }
                        let mut label = q.get("cwd").cloned().unwrap_or_default();
                        if let Some((_, ms)) = info {
                            label.push_str(&format!(" ({})", fmt_ms(*ms)));
                        }
                        let line = shape(label, hsla(ui(0x8a8f98)), false, font_size * 0.92);
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
                            backgrounds.push((Bounds::new(point(bounds.origin.x - px(8.), row_y - px(1.)), size(row_w + px(16.), px(1.))), hsla(ui(0x2a2c32))));
                        }
                        let green = hsla(ui(0xc3e88d));
                        let plain = hsla(ui(0xd7dae0));
                        let muted = hsla(ui(0x8a8f98));
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
                            items.push((None, format!("± {d}"), if dirty { hsla(ui(0xe5c07b)) } else { muted }));
                        }
                        let chip_h = cell_height - px(2.);
                        let tsize = font_size * 0.88;
                        let mut x = bounds.origin.x;
                        for (icon, label, color) in items {
                            let line = shape(label, color, true, tsize);
                            let icon_w = if icon.is_some() { px(12.) + px(5.) } else { px(0.) };
                            let w = px(7.) + icon_w + line.width + px(7.);
                            let b = Bounds::new(point(x, row_y + px(1.)), size(w, chip_h));
                            chips.push((b, hsla(ui(0x1c1e23)), hsla(ui(0x34373e))));
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
                let cmd_fg = ui(0xffffff);
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

            // Find-bar matches (over cell backgrounds, under the text).
            if let Some(s) = &view.search {
                let shift = snap.history_size as i32 - s.base as i32;
                let off = snap.display_offset as i32;
                let (rows, cols) = (snap.rows as i32, snap.cols);
                let hit_bg = Hsla { a: 0.28, ..hsla(ui(0xe5c07b)) };
                let cur_bg = Hsla { a: 0.75, ..hsla(ui(0xd19a66)) };
                for (i, h) in s.hits.iter().enumerate() {
                    let (r0, r1) = (h.start.0 - shift + off, h.end.0 - shift + off);
                    if r1 < 0 || r0 >= rows {
                        continue;
                    }
                    let color = if s.current == Some(i) { cur_bg } else { hit_bg };
                    for r in r0.max(0)..=r1.min(rows - 1) {
                        let c0 = if r == r0 { h.start.1 } else { 0 };
                        let c1 = if r == r1 { h.end.1 } else { cols.saturating_sub(1) };
                        if c1 >= c0 {
                            backgrounds.push((Bounds::new(cell_origin(r as usize, c0), size(cell_width * (c1 - c0 + 1) as f32, cell_height)), color));
                        }
                    }
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
    fn resolves_path_relative_to_a_subfolder() {
        let dir = std::env::temp_dir().join(format!("jt-sub-{}", std::process::id()));
        let leaf = dir.join("apps/store/06-x");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("logo.png"), b"x").unwrap();
        let rows = vec!["  ⎿ Wrote 06-x/logo.png ok".to_string()];
        let hit = path_at(&rows, &[false], 0, 14, Some(&dir)).unwrap();
        assert_eq!(hit.0, leaf.join("logo.png"));
        let _ = std::fs::remove_dir_all(&dir);
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
