//! Plugin "Portapapeles": history of copied text, searchable from the launcher (`clip …`) and
//! from its page; pinned entries work as snippets. Picking an entry pastes it into the app in
//! front (needs the Accessibility permission; otherwise it's just copied).

pub mod history;
pub mod page;

use gpui::{ClickEvent, Context, Render, Window, div, prelude::*, px};

use super::detail::{button, section_title};
use super::{Manifest, builtin, choice, spec};
use crate::theme;

pub const ABOUT: &str = r#"## Qué hace
Guarda todo el texto que copias para que lo vuelvas a pegar cuando quieras.

- **⌃⌘V** (desde cualquier app) o `clip` en el launcher (⌘Space): busca en el historial y
  **Enter** lo pega en la app donde estabas.
- **Fijados**: textos que usas seguido (firmas, comandos, direcciones…). Nunca se borran y
  salen primero.
- La pestaña **Portapapeles** (⌘⇧P › "Portapapeles") muestra el historial completo: copiar,
  fijar o borrar entradas.
- Lo que copian los gestores de contraseñas (1Password, Bitwarden…) no se guarda: esas apps
  marcan sus copias como ocultas.
- Todo queda en este Mac (`clipboard.json`), nada sale a internet.

## Pegar directo
Para pegar en la otra app, macOS pide el permiso de **Accesibilidad** para Fragua. Sin él, la
entrada se copia y la pegas tú con ⌘V."#;

pub fn manifest() -> Manifest {
    builtin(
        super::CLIPBOARD,
        "Portapapeles",
        "icons/clipboard.svg",
        "Historial de lo que copias, con búsqueda en el launcher (⌃⌘V o `clip`), fijados para textos frecuentes y pegado directo.",
        vec![
            spec("global_hotkey", "Atajo global ⌃⌘V", "Abre el historial en el launcher desde cualquier app"),
            spec("paste_on_pick", "Pegar al elegir", "Pega en la app de adelante (si no, solo copia)"),
            choice("history_size", "Entradas guardadas", "Los fijados no cuentan", &[("100", "100"), ("500", "500"), ("2000", "2000")], "500"),
            choice("history_days", "Borrar entradas de más de", "Los fijados nunca se borran", &[("1", "1 día"), ("7", "7 días"), ("30", "30 días"), ("never", "Nunca")], "30"),
        ],
    )
}

/// Page section: Accessibility status, entry count, clear history.
pub struct ClipboardConfig;

impl ClipboardConfig {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Some(h) = history::global(cx) {
            cx.observe(&h, |_, _, cx| cx.notify()).detach();
        }
        Self
    }
}

impl Render for ClipboardConfig {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let trusted = history::accessibility_trusted();
        let h = history::global(cx);
        let (total, pinned) = h.as_ref().map(|h| {
            let e = &h.read(cx).entries;
            (e.len(), e.iter().filter(|c| c.pinned).count())
        }).unwrap_or((0, 0));
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title("Pegar en otras apps"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().size(px(8.)).rounded_full().bg(theme::rgb(if trusted { theme::ADDED } else { theme::MODIFIED })))
                    .child(div().flex_1().text_size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(if trusted {
                        "Permiso de Accesibilidad concedido: Enter pega directo."
                    } else {
                        "Sin permiso de Accesibilidad: al elegir se copia y pegas con ⌘V."
                    }))
                    .when(!trusted, |d| d.child(button("clip-ax", "icons/lock.svg", "Dar permiso").on_click(|_, _, _| history::open_accessibility_settings()))),
            )
            .child(div().pt(px(8.)).child(section_title("Historial")))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{total} entradas · {pinned} fijadas")))
                    .when_some(h, |d, h| {
                        d.child(button("clip-clear", "icons/trash.svg", "Borrar historial").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| h.update(cx, |h, cx| h.clear(cx)))))
                    }),
            )
    }
}
