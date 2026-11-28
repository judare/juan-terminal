//! A plugin's page, opened in an editor tab from the Plugins list (like an extension page in
//! VS Code): header with icon, version and the on/off switch; a "Detalles" tab (the built-in
//! plugin's ABOUT text or an external plugin's README.md) and a "Configuración" tab (its
//! switches plus the plugin's own settings view: Google account, HTTP collections…).

use gpui::{
    AnyView, ClickEvent, Context, FocusHandle, Focusable, FontWeight, HighlightStyle, PromptLevel, Render, SharedString, StyledText, WeakEntity, Window, div,
    prelude::*, px,
};

use super::Manifest;
use super::panel::{switch, when};
use crate::theme;
use crate::workspace::Workspace;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Details,
    Settings,
}

pub struct PluginPage {
    m: Manifest,
    ws: WeakEntity<Workspace>,
    tab: Tab,
    /// The plugin's own settings view (Google account, HTTP collections…).
    config: Option<AnyView>,
    about: String,
    busy: Option<SharedString>,
    message: Option<(SharedString, bool)>,
    pub focus: FocusHandle,
}

impl PluginPage {
    pub fn new(m: Manifest, ws: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = super::config_view(&m.id, ws.clone(), window, cx);
        let about = about_text(&m);
        // Plugins with their own setup (Google: connect an account) open on that tab.
        let tab = if config.is_some() && super::needs_setup(&m.id) { Tab::Settings } else { Tab::Details };
        Self { m, ws, tab, config, about, busy: None, message: None, focus: cx.focus_handle() }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        super::reload();
        if let Some(m) = super::manifest(&self.m.id) {
            self.about = about_text(&m);
            self.m = m;
        }
        cx.notify();
    }

    /// Apply a change everywhere (players, services, other windows, the sidebar list).
    fn broadcast(&mut self, res: std::io::Result<()>, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = res {
            self.message = Some((format!("No se pudo guardar plugins.json: {e}").into(), true));
        }
        self.refresh(cx);
        if let Some(ws) = self.ws.upgrade() {
            ws.update(cx, |w, cx| w.plugins_changed_everywhere(window, cx));
        }
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.m.id.clone();
        let on = !super::wanted(&id);
        self.message = (!on && self.m.delayed_off).then(|| ("Se desactiva en 24 h (sin desbloqueos por impulso). Vuelve a activarlo para cancelar.".into(), false));
        let res = super::set_enabled(&id, on);
        self.broadcast(res, window, cx);
    }

    fn toggle_setting(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.m.id.clone();
        let on = !super::setting_wanted(&id, &key);
        self.message = (!on && self.m.delayed_off).then(|| ("Este cambio se aplica en 24 h. Vuelve a activarlo para cancelar.".into(), false));
        let res = super::set_setting(&id, &key, on);
        self.broadcast(res, window, cx);
    }

    fn run(&mut self, busy: &str, cx: &mut Context<Self>, job: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(busy.to_string().into());
        self.message = None;
        cx.notify();
        cx.spawn_in_window_free(job);
    }

    fn update_plugin(&mut self, cx: &mut Context<Self>) {
        let m = self.m.clone();
        self.run("Actualizando…", cx, move || {
            let summary = super::update(&m)?;
            let now = m.dir.as_deref().and_then(super::read_manifest).map(|n| n.version).unwrap_or_default();
            Ok(match (summary.is_empty(), now == m.version) {
                (_, false) => format!("{} actualizado a {now}", m.name),
                (true, true) => format!("{} ya está al día", m.name),
                (false, true) => format!("{}: {summary}", m.name),
            })
        });
    }

