//! Sidebar "Tools" mode: Oxlint/Oxfmt per repository with a problems list, and the Obscura
//! browser integration for Claude Code. Everything runs on demand, off the UI thread.

use std::path::PathBuf;

use code_quality::{Diagnostic, Located, Severity, Tool};
use gpui::{ClickEvent, Context, EventEmitter, PromptLevel, Render, SharedString, Window, div, prelude::*, px};

use crate::git_panel::icon_action;
use crate::obscura;
use crate::theme;

pub enum ToolsPanelEvent {
    OpenAt { path: PathBuf, line: usize, column: usize },
    /// Files were formatted on disk; open editors should reload.
    FilesChanged,
}

struct RepoTools {
    name: String,
    path: PathBuf,
    oxlint: Option<Located>,
    oxfmt: Option<Located>,
    diagnostics: Vec<Diagnostic>,
    linted: bool,
    busy: Option<&'static str>,
    message: Option<(String, bool)>,
}

#[derive(Default)]
struct ObscuraState {
    bin: Option<PathBuf>,
    version: Option<String>,
    registered: Option<bool>,
    busy: Option<&'static str>,
    message: Option<(String, bool)>,
    checked: bool,
}

pub struct ToolsPanel {
    repos: Vec<RepoTools>,
    obscura: ObscuraState,
    /// Running language servers ("tsgo · frontend").
    servers: Vec<String>,
    tsgo: Option<PathBuf>,
    tsgo_busy: bool,
    tsgo_message: Option<(String, bool)>,
    mcp_tools_open: bool,
}

impl EventEmitter<ToolsPanelEvent> for ToolsPanel {}

impl ToolsPanel {
    pub fn new() -> Self {
        Self {
            repos: Vec::new(),
            obscura: ObscuraState::default(),
            servers: Vec::new(),
            tsgo: crate::language_servers::locate_tsgo(std::path::Path::new("/nonexistent"), std::path::Path::new("/nonexistent")),
            tsgo_busy: false,
            tsgo_message: None,
            mcp_tools_open: false,
        }
    }

