//! Sidebar "Tools" mode: Oxlint/Oxfmt per repository with a problems list, and the Moli
//! headless-browser integration for Claude Code (binary + skills). Everything runs on demand, off the UI thread.

use std::path::PathBuf;

use code_quality::{Diagnostic, Located, Severity, Tool};
use gpui::{ClickEvent, Context, EventEmitter, PromptLevel, Render, SharedString, Window, div, prelude::*, px};

use crate::git_panel::icon_action;
use crate::moli;
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
struct MoliState {
    bin: Option<PathBuf>,
    version: Option<String>,
    /// Ref the installed skills came from; `None` = not (fully) installed.
    skills: Option<String>,
    obscura_leftovers: bool,
    busy: Option<&'static str>,
    message: Option<(String, bool)>,
    checked: bool,
}

pub struct ToolsPanel {
    repos: Vec<RepoTools>,
    moli: MoliState,
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
            moli: MoliState::default(),
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
            if !self.moli.checked {
                self.check_moli(cx);
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
        self.check_moli(cx);
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

    // ---- moli ---------------------------------------------------------------------------

    fn check_moli(&mut self, cx: &mut Context<Self>) {
        self.moli.checked = true;
        self.moli.busy = Some("Checking");
        cx.spawn(async move |this, cx| {
            let (bin, version, skills, leftovers) = cx
                .background_executor()
                .spawn(async move {
                    let bin = moli::locate();
                    let version = bin.as_deref().and_then(moli::version);
                    (bin, version, moli::skills_ref(), moli::obscura_leftovers())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.moli.bin = bin;
                this.moli.version = version;
                this.moli.skills = skills;
                this.moli.obscura_leftovers = leftovers;
                this.moli.busy = None;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn moli_op(&mut self, label: &'static str, cx: &mut Context<Self>, op: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        if self.moli.busy.is_some() {
            return;
        }
        self.moli.busy = Some(label);
        self.moli.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { op() }).await;
            let _ = this.update(cx, |this, cx| {
                this.moli.busy = None;
                this.moli.message = Some(match result {
                    Ok(s) => (s, false),
                    Err(e) => (format!("{e:#}"), true),
                });
                this.check_moli(cx);
            });
        })
        .detach();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>, level: PromptLevel, title: String, detail: String, ok: &'static str, run: impl FnOnce(&mut Self, &mut Context<Self>) + 'static) {
        let answer = window.prompt(level, &title, Some(&detail), &[ok, "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| run(this, cx));
        })
        .detach();
    }

    fn confirm_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = moli::release_url().unwrap_or_default();
        self.confirm(
            window,
            cx,
            PromptLevel::Info,
            format!("Download Moli ({})?", moli::RELEASE_SIZE_HINT),
            format!("Downloads the latest macOS release from GitHub\n{url}\ninto {}", moli::install_dir().display()),
            "Download",
            |this, cx| this.moli_op("Downloading", cx, || moli::install().map(|p| format!("Installed {}", p.display()))),
        );
    }

    fn confirm_install_skills(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let git_ref = moli::ref_for(self.moli.version.as_deref());
        self.confirm(
            window,
            cx,
            PromptLevel::Info,
            "Add Moli skills to Claude Code?".into(),
            format!(
                "Downloads {} from github.com/{}@{git_ref} into\n{}\n\nClaude Code then uses `moli fetch` / `moli serve` whenever it needs a browser (local dev servers included).",
                moli::SKILLS.join(", "),
                moli::REPO,
                moli::skills_dir().display()
            ),
            "Install",
            move |this, cx| this.moli_op("Installing skills", cx, move || moli::install_skills(&git_ref)),
        );
    }

    fn confirm_remove_skills(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirs = moli::SKILLS.iter().map(|s| moli::skills_dir().join(s).display().to_string()).collect::<Vec<_>>().join("\n");
        self.confirm(window, cx, PromptLevel::Warning, "Remove Moli skills from Claude Code?".into(), format!("Deletes:\n{dirs}"), "Remove", |this, cx| {
            this.moli_op("Removing", cx, moli::remove_skills)
        });
    }

    fn confirm_remove_obscura(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(
            window,
            cx,
            PromptLevel::Warning,
            "Remove Obscura?".into(),
            "Runs: claude mcp remove -s user obscura\nand deletes the Obscura binaries this app downloaded.".into(),
            "Remove",
            |this, cx| this.moli_op("Removing Obscura", cx, moli::remove_obscura),
        );
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

        // Moli.
        let m = &self.moli;
        list = list.child(
            header("BROWSER", "Moli · skills for Claude", false)
                .child(div().flex_1())
                .child(icon_action("moli-refresh", "icons/fetch.svg", "Re-check").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.check_moli(cx))))
                .child(icon_action("moli-docs", "icons/external.svg", "Docs").on_click(|_, _, cx| cx.open_url("https://github.com/lexmount/moli"))),
        );
        let want_ref = moli::ref_for(m.version.as_deref());
        let skills_stale = m.version.is_some() && m.skills.as_deref().is_some_and(|r| !r.is_empty() && r != want_ref);
        list = list
            .child(status_line(
                "binary",
                match (&m.bin, &m.version) {
                    (Some(_), Some(v)) => v.clone(),
                    (Some(_), None) => "installed (does not run)".into(),
                    (None, _) => "not installed".into(),
                },
                m.version.is_some(),
            ))
            .child(status_line(
                "skills",
                match m.skills.as_deref() {
                    None => "not installed".into(),
                    Some("") => "installed".into(),
                    Some(r) if skills_stale => format!("{r} (binary is {want_ref})"),
                    Some(r) => r.to_string(),
                },
                m.skills.is_some() && !skills_stale,
            ));
        let mut buttons = div().flex().flex_wrap().gap(px(6.)).pl(px(44.)).pr(px(12.)).py(px(6.));
        if m.bin.is_none() {
            buttons = buttons.child(tool_button("moli-install", "Download & install", true).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_install(w, cx))));
        } else {
            buttons = buttons.child(tool_button("moli-update", "Update binary", false).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_install(w, cx))));
        }
        if m.skills.is_none() || skills_stale {
            let label = if skills_stale { "Update skills" } else { "Add skills to Claude" };
            buttons = buttons.child(tool_button("moli-skills", label, m.bin.is_some()).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_install_skills(w, cx))));
        } else {
            buttons = buttons.child(tool_button("moli-unskills", "Remove skills", false).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_remove_skills(w, cx))));
        }
        if m.obscura_leftovers {
            buttons = buttons.child(tool_button("obscura-remove", "Remove old Obscura", false).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.confirm_remove_obscura(w, cx))));
        }
        list = list.child(buttons);
        if let Some(b) = m.busy {
            list = list.child(div().px(px(24.)).text_size(px(11.5)).text_color(theme::rgb(theme::ACCENT)).child(format!("{b}…")));
        }
        if let Some(msg) = &m.message {
            list = list.child(message("moli-msg".into(), msg));
        }
        let open = self.mcp_tools_open;
        list = list.child(
            div()
                .id("moli-skills-list")
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
                .child(format!("{} skills Claude gets", moli::SKILL_INFO.len()))
                .child(gpui::svg().path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(8.)).text_color(theme::rgb(theme::TEXT_FAINT))),
        );
        for (name, desc) in moli::SKILL_INFO.iter().filter(|_| open) {
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
