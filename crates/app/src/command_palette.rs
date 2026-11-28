//! Command palette (⌘⇧P): fuzzy-search every app action and run it on the previously focused
//! element (so editor commands apply to the editor you were in).

use gpui::{
    Action, App, ClickEvent, SharedString, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    ScrollStrategy, Subscription, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use crate::glass::GlassExt;
use crate::quick_open::{SelectNext, SelectPrevious};
use crate::text_input::{TextInput, TextInputEvent};
use crate::{editor_view as e, terminal_view as t, theme, workspace as w};

pub struct Command {
    pub label: SharedString,
    pub shortcut: &'static str,
    pub run: Run,
}

pub enum Run {
    Action(Box<dyn Action>),
    /// Command `index` of the external plugin `id` (see `plugins`).
    Plugin(String, usize),
}

/// App commands (minus those of plugins that are off) plus the commands of external plugins.
pub fn commands() -> Vec<Command> {
    use crate::plugins;
    let mut v = builtin_commands();
    v.retain(|c| match &c.run {
        Run::Action(a) if a.partial_eq(&w::ShowUsage) => plugins::enabled(plugins::AI_USAGE),
        Run::Action(a) if a.partial_eq(&w::ToggleMusic) => plugins::enabled(plugins::MEDIA),
        _ => true,
    });
    for m in plugins::external().into_iter().filter(|m| plugins::enabled(&m.id)) {
        for (i, cmd) in m.commands.iter().enumerate() {
            v.push(Command { label: format!("{}: {}", m.name, cmd.title).into(), shortcut: "", run: Run::Plugin(m.id.clone(), i) });
        }
    }
    v
}

fn builtin_commands() -> Vec<Command> {
    let c = |label: &'static str, shortcut, action: Box<dyn Action>| Command { label: label.into(), shortcut, run: Run::Action(action) };
    vec![
        c("File: Go to File…", "⌘P", Box::new(w::ToggleQuickOpen)),
        c("File: Open Project Folder…", "⌘O", Box::new(w::OpenProject)),
        c("File: Add Repository…", "⌘⇧O", Box::new(w::AddRepository)),
        c("File: New Word Document (.docx)…", "", Box::new(w::NewWordDocument)),
        c("File: New Excel Workbook (.xlsx)…", "", Box::new(w::NewSpreadsheet)),
        c("File: New PowerPoint Presentation (.pptx)…", "", Box::new(w::NewPresentation)),
        c("File: Save", "⌘S", Box::new(e::Save)),
        c("File: Save All", "⌘⌥S", Box::new(w::SaveAll)),
        c("File: Toggle Autosave", "", Box::new(w::ToggleAutosave)),
        c("File: Close Editor", "⌘W", Box::new(w::CloseEditor)),
        c("Editor: Find", "⌘F", Box::new(e::Find)),
        c("Editor: Go to Line…", "⌃G", Box::new(e::GoToLine)),
        c("Editor: Toggle Comment", "⌘/", Box::new(e::ToggleComment)),
        c("Editor: Select All", "⌘A", Box::new(e::SelectAll)),
        c("Editor: Undo", "⌘Z", Box::new(e::Undo)),
        c("Editor: Redo", "⌘⇧Z", Box::new(e::Redo)),
        c("Editor: Toggle Markdown Live / SVG Preview", "⌘⇧V", Box::new(e::ToggleMarkdownLive)),
        c("Markdown: Insert Table", "", Box::new(e::InsertTable)),
        c("Markdown / Word: Export PDF (letterhead, watermark)…", "", Box::new(e::ExportPdf)),
        c("View: Toggle Word Wrap", "⌥Z", Box::new(e::ToggleWordWrap)),
        c("View: Toggle Light / Dark Theme", "⌘⌥T", Box::new(w::ToggleTheme)),
        c("Editor: Go to Definition", "F12 / ⌘-click", Box::new(e::GoToDefinition)),
        c("Editor: Next Tab", "⌘⇧]", Box::new(w::NextEditor)),
        c("Editor: Previous Tab", "⌘⇧[", Box::new(w::PreviousEditor)),
        c("Search: Find in Project", "⌘⇧F", Box::new(w::SearchInProject)),
        c("Terminal: New Terminal", "⌘T", Box::new(w::NewTerminal)),
        c("Terminal: Close Terminal", "", Box::new(w::CloseTerminal)),
        c("Terminal: Toggle / Focus Terminal Panel", "⌘J", Box::new(w::ToggleTerminalPanel)),
        c("Terminal: Next Terminal", "", Box::new(w::NextTerminal)),
        c("Terminal: Previous Terminal", "", Box::new(w::PreviousTerminal)),
        c("Terminal: Clear", "⌘K", Box::new(t::Clear)),
        c("Claude: New Claude Session", "⌘⇧C", Box::new(w::OpenClaude)),
        c("Git: Show Git Panel", "⌘⇧G", Box::new(w::ShowGit)),
        c("Git: Close Diff", "", Box::new(w::CloseDiff)),
        c("Code Quality: Lint Project (oxlint)", "⌘⇧L", Box::new(w::LintProject)),
        c("Code Quality: Format File (oxfmt)", "⌥⇧F", Box::new(w::FormatFile)),
        c("View: Show Files", "⌘⇧E", Box::new(w::ShowFiles)),
        c("View: Show Tools", "", Box::new(w::ShowTools)),
        c("View: Agent Usage (tokens, tasks, models)", "⌘⇧U", Box::new(w::ShowUsage)),
        c("View: Toggle Sidebar", "⌘B", Box::new(w::ToggleSidebar)),
        c("View: Increase Font Size", "⌘=", Box::new(w::IncreaseFontSize)),
        c("View: Decrease Font Size", "⌘-", Box::new(w::DecreaseFontSize)),
        c("View: Reset Font Size", "⌘0", Box::new(w::ResetFontSize)),
        c("View: Música, podcasts y videos", "⌘⇧M", Box::new(w::ToggleMusic)),
        c("App: Plugins (activar, configurar, instalar)", "", Box::new(w::ShowPlugins)),
        c("App: Settings (schedules, blocked websites)", "⌘,", Box::new(w::OpenSettings)),
        c("Project: Keep Project Open 1 More Hour (emergency)", "", Box::new(w::ExtendProjectHours)),
        c("App: Quit", "⌘Q", Box::new(w::Quit)),
    ]
}

pub enum PaletteEvent {
    Run(Box<dyn Action>),
    RunPlugin(String, usize),
    Dismiss,
}

pub struct CommandPalette {
    input: Entity<TextInput>,
    commands: Vec<Command>,
    results: Vec<usize>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _sub: Subscription,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl CommandPalette {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Type a command…", cx));
        let sub = cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
            TextInputEvent::Changed => this.filter(cx),
            TextInputEvent::Submit { .. } => this.confirm(this.selected, cx),
            TextInputEvent::Cancel => cx.emit(PaletteEvent::Dismiss),
        });
        window.focus(&input.focus_handle(cx));
        let commands = commands();
        let results = (0..commands.len()).collect();
        Self { input, commands, results, selected: 0, scroll: UniformListScrollHandle::new(), _sub: sub }
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let q = self.input.read(cx).content.clone();
        let mut scored: Vec<(i64, usize)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| project_search::fuzzy_match(&q, &c.label).map(|(s, _)| (s, i)))
            .collect();
        if !q.trim().is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0));
        }
        self.results = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn confirm(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(&ci) = self.results.get(ix) {
            match &self.commands[ci].run {
                Run::Action(a) => cx.emit(PaletteEvent::Run(a.boxed_clone())),
                Run::Plugin(id, i) => cx.emit(PaletteEvent::RunPlugin(id.clone(), *i)),
            }
        }
    }

    fn move_selection(&mut self, d: isize, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            return;
        }
        let n = self.results.len() as isize;
        self.selected = (self.selected as isize + d).rem_euclid(n) as usize;
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.results.len();
        div()
            .key_context("QuickOpen")
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .w(px(560.))
            .flex()
            .flex_col()
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .rounded(px(10.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .m(px(8.))
                    .px(px(14.))
                    .h(px(36.))
                    .flex_none()
                    .rounded(px(18.))
                    .glass()
                    .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(">"))
                    .child(self.input.clone()),
            )
            .child(
                uniform_list(
                    "palette",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|ix| {
                                let c = &this.commands[this.results[ix]];
                                let selected = ix == this.selected;
                                div().w_full().px(px(6.)).child(
                                    div()
                                        .id(ix)
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .gap(px(12.))
                                        .h(px(30.))
                                        .px(px(12.))
                                        .rounded(px(6.))
                                        .cursor_pointer()
                                        .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                                        .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.confirm(ix, cx)))
                                        .child(div().flex_1().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).child(c.label.clone()))
                                        .child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(c.shortcut)),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .w_full()
                .h(px((count.min(12) as f32) * 30. + 8.))
                .py(px(4.)),
            )
    }
}
