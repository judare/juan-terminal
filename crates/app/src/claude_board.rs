//! Claude sessions board (sidebar "Claude" mode): every Claude Code terminal of every open
//! window, with what it is doing (from our hooks), how long, context and cost, and a click to
//! jump to it.

use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, App, ClickEvent, Context, FontWeight, SharedString, Window, WindowHandle, div, prelude::*, px};

use crate::terminal_view::{ClaudeStatus, TerminalView};
use crate::theme;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeState {
    /// Running a turn (prompt submitted, tools running).
    Working,
    /// Waiting for a permission or an answer.
    NeedsInput,
    /// Finished its turn.
    Done,
}

/// What Claude is doing in one terminal, built from hook events.
#[derive(Clone, Debug, Default)]
pub struct ClaudeActivity {
    pub state: Option<ClaudeState>,
    /// When `state` began.
    pub since: Option<Instant>,
    /// Start of the current (or last) turn.
    pub turn_start: Option<Instant>,
    /// Length of the last finished turn.
    pub last_turn: Option<Duration>,
    /// Last prompt the user sent.
    pub prompt: Option<String>,
    /// Last tool run in this turn ("Edit · lib.rs").
    pub tool: Option<String>,
    /// Tools run in this turn.
    pub tools: u32,
    /// Notification text while waiting ("Claude needs your permission to use Bash").
    pub message: Option<String>,
    pub turns: u32,
}

impl ClaudeActivity {
    /// Apply a hook event. Returns true when the state changed (worth repainting other windows).
    pub fn apply(&mut self, event: &str, prompt: Option<String>, tool: Option<String>, message: Option<String>) -> bool {
        let now = Instant::now();
        let before = self.state;
        match event {
            "UserPromptSubmit" => {
                self.state = Some(ClaudeState::Working);
                self.since = Some(now);
                self.turn_start = Some(now);
                self.prompt = prompt.filter(|p| !p.trim().is_empty()).or(self.prompt.take());
                self.tool = None;
                self.tools = 0;
                self.message = None;
                self.turns += 1;
            }
            "PreToolUse" => {
                if self.state != Some(ClaudeState::Working) {
                    self.state = Some(ClaudeState::Working);
                    self.since = Some(now);
                    self.turn_start.get_or_insert(now);
                }
                self.message = None;
                self.tool = tool.or(self.tool.take());
                self.tools += 1;
            }
            "Notification" => {
                self.state = Some(ClaudeState::NeedsInput);
                self.since = Some(now);
                self.message = message;
            }
            "Stop" => {
                self.state = Some(ClaudeState::Done);
                self.since = Some(now);
                self.last_turn = self.turn_start.map(|t| now - t);
                self.message = None;
            }
            "SessionEnd" => *self = Self::default(),
            _ => {}
        }
        self.state != before
    }
}

/// One session row, copied out of whichever window owns the terminal.
struct Row {
    window: WindowHandle<Workspace>,
    mine: bool,
    project: String,
    terminal_id: String,
    title: SharedString,
    activity: ClaudeActivity,
    status: Option<ClaudeStatus>,
}

fn rows_of(ws: &Workspace, window: WindowHandle<Workspace>, mine: bool, cx: &App) -> Vec<Row> {
    let project = ws.active_project().map(|p| p.name.clone()).unwrap_or_else(|| "Sin proyecto".into());
    ws.terminals
        .values()
        .flat_map(|t| t.tabs.iter())
        .map(|t| t.read(cx))
        .filter(|t: &&TerminalView| !t.has_exited() && (t.activity.state.is_some() || t.claude_status.is_some()))
        .map(|t| Row {
            window,
            mine,
            project: project.clone(),
            terminal_id: t.jt_id.clone(),
            title: t.repository.clone().map(SharedString::from).unwrap_or_else(|| t.title()),
            activity: t.activity.clone(),
            status: t.claude_status.clone(),
        })
        .collect()
}

fn rank(a: &ClaudeActivity) -> u8 {
    match a.state {
        Some(ClaudeState::NeedsInput) => 0,
        Some(ClaudeState::Working) => 1,
        Some(ClaudeState::Done) => 2,
        None => 3,
    }
}

/// "45s", "4m 12s", "1h 05m".
pub fn short_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

