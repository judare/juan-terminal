//! Global search across the project's files (sidebar "Search" mode).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    KeyBinding, PromptLevel, Render, SharedString, StrikethroughStyle, StyledText, Subscription,
    Task, Timer, Window, actions, div, prelude::*, px, uniform_list,
};
use project_search::{IndexedFile, SearchMatch, SearchOptions, replace_in_file, search};

use crate::text_input::{TextInput, TextInputEvent};
use crate::glass::GlassExt;
use crate::theme;

const MAX_MATCHES: usize = 5000;

actions!(search_panel, [ToggleCaseSensitive, ToggleWholeWord, ToggleRegex, ToggleReplace]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("SearchPanel");
    cx.bind_keys([
        KeyBinding::new("alt-cmd-c", ToggleCaseSensitive, c),
        KeyBinding::new("alt-cmd-w", ToggleWholeWord, c),
        KeyBinding::new("alt-cmd-r", ToggleRegex, c),
        KeyBinding::new("cmd-shift-h", ToggleReplace, c),
    ]);
}

pub enum SearchPanelEvent {
    Open { path: PathBuf, line: usize, column: usize },
    /// Files were rewritten on disk by a replace; open editors should reload.
    Replaced(Vec<PathBuf>),
}

#[derive(Clone)]
enum Row {
    File(usize, usize), // file index, match count
    Match(usize),       // index into matches
}

