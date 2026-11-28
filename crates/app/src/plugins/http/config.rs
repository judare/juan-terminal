//! HTTP plugin section of its page: import from Postman, the collections and environments
//! stored, open the client.

use std::path::Path;

use gpui::{App, ClickEvent, Context, Entity, PathPromptOptions, Render, SharedString, WeakEntity, Window, div, prelude::*, px};

use super::store;
use crate::plugins::detail::{button, primary_button, section_title};
use crate::theme;
use crate::workspace::Workspace;

pub struct HttpConfig {
    ws: WeakEntity<Workspace>,
}

impl HttpConfig {
    pub fn new(ws: WeakEntity<Workspace>, _: &mut Context<Self>) -> Self {
        Self { ws }
    }
}

/// Move a stored collection / environment to the Trash.
pub fn trash(path: &Path) -> anyhow::Result<()> {
    let script = format!("tell application \"Finder\" to delete POSIX file \"{}\"", path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""));
    let out = std::process::Command::new("osascript").args(["-e", &script]).output()?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

/// File picker for Postman exports; imports them through the workspace (which also opens the
/// HTTP tab).
pub fn pick_and_import(ws: Entity<Workspace>, cx: &mut App) {
    let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Importar".into()) });
    let ws = ws.downgrade();
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = rx.await else { return };
        // Imported on the workspace's next render (it needs its window to open the tab).
        let _ = ws.update(cx, |w, cx| w.queue_import(paths, cx));
    })
    .detach();
}

impl Render for HttpConfig {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let collections = store::load_collections();
        let envs = store::load_environments();
        let active = store::active_environment();
        let ws = self.ws.clone();
        let ws2 = self.ws.clone();
        let row = |id: SharedString, icon: &'static str, title: String, sub: String, file: std::path::PathBuf, cx: &mut Context<Self>| {
            let f2 = file.clone();
            let ws = self.ws.clone();
            div()
                .id(id)
                .group("http-cfg-row")
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(8.))
                .border_t_1()
                .border_color(theme::rgb(theme::BORDER))
                .child(gpui::svg().path(icon).size(px(14.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                .child(div().flex_1().min_w_0().flex().flex_col().child(div().text_size(px(13.)).child(title)).child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(sub)))
                .child(
                    crate::git_panel::icon_action(SharedString::from(format!("{}-edit", file.display())), "icons/edit.svg", "Abrir el JSON en el editor").on_click(move |_, window, cx| {
                        if let Some(ws) = ws.upgrade() {
                            ws.update(cx, |w, cx| w.open_file(&f2, true, window, cx));
                        }
                    }),
                )
                .child(crate::git_panel::icon_action(SharedString::from(format!("{}-del", file.display())), "icons/trash.svg", "Borrar (va a la Papelera)").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    let _ = trash(&file);
                    cx.notify();
                })))
        };
        let mut coll_list = div().flex().flex_col().rounded(px(8.)).border_1().border_color(theme::rgb(theme::BORDER)).overflow_hidden().child(
            div().px(px(12.)).py(px(8.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(if collections.is_empty() { "Ninguna colección todavía.".to_string() } else { format!("{} colecciones", collections.len()) }),
        );
        for c in &collections {
            let sub = format!("{} requests · {}", c.request_count(), c.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            coll_list = coll_list.child(row(SharedString::from(format!("hc-{}", c.file.display())), "icons/folder.svg", c.name(), sub, c.file.clone(), cx));
        }
        let mut env_list = div().flex().flex_col().rounded(px(8.)).border_1().border_color(theme::rgb(theme::BORDER)).overflow_hidden().child(
            div().px(px(12.)).py(px(8.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(if envs.is_empty() { "Ningún entorno todavía.".to_string() } else { format!("{} entornos", envs.len()) }),
        );
        for e in &envs {
            let on = active.as_deref() == Some(e.id().as_str());
            let sub = format!("{} variables{}", e.variables().len(), if on { " · activo" } else { "" });
            env_list = env_list.child(row(SharedString::from(format!("he-{}", e.file.display())), "icons/key.svg", e.name(), sub, e.file.clone(), cx));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(section_title("Postman"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(primary_button("http-open-client", "Abrir cliente HTTP").on_click(move |_, window, cx| {
                        if let Some(ws) = ws.upgrade() {
                            ws.update(cx, |w, cx| w.open_http(window, cx));
                        }
                    }))
                    .child(button("http-cfg-import", "icons/pull.svg", "Importar de Postman…").on_click(move |_, _, cx| {
                        if let Some(ws) = ws2.upgrade() {
                            pick_and_import(ws, cx);
                        }
                    }))
                    .child(button("http-cfg-folder", "icons/finder.svg", "Carpeta").on_click(|_, _, _| {
                        let d = store::dir();
                        let _ = std::fs::create_dir_all(&d);
                        let _ = std::process::Command::new("/usr/bin/open").arg(d).spawn();
                    })),
            )
            .child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("También puedes arrastrar archivos .postman_collection.json o .postman_environment.json a la ventana."))
            .child(div().pt(px(6.)).child(section_title("Colecciones")))
            .child(coll_list)
            .child(div().pt(px(6.)).child(section_title("Entornos")))
            .child(env_list)
    }
}
