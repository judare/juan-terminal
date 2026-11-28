//! "Agenda y correo" tab: today's and tomorrow's events (join the call in one click) next to
//! the unread inbox (click opens the thread in Gmail).

use chrono::{Datelike, Local, TimeZone};
use gpui::{ClickEvent, Context, FocusHandle, Focusable, FontWeight, Render, WeakEntity, Window, div, prelude::*, px};

use super::sync::{self, Event, Mail};
use super::auth;
use crate::plugins::detail::{button, primary_button};
use crate::theme;
use crate::workspace::Workspace;

pub struct AgendaPage {
    ws: WeakEntity<Workspace>,
    pub focus: FocusHandle,
}

impl AgendaPage {
    pub fn new(ws: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        if let Some(g) = sync::global(cx) {
            cx.observe(&g, |_, _, cx| cx.notify()).detach();
            g.update(cx, |g, cx| {
                // Fresh data when the tab opens (if the last sync is over a minute old).
                if g.last_sync.is_none_or(|t| sync::now() - t > 60) {
                    g.sync(cx);
                }
            });
        }
        // Repaint the "en N min" labels.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(30)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self { ws, focus: cx.focus_handle() }
    }
}

const DAYS: [&str; 7] = ["lunes", "martes", "miércoles", "jueves", "viernes", "sábado", "domingo"];
const MONTHS: [&str; 12] = ["enero", "febrero", "marzo", "abril", "mayo", "junio", "julio", "agosto", "septiembre", "octubre", "noviembre", "diciembre"];

fn day_label(t: i64) -> String {
    let Some(d) = Local.timestamp_opt(t, 0).single() else { return String::new() };
    format!("{} {} de {}", DAYS[d.weekday().num_days_from_monday() as usize], d.day(), MONTHS[d.month0() as usize])
}

fn event_row(ix: usize, e: &Event) -> gpui::AnyElement {
    let t = sync::now();
    let now = e.is_now(t);
    let past = e.end <= t;
    let time = if e.all_day { "Todo el día".to_string() } else { format!("{} – {}", sync::fmt_time(e.start), sync::fmt_time(e.end)) };
    let link = e.link.clone();
    let meeting = e.meeting.clone();
    let soon = !e.all_day && !past && e.start - t < 3600;
    div()
        .id(("ev", ix))
        .flex()
        .gap(px(10.))
        .p(px(10.))
        .rounded(px(8.))
        .cursor_pointer()
        .when(now, |d| d.bg(theme::rgb(theme::BRAND_TINT)))
        .when(!now, |d| d.hover(|s| s.bg(theme::rgb(theme::HOVER))))
        .when(past, |d| d.opacity(0.45))
        .child(div().w(px(3.)).flex_none().rounded(px(2.)).bg(theme::rgb(e.color)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(e.title.clone()))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .text_size(px(11.5))
                        .text_color(theme::rgb(theme::TEXT_FAINT))
                        .child(time)
                        .when(soon || now, |d| d.child(div().text_color(theme::rgb(theme::ACCENT)).child(sync::fmt_until(e.start, e.end))))
                        .when(!e.calendar.is_empty(), |d| d.child(e.calendar.clone())),
                )
                .when(!e.location.is_empty() && e.meeting.as_deref() != Some(e.location.as_str()), |d| d.child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_MUTED)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(e.location.clone()))),
        )
        .when_some(meeting.filter(|_| !past), |d, url| {
            d.child(primary_button(("ev-join", ix), "Unirse").on_click(move |_, _, cx| {
                cx.stop_propagation();
                sync::open(&url);
            }))
        })
        .on_click(move |_, _, _| {
            if !link.is_empty() {
                sync::open(&link);
            }
        })
        .into_any_element()
}

fn mail_row(ix: usize, m: &Mail) -> gpui::AnyElement {
    let url = sync::mail_url(&m.thread);
    div()
        .id(("mail", ix))
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(10.))
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::rgb(theme::HOVER)))
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(m.from.clone()))
                .child(div().flex_none().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(crate::plugins::clipboard::history::ago(m.at))),
        )
        .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(if m.subject.is_empty() { "(sin asunto)".to_string() } else { m.subject.clone() }))
        .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(m.snippet.clone()))
        .on_click(move |_, _, _| sync::open(&url))
        .into_any_element()
}