pub struct SearchPanel {
    input: Entity<TextInput>,
    replace_input: Entity<TextInput>,
    show_replace: bool,
    opts: SearchOptions,
    /// Query/options the current `matches` were computed with (replace must use the same).
    searched: (String, SearchOptions),
    files: Arc<Vec<IndexedFile>>,
    matches: Vec<SearchMatch>,
    rows: Vec<Row>,
    status: String,
    /// Keep the replace summary visible across the refresh search that follows it.
    status_sticky: bool,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SearchPanelEvent> for SearchPanel {}

impl SearchPanel {
    pub fn new(files: Arc<Vec<IndexedFile>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search", cx));
        let replace_input = cx.new(|cx| TextInput::new("Replace", cx));
        let sub = cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
            TextInputEvent::Changed => this.schedule(Duration::from_millis(180), cx),
            TextInputEvent::Submit { .. } => this.schedule(Duration::ZERO, cx),
            TextInputEvent::Cancel => {}
        });
        // Typing a replacement only changes the preview.
        let sub2 = cx.subscribe_in(&replace_input, window, |this, _, event, window, cx| match event {
            TextInputEvent::Changed => cx.notify(),
            TextInputEvent::Submit { .. } => this.replace_all(window, cx),
            TextInputEvent::Cancel => {}
        });
        Self {
            input,
            replace_input,
            show_replace: false,
            opts: SearchOptions::default(),
            searched: (String::new(), SearchOptions::default()),
            files,
            matches: Vec::new(),
            rows: Vec::new(),
            status: String::new(),
            status_sticky: false,
            cancel: Arc::new(AtomicBool::new(false)),
            task: None,
            _subs: vec![sub, sub2],
        }
    }

    fn toggle(&mut self, f: impl FnOnce(&mut SearchOptions), cx: &mut Context<Self>) {
        f(&mut self.opts);
        self.schedule(Duration::ZERO, cx);
    }

    fn replacement(&self, cx: &App) -> Option<String> {
        self.show_replace.then(|| self.replace_input.read(cx).content.clone())
    }

    /// Replace matches: one (`only = Some(match index)`), one file, or everything listed.
    fn do_replace(&mut self, targets: Vec<(usize, Option<usize>)>, cx: &mut Context<Self>) {
        let Some(replacement) = self.replacement(cx) else { return };
        let (query, opts) = self.searched.clone();
        let jobs: Vec<(PathBuf, Option<(usize, std::ops::Range<usize>)>)> = targets
            .into_iter()
            .map(|(fi, mi)| (self.files[fi].path.clone(), mi.map(|mi| (self.matches[mi].line, self.matches[mi].byte_range.clone()))))
            .collect();
        self.status = "Replacing…".into();
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let (count, changed, errors) = cx
                .background_executor()
                .spawn(async move {
                    let (mut count, mut changed, mut errors) = (0usize, Vec::new(), Vec::new());
                    for (path, only) in jobs {
                        match replace_in_file(&path, &query, opts, &replacement, only) {
                            Ok(0) => {}
                            Ok(n) => {
                                count += n;
                                changed.push(path);
                            }
                            Err(e) => errors.push(format!("{}: {e:#}", path.display())),
                        }
                    }
                    (count, changed, errors)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                cx.emit(SearchPanelEvent::Replaced(changed.clone()));
                this.schedule(Duration::ZERO, cx);
                this.status = if errors.is_empty() {
                    format!("Replaced {count} in {} files", changed.len())
                } else {
                    format!("Replaced {count}; {} failed: {}", errors.len(), errors.join("; "))
                };
                this.status_sticky = true;
            });
        }));
    }

    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.matches.is_empty() || !self.show_replace {
            return;
        }
        let mut files: Vec<usize> = self.matches.iter().map(|m| m.file).collect();
        files.dedup();
        let detail = format!("Replace {} occurrence(s) in {} file(s) with “{}”? Files are written to disk.", self.matches.len(), files.len(), self.replace_input.read(cx).content);
        let answer = window.prompt(PromptLevel::Warning, "Replace all?", Some(&detail), &["Replace", "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| this.do_replace(files.into_iter().map(|f| (f, None)).collect(), cx));
            }
        })
        .detach();
    }

    pub fn set_files(&mut self, files: Arc<Vec<IndexedFile>>, cx: &mut Context<Self>) {
        self.files = files;
        if !self.input.read(cx).content.is_empty() {
            self.schedule(Duration::ZERO, cx);
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.focus_handle(cx));
        self.input.update(cx, |i, cx| i.select_all_text(cx));
    }

    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.set_text(query, cx));
    }

    fn schedule(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let query = self.input.read(cx).content.clone();
        let opts = self.opts;
        let files = self.files.clone();
        if query.is_empty() {
            self.matches.clear();
            self.rows.clear();
            self.status.clear();
            self.task = None;
            cx.notify();
            return;
        }
        self.status = "Searching…".into();
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                Timer::after(delay).await;
            }
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let c2 = cancel.clone();
            let q = query.clone();
            let results = cx.background_executor().spawn(async move { search(&q, opts, &files, MAX_MATCHES, &c2) }).await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let results = match results {
                    Ok(r) => r,
                    Err(e) => {
                        this.matches.clear();
                        this.rows.clear();
                        this.status = format!("Invalid pattern: {}", e.to_string().lines().last().unwrap_or(""));
                        cx.notify();
                        return;
                    }
                };
                this.searched = (query, opts);
                let keep = std::mem::take(&mut this.status_sticky);
                let file_count = {
                    let mut f: Vec<usize> = results.matches.iter().map(|m| m.file).collect();
                    f.dedup();
                    f.len()
                };
                let found = if results.matches.is_empty() {
                    format!("No results ({} files searched)", results.files_searched)
                } else {
                    format!(
                        "{}{} results in {} files",
                        results.matches.len(),
                        if results.truncated { "+" } else { "" },
                        file_count
                    )
                };
                this.status = if keep { format!("{} · {found}", this.status) } else { found };
                this.matches = results.matches;
                this.rebuild_rows();
                cx.notify();
            });
        }));
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let mut i = 0;
        while i < self.matches.len() {
            let file = self.matches[i].file;
            let start = i;
            while i < self.matches.len() && self.matches[i].file == file {
                i += 1;
            }
            self.rows.push(Row::File(file, i - start));
            self.rows.extend((start..i).map(Row::Match));
        }
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