    fn uninstall(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let m = self.m.clone();
        let answer = window.prompt(PromptLevel::Warning, &format!("¿Quitar el plugin “{}”?", m.name), Some("Su carpeta va a la Papelera."), &["Quitar", "Cancelar"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let name = m.name.clone();
                this.run("Quitando…", cx, move || super::uninstall(&m).map(|_| format!("{name} quitado. Puedes cerrar esta pestaña.")));
            });
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let m = &self.m;
        let wanted = super::wanted(&m.id);
        let active = super::enabled(&m.id);
        let pending = super::pending_off(&m.id, None);
        let meta = {
            let mut parts = vec![m.id.clone()];
            if !m.version.is_empty() {
                parts.push(format!("v{}", m.version));
            }
            parts.push(if m.builtin { "Incluido con Fragua".to_string() } else if m.author.is_empty() { "Externo".to_string() } else { m.author.clone() });
            parts.join("  ·  ")
        };
        let status = match (active, pending) {
            (_, Some(t)) => (format!("Activo · se desactiva {}", when(t)), theme::MODIFIED),
            (true, None) => ("Activo".to_string(), theme::ADDED),
            (false, None) => ("Desactivado".to_string(), theme::TEXT_FAINT),
        };
        let mut actions = div().flex().items_center().gap(px(6.)).child(
            div()
                .id("pp-toggle")
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .h(px(28.))
                .rounded(px(6.))
                .cursor_pointer()
                .bg(theme::rgb(if wanted { theme::HOVER } else { theme::BRAND_DEEP }))
                .hover(|s| s.bg(theme::rgb(if wanted { theme::SELECTED } else { theme::BRAND_DEEP_HOVER })))
                .border_1()
                .border_color(theme::rgb(if wanted { theme::BORDER } else { theme::BRAND_EDGE }))
                .text_size(px(12.))
                .text_color(theme::rgb(theme::TEXT))
                .child(switch(wanted))
                .child(if wanted { "Desactivar" } else { "Activar" })
                .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.toggle(w, cx))),
        );
        if !m.builtin {
            let dir = m.dir.clone();
            actions = actions
                .child(button("pp-update", "icons/pull.svg", "Actualizar").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.update_plugin(cx))))
                .child(button("pp-folder", "icons/finder.svg", "Carpeta").on_click(move |_, _, _| {
                    if let Some(d) = &dir {
                        let _ = std::process::Command::new("/usr/bin/open").arg(d).spawn();
                    }
                }))
                .child(button("pp-remove", "icons/trash.svg", "Quitar").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.uninstall(w, cx))));
        }
        div()
            .flex()
            .gap(px(20.))
            .px(px(32.))
            .pt(px(28.))
            .pb(px(18.))
            .child(
                div()
                    .size(px(84.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(18.))
                    .bg(theme::rgb(if active { theme::BRAND_TINT } else { theme::HOVER }))
                    .border_1()
                    .border_color(theme::rgb(if active { theme::MARK_A } else { theme::BORDER }))
                    .child(gpui::svg().path(m.icon).size(px(42.)).text_color(theme::rgb(if active { theme::ACCENT } else { theme::TEXT_FAINT }))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(24.)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(m.name.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .text_size(px(12.))
                            .child(div().text_color(theme::rgb(theme::TEXT_FAINT)).child(meta))
                            .child(div().flex().items_center().gap(px(5.)).child(div().size(px(7.)).rounded_full().bg(theme::rgb(status.1))).child(div().text_color(theme::rgb(status.1)).child(status.0))),
                    )
                    .when(!m.description.is_empty(), |d| d.child(div().text_size(px(13.)).line_height(px(19.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(m.description.clone())))
                    .child(div().pt(px(6.)).child(actions)),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_settings = !self.m.settings.is_empty() || self.config.is_some() || self.m.id == super::WEBSITE_BLOCKER;
        let tab = |id: &'static str, label: &'static str, t: Tab, current: Tab| {
            let on = t == current;
            div()
                .id(id)
                .px(px(2.))
                .mr(px(18.))
                .h(px(32.))
                .flex()
                .items_center()
                .cursor_pointer()
                .border_b_2()
                .border_color(theme::rgb(if on { theme::ACCENT } else { theme::APP_BG }))
                .text_size(px(12.5))
                .text_color(theme::rgb(if on { theme::TEXT } else { theme::TEXT_MUTED }))
                .child(label)
        };
        div()
            .flex()
            .px(px(32.))
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .child(tab("pp-tab-details", "Detalles", Tab::Details, self.tab).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.tab = Tab::Details;
                cx.notify();
            })))
            .when(has_settings, |d| {
                d.child(tab("pp-tab-settings", "Configuración", Tab::Settings, self.tab).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.tab = Tab::Settings;
                    cx.notify();
                })))
            })
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = self.m.id.clone();
        let wanted = super::wanted(&id);
        let mut col = div().flex().flex_col().gap(px(18.));
        if !self.m.settings.is_empty() {
            let mut list = div().flex().flex_col().rounded(px(8.)).border_1().border_color(theme::rgb(theme::BORDER)).overflow_hidden();
            for (i, s) in self.m.settings.iter().enumerate() {
                if s.is_choice() {
                    list = list.child(self.render_choice(i, s, wanted, cx));
                    continue;
                }
                let on = super::setting_wanted(&id, &s.key);
                let pending = super::pending_off(&id, Some(&s.key));
                let key = s.key.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("pp-set-{}", s.key)))
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .px(px(14.))
                        .py(px(10.))
                        .when(i > 0, |d| d.border_t_1().border_color(theme::rgb(theme::BORDER)))
                        .when(wanted, |d| d.cursor_pointer().hover(|s| s.bg(theme::rgb(theme::HOVER))))
                        .when(!wanted, |d| d.opacity(0.5))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .flex_1()
                                .min_w_0()
                                .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).child(s.label.clone()))
                                .when(!s.help.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(s.help.clone())))
                                .when_some(pending, |d, t| d.child(div().text_size(px(11.)).text_color(theme::rgb(theme::MODIFIED)).child(format!("Se apaga {}", when(t))))),
                        )
                        .child(switch(on))
                        .when(wanted, |d| d.on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.toggle_setting(key.clone(), w, cx)))),
                );
            }
            col = col.child(section_title("Opciones")).child(list);
            if !wanted {
                col = col.child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Activa el plugin para cambiar sus opciones."));
            }
        }
        if id == super::WEBSITE_BLOCKER {
            let ws = self.ws.clone();
            col = col.child(section_title("Listas y horarios")).child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(button("pp-blocker-lists", "icons/edit.svg", "Editar en settings.toml").on_click(move |_, window, cx| {
                        if let Some(ws) = ws.upgrade() {
                            ws.update(cx, |w, cx| w.open_settings(&crate::workspace::OpenSettings, window, cx));
                        }
                    }))
                    .child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Sitios bloqueados, horarios y YouTube sin video.")),
            );
        }
        if let Some(v) = self.config.clone() {
            col = col.child(v);
        }
        col.into_any_element()
    }

    fn set_choice(&mut self, key: String, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let res = super::set_choice(&self.m.id, &key, &value);
        self.message = None;
        self.broadcast(res, window, cx);
    }

    /// A choice setting: label + help on the left, one pill per value on the right.
    fn render_choice(&self, i: usize, s: &super::SettingSpec, enabled: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let current = super::choice_value(&self.m.id, &s.key);
        let mut pills = div().flex().flex_none().items_center().p(px(2.)).gap(px(2.)).rounded(px(7.)).bg(theme::rgb(theme::HOVER)).border_1().border_color(theme::rgb(theme::BORDER));
        for c in &s.choices {
            let on = c.value == current;
            let (key, value) = (s.key.clone(), c.value.clone());
            pills = pills.child(
                div()
                    .id(SharedString::from(format!("pp-ch-{}-{}", s.key, c.value)))
                    .px(px(9.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .text_size(px(11.5))
                    .when(on, |d| d.bg(theme::rgb(theme::BRAND_DEEP)).text_color(theme::rgb(0xffffff)))
                    .when(!on, |d| d.text_color(theme::rgb(theme::TEXT_MUTED)))
                    .when(enabled && !on, |d| d.cursor_pointer().hover(|s| s.bg(theme::rgb(theme::SELECTED))))
                    .child(c.label.clone())
                    .when(enabled, |d| d.on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.set_choice(key.clone(), value.clone(), w, cx)))),
            );
        }
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .py(px(10.))
            .when(i > 0, |d| d.border_t_1().border_color(theme::rgb(theme::BORDER)))
            .when(!enabled, |d| d.opacity(0.5))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).child(s.label.clone()))
                    .when(!s.help.is_empty(), |d| d.child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(s.help.clone()))),
            )
            .child(pills)
            .into_any_element()
    }

    fn render_details(&self) -> gpui::AnyElement {
        let mut col = div().flex().flex_col().gap(px(8.)).max_w(px(760.));
        col = col.children(markdown(&self.about));
        let m = &self.m;
        if !m.commands.is_empty() {
            col = col.child(div().pt(px(10.)).child(section_title("Comandos (⌘⇧P)")));
            for c in &m.commands {
                col = col.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).child(format!("{}: {}", m.name, c.title)))
                        .child(div().font_family(theme::MONO_FONT).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(c.run.clone())),
                );
            }
        }
        if let Some(mcp) = &m.mcp {
            col = col
                .child(div().pt(px(10.)).child(section_title("Servidor MCP")))
                .child(div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("plugin-{} · se agrega a las sesiones nuevas de Claude", m.id)))
                .child(div().font_family(theme::MONO_FONT).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("{} {}", mcp.command, mcp.args.join(" "))));
        }
        col.into_any_element()
    }
}

