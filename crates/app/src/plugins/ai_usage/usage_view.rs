//! Agent usage dashboard (full-window overlay): tokens, tasks, agent time, activity heatmap,
//! models and agents, over the last 7 / 30 / 90 days or all history. Data: `usage_stats`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    KeyDownEvent, Render, SharedString, Task, Window, div, prelude::*, px, relative,
};

use crate::glass::GlassExt;
use crate::theme;
use crate::usage_stats::{self as us, Bucket, Range, Stats};

pub enum UsageEvent {
    Dismiss,
}

static LAST_RANGE: AtomicU8 = AtomicU8::new(1);

const RANGES: [(Range, &str); 4] = [(Range::Days7, "7 days"), (Range::Days30, "30 days"), (Range::Days90, "90 days"), (Range::All, "All")];

/// Brand-red ramp (Fragua palette) for series, providers and the heatmap.
const RAMP: [u32; 4] = [theme::ACCENT, theme::BRAND_EDGE, theme::MARK_B, theme::TEXT_FAINT];

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

/// Color of a provider: its rank in this period's share list, on the red ramp.
fn provider_color(providers: &[(&'static str, f32)], p: &str) -> u32 {
    let i = providers.iter().position(|(name, _)| *name == p).unwrap_or(RAMP.len() - 1);
    RAMP[i.min(RAMP.len() - 1)]
}

fn dot(color: u32, size: f32) -> gpui::Div {
    div().flex_none().size(px(size)).rounded_full().bg(c(color))
}

fn agent_icon(agent: u8) -> AnyElement {
    if agent == us::CLAUDE { crate::assets::claude_logo(15.).into_any_element() } else { div().flex_none().size(px(15.)).flex().items_center().justify_center().child(dot(theme::TEXT_MUTED, 10.)).into_any_element() }
}

/// Faint track for bars and meters.
fn track() -> Hsla {
    gpui::hsla(0., 0., if theme::is_light() { 0. } else { 1. }, 0.07)
}

/// Glass panel holding one block of the dashboard.
fn panel() -> gpui::Div {
    div().flex_1().min_w(px(340.)).flex().flex_col().gap(px(14.)).p(px(18.)).rounded(px(18.)).glass()
}

/// Panel header: bold title, quiet note on the right.
fn section(title: &'static str, note: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_baseline()
        .justify_between()
        .gap(px(10.))
        .child(div().flex_none().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(c(theme::TEXT)).child(title))
        .child(div().min_w_0().truncate().text_size(px(11.5)).text_color(c(theme::TEXT_FAINT)).child(note.into()))
}

/// Rounded bars in brand red; the latest day is fully lit.
fn sparkline(values: &[u64]) -> gpui::Div {
    let max = values.iter().copied().max().unwrap_or(0).max(1) as f32;
    let n = values.len();
    let gap = if n > 60 { 1. } else { 2. };
    div().flex().items_end().gap(px(gap)).h(px(30.)).children(values.iter().enumerate().map(|(i, &v)| {
        let f = v as f32 / max;
        let last = i + 1 == n;
        div()
            .flex_1()
            .rounded_t(px(2.))
            .h(if v == 0 { gpui::DefiniteLength::from(px(2.)) } else { relative(f.max(0.08)) })
            .bg(if v == 0 { track() } else { c(theme::ACCENT).opacity(if last { 1. } else { 0.3 + 0.45 * f }) })
    }))
}

/// Change vs the previous period: a quiet glass pill with an arrow (no traffic-light colors).
fn delta(cur: u64, prev: Option<u64>) -> Option<gpui::Div> {
    let prev = prev.filter(|&p| p > 0)?;
    let pct = ((cur as f64 - prev as f64) / prev as f64 * 100.).round() as i64;
    let up = pct >= 0;
    let label = if pct > 999 { "↑ 999%".to_string() } else if up { format!("↑ {pct}%") } else { format!("↓ {}%", -pct) };
    Some(
        div()
            .px(px(7.))
            .py(px(1.))
            .rounded_full()
            .text_size(px(11.))
            .font_weight(FontWeight::MEDIUM)
            .when(up, |d| d.bg(c(theme::ACCENT).opacity(0.14)).text_color(c(theme::ACCENT)))
            .when(!up, |d| d.glass_button().text_color(c(theme::TEXT_MUTED)))
            .child(label),
    )
}

fn card(label: &'static str, value: String, sub: String, change: Option<gpui::Div>, spark: &[u64]) -> gpui::Div {
    div()
        .flex_1()
        .min_w(px(170.))
        .flex()
        .flex_col()
        .gap(px(3.))
        .p(px(16.))
        .rounded(px(18.))
        .glass()
        .child(div().flex().items_center().justify_between().child(div().text_size(px(11.)).font_weight(FontWeight::MEDIUM).text_color(c(theme::TEXT_MUTED)).child(label)).children(change))
        .child(div().pt(px(4.)).text_size(px(32.)).line_height(px(38.)).font_weight(FontWeight::SEMIBOLD).text_color(c(theme::TEXT)).child(value))
        .child(div().text_size(px(12.)).text_color(c(theme::TEXT_FAINT)).child(sub))
        .child(div().pt(px(12.)).child(sparkline(spark)))
}

fn segmented(this: &UsageView, cx: &mut Context<UsageView>) -> gpui::Div {
    div()
        .flex()
        .p(px(3.))
        .gap(px(2.))
        .rounded_full()
        .bg(gpui::hsla(0., 0., 0., if theme::is_light() { 0.05 } else { 0.25 }))
        .border_1()
        .border_color(crate::glass::edge())
        .children(RANGES.iter().map(|&(r, label)| {
            let active = this.range == r;
            div()
                .id(label)
                .px(px(12.))
                .py(px(4.))
                .rounded_full()
                .cursor_pointer()
                .text_size(px(12.5))
                .when(active, |d| d.glass_button().font_weight(FontWeight::MEDIUM).text_color(c(theme::TEXT)))
                .when(!active, |d| d.text_color(c(theme::TEXT_MUTED)).hover(|s| s.text_color(c(theme::TEXT))))
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_range(r, cx)))
        }))
}