fn opt_toggle(id: &'static str, label: &'static str, on: bool, tip: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(22.))
        .rounded(px(11.))
        .cursor_pointer()
        .text_size(px(11.))
        .font_family(theme::MONO_FONT)
        .border_1()
        .when(on, |d| d.glass_active().text_color(theme::rgb(theme::TEXT)))
        .when(!on, |d| d.border_color(gpui::transparent_black()).text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.glass_hover()))
        .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
        .child(label)
}

fn field() -> gpui::Div {
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap(px(4.))
        .pl(px(11.))
        .pr(px(4.))
        .h(px(30.))
        .rounded(px(15.))
        .glass()
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.rows.len();
        let opts = self.opts;
        let show_replace = self.show_replace;
        let replacement = self.replacement(cx);
        let can_replace = show_replace && !self.matches.is_empty();
        div()
            .key_context("SearchPanel")
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _, cx| this.toggle(|o| o.case_sensitive = !o.case_sensitive, cx)))
            .on_action(cx.listener(|this, _: &ToggleWholeWord, _, cx| this.toggle(|o| o.whole_word = !o.whole_word, cx)))
            .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| this.toggle(|o| o.regex = !o.regex, cx)))
            .on_action(cx.listener(|this, _: &ToggleReplace, w, cx| {
                this.show_replace = !this.show_replace;
                if this.show_replace {
                    w.focus(&this.replace_input.focus_handle(cx));
                }
                cx.notify();
            }))
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .pl(px(4.))
                    .pr(px(10.))
                    .flex_none()
                    .child(
                        div()
                            .id("toggle-replace")
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(16.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                            .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Toggle Replace (⌘⇧H)")).into())
                            .on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                                this.show_replace = !this.show_replace;
                                if this.show_replace {
                                    w.focus(&this.replace_input.focus_handle(cx));
                                }
                                cx.notify();
                            }))
                            .child(gpui::svg().path(if show_replace { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(9.)).text_color(theme::rgb(theme::TEXT_MUTED))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(4.))
                            .child(
                                field()
                                    .child(gpui::svg().path("icons/search.svg").size(px(12.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)))
                                    .child(div().flex_1().min_w_0().child(self.input.clone()))
                                    .child(opt_toggle("opt-case", "Aa", opts.case_sensitive, "Match Case (⌥⌘C)").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(|o| o.case_sensitive = !o.case_sensitive, cx))))
                                    .child(opt_toggle("opt-word", "ab", opts.whole_word, "Match Whole Word (⌥⌘W)").underline().on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(|o| o.whole_word = !o.whole_word, cx))))
                                    .child(opt_toggle("opt-regex", ".*", opts.regex, "Use Regular Expression (⌥⌘R)").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(|o| o.regex = !o.regex, cx)))),
                            )
                            .when(show_replace, |d| {
                                d.child(
                                    div()
                                        .flex()
                                        .gap(px(4.))
                                        .items_center()
                                        .child(field().child(gpui::svg().path("icons/edit.svg").size(px(12.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT))).child(div().flex_1().min_w_0().child(self.replace_input.clone())))
                                        .child(
                                            crate::git_panel::icon_action("replace-all", "icons/check.svg", "Replace All (Enter)")
                                                .when(!can_replace, |d| d.opacity(0.4))
                                                .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.replace_all(w, cx))),
                                        ),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .px(px(14.))
                    .py(px(6.))
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme::rgb(if self.status.starts_with("Invalid") { theme::DELETED } else { theme::TEXT_FAINT }))
                    .child(self.status.clone()),
            )
            .child(
                uniform_list(
                    "search-results",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|ix| match this.rows[ix].clone() {
                                Row::File(fi, n) => {
                                    let file = &this.files[fi];
                                    let name = file.display.rsplit('/').next().unwrap_or("").to_string();
                                    let dir = file.display.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
                                    let (icon, color) = crate::assets::file_icon(&name);
                                    let group: SharedString = format!("sf-{ix}").into();
                                    div()
                                        .id(ix)
                                        .group(group.clone())
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .h(px(24.))
                                        .pl(px(12.))
                                        .pr(px(10.))
                                        .child(gpui::svg().path(icon).size(px(13.)).flex_none().text_color(theme::rgb(color)))
                                        .child(
                                            div()
                                                .flex()
                                                .flex_1()
                                                .min_w_0()
                                                .items_baseline()
                                                .gap(px(6.))
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(div().flex_none().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(name))
                                                .child(div().min_w_0().overflow_hidden().text_ellipsis().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(dir)),
                                        )
                                        .when(can_replace, |d| {
                                            d.child(
                                                crate::git_panel::icon_action(SharedString::from(format!("rf-{ix}")), "icons/check.svg", "Replace in file")
                                                    .invisible()
                                                    .group_hover(group.clone(), |s| s.visible())
                                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.do_replace(vec![(fi, None)], cx))),
                                            )
                                        })
                                        .child(div().flex_none().px(px(6.)).rounded_full().bg(theme::rgb(theme::HOVER)).text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_MUTED)).child(n.to_string()))
                                }
                                Row::Match(mi) => {
                                    let m = &this.matches[mi];
                                    let fi = m.file;
                                    let path = this.files[m.file].path.clone();
                                    let (line, column) = (m.line, m.column);
                                    // Trim leading whitespace, keeping the highlight aligned.
                                    let trimmed = m.line_text.len() - m.line_text.trim_start().len();
                                    let mut text = m.line_text[trimmed..].to_string();
                                    let range = m.range.start.saturating_sub(trimmed)..m.range.end.saturating_sub(trimmed);
                                    let valid = range.end > range.start && range.end <= text.len();
                                    let mut highlights = Vec::new();
                                    match (&replacement, valid) {
                                        // Preview: old text struck through, new text in green right after it.
                                        (Some(rep), true) if !this.searched.1.regex || !rep.contains('$') => {
                                            text.insert_str(range.end, rep);
                                            highlights.push((range.clone(), HighlightStyle { color: Some(theme::rgb(0xf0b3b8)), background_color: Some(theme::rgb(0x4a2226)), strikethrough: Some(StrikethroughStyle { thickness: px(1.), color: Some(theme::rgb(0xf0b3b8)) }), ..Default::default() }));
                                            if !rep.is_empty() {
                                                highlights.push((range.end..range.end + rep.len(), HighlightStyle { color: Some(theme::rgb(theme::TEXT)), background_color: Some(theme::rgb(0x23402a)), ..Default::default() }));
                                            }
                                        }
                                        (_, true) => highlights.push((range, HighlightStyle { color: Some(theme::rgb(theme::TEXT)), background_color: Some(theme::rgb(theme::FIND_MATCH_CURRENT)), ..Default::default() })),
                                        _ => {}
                                    }
                                    let group: SharedString = format!("sm-{ix}").into();
                                    div()
                                        .id(ix)
                                        .group(group.clone())
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .h(px(22.))
                                        .pl(px(30.))
                                        .pr(px(6.))
                                        .mx(px(4.))
                                        .rounded(px(4.))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                            cx.emit(SearchPanelEvent::Open { path: path.clone(), line, column })
                                        }))
                                        .child(
                                            div()
                                                .flex_none()
                                                .w(px(32.))
                                                .text_size(px(11.))
                                                .font_family(theme::MONO_FONT)
                                                .text_color(theme::rgb(theme::TEXT_FAINT))
                                                .child((line + 1).to_string()),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_size(px(12.))
                                                .font_family(theme::MONO_FONT)
                                                .text_color(theme::rgb(theme::TEXT_MUTED))
                                                .child(StyledText::new(SharedString::from(text)).with_highlights(highlights)),
                                        )
                                        .when(can_replace, |d| {
                                            d.child(
                                                crate::git_panel::icon_action(SharedString::from(format!("rm-{ix}")), "icons/check.svg", "Replace")
                                                    .invisible()
                                                    .group_hover(group.clone(), |s| s.visible())
                                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                        cx.stop_propagation();
                                                        this.do_replace(vec![(fi, Some(mi))], cx)
                                                    })),
                                            )
                                        })
                                }
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .w_full(),
            )
    }
}