fn column_title(icon: &'static str, title: String, extra: Option<String>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(10.))
        .pb(px(6.))
        .child(gpui::svg().path(icon).size(px(14.)).text_color(theme::rgb(theme::TEXT_MUTED)))
        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child(title.to_uppercase()))
        .when_some(extra, |d, e| d.child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(e)))
}

impl Focusable for AgendaPage {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AgendaPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div().id("agenda-page").track_focus(&self.focus).size_full().flex().flex_col().bg(theme::rgb(theme::APP_BG));
        let g = sync::global(cx);
        if !crate::plugins::enabled(crate::plugins::GOOGLE) || !auth::connected() || g.is_none() {
            let ws = self.ws.clone();
            return root.items_center().justify_center().gap(px(10.)).child(gpui::svg().path("icons/calendar.svg").size(px(40.)).text_color(theme::rgb(theme::TEXT_FAINT))).child(div().text_size(px(15.)).child("Conecta tu cuenta de Google")).child(div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Agenda de hoy, link de Meet en un clic y correos sin leer.")).child(
                primary_button("agenda-setup", "Configurar").on_click(move |_, window, cx| {
                    if let Some(ws) = ws.upgrade() {
                        ws.update(cx, |w, cx| w.open_plugin_page(crate::plugins::GOOGLE, window, cx));
                    }
                }),
            );
        }
        let g = g.expect("checked");
        let (events, mails, unread, syncing, error) = {
            let g = g.read(cx);
            (g.events.clone(), g.mails.clone(), g.unread, g.syncing, g.error.clone())
        };
        let today_end = sync::end_of_today();
        let (today, tomorrow): (Vec<&Event>, Vec<&Event>) = events.iter().partition(|e| e.start < today_end);
        let mut agenda = div().flex().flex_col().gap(px(2.)).child(column_title("icons/calendar.svg", format!("Hoy · {}", day_label(sync::now())), None));
        if today.is_empty() {
            agenda = agenda.child(div().px(px(10.)).py(px(6.)).text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Nada más hoy."));
        }
        let mut ix = 0;
        for e in today {
            agenda = agenda.child(event_row(ix, e));
            ix += 1;
        }
        agenda = agenda.child(div().pt(px(16.)).child(column_title("icons/calendar.svg", format!("Mañana · {}", day_label(sync::now() + 86_400)), None)));
        if tomorrow.is_empty() {
            agenda = agenda.child(div().px(px(10.)).py(px(6.)).text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Sin eventos."));
        }
        for e in tomorrow {
            agenda = agenda.child(event_row(ix, e));
            ix += 1;
        }
        let mut inbox = div().flex().flex_col().gap(px(2.)).child(column_title("icons/mail.svg", "Sin leer".into(), Some(unread.to_string())));
        if mails.is_empty() {
            inbox = inbox.child(div().px(px(10.)).py(px(6.)).text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Bandeja al día."));
        }
        for (i, m) in mails.iter().enumerate() {
            inbox = inbox.child(mail_row(i, m));
        }
        let g2 = g.clone();
        root.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(24.))
                .py(px(12.))
                .border_b_1()
                .border_color(theme::rgb(theme::BORDER))
                .child(div().flex_1().text_size(px(12.)).text_color(theme::rgb(if error.is_some() { theme::MODIFIED } else { theme::TEXT_FAINT })).child(error.unwrap_or_else(|| auth::email().unwrap_or_default())))
                .child(button("agenda-cal", "icons/external.svg", "Calendar").on_click(|_, _, _| sync::open("https://calendar.google.com")))
                .child(button("agenda-mail", "icons/external.svg", "Gmail").on_click(|_, _, _| sync::open(&sync::mail_url(""))))
                .child(button("agenda-sync", "icons/fetch.svg", if syncing { "Sincronizando…" } else { "Actualizar" }).on_click(cx.listener(move |_, _: &ClickEvent, _, cx| g2.update(cx, |g, cx| g.sync(cx))))),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .gap(px(16.))
                .p(px(16.))
                .child(div().id("agenda-col").flex_1().min_w_0().overflow_y_scroll().child(agenda))
                .child(div().w(px(1.)).bg(theme::rgb(theme::BORDER)))
                .child(div().id("inbox-col").flex_1().min_w_0().overflow_y_scroll().child(inbox)),
        )
    }
}
