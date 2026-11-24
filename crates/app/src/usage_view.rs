//! Agent usage dashboard (full-window overlay): tokens, tasks, agent time, activity heatmap,
//! models and agents, over the last 7 / 30 / 90 days or all history. Data: `usage_stats`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    KeyDownEvent, Render, SharedString, Task, Window, div, prelude::*, px, relative,
};

use crate::theme;
use crate::usage_stats::{self as us, Bucket, Range, Stats};

pub enum UsageEvent {
    Dismiss,
}

static LAST_RANGE: AtomicU8 = AtomicU8::new(1);

const RANGES: [(Range, &str); 4] = [(Range::Days7, "7 days"), (Range::Days30, "30 days"), (Range::Days90, "90 days"), (Range::All, "All")];

const CARD_BG: u32 = 0x1a1b1f;
const INPUT_BAR: u32 = 0x5c6168;
const OUTPUT_BAR: u32 = 0x4c9bf5;
const GREEN: u32 = 0x98c379;
const RED: u32 = 0xe06c75;

pub struct UsageView {
    focus: FocusHandle,
    range: Range,
    agent: Option<u8>,
    buckets: Arc<Vec<Bucket>>,
    stats: Stats,
    loaded: bool,
    loading: bool,
    _refresh: Option<Task<()>>,
    _ticker: Task<()>,
}

impl EventEmitter<UsageEvent> for UsageView {}

impl Focusable for UsageView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

fn local_now() -> (i64, i64) {
    // SAFETY: localtime_r only writes into the struct we pass.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (t as i64, tm.tm_gmtoff as i64)
    }
}

impl UsageView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let range = RANGES[(LAST_RANGE.load(Ordering::Relaxed) as usize).min(3)].0;
        // Repaints the indexing progress and picks up new usage while the dashboard is open.
        let ticker = cx.spawn(async move |this, cx| {
            let mut n = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                n += 1;
                let ok = this
                    .update(cx, |this, cx| {
                        if this.loading {
                            cx.notify();
                        } else if n % 100 == 0 {
                            this.refresh(cx);
                        }
                    })
                    .is_ok();
                if !ok {
                    break;
                }
            }
        });
        let mut this = Self { focus, range, agent: None, buckets: Arc::new(Vec::new()), stats: Stats::default(), loaded: false, loading: false, _refresh: None, _ticker: ticker };
        this.recompute();
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        self._refresh = Some(cx.spawn(async move |this, cx| {
            let buckets = cx.background_executor().spawn(async move { us::refresh() }).await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                this.loaded = true;
                this.buckets = buckets;
                this.recompute();
                cx.notify();
            });
        }));
    }

    fn recompute(&mut self) {
        let (now, offset) = local_now();
        self.stats = us::compute(&self.buckets, self.range, self.agent, now, offset);
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        self.range = range;
        LAST_RANGE.store(RANGES.iter().position(|r| r.0 == range).unwrap_or(1) as u8, Ordering::Relaxed);
        self.recompute();
        cx.notify();
    }

    fn toggle_agent(&mut self, agent: u8, cx: &mut Context<Self>) {
        self.agent = if self.agent == Some(agent) { None } else { Some(agent) };
        self.recompute();
        cx.notify();
    }
}

fn c(hex: u32) -> Hsla {
    theme::rgb(hex)
}

fn muted(s: impl Into<SharedString>) -> gpui::Div {
    div().text_color(c(theme::TEXT_MUTED)).child(s.into())
}

fn faint(s: impl Into<SharedString>) -> gpui::Div {
    div().text_color(c(theme::TEXT_FAINT)).child(s.into())
}

fn provider_color(p: &str) -> u32 {
    match p {
        "Anthropic" => 0xd97757,
        "OpenAI" => 0x10a37f,
        "Google" => 0x4285f4,
        "xAI" => 0xa9aeb6,
        _ => 0x8a8f98,
    }
}

fn dot(color: u32, size: f32) -> gpui::Div {
    div().flex_none().size(px(size)).rounded_full().bg(c(color))
}

