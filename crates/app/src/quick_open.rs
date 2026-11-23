//! Quick Open (⌘P): fuzzy file finder over the active project's index.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    KeyBinding, Render, SharedString, StyledText, Subscription, Window, actions, div, prelude::*,
    px, uniform_list, UniformListScrollHandle, ScrollStrategy,
};
use project_search::{IndexedFile, fuzzy_filter};

use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

actions!(quick_open, [SelectNext, SelectPrevious]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("QuickOpen");
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, c),
        KeyBinding::new("up", SelectPrevious, c),
        KeyBinding::new("ctrl-n", SelectNext, c),
        KeyBinding::new("ctrl-p", SelectPrevious, c),
    ]);
}

pub enum QuickOpenEvent {
    Open(PathBuf),
    Dismiss,
}

pub struct QuickOpen {
    input: Entity<TextInput>,
    files: Arc<Vec<IndexedFile>>,
    results: Vec<(usize, Vec<usize>)>,
    selected: usize,
    scroll: UniformListScrollHandle,
    indexing: bool,
    _sub: Subscription,
}

impl EventEmitter<QuickOpenEvent> for QuickOpen {}

impl QuickOpen {
    pub fn new(files: Arc<Vec<IndexedFile>>, indexing: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Go to file…", cx));
        let sub = cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
            TextInputEvent::Changed => this.update_results(cx),
            TextInputEvent::Submit { .. } => this.confirm(cx),
            TextInputEvent::Cancel => cx.emit(QuickOpenEvent::Dismiss),
        });
        window.focus(&input.focus_handle(cx));
        let mut this = Self { input, files, results: Vec::new(), selected: 0, scroll: UniformListScrollHandle::new(), indexing, _sub: sub };
        this.update_results(cx);
        this
    }

    pub fn set_files(&mut self, files: Arc<Vec<IndexedFile>>, cx: &mut Context<Self>) {
        self.files = files;
        self.indexing = false;
        self.update_results(cx);
    }

    fn update_results(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).content.clone();
        self.results = if query.trim().is_empty() {
            (0..self.files.len().min(200)).map(|i| (i, Vec::new())).collect()
        } else {
            fuzzy_filter(&query, &self.files, 200)
        };
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some((i, _)) = self.results.get(self.selected) {
            cx.emit(QuickOpenEvent::Open(self.files[*i].path.clone()));
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            return;
        }
        let n = self.results.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }
}

impl Focusable for QuickOpen {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for QuickOpen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.results.len();
        let status = if self.indexing && self.files.is_empty() {
            "Indexing…".to_string()
        } else if count == 0 {
            "No matching files".to_string()
        } else {
            String::new()
        };
        div()
            .key_context("QuickOpen")
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .w(px(620.))
            .max_h(px(440.))
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
                    .px(px(12.))
                    .h(px(42.))
                    .border_b_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .child(gpui::svg().path("icons/search.svg").size(px(14.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                    .child(self.input.clone()),
            )
            .when(!status.is_empty(), |d| {
                d.child(div().px(px(14.)).py(px(10.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(status))
            })
            .child(
                uniform_list(
                    "quick-open-results",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|ix| {
                                let (fi, positions) = &this.results[ix];
                                let file = &this.files[*fi];
                                let display = file.display.clone();
                                let split = display.rfind('/').map(|i| i + 1).unwrap_or(0);
                                let name: String = display[split..].to_string();
                                let dir: String = display[..split].trim_end_matches('/').to_string();
                                let name_chars = display[..split].chars().count();
                                // Highlight matched chars in the file name.
                                let name_highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = positions
                                    .iter()
                                    .filter(|p| **p >= name_chars)
                                    .filter_map(|p| {
                                        let ci = p - name_chars;
                                        let start = name.char_indices().nth(ci)?.0;
                                        let len = name[start..].chars().next()?.len_utf8();
                                        Some((start..start + len, HighlightStyle { color: Some(theme::rgb(theme::ACCENT)), ..Default::default() }))
                                    })
                                    .collect();
                                let (icon, color) = crate::assets::file_icon(&name);
                                let selected = ix == this.selected;
                                let path = file.path.clone();
                                div()
                                    .id(ix)
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .h(px(30.))
                                    .px(px(12.))
                                    .mx(px(6.))
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(QuickOpenEvent::Open(path.clone()))))
                                    .child(gpui::svg().path(icon).size(px(14.)).flex_none().text_color(theme::rgb(color)))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(13.))
                                            .text_color(theme::rgb(theme::TEXT))
                                            .child(StyledText::new(SharedString::from(name)).with_highlights(name_highlights)),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(px(12.))
                                            .text_color(theme::rgb(theme::TEXT_FAINT))
                                            .child(dir),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .h(px(380.))
                .py(px(4.)),
            )
    }
}