    /// Detect tools for the active project's repositories (cheap: file checks + `--version`).
    pub fn set_repos(&mut self, repos: Vec<(String, PathBuf)>, cx: &mut Context<Self>) {
        if repos.len() == self.repos.len() && repos.iter().zip(&self.repos).all(|((_, p), r)| *p == r.path) {
            if !self.obscura.checked {
                self.check_obscura(cx);
            }
            return;
        }
        self.repos = repos
            .iter()
            .map(|(name, path)| RepoTools { name: name.clone(), path: path.clone(), oxlint: None, oxfmt: None, diagnostics: Vec::new(), linted: false, busy: Some("Detecting"), message: None })
            .collect();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    repos
                        .into_iter()
                        .map(|(_, p)| (code_quality::locate(Tool::Oxlint, &p), code_quality::locate(Tool::Oxfmt, &p), p))
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (lint, fmt, path) in found {
                    if let Some(r) = this.repos.iter_mut().find(|r| r.path == path) {
                        r.oxlint = lint;
                        r.oxfmt = fmt;
                        r.busy = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        self.check_obscura(cx);
        cx.notify();
    }

    pub fn set_servers(&mut self, servers: Vec<String>, cx: &mut Context<Self>) {
        self.servers = servers;
        cx.notify();
    }

    fn confirm_install_tsgo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Info,
            "Install TypeScript Go (tsgo)?",
            Some(&format!("Runs npm install @typescript/native-preview into\n{} (≈10 MB download). Your repositories are not modified.", crate::language_servers::app_tools_dir().join("tsgo").display())),
            &["Install", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.tsgo_busy = true;
                cx.notify();
                cx.spawn(async move |this, cx| {
                    let r = cx.background_executor().spawn(async { crate::language_servers::install_tsgo() }).await;
                    let _ = this.update(cx, |this, cx| {
                        this.tsgo_busy = false;
                        match r {
                            Ok(p) => {
                                this.tsgo_message = Some(("Installed. Reopen TypeScript files to start the server.".into(), false));
                                this.tsgo = Some(p);
                            }
                            Err(e) => this.tsgo_message = Some((format!("{e:#}"), true)),
                        }
                        cx.notify();
                    });
                })
                .detach();
            });
        })
        .detach();
    }

    pub fn total_problems(&self) -> usize {
        self.repos.iter().map(|r| r.diagnostics.len()).sum()
    }

    /// Lint every repository that has oxlint.
    pub fn lint_all(&mut self, cx: &mut Context<Self>) {
        for ix in 0..self.repos.len() {
            self.lint(ix, cx);
        }
    }

    fn lint(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(r) = self.repos.get_mut(ix) else { return };
        let Some(tool) = r.oxlint.clone() else {
            r.message = Some(("oxlint not found. Add it to the repo (`pnpm add -D oxlint`) or install it globally.".into(), true));
            cx.notify();
            return;
        };
        r.busy = Some("Linting");
        r.message = None;
        let path = r.path.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { code_quality::lint(&tool, &[]) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(r) = this.repos.iter_mut().find(|r| r.path == path) {
                    r.busy = None;
                    r.linted = true;
                    match result {
                        Ok(d) => {
                            r.message = d.is_empty().then(|| ("No problems found".to_string(), false));
                            r.diagnostics = d;
                        }
                        Err(e) => r.message = Some((format!("{e:#}"), true)),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Format the repository's changed files (git status) with oxfmt.
    fn format_changed(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(r) = self.repos.get_mut(ix) else { return };
        let Some(tool) = r.oxfmt.clone() else {
            r.message = Some(("oxfmt not found. Add it to the repo (`pnpm add -D oxfmt`) or install it globally.".into(), true));
            cx.notify();
            return;
        };
        r.busy = Some("Formatting");
        r.message = None;
        let path = r.path.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let st = git_engine::status(&p)?;
                    let files: Vec<PathBuf> = st
                        .files
                        .iter()
                        .filter(|f| f.unstaged != Some(git_engine::ChangeKind::Deleted) && f.staged != Some(git_engine::ChangeKind::Deleted))
                        .map(|f| p.join(&f.path))
                        .filter(|f| f.is_file())
                        .collect();
                    if files.is_empty() {
                        return Ok("No changed files".to_string());
                    }
                    code_quality::format(&tool, &files).map(|s| format!("{} file(s): {s}", files.len()))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(r) = this.repos.iter_mut().find(|r| r.path == path) {
                    r.busy = None;
                    r.message = Some(match result {
                        Ok(s) => (s, false),
                        Err(e) => (format!("{e:#}"), true),
                    });
                }
                cx.emit(ToolsPanelEvent::FilesChanged);
                cx.notify();
            });
        })
        .detach();
    }

    // ---- obscura ------------------------------------------------------------------------

    fn check_obscura(&mut self, cx: &mut Context<Self>) {
        self.obscura.checked = true;
        self.obscura.busy = Some("Checking");
        cx.spawn(async move |this, cx| {
            let (bin, version, registered) = cx
                .background_executor()
                .spawn(async move {
                    let bin = obscura::locate();
                    let version = bin.as_deref().and_then(obscura::version);
                    let registered = obscura::registered().ok();
                    (bin, version, registered)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.obscura.bin = bin;
                this.obscura.version = version;
                this.obscura.registered = registered;
                this.obscura.busy = None;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn obscura_op(&mut self, label: &'static str, cx: &mut Context<Self>, op: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        if self.obscura.busy.is_some() {
            return;
        }
        self.obscura.busy = Some(label);
        self.obscura.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { op() }).await;
            let _ = this.update(cx, |this, cx| {
                this.obscura.busy = None;
                this.obscura.message = Some(match result {
                    Ok(s) => (s, false),
                    Err(e) => (format!("{e:#}"), true),
                });
                this.check_obscura(cx);
            });
        })
        .detach();
    }

    fn confirm_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Download Obscura ({})?", obscura::RELEASE_SIZE_HINT),
            Some(&format!("Downloads the latest macOS Apple Silicon release from GitHub into\n{}", obscura::app_bin_dir().display())),
            &["Download", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.obscura_op("Downloading", cx, || obscura::install().map(|p| format!("Installed {}", p.display())));
            });
        })
        .detach();
    }

    fn confirm_register(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bin) = self.obscura.bin.clone() else { return };
        let answer = window.prompt(
            PromptLevel::Info,
            "Add Obscura to Claude Code?",
            Some(&format!(
                "Runs:\nclaude mcp add -s user -e OBSCURA_ALLOW_PRIVATE_NETWORK=1 obscura -- {} mcp\n\nPrivate networks are allowed so Claude can open your local dev servers (localhost). Claude Code starts the Obscura MCP server only when it needs a browser.",
                bin.display()
            )),
            &["Add", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| this.obscura_op("Registering", cx, move || obscura::register(&bin)));
        })
        .detach();
    }

    fn confirm_unregister(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Warning, "Remove Obscura from Claude Code?", Some("Runs: claude mcp remove -s user obscura"), &["Remove", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| this.obscura_op("Removing", cx, obscura::unregister));
        })
        .detach();
    }
}

