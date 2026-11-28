//! Plugin "Google Calendar y Gmail": today's agenda with one-click Meet / Zoom links, meeting
//! reminders, unread mail, a "next meeting" card in the sidebar and launcher results.
//! Read-only, through the account connected with OAuth (`auth.rs`); data in `sync.rs`.

pub mod auth;
pub mod config;
pub mod page;
pub mod sync;

use gpui::{App, FontWeight, div, prelude::*, px};

use super::{GOOGLE, Manifest, builtin, choice, spec, spec_off};
use crate::theme;

pub const ABOUT: &str = r#"## Qué hace
Tu día de Google sin abrir el navegador.

- **Agenda** de hoy y mañana con todos los calendarios que ves en Google Calendar.
- **Unirse** abre el link de Meet, Zoom o Teams de la reunión en un clic.
- **Aviso** de macOS unos minutos antes de cada reunión.
- **Próxima reunión** al pie de la barra lateral cuando falta menos de una hora.
- **Correos sin leer** de la bandeja de entrada; un clic abre el hilo en Gmail.
- En el launcher (⌘Space): `meet` para unirte a la reunión actual o la siguiente, `agenda`
  para ver el día y `mail` para los correos sin leer.

## Privacidad
Solo lectura: Fragua no envía correos ni cambia eventos. El token queda en el Llavero de
macOS y los datos se consultan directo a Google cada 2 minutos.

## Conectar
Necesita un **OAuth Client ID** de tipo *App de escritorio* de Google Cloud (se crea una vez).
Los pasos están en **Configuración**."#;

pub fn manifest() -> Manifest {
    builtin(
        GOOGLE,
        "Google Calendar y Gmail",
        "icons/calendar.svg",
        "Agenda del día con link de Meet en un clic, avisos antes de cada reunión y correos sin leer, en la app y en el launcher.",
        vec![
            choice("notify_minutes", "Aviso antes de las reuniones", "Notificación de macOS", &[("off", "No avisar"), ("1", "1 min"), ("5", "5 min"), ("10", "10 min"), ("15", "15 min")], "5"),
            spec("sidebar_card", "Próxima reunión en la barra lateral", "Cuando falta menos de una hora, con botón para unirse"),
            spec("launcher_results", "Agenda y correo en el launcher", "meet, agenda, mail en ⌘Space"),
            spec_off("mail_notifications", "Notificar correos nuevos", "Un aviso por cada correo sin leer que llega a la bandeja"),
        ],
    )
}

/// Start syncing when the plugin is on (called at launch and when it's switched on).
pub fn init_if_enabled(cx: &mut App) {
    if super::enabled(GOOGLE) {
        sync::init(cx);
    }
}

/// Sidebar card for the meeting happening now or starting within the hour.
pub fn sidebar_card(cx: &App) -> Option<gpui::AnyElement> {
    if !super::setting(GOOGLE, "sidebar_card") {
        return None;
    }
    let g = sync::global(cx)?;
    let e = g.read(cx).next_meeting()?.clone();
    let t = sync::now();
    if e.start - t > 3600 {
        return None;
    }
    let meeting = e.meeting.clone();
    let link = e.link.clone();
    Some(
        div()
            .id("next-meeting")
            .flex()
            .items_center()
            .gap(px(10.))
            .mx(px(8.))
            .mb(px(8.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(12.))
            .bg(theme::rgb(theme::HOVER))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .cursor_pointer()
            .child(div().w(px(3.)).h(px(30.)).flex_none().rounded(px(2.)).bg(theme::rgb(e.color)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(e.title.clone()))
                    .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(theme::rgb(if e.is_now(t) || e.start - t < 600 { theme::ACCENT } else { theme::TEXT_FAINT })).child(format!("{} · {}", sync::fmt_time(e.start), sync::fmt_until(e.start, e.end)))),
            )
            .when_some(meeting, |d, url| {
                d.child(crate::plugins::detail::primary_button("next-meeting-join", "Unirse").h(px(24.)).px(px(10.)).on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    sync::open(&url);
                }))
            })
            .on_click(move |_, _, _| {
                if !link.is_empty() {
                    sync::open(&link);
                }
            })
            .into_any_element(),
    )
}