/// Runs a plugin job off the UI thread and reports its result on the page.
trait SpawnJob {
    fn spawn_in_window_free(&mut self, job: impl FnOnce() -> anyhow::Result<String> + Send + 'static);
}

impl SpawnJob for Context<'_, PluginPage> {
    fn spawn_in_window_free(&mut self, job: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        self.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                this.message = Some(match res {
                    Ok(t) => (t.into(), false),
                    Err(e) => (format!("{e:#}").into(), true),
                });
                this.refresh(cx);
                // The sidebar list and the other windows pick it up on their next reload.
                if let Some(ws) = this.ws.upgrade() {
                    ws.update(cx, |w, cx| w.reload_plugins_panel(cx));
                }
            });
        })
        .detach();
    }
}

fn about_text(m: &Manifest) -> String {
    if m.builtin {
        return super::about(&m.id).to_string();
    }
    let readme = m.dir.as_ref().and_then(|d| ["README.md", "readme.md", "Readme.md"].iter().find_map(|n| std::fs::read_to_string(d.join(n)).ok()));
    readme.unwrap_or_else(|| if m.description.is_empty() { "Este plugin no tiene README.md.".into() } else { m.description.clone() })
}

pub(crate) fn section_title(label: &'static str) -> gpui::Div {
    div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child(label.to_uppercase())
}