impl Default for ToolsPanel {
    fn default() -> Self {
        Self::new()
    }
}

fn header(label: &'static str, sub: &'static str, first: bool) -> gpui::Div {
    div()
        .flex()
        .items_baseline()
        .gap(px(8.))
        .px(px(14.))
        .pt(px(12.))
        .pb(px(6.))
        .when(!first, |d| d.mt(px(8.)).border_t_1().border_color(theme::rgb(theme::BORDER)))
        .child(div().flex_none().text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child(label))
        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(sub))
}

fn dot(ok: bool) -> gpui::Div {
    div().flex_none().size(px(6.)).rounded_full().bg(theme::rgb(if ok { theme::ADDED } else { 0x3a3d44 }))
}

fn badge(text: &'static str, ok: bool) -> gpui::Div {
    div()
        .flex_none()
        .px(px(5.))
        .rounded(px(3.))
        .text_size(px(10.))
        .bg(theme::rgb(if ok { 0x1d2b22 } else { theme::HOVER }))
        .text_color(theme::rgb(if ok { 0xa8d8a0 } else { theme::TEXT_FAINT }))
        .child(text)
}

/// `● label   value  [badge]` — one tool/status row.
fn status_line(label: &'static str, value: String, ok: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .pl(px(30.))
        .pr(px(12.))
        .h(px(22.))
        .text_size(px(11.5))
        .child(dot(ok))
        .child(div().w(px(52.)).flex_none().text_color(theme::rgb(theme::TEXT_MUTED)).child(label))
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().font_family(if ok { theme::MONO_FONT } else { theme::UI_FONT }).text_size(px(11.)).text_color(theme::rgb(if ok { theme::TEXT } else { theme::TEXT_FAINT })).child(value))
}

fn tool_button(id: impl Into<gpui::ElementId>, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(10.))
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(11.5))
        .when(primary, |d| d.bg(theme::rgb(0x2b4a7a)).text_color(theme::rgb(theme::TEXT)).hover(|s| s.bg(theme::rgb(0x355a92))))
        .when(!primary, |d| d.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT))))
        .child(label)
}

fn pill(n: usize, fg: u32, bg: u32) -> gpui::Div {
    div().flex_none().px(px(6.)).h(px(16.)).flex().items_center().rounded_full().bg(theme::rgb(bg)).text_size(px(10.5)).text_color(theme::rgb(fg)).child(n.to_string())
}

fn message(id: SharedString, msg: &(String, bool)) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .mx(px(12.))
        .my(px(3.))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(4.))
        .bg(theme::rgb(if msg.1 { 0x3a1f22 } else { 0x1d2b22 }))
        .text_size(px(11.5))
        .text_color(theme::rgb(if msg.1 { 0xf0b3b8 } else { 0xa8d8a0 }))
        .child(msg.0.clone())
}

