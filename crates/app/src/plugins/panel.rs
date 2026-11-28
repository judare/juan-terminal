//! Sidebar "Plugins" view: every plugin as a compact row with an on/off switch, plus "install
//! from git". A click opens the plugin's page (`detail.rs`): description, settings, update /
//! remove.

use gpui::{ClickEvent, Context, Entity, EventEmitter, Focusable, Render, SharedString, Subscription, Window, div, prelude::*, px};

use super::Manifest;
use crate::git_panel::{Tooltip, icon_action};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

pub enum PluginsPanelEvent {
    /// A plugin was switched on/off, reconfigured, installed, updated or removed.
    Changed,
    /// Open the plugin's page (details and settings) in a tab.
    Open(String),
}

struct InstallForm {
    url: Entity<TextInput>,
    _sub: Subscription,
}

pub struct PluginsPanel {
    plugins: Vec<Manifest>,
    install: Option<InstallForm>,
    busy: Option<SharedString>,
    /// (text, is_error)
    message: Option<(SharedString, bool)>,
}

impl EventEmitter<PluginsPanelEvent> for PluginsPanel {}

impl PluginsPanel {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { plugins: super::all(), install: None, busy: None, message: None }
    }

    /// Re-read manifests and state (after a change here or in another window).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        super::reload();
        self.plugins = super::all();
        cx.notify();
    }

    fn changed(&mut self, res: std::io::Result<()>, cx: &mut Context<Self>) {
        if let Err(e) = res {
            self.message = Some((format!("No se pudo guardar plugins.json: {e}").into(), true));
        }
        self.reload(cx);
        cx.emit(PluginsPanelEvent::Changed);
    }

    fn toggle(&mut self, id: String, cx: &mut Context<Self>) {
        let on = !super::wanted(&id);
        self.message = None;
        if !on && super::manifest(&id).is_some_and(|m| m.delayed_off) {
            self.message = Some(("Se desactiva en 24 h (sin desbloqueos por impulso). Vuelve a activarlo para cancelar.".into(), false));
        }
        let res = super::set_enabled(&id, on);
        self.changed(res, cx);
    }

    fn open_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.install.is_some() {
            self.install = None;
            cx.notify();
            return;
        }
        let url = cx.new(|cx| TextInput::new("https://github.com/usuario/mi-plugin.git", cx));
        let sub = cx.subscribe_in(&url, window, |this, _, e, _, cx| match e {
            TextInputEvent::Submit { .. } => this.install(cx),
            TextInputEvent::Cancel => {
                this.install = None;
                cx.notify();
            }
            TextInputEvent::Changed => {}
        });
        window.focus(&url.read(cx).focus_handle(cx));
        self.install = Some(InstallForm { url, _sub: sub });
        cx.notify();
    }

    /// Runs `job` off the UI thread with a busy line; its Ok text becomes the message.
    fn run(&mut self, busy: &str, cx: &mut Context<Self>, job: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(busy.to_string().into());
        self.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                this.message = Some(match res {
                    Ok(t) => (t.into(), false),
                    Err(e) => (format!("{e:#}").into(), true),
                });
                this.reload(cx);
                cx.emit(PluginsPanelEvent::Changed);
            });
        })
        .detach();
    }

    fn install(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &self.install else { return };
        let url = f.url.read(cx).content.trim().to_string();
        self.install = None;
        self.run("Instalando…", cx, move || super::install_from_git(&url).map(|m| format!("Instalado: {} {}", m.name, m.version)));
    }

    /// Compact row (icon, name, one-line description, switch); a click opens its page.
    fn render_plugin(&self, m: &Manifest, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = m.id.clone();
        let wanted = super::wanted(&id);
        let active = super::enabled(&id);
        let pending = super::pending_off(&id, None);
        let sub = match pending {
            Some(t) => format!("Se desactiva {}", when(t)),
            None => m.description.clone(),
        };
        div()
            .id(SharedString::from(format!("plg-row-{id}")))
            .flex()
            .items_center()
            .gap(px(10.))
            .mx(px(6.))
            .px(px(8.))
            .py(px(7.))
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
            .child(
                div()
                    .size(px(30.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .bg(theme::rgb(if active { theme::BRAND_TINT } else { theme::HOVER }))
                    .child(gpui::svg().path(m.icon).size(px(16.)).text_color(theme::rgb(if active { theme::ACCENT } else { theme::TEXT_FAINT }))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(if active { theme::TEXT } else { theme::TEXT_MUTED })).child(m.name.clone()))
                    .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).text_color(theme::rgb(if pending.is_some() { theme::MODIFIED } else { theme::TEXT_FAINT })).child(sub)),
            )
            .child(
                div()
                    .id(SharedString::from(format!("plg-on-{id}")))
                    .cursor_pointer()
                    .child(switch(wanted))
                    .tooltip(move |_, cx| cx.new(|_| Tooltip(if wanted { "Desactivar" } else { "Activar" })).into())
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.toggle(id.clone(), cx)
                        }
                    })),
            )
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(PluginsPanelEvent::Open(id.clone()))))
            .into_any_element()
    }
}