fn agent_icon(agent: u8) -> AnyElement {
    if agent == us::CLAUDE { crate::assets::claude_logo(15.).into_any_element() } else { div().flex_none().size(px(15.)).flex().items_center().justify_center().child(dot(0x10a37f, 11.)).into_any_element() }
}

/// Section title with a hairline to the right-side note.
fn section(title: &'static str, note: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .text_size(px(12.))
        .child(div().flex_none().text_color(c(theme::TEXT_MUTED)).child(title))
        .child(div().flex_1().h(px(1.)).bg(c(theme::BORDER)))
        .child(div().flex_none().text_color(c(theme::TEXT_MUTED)).child(note.into()))
}

fn sparkline(values: &[u64]) -> gpui::Div {
    let max = values.iter().copied().max().unwrap_or(0).max(1) as f32;
    div().flex().items_end().gap(px(2.)).h(px(24.)).children(values.iter().map(|&v| {
        let f = v as f32 / max;
        div().flex_1().h(if v == 0 { gpui::DefiniteLength::from(px(1.)) } else { relative(f.max(0.06)) }).bg(c(theme::TEXT_FAINT).opacity(if v == 0 { 0.35 } else { 0.9 }))
    }))
}

fn delta(cur: u64, prev: Option<u64>) -> Option<gpui::Div> {
    let prev = prev.filter(|&p| p > 0)?;
    let pct = ((cur as f64 - prev as f64) / prev as f64 * 100.).round() as i64;
    let up = pct >= 0;
    let label = if pct > 999 { "+999%".to_string() } else if up { format!("+{pct}%") } else { format!("{pct}%") };
    let color = if up { GREEN } else { RED };
    Some(div().px(px(6.)).py(px(1.)).rounded(px(4.)).text_size(px(12.)).bg(c(color).opacity(0.14)).text_color(c(color)).child(label))
}

fn card(label: &'static str, value: String, sub: String, change: Option<gpui::Div>, spark: &[u64]) -> gpui::Div {
    div()
        .flex_1()
        .min_w(px(150.))
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(14.))
        .rounded(px(8.))
        .border_1()
        .border_color(c(theme::BORDER))
        .bg(c(CARD_BG))
        .child(div().flex().items_center().justify_between().text_size(px(12.)).child(muted(label)).children(change))
        .child(div().text_size(px(30.)).line_height(px(36.)).text_color(c(theme::TEXT)).child(value))
        .child(div().text_size(px(12.)).child(muted(sub)))
        .child(div().pt(px(10.)).child(sparkline(spark)))
}

fn segmented(this: &UsageView, cx: &mut Context<UsageView>) -> gpui::Div {
    div().flex().p(px(2.)).gap(px(2.)).rounded(px(7.)).bg(c(CARD_BG)).border_1().border_color(c(theme::BORDER)).children(RANGES.iter().map(|&(r, label)| {
        let active = this.range == r;
        div()
            .id(label)
            .px(px(10.))
            .py(px(3.))
            .rounded(px(5.))
            .cursor_pointer()
            .text_size(px(13.))
            .when(active, |d| d.bg(c(theme::SELECTED)).text_color(c(theme::TEXT)))
            .when(!active, |d| d.text_color(c(theme::TEXT_MUTED)).hover(|s| s.bg(c(theme::HOVER))))
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_range(r, cx)))
    }))
}

