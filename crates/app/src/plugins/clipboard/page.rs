//! "Portapapeles" tab: the whole history with search; click copies, 📌 pins, 🗑 deletes.

use gpui::{ClickEvent, Context, Entity, FocusHandle, Focusable, Render, SharedString, Subscription, Window, div, prelude::*, px};

use super::history::{self, Clip, History};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

/// Entries rendered at most (the search narrows the rest).
const SHOWN: usize = 300;

pub struct ClipboardPage {
    history: Option<Entity<History>>,
    search: Entity<TextInput>,
    copied: Option<String>,
    _subs: Vec<Subscription>,
}

impl ClipboardPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Buscar en el portapapeles…", cx));
        let mut subs = vec![cx.subscribe_in(&search, window, |_, _, e, _, cx| {
            if let TextInputEvent::Changed = e {
                cx.notify();
            }
        })];
        let history = history::global(cx);
        if let Some(h) = &history {
            subs.push(cx.observe(h, |_, _, cx| cx.notify()));
        }
        Self { history, search, copied: None, _subs: subs }
    }

    fn row(&self, ix: usize, c: &Clip, cx: &mut Context<Self>) -> gpui::AnyElement {
        let preview: String = c.text.lines().filter(|l| !l.trim().is_empty()).take(3).map(|l| l.chars().take(220).collect::<String>()).collect::<Vec<_>>().join("\n");
        let lines = c.text.lines().count();
        let mut meta = vec![history::ago(c.at)];
        if !c.app.is_empty() {
            meta.push(c.app.clone());
        }
        if lines > 3 {
            meta.push(format!("{lines} líneas"));
        }
        meta.push(format!("{} caracteres", c.text.chars().count()));
        let copied = self.copied.as_deref() == Some(c.text.as_str());
        let (t1, t2, t3) = (c.text.clone(), c.text.clone(), c.text.clone());
        let h1 = self.history.clone();
        let h2 = self.history.clone();
        div()
            .id(("clip", ix))
            .group("clip-row")
            .flex()
            .gap(px(12.))
            .px(px(14.))
            .py(px(10.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .flex_1()
                    .min_w_0()
                    .child(div().font_family(theme::MONO_FONT).text_size(px(12.)).line_height(px(17.)).text_color(theme::rgb(theme::TEXT)).overflow_hidden().child(preview))
                    .child(div().text_size(px(10.5)).text_color(theme::rgb(if copied { theme::ADDED } else { theme::TEXT_FAINT })).child(if copied { "Copiado".to_string() } else { meta.join(" · ") })),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_start()
                    .gap(px(2.))
                    .child(
                        div()
                            .id(("clip-pin", ix))
                            .p(px(5.))
                            .rounded(px(5.))
                            .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                            .when(!c.pinned, |d| d.invisible().group_hover("clip-row", |s| s.visible()))
                            .child(gpui::svg().path(if c.pinned { "icons/bookmark-fill.svg" } else { "icons/bookmark.svg" }).size(px(13.)).text_color(theme::rgb(if c.pinned { theme::ACCENT } else { theme::TEXT_MUTED })))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if let Some(h) = &h1 {
                                    h.update(cx, |h, cx| h.toggle_pin(&t1, cx));
                                }
                            })),
                    )
                    .child(
                        div()
                            .id(("clip-del", ix))
                            .p(px(5.))
                            .rounded(px(5.))
                            .invisible()
                            .group_hover("clip-row", |s| s.visible())
                            .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                            .child(gpui::svg().path("icons/trash.svg").size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if let Some(h) = &h2 {
                                    h.update(cx, |h, cx| h.remove(&t2, cx));
                                }
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(t3.clone()));
                this.copied = Some(t3.clone());
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Focusable for ClipboardPage {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl Render for ClipboardPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let q = self.search.read(cx).content.trim().to_lowercase();
        let entries: Vec<Clip> = self.history.as_ref().map(|h| h.read(cx).entries.clone()).unwrap_or_default();
        let matches: Vec<&Clip> = entries.iter().filter(|c| q.is_empty() || c.text.to_lowercase().contains(&q) || c.app.to_lowercase().contains(&q)).collect();
        let (pinned, rest): (Vec<&Clip>, Vec<&Clip>) = matches.into_iter().partition(|c| c.pinned);
        let mut list = div().flex().flex_col().gap(px(2.));
        let header = |label: SharedString| div().px(px(14.)).pt(px(14.)).pb(px(4.)).text_size(px(11.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child(label);
        let mut ix = 0;
        if !pinned.is_empty() {
            list = list.child(header("FIJADOS".into()));
            for c in pinned {
                list = list.child(self.row(ix, c, cx));
                ix += 1;
            }
        }
        list = list.child(header(format!("HISTORIAL · {}", rest.len()).into()));
        if self.history.is_none() {
            list = list.child(div().px(px(14.)).text_size(px(13.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Activa el plugin Portapapeles para guardar lo que copias."));
        } else if rest.is_empty() {
            list = list.child(div().px(px(14.)).text_size(px(13.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(if q.is_empty() { "Todavía no has copiado nada." } else { "Nada coincide." }));
        }
        for c in rest.into_iter().take(SHOWN) {
            list = list.child(self.row(ix, c, cx));
            ix += 1;
        }
        div()
            .id("clipboard-page")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::rgb(theme::APP_BG))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(24.))
                    .py(px(14.))
                    .border_b_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .child(gpui::svg().path("icons/search.svg").size(px(14.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                    .child(div().flex_1().text_size(px(14.)).child(self.search.clone())),
            )
            .child(div().id("clip-scroll").flex_1().min_h_0().overflow_y_scroll().px(px(10.)).pb(px(24.)).child(list))
    }
}