impl Workspace {
    fn board_rows(&self, window: &Window, cx: &App) -> Vec<Row> {
        let me = window.window_handle();
        let mut rows = Vec::new();
        if let Some(h) = me.downcast::<Workspace>() {
            rows.extend(rows_of(self, h, true, cx));
        }
        for w in crate::workspace::workspace_windows(cx) {
            if AnyWindowHandle::from(w) == me {
                continue;
            }
            if let Ok(ws) = w.read(cx) {
                rows.extend(rows_of(ws, w, false, cx));
            }
        }
        rows.sort_by(|a, b| rank(&a.activity).cmp(&rank(&b.activity)).then_with(|| b.mine.cmp(&a.mine)).then_with(|| a.project.cmp(&b.project)));
        rows
    }

    /// Sessions waiting for the user, across all windows (badge on the sidebar tab).
    pub(crate) fn claude_needs_you(&self, window: &Window, cx: &App) -> usize {
        self.board_rows(window, cx).iter().filter(|r| r.activity.state == Some(ClaudeState::NeedsInput)).count()
    }

    fn jump_to_session(&mut self, target: WindowHandle<Workspace>, mine: bool, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if mine {
            self.focus_terminal_id(&id, window, cx);
            return;
        }
        cx.defer(move |cx| {
            cx.activate(true);
            let _ = target.update(cx, |ws, window, cx| {
                window.activate_window();
                ws.focus_terminal_id(&id, window, cx);
            });
        });
    }