/// Bordered button with an icon and a label.
pub(crate) fn button(id: &'static str, icon: &'static str, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .h(px(28.))
        .rounded(px(6.))
        .cursor_pointer()
        .border_1()
        .border_color(theme::rgb(theme::BORDER))
        .bg(theme::rgb(theme::HOVER))
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
        .text_size(px(12.))
        .text_color(theme::rgb(theme::TEXT))
        .child(gpui::svg().path(icon).size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)))
        .child(label.into())
}

/// Filled (primary) button.
pub(crate) fn primary_button(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .px(px(14.))
        .h(px(28.))
        .rounded(px(6.))
        .cursor_pointer()
        .bg(theme::rgb(theme::BRAND_DEEP))
        .hover(|s| s.bg(theme::rgb(theme::BRAND_DEEP_HOVER)))
        .border_1()
        .border_color(theme::rgb(theme::BRAND_EDGE))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::rgb(0xffffff))
        .child(label.into())
}

/// Minimal Markdown for plugin pages: headings, lists, code blocks, paragraphs, **bold** and
/// `code`.
pub(crate) fn markdown(text: &str) -> Vec<gpui::AnyElement> {
    let mut out = Vec::new();
    let mut code: Option<Vec<String>> = None;
    let mut para: Vec<String> = Vec::new();
    let flush = |para: &mut Vec<String>, out: &mut Vec<gpui::AnyElement>| {
        if !para.is_empty() {
            out.push(div().text_size(px(13.)).line_height(px(20.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(inline(&para.join(" "))).into_any_element());
            para.clear();
        }
    };
    for line in text.lines() {
        let t = line.trim_end();
        if t.trim_start().starts_with("```") {
            match code.take() {
                Some(lines) => out.push(
                    div()
                        .p(px(10.))
                        .rounded(px(6.))
                        .bg(theme::rgb(theme::HOVER))
                        .font_family(theme::MONO_FONT)
                        .text_size(px(11.5))
                        .line_height(px(17.))
                        .text_color(theme::rgb(theme::TEXT))
                        .children(lines.into_iter().map(|l| div().whitespace_nowrap().child(if l.is_empty() { " ".to_string() } else { l })))
                        .into_any_element(),
                ),
                None => {
                    flush(&mut para, &mut out);
                    code = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(c) = &mut code {
            c.push(t.to_string());
            continue;
        }
        let s = t.trim_start();
        if s.is_empty() {
            flush(&mut para, &mut out);
            continue;
        }
        let heading = [("### ", 14.), ("## ", 16.), ("# ", 19.)].iter().find_map(|(p, size)| s.strip_prefix(p).map(|h| (h, *size)));
        if let Some((h, size)) = heading {
            flush(&mut para, &mut out);
            out.push(div().pt(px(10.)).text_size(px(size)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(inline(h)).into_any_element());
            continue;
        }
        let bullet = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")).map(|b| ("•".to_string(), b)).or_else(|| {
            let (n, rest) = s.split_once(". ")?;
            n.chars().all(|c| c.is_ascii_digit()).then(|| (format!("{n}."), rest))
        });
        if let Some((mark, b)) = bullet {
            flush(&mut para, &mut out);
            let indent = (t.len() - s.len()) as f32;
            out.push(
                div()
                    .flex()
                    .gap(px(8.))
                    .pl(px(4. + indent * 6.))
                    .text_size(px(13.))
                    .line_height(px(20.))
                    .text_color(theme::rgb(theme::TEXT_MUTED))
                    .child(div().flex_none().min_w(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(mark))
                    .child(div().flex_1().min_w_0().child(inline(b)))
                    .into_any_element(),
            );
            continue;
        }
        para.push(s.to_string());
    }
    flush(&mut para, &mut out);
    out
}

/// `code` and **bold** spans.
fn inline(text: &str) -> StyledText {
    let mut plain = String::new();
    let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next_code = rest.find('`');
        let next_bold = rest.find("**");
        let (ix, is_code) = match (next_code, next_bold) {
            (Some(c), Some(b)) if c < b => (c, true),
            (Some(c), None) => (c, true),
            (_, Some(b)) => (b, false),
            (None, None) => {
                plain.push_str(rest);
                break;
            }
        };
        let delim = if is_code { "`" } else { "**" };
        let after = &rest[ix + delim.len()..];
        let Some(end) = after.find(delim) else {
            plain.push_str(rest);
            break;
        };
        plain.push_str(&rest[..ix]);
        let start = plain.len();
        plain.push_str(&after[..end]);
        let style = if is_code {
            HighlightStyle { color: Some(theme::rgb(theme::MODIFIED)), ..Default::default() }
        } else {
            HighlightStyle { font_weight: Some(FontWeight::SEMIBOLD), color: Some(theme::rgb(theme::TEXT)), ..Default::default() }
        };
        highlights.push((start..plain.len(), style));
        rest = &after[end + delim.len()..];
    }
    StyledText::new(SharedString::from(plain)).with_highlights(highlights)
}

impl Focusable for PluginPage {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PluginPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            Tab::Details => self.render_details(),
            Tab::Settings => self.render_settings(cx),
        };
        div()
            .id("plugin-page")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .bg(theme::rgb(theme::APP_BG))
            .child(self.render_header(cx))
            .when_some(self.busy.clone(), |d, b| d.child(div().px(px(32.)).pb(px(8.)).text_size(px(12.)).text_color(theme::rgb(theme::ACCENT)).child(b)))
            .when_some(self.message.clone(), |d, (m, err)| {
                d.child(div().px(px(32.)).pb(px(8.)).text_size(px(12.)).text_color(theme::rgb(if err { 0xf0b3b8 } else { theme::TEXT_MUTED })).child(m))
            })
            .child(self.render_tabs(cx))
            .child(div().px(px(32.)).py(px(22.)).pb(px(40.)).child(body))
    }
}