/// "en ~N h" / "en ~N min" until Unix time `t`.
pub(crate) fn when(t: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let left = (t - now).max(0);
    if left >= 3600 { format!("en ~{} h", (left + 3599) / 3600) } else { format!("en ~{} min", (left + 59) / 60) }
}

/// iOS-style on/off switch.
pub(crate) fn switch(on: bool) -> gpui::Div {
    div()
        .flex_none()
        .w(px(26.))
        .h(px(15.))
        .p(px(2.))
        .rounded_full()
        .flex()
        .when(on, |d| d.justify_end().bg(theme::rgb(0x2f7d4f)))
        .when(!on, |d| d.bg(theme::rgb(0x3a3d44)))
        .child(div().size(px(11.)).rounded_full().bg(theme::rgb(if on { 0xffffff } else { 0x9a9ea6 })))
}

impl Render for PluginsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().id("plugins-panel").flex().flex_col().size_full().overflow_y_scroll().pb(px(16.));
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(14.))
                .pr(px(8.))
                .h(px(30.))
                .child(div().text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child("PLUGINS"))
                .child(div().flex_1())
                .child(icon_action("plg-folder", "icons/finder.svg", "Carpeta de plugins externos").on_click(|_, _, _| {
                    let dir = super::plugins_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("/usr/bin/open").arg(dir).spawn();
                }))
                .child(icon_action("plg-reload", "icons/fetch.svg", "Volver a leer los plugins").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.reload(cx);
                    cx.emit(PluginsPanelEvent::Changed);
                })))
                .child(icon_action("plg-install", "icons/plus.svg", "Instalar plugin desde git").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.open_install(w, cx)))),
        );
        if let Some(f) = &self.install {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .mx(px(8.))
                    .mb(px(8.))
                    .p(px(10.))
                    .rounded(px(8.))
                    .bg(theme::rgb(0x202227))
                    .border_1()
                    .border_color(theme::rgb(0x33363d))
                    .child(div().text_size(px(12.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Instalar plugin"))
                    .child(div().px(px(8.)).h(px(26.)).flex().items_center().rounded(px(5.)).bg(theme::rgb(theme::EDITOR_BG)).border_1().border_color(theme::rgb(theme::BORDER)).child(f.url.clone()))
                    .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("URL git de un repositorio con plugin.json (ver docs/PLUGINS.md). ↩ para instalar.")),
            );
        }
        if let Some(b) = self.busy.clone() {
            list = list.child(div().px(px(14.)).pb(px(6.)).text_size(px(11.)).text_color(theme::rgb(theme::ACCENT)).child(b));
        }
        if let Some((m, err)) = self.message.clone() {
            list = list.child(div().px(px(14.)).pb(px(6.)).text_size(px(11.)).text_color(theme::rgb(if err { 0xf0b3b8 } else { theme::TEXT_MUTED })).child(m));
        }
        let plugins = self.plugins.clone();
        let (builtin, external): (Vec<_>, Vec<_>) = plugins.iter().partition(|m| m.builtin);
        list = list.child(section("INCLUIDOS"));
        for m in builtin {
            list = list.child(self.render_plugin(m, cx));
        }
        list = list.child(section("EXTERNOS"));
        if external.is_empty() {
            list = list.child(div().px(px(16.)).py(px(4.)).text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Ninguno. Instala uno con + (URL git) o copia su carpeta en la carpeta de plugins."));
        }
        for m in external {
            list = list.child(self.render_plugin(m, cx));
        }
        list
    }
}

fn section(label: &'static str) -> gpui::Div {
    div().px(px(14.)).pt(px(8.)).pb(px(6.)).text_size(px(10.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child(label)
}