impl Render for ToolsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().id("tools-panel").flex().flex_col().size_full().overflow_y_scroll().pb(px(16.));

        list = list.child(header("CODE QUALITY", "oxlint · oxfmt", true));
        if self.repos.is_empty() {
            list = list.child(div().px(px(16.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("No repositories in this project."));
        }
        for ix in 0..self.repos.len() {
            let r = &self.repos[ix];
            let ver = |l: &Option<Located>| match l {
                Some(l) => (l.version.clone().unwrap_or_else(|| "?".into()), true, l.config.is_some()),
                None => ("not found".to_string(), false, false),
            };
            let (lv, lok, lcfg) = ver(&r.oxlint);
            let (fv, fok, fcfg) = ver(&r.oxfmt);
            let errors = r.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
            let warnings = r.diagnostics.len() - errors;
            list = list
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .pl(px(14.))
                        .pr(px(8.))
                        .h(px(30.))
                        .child(gpui::svg().path("icons/repo.svg").size(px(13.)).flex_none().text_color(theme::rgb(0xe0714f)))
                        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.)).font_weight(gpui::FontWeight::SEMIBOLD).child(r.name.clone()))
                        .when(r.linted && errors > 0, |d| d.child(pill(errors, theme::DELETED, 0x3a1f22)))
                        .when(r.linted && warnings > 0, |d| d.child(pill(warnings, theme::MODIFIED, 0x3a3220)))
                        .when(r.linted && errors + warnings == 0, |d| d.child(gpui::svg().path("icons/check.svg").size(px(12.)).flex_none().text_color(theme::rgb(theme::ADDED))))
                        .child(div().flex_1())
                        .when(lok, |d| d.child(icon_action(SharedString::from(format!("lint-{ix}")), "icons/lint.svg", "Lint repository (oxlint)").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.lint(ix, cx)))))
                        .when(fok, |d| d.child(icon_action(SharedString::from(format!("fmt-{ix}")), "icons/format.svg", "Format changed files (oxfmt)").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.format_changed(ix, cx))))),
                )
                .child(status_line("oxlint", lv, lok).when(lok, |d| d.child(badge(if lcfg { "config" } else { "default" }, lcfg))))
                .child(status_line("oxfmt", fv, fok).when(fok, |d| d.child(badge(if fcfg { "config" } else { "default" }, fcfg))));
            if let Some(b) = r.busy {
                list = list.child(div().px(px(24.)).text_size(px(11.5)).text_color(theme::rgb(theme::ACCENT)).child(format!("{b}…")));
            }
            if let Some(m) = &r.message {
                list = list.child(message(SharedString::from(format!("tmsg-{ix}")), m));
            }
            let mut last_file: Option<PathBuf> = None;
            for (di, d) in r.diagnostics.iter().take(1500).enumerate() {
                if last_file.as_ref() != Some(&d.file) {
                    last_file = Some(d.file.clone());
                    let rel = d.file.strip_prefix(&r.path).unwrap_or(&d.file).to_string_lossy().into_owned();
                    let (icon, color) = crate::assets::file_icon(&rel);
                    list = list.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(20.))
                            .pt(px(4.))
                            .h(px(24.))
                            .child(gpui::svg().path(icon).size(px(12.)).text_color(theme::rgb(color)))
                            .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).text_color(theme::rgb(theme::TEXT)).child(rel)),
                    );
                }
                let (path, line, column) = (d.file.clone(), d.line.saturating_sub(1), d.column.saturating_sub(1));
                let sev_color = match d.severity {
                    Severity::Error => theme::DELETED,
                    Severity::Warning => theme::MODIFIED,
                    Severity::Advice => theme::ACCENT,
                };
                list = list.child(
                    div()
                        .id(SharedString::from(format!("diag-{ix}-{di}")))
                        .flex()
                        .gap(px(8.))
                        .pl(px(34.))
                        .pr(px(10.))
                        .py(px(3.))
                        .mx(px(4.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(ToolsPanelEvent::OpenAt { path: path.clone(), line, column })))
                        .child(div().mt(px(5.)).size(px(6.)).flex_none().rounded_full().bg(theme::rgb(sev_color)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT)).child(d.message.clone()))
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(theme::rgb(theme::TEXT_FAINT))
                                        .child(format!("{}:{}  {}", d.line, d.column, d.code.clone().unwrap_or_default())),
                                ),
                        ),
                );
            }
        }

        // TypeScript Go.
        list = list.child(header("TYPESCRIPT", "tsgo · live errors", false));
        let tsgo_src = self.tsgo.as_ref().map(|p| {
            if p.starts_with(crate::language_servers::app_tools_dir()) {
                "app-managed".to_string()
            } else if p.components().any(|c| c.as_os_str() == "node_modules") {
                "project node_modules".to_string()
            } else {
                "PATH".to_string()
            }
        });
        list = list.child(status_line("tsgo", tsgo_src.unwrap_or_else(|| "not installed".into()), self.tsgo.is_some()));
        if self.servers.is_empty() {
            list = list.child(div().pl(px(44.)).pr(px(12.)).py(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Servers start when you open a TS/JS file."));
        } else {
            let mut chips = div().flex().flex_wrap().gap(px(4.)).pl(px(44.)).pr(px(12.)).py(px(4.));
            for s in &self.servers {
                chips = chips.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .px(px(6.))
                        .h(px(20.))
                        .rounded(px(4.))
                        .bg(theme::rgb(theme::HOVER))
                        .text_size(px(11.))
                        .text_color(theme::rgb(theme::TEXT_MUTED))
                        .child(dot(true))
                        .child(s.clone()),
                );
            }
            list = list.child(chips);
        }
        if self.tsgo.is_none() {
            list = list.child(
                div().pl(px(44.)).py(px(6.)).child(tool_button("tsgo-install", "Install tsgo", true).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_install_tsgo(w, cx)))),
            );
        }
        if self.tsgo_busy {
            list = list.child(div().px(px(24.)).text_size(px(11.5)).text_color(theme::rgb(theme::ACCENT)).child("Installing…"));
        }
        if let Some(m) = &self.tsgo_message {
            list = list.child(message("tsgo-msg".into(), m));
        }

        // Obscura.
        let o = &self.obscura;
        list = list.child(
            header("BROWSER", "Obscura · MCP for Claude", false)
                .child(div().flex_1())
                .child(icon_action("obs-refresh", "icons/fetch.svg", "Re-check").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.check_obscura(cx))))
                .child(icon_action("obs-docs", "icons/external.svg", "Docs").on_click(|_, _, cx| cx.open_url("https://github.com/h4ckf0r0day/obscura/blob/main/docs/Use-the-MCP-server.md"))),
        );
        list = list
            .child(status_line(
                "binary",
                match (&o.bin, &o.version) {
                    (Some(_), v) => v.clone().unwrap_or_else(|| "installed".into()),
                    (None, _) => "not installed".into(),
                },
                o.bin.is_some(),
            ))
            .child(status_line(
                "Claude",
                match o.registered {
                    Some(true) => "registered".into(),
                    Some(false) => "not registered".into(),
                    None => "claude CLI not found".into(),
                },
                o.registered == Some(true),
            ));
        let mut buttons = div().flex().flex_wrap().gap(px(6.)).pl(px(44.)).pr(px(12.)).py(px(6.));
        if o.bin.is_none() {
            buttons = buttons.child(tool_button("obs-install", "Download & install", true).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_install(w, cx))));
        } else if o.registered == Some(true) {
            buttons = buttons.child(tool_button("obs-unreg", "Remove from Claude", false).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_unregister(w, cx))));
        } else {
            buttons = buttons.child(tool_button("obs-reg", "Add to Claude Code", true).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_register(w, cx))));
        }
        list = list.child(buttons);
        if let Some(b) = o.busy {
            list = list.child(div().px(px(24.)).text_size(px(11.5)).text_color(theme::rgb(theme::ACCENT)).child(format!("{b}…")));
        }
        if let Some(m) = &o.message {
            list = list.child(message("obs-msg".into(), m));
        }
        let open = self.mcp_tools_open;
        list = list.child(
            div()
                .id("mcp-tools")
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(44.))
                .h(px(24.))
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(theme::rgb(theme::TEXT_FAINT))
                .hover(|s| s.text_color(theme::rgb(theme::TEXT_MUTED)))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.mcp_tools_open = !this.mcp_tools_open;
                    cx.notify();
                }))
                .child(format!("{} tools Claude gets", obscura::TOOLS.len()))
                .child(gpui::svg().path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(8.)).text_color(theme::rgb(theme::TEXT_FAINT))),
        );
        for (name, desc) in obscura::TOOLS.iter().filter(|_| open) {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .pl(px(44.))
                    .pr(px(12.))
                    .py(px(2.))
                    .child(div().text_size(px(11.5)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_MUTED)).child(*name))
                    .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(*desc)),
            );
        }
        list
    }
}