impl UsageView {
    fn render_tokens_by_day(&self) -> gpui::Div {
        let s = &self.stats;
        let max_in = s.daily.iter().map(|d| d.input).max().unwrap_or(0);
        let max_out = s.daily.iter().map(|d| d.output).max().unwrap_or(0);
        let active_days = s.daily.iter().filter(|d| d.input + d.output > 0).count();
        let avg = (s.totals.input + s.totals.output) / active_days.max(1) as u64;
        let note = format!("{active_days} of {} days with agents · {}/day", s.daily.len(), us::fmt_tokens(avg));
        let n = s.daily.len();
        let gap = if n > 60 { 1. } else { 3. };
        let bar = |v: u64, max: u64, color: u32| {
            let f = if max == 0 { 0. } else { v as f32 / max as f32 };
            div().flex_1().h(if v == 0 { gpui::DefiniteLength::from(px(0.)) } else { relative(f.max(0.02)) }).bg(c(color))
        };
        let label_col = div()
            .w(px(64.))
            .flex_none()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .child(div().h(px(96.)).flex().flex_col().child(div().text_color(c(theme::TEXT)).child(us::fmt_tokens(max_in))).child(faint("input")))
            .child(div().h(px(48.)).pt(px(4.)).flex().flex_col().child(div().text_color(c(theme::TEXT)).child(us::fmt_tokens(max_out))).child(div().text_color(c(OUTPUT_BAR)).child("output")));
        let mid = s.start_day + (n as i64 - 1) / 2;
        div()
            .flex_1()
            .min_w(px(320.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(section("TOKENS BY DAY", note))
            .child(
                div()
                    .flex()
                    .child(label_col)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(div().h(px(96.)).flex().items_end().gap(px(gap)).children(s.daily.iter().map(|d| bar(d.input, max_in, INPUT_BAR))))
                            .child(div().h(px(1.)).bg(c(theme::BORDER)))
                            .child(div().h(px(48.)).flex().items_start().gap(px(gap)).children(s.daily.iter().map(|d| bar(d.output, max_out, OUTPUT_BAR)))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .pl(px(64.))
                    .text_size(px(12.))
                    .child(muted(us::month_day(s.start_day)))
                    .when(n > 2, |d| d.child(muted(us::month_day(mid))))
                    .child(muted("today")),
            )
    }

    fn render_heatmap(&self) -> gpui::Div {
        const DAYS: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];
        const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let heat = &self.stats.heat;
        let max = heat.iter().flatten().copied().max().unwrap_or(0);
        let mut best = (0, 0, 0u64);
        for (d, row) in heat.iter().enumerate() {
            for (h, &v) in row.iter().enumerate() {
                if v > best.2 {
                    best = (d, h, v);
                }
            }
        }
        let note = if max == 0 { String::new() } else { format!("most active {} {} h", NAMES[best.0], best.1) };
        let cell = move |v: u64| {
            let f = if max == 0 { 0. } else { (v as f32 / max as f32).sqrt() };
            div().flex_1().h(px(14.)).rounded(px(2.)).bg(c(theme::TEXT).opacity(0.06 + 0.84 * f))
        };
        div()
            .flex_1()
            .min_w(px(320.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(section("WHEN THEY WORK", note))
            .child(div().flex().flex_col().gap(px(2.)).children(heat.iter().enumerate().map(|(d, row)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .child(div().w(px(18.)).flex_none().text_size(px(12.)).text_color(c(theme::TEXT_MUTED)).child(DAYS[d]))
                    .children(row.iter().map(|&v| cell(v)))
            })))
            .child(
                div().flex().pl(px(20.)).text_size(px(12.)).text_color(c(theme::TEXT_MUTED)).children([0, 6, 12, 18].map(|h| div().flex_1().child(h.to_string()))),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .items_center()
                    .gap(px(3.))
                    .text_size(px(12.))
                    .child(muted("less").mr(px(3.)))
                    .children([0.1, 0.35, 0.6, 0.9].map(|a| div().size(px(10.)).rounded(px(2.)).bg(c(theme::TEXT).opacity(a))))
                    .child(muted("more").ml(px(3.))),
            )
    }

    fn render_models(&self) -> gpui::Div {
        let s = &self.stats;
        let max = s.models.first().map(|m| m.input + m.output).unwrap_or(0).max(1);
        let num = |v: String| div().w(px(64.)).flex_none().text_right().child(v);
        let header = div().flex().text_size(px(12.)).text_color(c(theme::TEXT_MUTED)).child(div().flex_1()).child(num("input".into())).child(num("output".into())).child(num("tasks".into()));
        let shown = 6;
        let rest = &s.models[s.models.len().min(shown)..];
        let row = |icon: AnyElement, name: String, share: f32, color: u32, input: u64, output: u64, tasks: usize, dim: bool| {
            div()
                .flex()
                .items_center()
                .h(px(26.))
                .text_size(px(13.))
                .text_color(c(if dim { theme::TEXT_MUTED } else { theme::TEXT }))
                .child(div().w(px(200.)).flex_none().flex().items_center().gap(px(8.)).overflow_hidden().child(icon).child(div().truncate().child(name)))
                .child(div().flex_1().pr(px(12.)).child(div().h(px(6.)).rounded(px(3.)).w(relative(share.max(0.015))).bg(c(color))))
                .child(num(us::fmt_tokens(input)))
                .child(num(us::fmt_tokens(output)))
                .child(num(us::fmt_int(tasks as u64)).text_color(c(theme::TEXT_MUTED)))
        };
        let mut list = div().flex().flex_col();
        for m in s.models.iter().take(shown) {
            let p = us::provider(&m.name);
            list = list.child(row(dot(provider_color(p), 9.).into_any_element(), m.name.clone(), (m.input + m.output) as f32 / max as f32, provider_color(p), m.input, m.output, m.tasks, false));
        }
        if !rest.is_empty() {
            let (i, o, t) = rest.iter().fold((0, 0, 0), |a, m| (a.0 + m.input, a.1 + m.output, a.2 + m.tasks));
            list = list.child(row(div().size(px(9.)).into_any_element(), format!("{} more", rest.len()), (i + o) as f32 / max as f32, theme::TEXT_FAINT, i, o, t, true));
        }
        let note = match s.models.len() {
            1 => "1 model".to_string(),
            n => format!("{n} models"),
        };
        div()
            .flex_1()
            .min_w(px(320.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section("MODELS", note))
            .child(div().flex().h(px(6.)).gap(px(2.)).children(s.providers.iter().map(|&(p, share)| div().h_full().rounded(px(3.)).w(relative(share.max(0.01))).bg(c(provider_color(p))))))
            .child(div().flex().gap(px(14.)).text_size(px(12.)).children(s.providers.iter().map(|&(p, share)| {
                div().flex().items_center().gap(px(5.)).child(dot(provider_color(p), 8.)).child(muted(format!("{p} {}%", (share * 100.).round() as u32)))
            })))
            .child(div().pt(px(6.)).child(header))
            .child(list)
            .when(s.models.is_empty(), |d| d.child(faint("No usage in this period.").text_size(px(13.))))
    }

    fn render_agents(&self, cx: &mut Context<Self>) -> gpui::Div {
        let s = &self.stats;
        let max = s.agents.first().map(|a| a.input + a.output).unwrap_or(0).max(1);
        let num = |v: String| div().w(px(64.)).flex_none().text_right().child(v);
        let header = div().flex().text_size(px(12.)).text_color(c(theme::TEXT_MUTED)).child(div().flex_1()).child(num("input".into())).child(num("output".into())).child(num("time".into()));
        let selected = self.agent;
        div()
            .flex_1()
            .min_w(px(320.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section("AGENTS", if selected.is_some() { "click again to see all" } else { "click one to see only its usage" }))
            .child(div().pt(px(30.)).child(header))
            .children(s.agents.iter().map(|a| {
                let agent = a.agent;
                let on = selected.is_none_or(|x| x == agent);
                let share = (a.input + a.output) as f32 / max as f32;
                div()
                    .id(("usage-agent", agent as usize))
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px(px(6.))
                    .mx(px(-6.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(c(if on { theme::TEXT } else { theme::TEXT_FAINT }))
                    .when(selected == Some(agent), |d| d.bg(c(theme::SELECTED)))
                    .hover(|d| d.bg(c(theme::HOVER)))
                    .child(div().w(px(200.)).flex_none().flex().items_center().gap(px(8.)).child(agent_icon(agent)).child(us::AGENT_NAMES.get(agent as usize).copied().unwrap_or("?")))
                    .child(div().flex_1().pr(px(12.)).child(div().h(px(6.)).rounded(px(3.)).w(relative(share.max(0.015))).bg(c(if on { 0xa9aeb6 } else { theme::TEXT_FAINT }))))
                    .child(num(us::fmt_tokens(a.input)))
                    .child(num(us::fmt_tokens(a.output)))
                    .child(num(us::fmt_duration(a.active)).text_color(c(theme::TEXT_MUTED)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_agent(agent, cx)))
            }))
    }
}

impl Render for UsageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.stats;
        let t = s.totals;
        let p = s.prev;
        let cache_pct = if t.input == 0 { 0 } else { (t.cached as f64 / t.input as f64 * 100.).round() as u64 };
        let per_turn = t.output / t.turns.max(1);
        let mut subtitle = format!("{} – today", us::month_day(s.start_day));
        if let Some(a) = self.agent {
            subtitle.push_str(&format!(" · {}", us::AGENT_NAMES.get(a as usize).copied().unwrap_or("?")));
        }
        let status: Option<String> = match us::progress() {
            Some(f) if self.loading => Some(format!("indexing history… {}%", (f * 100.).round() as u32)),
            _ if self.loading && !self.loaded => Some("loading…".into()),
            _ => None,
        };
        let spark = |f: fn(&us::Day) -> u64| s.daily.iter().map(f).collect::<Vec<u64>>();
        div()
            .id("usage-view")
            .track_focus(&self.focus)
            .key_context("UsageView")
            .on_key_down(cx.listener(|_, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cx.emit(UsageEvent::Dismiss);
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .overflow_y_scroll()
            .bg(c(theme::APP_BG))
            .text_color(c(theme::TEXT))
            .font_family(theme::UI_FONT)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(26.))
                    .px(px(28.))
                    .pt(px(46.))
                    .pb(px(28.))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Agent usage"))
                                    .child(div().flex().gap(px(10.)).text_size(px(13.)).child(muted(subtitle)).children(status.map(|s| faint(s)))),
                            )
                            .child(
                                div().flex().items_center().gap(px(16.)).child(segmented(self, cx)).child(
                                    div()
                                        .id("usage-esc")
                                        .px(px(7.))
                                        .py(px(2.))
                                        .rounded(px(5.))
                                        .border_1()
                                        .border_color(c(theme::BORDER))
                                        .bg(c(CARD_BG))
                                        .text_size(px(12.))
                                        .text_color(c(theme::TEXT_MUTED))
                                        .cursor_pointer()
                                        .hover(|d| d.bg(c(theme::HOVER)))
                                        .child("esc")
                                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(UsageEvent::Dismiss))),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(12.))
                            .child(card("INPUT TOKENS", us::fmt_tokens(t.input), format!("{cache_pct}% from cache"), delta(t.input, p.map(|p| p.input)), &spark(|d| d.input)))
                            .child(card("OUTPUT TOKENS", us::fmt_tokens(t.output), format!("{} per turn", us::fmt_tokens(per_turn)), delta(t.output, p.map(|p| p.output)), &spark(|d| d.output)))
                            .child(card("TASKS", us::fmt_int(t.tasks as u64), format!("{} turns", us::fmt_int(t.turns)), delta(t.tasks as u64, p.map(|p| p.tasks as u64)), &spark(|d| d.tasks as u64)))
                            .child(card(
                                "AGENT TIME",
                                us::fmt_duration(t.active),
                                if s.max_concurrent > 1 { format!("up to {} at once", s.max_concurrent) } else { "one at a time".into() },
                                delta(t.active, p.map(|p| p.active)),
                                &spark(|d| d.active),
                            )),
                    )
                    .child(div().flex().flex_wrap().gap(px(40.)).child(self.render_tokens_by_day()).child(self.render_heatmap()))
                    .child(div().flex().flex_wrap().gap(px(40.)).child(self.render_models()).child(self.render_agents(cx))),
            )
    }
}