fn legend(color: Hsla, label: &'static str) -> gpui::Div {
    div().flex().items_center().gap(px(5.)).child(div().size(px(8.)).rounded(px(2.)).bg(color)).child(muted(label))
}

impl UsageView {
    fn render_tokens_by_day(&self) -> gpui::Div {
        let s = &self.stats;
        let max_in = s.daily.iter().map(|d| d.input).max().unwrap_or(0);
        let max_out = s.daily.iter().map(|d| d.output).max().unwrap_or(0);
        let active_days = s.daily.iter().filter(|d| d.input + d.output > 0).count();
        let avg = (s.totals.input + s.totals.output) / active_days.max(1) as u64;
        let note = format!("{active_days} of {} days · {}/day", s.daily.len(), us::fmt_tokens(avg));
        let n = s.daily.len();
        let gap = if n > 60 { 1. } else { 3. };
        let (in_color, out_color) = (c(theme::ACCENT).opacity(0.45), c(theme::ACCENT));
        let h = |v: u64, max: u64| {
            let f = if max == 0 { 0. } else { v as f32 / max as f32 };
            if v == 0 { gpui::DefiniteLength::from(px(0.)) } else { relative(f.max(0.03)) }
        };
        // One hoverable column per day: input grows up, output grows down.
        let columns = s.daily.iter().enumerate().map(|(i, d)| {
            let tip: SharedString = format!("{} · {} in · {} out", us::month_day(s.start_day + i as i64), us::fmt_tokens(d.input), us::fmt_tokens(d.output)).into();
            div()
                .id(("usage-day", i))
                .flex_1()
                .flex()
                .flex_col()
                .rounded(px(3.))
                .hover(|st| st.bg(track()))
                .child(div().h(px(110.)).flex().items_end().child(div().w_full().rounded_t(px(3.)).h(h(d.input, max_in)).bg(in_color)))
                .child(div().h(px(3.)))
                .child(div().h(px(52.)).flex().items_start().child(div().w_full().rounded_b(px(3.)).h(h(d.output, max_out)).bg(out_color)))
                .tooltip(move |_, cx| cx.new(|_| crate::git_panel::DynTooltip(tip.clone())).into())
        });
        let mid = s.start_day + (n as i64 - 1) / 2;
        panel()
            .child(section("Tokens by day", note))
            .child(
                div()
                    .flex()
                    .gap(px(14.))
                    .text_size(px(11.5))
                    .child(legend(in_color, "input"))
                    .child(legend(out_color, "output"))
                    .child(div().flex_1())
                    .child(faint(format!("peak {} in · {} out", us::fmt_tokens(max_in), us::fmt_tokens(max_out)))),
            )
            .child(div().flex().gap(px(gap)).children(columns))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(11.))
                    .child(faint(us::month_day(s.start_day)))
                    .when(n > 2, |d| d.child(faint(us::month_day(mid))))
                    .child(faint("today")),
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
        let note = if max == 0 { String::new() } else { format!("most active {} {}:00", NAMES[best.0], best.1) };
        let shade = |f: f32| if f <= 0. { track() } else { c(theme::ACCENT).opacity(0.12 + 0.88 * f) };
        let cell = move |v: u64| {
            let f = if max == 0 { 0. } else { (v as f32 / max as f32).sqrt() };
            div().flex_1().h(px(16.)).rounded(px(4.)).bg(shade(f))
        };
        panel()
            .child(section("When they work", note))
            .child(div().flex().flex_col().gap(px(3.)).children(heat.iter().enumerate().map(|(d, row)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .child(div().w(px(16.)).flex_none().text_size(px(11.)).text_color(c(theme::TEXT_FAINT)).child(DAYS[d]))
                    .children(row.iter().map(|&v| cell(v)))
            })))
            .child(
                div()
                    .flex()
                    .items_center()
                    .pl(px(19.))
                    .text_size(px(11.))
                    .text_color(c(theme::TEXT_FAINT))
                    .children([0, 6, 12, 18].map(|h| div().flex_1().child(format!("{h}:00"))))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(3.))
                            .child(div().mr(px(3.)).child("less"))
                            .children([0., 0.3, 0.6, 1.].map(|f| div().size(px(10.)).rounded(px(3.)).bg(shade(f))))
                            .child(div().ml(px(3.)).child("more")),
                    ),
            )
    }

    fn render_models(&self) -> gpui::Div {
        let s = &self.stats;
        let max = s.models.first().map(|m| m.input + m.output).unwrap_or(0).max(1);
        let num = |v: String| div().w(px(60.)).flex_none().text_right().child(v);
        let header = div()
            .flex()
            .text_size(px(11.))
            .text_color(c(theme::TEXT_FAINT))
            .child(div().w(px(190.)).flex_none().child("model"))
            .child(div().flex_1())
            .child(num("input".into()))
            .child(num("output".into()))
            .child(num("tasks".into()));
        let shown = 6;
        let rest = &s.models[s.models.len().min(shown)..];
        let row = |icon: AnyElement, name: String, share: f32, color: u32, input: u64, output: u64, tasks: usize, dim: bool| {
            div()
                .flex()
                .items_center()
                .h(px(30.))
                .text_size(px(12.5))
                .text_color(c(if dim { theme::TEXT_MUTED } else { theme::TEXT }))
                .child(div().w(px(190.)).flex_none().flex().items_center().gap(px(8.)).overflow_hidden().child(icon).child(div().truncate().child(name)))
                .child(meter(share, c(color)))
                .child(num(us::fmt_tokens(input)))
                .child(num(us::fmt_tokens(output)).text_color(c(theme::TEXT_MUTED)))
                .child(num(us::fmt_int(tasks as u64)).text_color(c(theme::TEXT_FAINT)))
        };
        let mut list = div().flex().flex_col();
        for m in s.models.iter().take(shown) {
            let color = provider_color(&s.providers, us::provider(&m.name));
            list = list.child(row(dot(color, 8.).into_any_element(), m.name.clone(), (m.input + m.output) as f32 / max as f32, color, m.input, m.output, m.tasks, false));
        }
        if !rest.is_empty() {
            let (i, o, t) = rest.iter().fold((0, 0, 0), |a, m| (a.0 + m.input, a.1 + m.output, a.2 + m.tasks));
            list = list.child(row(div().size(px(8.)).into_any_element(), format!("{} more", rest.len()), (i + o) as f32 / max as f32, theme::TEXT_FAINT, i, o, t, true));
        }
        let note = match s.models.len() {
            1 => "1 model".to_string(),
            n => format!("{n} models"),
        };
        panel()
            .child(section("Models", note))
            .child(
                div()
                    .flex()
                    .h(px(8.))
                    .gap(px(2.))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(track())
                    .children(s.providers.iter().map(|&(p, share)| div().h_full().w(relative(share.max(0.01))).bg(c(provider_color(&s.providers, p))))),
            )
            .child(div().flex().flex_wrap().gap(px(14.)).text_size(px(11.5)).children(s.providers.iter().map(|&(p, share)| {
                div().flex().items_center().gap(px(5.)).child(dot(provider_color(&s.providers, p), 7.)).child(muted(p)).child(faint(format!("{}%", (share * 100.).round() as u32)))
            })))
            .child(div().pt(px(4.)).child(header))
            .child(list)
            .when(s.models.is_empty(), |d| d.child(faint("No usage in this period.").text_size(px(13.))))
    }

    fn render_agents(&self, cx: &mut Context<Self>) -> gpui::Div {
        let s = &self.stats;
        let max = s.agents.first().map(|a| a.input + a.output).unwrap_or(0).max(1);
        let num = |v: String| div().w(px(60.)).flex_none().text_right().child(v);
        let header = div()
            .flex()
            .px(px(8.))
            .text_size(px(11.))
            .text_color(c(theme::TEXT_FAINT))
            .child(div().w(px(190.)).flex_none().child("agent"))
            .child(div().flex_1())
            .child(num("input".into()))
            .child(num("output".into()))
            .child(num("time".into()));
        let selected = self.agent;
        panel()
            .child(section("Agents", if selected.is_some() { "click again to see all" } else { "click one to filter" }))
            .child(header)
            .child(div().flex().flex_col().gap(px(2.)).children(s.agents.iter().map(|a| {
                let agent = a.agent;
                let on = selected.is_none_or(|x| x == agent);
                let share = (a.input + a.output) as f32 / max as f32;
                div()
                    .id(("usage-agent", agent as usize))
                    .flex()
                    .items_center()
                    .h(px(34.))
                    .px(px(8.))
                    .rounded(px(10.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .text_color(c(if on { theme::TEXT } else { theme::TEXT_FAINT }))
                    .when(selected == Some(agent), |d| d.glass_active())
                    .when(selected != Some(agent), |d| d.hover(|d| d.glass_hover()))
                    .child(div().w(px(190.)).flex_none().flex().items_center().gap(px(8.)).child(agent_icon(agent)).child(us::AGENT_NAMES.get(agent as usize).copied().unwrap_or("?")))
                    .child(meter(share, if on { c(theme::ACCENT) } else { c(theme::TEXT_FAINT) }))
                    .child(num(us::fmt_tokens(a.input)))
                    .child(num(us::fmt_tokens(a.output)).text_color(c(theme::TEXT_MUTED)))
                    .child(num(us::fmt_duration(a.active)).text_color(c(theme::TEXT_FAINT)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_agent(agent, cx)))
            })))
    }
}

/// Thin rounded share meter on a faint track.
fn meter(share: f32, color: Hsla) -> gpui::Div {
    div().flex_1().pr(px(14.)).child(div().h(px(4.)).rounded_full().bg(track()).child(div().h_full().rounded_full().w(relative(share.clamp(0.015, 1.))).bg(color)))
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
            // Faint brand glow at the top fading into the app background.
            .bg(gpui::linear_gradient(180., gpui::linear_color_stop(c(theme::BRAND_TINT), 0.), gpui::linear_color_stop(c(theme::APP_BG), 0.45)))
            .text_color(c(theme::TEXT))
            .font_family(theme::UI_FONT)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(18.))
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
                                    .gap(px(2.))
                                    .child(div().text_size(px(24.)).font_weight(FontWeight::BOLD).child("Agent usage"))
                                    .child(div().flex().gap(px(10.)).text_size(px(13.)).child(muted(subtitle)).children(status.map(|s| faint(s)))),
                            )
                            .child(
                                div().flex().items_center().gap(px(16.)).child(segmented(self, cx)).child(
                                    div()
                                        .id("usage-esc")
                                        .px(px(10.))
                                        .py(px(4.))
                                        .rounded_full()
                                        .glass_button()
                                        .text_size(px(12.))
                                        .text_color(c(theme::TEXT_MUTED))
                                        .cursor_pointer()
                                        .hover(|d| d.glass_hover().text_color(c(theme::TEXT)))
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
                    .child(div().flex().flex_wrap().gap(px(12.)).child(self.render_tokens_by_day()).child(self.render_heatmap()))
                    .child(div().flex().flex_wrap().gap(px(12.)).child(self.render_models()).child(self.render_agents(cx))),
            )
    }
}