    /// Repaint every second while the board is on screen (elapsed times, other windows).
    pub(crate) fn ensure_board_tick(&mut self, cx: &mut Context<Self>) {
        if self.board_tick.is_some() {
            return;
        }
        self.board_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_secs(1)).await;
                let keep = this
                    .update(cx, |this, cx| {
                        let on = this.claude_board_visible();
                        if on {
                            cx.notify();
                        } else {
                            this.board_tick = None;
                        }
                        on
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    pub(crate) fn render_claude_board(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        self.ensure_board_tick(cx);
        let rows = self.board_rows(window, cx);
        let count = |s: ClaudeState| rows.iter().filter(|r| r.activity.state == Some(s)).count();
        let (needs, working, done) = (count(ClaudeState::NeedsInput), count(ClaudeState::Working), count(ClaudeState::Done));
        let mut summary = Vec::new();
        if needs > 0 {
            summary.push(format!("{needs} te necesita{}", if needs == 1 { "" } else { "n" }));
        }
        if working > 0 {
            summary.push(format!("{working} trabajando"));
        }
        if done > 0 {
            summary.push(format!("{done} listo{}", if done == 1 { "" } else { "s" }));
        }
        let header = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .pt(px(6.))
            .pb(px(8.))
            .child(div().flex_1().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child("CLAUDE SESSIONS"))
            .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(summary.join(" · ")));
        let mut list = div().id("claude-board").flex().flex_col().gap(px(8.)).px(px(10.)).pb(px(16.)).flex_1().min_h_0().overflow_y_scroll();
        if rows.is_empty() {
            list = list.child(
                div()
                    .px(px(6.))
                    .py(px(10.))
                    .text_size(px(12.5))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child("No hay sesiones de Claude abiertas. Abre una con ⌘⇧C o desde el + de las terminales; aquí verás qué hace cada una, cuánto lleva y cuándo te necesita."),
            );
        }
        let now = Instant::now();
        for (i, r) in rows.into_iter().enumerate() {
            list = list.child(self.render_session_card(i, r, now, cx));
        }
        div().flex().flex_col().size_full().child(header).child(list).into_any_element()
    }

    fn render_session_card(&self, i: usize, r: Row, now: Instant, cx: &mut Context<Self>) -> gpui::AnyElement {
        let a = &r.activity;
        let since = a.since.map(|s| now.saturating_duration_since(s)).unwrap_or_default();
        let (dot, label, right) = match a.state {
            Some(ClaudeState::NeedsInput) => (0xe5c07b, "Te necesita".to_string(), short_duration(since)),
            Some(ClaudeState::Working) => (0x7aa2f7, "Trabajando".to_string(), short_duration(a.turn_start.map(|t| now.saturating_duration_since(t)).unwrap_or(since))),
            Some(ClaudeState::Done) => (
                0x98c379,
                match a.last_turn {
                    Some(d) => format!("Terminó en {}", short_duration(d)),
                    None => "Terminó".into(),
                },
                format!("hace {}", short_duration(since)),
            ),
            None => (0x5c6370, "Inactiva".to_string(), String::new()),
        };
        let border = if a.state == Some(ClaudeState::NeedsInput) { 0x7a6232 } else { theme::BORDER };
        let detail = match a.state {
            Some(ClaudeState::NeedsInput) => a.message.clone(),
            Some(ClaudeState::Working) => a.tool.clone().map(|t| format!("▸ {t}")),
            _ => None,
        };
        let status = r.status.clone();
        let (target, mine, id) = (r.window, r.mine, r.terminal_id.clone());
        div()
            .id(("claude-session", i))
            .flex()
            .flex_col()
            .gap(px(5.))
            .p(px(10.))
            .rounded(px(8.))
            .bg(theme::rgb(theme::EDITOR_BG))
            .border_1()
            .border_color(theme::rgb(border))
            .cursor_pointer()
            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.jump_to_session(target, mine, id.clone(), window, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .child(div().size(px(8.)).flex_none().rounded_full().bg(theme::rgb(dot)))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(r.title.clone()))
                    .child(div().flex_none().text_size(px(11.)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_MUTED)).child(right)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .text_size(px(11.))
                    .child(div().text_color(theme::rgb(dot)).child(label))
                    .when(!r.mine, |d| d.child(div().text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("· {}", r.project))))
                    .when(a.tools > 0, |d| d.child(div().text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("· {} tools", a.tools)))),
            )
            .when_some(a.prompt.clone(), |d, p| {
                let one: String = p.split_whitespace().collect::<Vec<_>>().join(" ");
                let one = if one.chars().count() > 160 { format!("{}…", one.chars().take(160).collect::<String>()) } else { one };
                d.child(div().text_size(px(12.)).line_height(px(16.)).text_color(theme::rgb(theme::TEXT_MUTED)).line_clamp(2).child(one))
            })
            .when_some(detail, |d, t| d.child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_FAINT)).child(t)))
            .when_some(status, |d, st| {
                let pct = st.ctx_pct.clamp(0., 100.);
                let bar = if pct >= 85. { 0xe06c75 } else if pct >= 65. { 0xe5c07b } else { 0x7aa2f7 };
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .pt(px(2.))
                        .text_size(px(10.5))
                        .text_color(theme::rgb(theme::TEXT_FAINT))
                        .when(!st.model.is_empty(), |d| d.child(st.model.clone()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .child("ctx")
                                .child(div().w(px(40.)).h(px(4.)).rounded_full().bg(theme::rgb(0x2e3138)).child(div().h_full().rounded_full().w(px(40. * pct / 100.)).bg(theme::rgb(bar))))
                                .child(format!("{pct:.0}%")),
                        )
                        .when(st.cost_usd > 0., |d| d.child(format!("${:.2}", st.cost_usd))),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_follows_hooks() {
        let mut a = ClaudeActivity::default();
        assert!(a.apply("UserPromptSubmit", Some("fix the bug".into()), None, None));
        assert_eq!(a.state, Some(ClaudeState::Working));
        assert!(!a.apply("PreToolUse", None, Some("Edit · lib.rs".into()), None));
        a.apply("PreToolUse", None, Some("Bash · cargo test".into()), None);
        assert_eq!((a.tools, a.tool.as_deref()), (2, Some("Bash · cargo test")));
        assert!(a.apply("Notification", None, None, Some("needs permission".into())));
        assert_eq!(a.state, Some(ClaudeState::NeedsInput));
        // Approving the permission resumes tools: back to working, message cleared.
        assert!(a.apply("PreToolUse", None, Some("Bash · ls".into()), None));
        assert_eq!((a.state, a.message.clone()), (Some(ClaudeState::Working), None));
        assert!(a.apply("Stop", None, None, None));
        assert!(a.last_turn.is_some());
        assert_eq!(a.prompt.as_deref(), Some("fix the bug"));
        a.apply("SessionEnd", None, None, None);
        assert_eq!(a.state, None);
    }

    #[test]
    fn durations() {
        assert_eq!(short_duration(Duration::from_secs(45)), "45s");
        assert_eq!(short_duration(Duration::from_secs(252)), "4m 12s");
        assert_eq!(short_duration(Duration::from_secs(3900)), "1h 05m");
    }
}
