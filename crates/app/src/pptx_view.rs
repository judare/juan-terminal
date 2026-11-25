//! PowerPoint viewer/editor: slide list, the current slide drawn from its shapes (fills, text,
//! pictures, tables) and click-to-edit text boxes. Only the current slide's pictures are
//! decoded (cache dropped on slide change), so a big deck stays light.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight, Image, ImageFormat, MouseButton, MouseDownEvent, Render, ScrollHandle, Window, div, img,
    prelude::*, px,
};

use crate::office::pptx::{Align, Anchor, PptxModel, Slide};
use crate::text_area::{TextArea, TextAreaEvent};
use crate::theme;

pub struct PptxView {
    model: Option<PptxModel>,
    error: Option<String>,
    current: usize,
    slide: Option<Slide>,
    titles: Vec<String>,
    images: HashMap<String, Option<Arc<Image>>>,
    /// (shape index on the slide, editor).
    editing: Option<(usize, Entity<TextArea>)>,
    focus: FocusHandle,
    list_scroll: ScrollHandle,
    status: Option<String>,
    /// Full-window slideshow; `true` in .1 when we made the window fullscreen for it.
    pub presenting: bool,
    made_fullscreen: bool,
}

impl PptxView {
    pub fn new(path: PathBuf, focus: FocusHandle, _cx: &mut Context<Self>) -> Self {
        let (model, error) = match PptxModel::load(&path) {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e)),
        };
        let mut this = Self {
            model,
            error,
            current: 0,
            slide: None,
            titles: Vec::new(),
            images: HashMap::new(),
            editing: None,
            focus,
            list_scroll: ScrollHandle::new(),
            status: None,
            presenting: false,
            made_fullscreen: false,
        };
        this.reload_titles();
        this.load_slide();
        this
    }

    pub fn present(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        self.presenting = true;
        if !window.is_fullscreen() {
            window.toggle_fullscreen();
            self.made_fullscreen = true;
        }
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn stop_presenting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.presenting = false;
        if self.made_fullscreen && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        self.made_fullscreen = false;
        cx.notify();
    }

    /// Slideshow navigation; past the last slide ends the show.
    pub fn step(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.titles.len() as i32;
        let next = self.current as i32 + delta;
        if next >= n && delta > 0 {
            self.stop_presenting(window, cx);
            return;
        }
        let next = next.clamp(0, (n - 1).max(0)) as usize;
        if next != self.current {
            self.go(next, window, cx);
        }
    }

    /// ⌘F: next/previous slide containing `q`. Returns a status.
    pub fn find(&mut self, q: &str, back: bool, fresh: bool, window: &mut Window, cx: &mut Context<Self>) -> String {
        let q = q.trim().to_lowercase();
        let Some(m) = &self.model else { return String::new() };
        if q.is_empty() {
            return String::new();
        }
        let hits: Vec<usize> = (0..m.len())
            .filter(|&i| m.slide(i).is_some_and(|s| s.shapes.iter().any(|sh| sh.paras.iter().any(|p| p.text.to_lowercase().contains(&q)) || sh.table.iter().flatten().any(|c| c.to_lowercase().contains(&q)))))
            .collect();
        if hits.is_empty() {
            return "No results".into();
        }
        let cur = self.current;
        let target = if fresh && hits.contains(&cur) {
            cur
        } else if back {
            hits.iter().rev().find(|&&i| i < cur).or(hits.last()).copied().unwrap()
        } else {
            hits.iter().find(|&&i| i > cur).or(hits.first()).copied().unwrap()
        };
        self.go(target, window, cx);
        format!("slide {} · {} of {}", target + 1, hits.iter().position(|&i| i == target).unwrap_or(0) + 1, hits.len())
    }

    pub fn is_dirty(&self) -> bool {
        self.model.as_ref().is_some_and(|m| m.dirty) || self.editing.is_some()
    }

    fn reload_titles(&mut self) {
        let Some(m) = &self.model else { return };
        self.titles = (0..m.len()).map(|i| m.slide(i).map(|s| s.title).unwrap_or_default()).collect();
    }

    fn load_slide(&mut self) {
        let Some(m) = &self.model else { return };
        self.current = self.current.min(m.len().saturating_sub(1));
        self.slide = m.slide(self.current);
        if let Some(t) = self.slide.as_ref().map(|s| s.title.clone()) {
            if let Some(slot) = self.titles.get_mut(self.current) {
                *slot = t;
            }
        }
        // Keep only this slide's pictures.
        let want: Vec<String> = self.slide.iter().flat_map(|s| s.shapes.iter().filter_map(|sh| sh.image.clone())).collect();
        self.images.retain(|k, _| want.contains(k));
        for part in want {
            if self.images.contains_key(&part) {
                continue;
            }
            let fmt = match part.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref() {
                Some("png") => Some(ImageFormat::Png),
                Some("jpg" | "jpeg") => Some(ImageFormat::Jpeg),
                Some("gif") => Some(ImageFormat::Gif),
                Some("svg") => Some(ImageFormat::Svg),
                Some("bmp") => Some(ImageFormat::Bmp),
                Some("tif" | "tiff") => Some(ImageFormat::Tiff),
                Some("webp") => Some(ImageFormat::Webp),
                _ => None,
            };
            let im = fmt.and_then(|f| m.media(&part).map(|b| Arc::new(Image::from_bytes(f, b))));
            self.images.insert(part, im);
        }
    }

    fn go(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        self.current = ix;
        self.load_slide();
        cx.notify();
    }

    fn start_edit(&mut self, sp: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let Some(m) = &self.model else { return };
        let text = m.shape_text(self.current, sp).replace('\u{b}', "\n");
        let area = cx.new(|cx| {
            let mut a = TextArea::new("Text", 1, 30, cx);
            a.set_text(&text, cx);
            a
        });
        cx.subscribe_in(&area, window, |this, _, e: &TextAreaEvent, window, cx| match e {
            TextAreaEvent::Submit => this.commit(window, cx),
            TextAreaEvent::Cancel => {
                this.editing = None;
                window.focus(&this.focus);
                cx.notify();
            }
            TextAreaEvent::Changed | TextAreaEvent::PastedImage(_) => {}
        })
        .detach();
        window.focus(&area.focus_handle(cx));
        self.editing = Some((sp, area));
        cx.notify();
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((sp, area)) = self.editing.take() else { return };
        let text = area.read(cx).content.clone();
        window.focus(&self.focus);
        if let Some(m) = self.model.as_mut() {
            if m.shape_text(self.current, sp).replace('\u{b}', "\n") != text {
                if let Err(e) = m.set_text(self.current, sp, &text) {
                    self.status = Some(e);
                }
                self.load_slide();
            }
        }
        cx.notify();
    }

    pub fn save(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) -> Result<(), String> {
        if let Some(w) = window {
            self.commit(w, cx);
        }
        let Some(m) = self.model.as_mut() else { return Ok(()) };
        m.save()?;
        self.status = Some("Saved".into());
        cx.notify();
        Ok(())
    }

    fn slide_op(&mut self, op: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(window, cx);
        let Some(m) = self.model.as_mut() else { return };
        let i = self.current;
        match op {
            "dup" => match m.duplicate_slide(i) {
                Ok(n) => self.current = n,
                Err(e) => self.status = Some(e),
            },
            "del" => {
                if !m.delete_slide(i) {
                    self.status = Some("A presentation needs at least one slide.".into());
                }
            }
            "up" => {
                if m.move_slide(i, false) {
                    self.current = i - 1;
                }
            }
            _ => {
                if m.move_slide(i, true) {
                    self.current = i + 1;
                }
            }
        }
        self.reload_titles();
        self.load_slide();
        cx.notify();
    }

    fn render_slide(&self, scale: f32, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (Some(m), Some(s)) = (&self.model, &self.slide) else { return div().into_any_element() };
        let (w, h) = (m.width * scale, m.height * scale);
        let mut canvas = div().relative().flex_none().w(px(w)).h(px(h)).overflow_hidden().shadow_lg().bg(gpui::rgb(s.bg.unwrap_or(0xffffff)));
        let dark_bg = s.bg.is_some_and(|c| luma(c) < 0.45);
        for (ix, sh) in s.shapes.iter().enumerate() {
            let r = sh.rect;
            let mut d = div().absolute().left(px(r.x * scale)).top(px(r.y * scale)).w(px(r.w * scale)).h(px(r.h * scale)).flex().flex_col();
            if let Some(f) = sh.fill {
                d = d.bg(gpui::rgb(f));
            }
            if let Some(l) = sh.line {
                d = d.border_1().border_color(gpui::rgb(l));
            }
            if sh.ellipse {
                d = d.rounded_full();
            }
            if let Some(part) = &sh.image {
                d = match self.images.get(part).cloned().flatten() {
                    Some(im) => d.child(img(im).size_full()),
                    None => d.bg(gpui::rgba(0x8080802a)),
                };
            }
            if !sh.table.is_empty() {
                let fs = (12. * scale).max(6.);
                d = d.children(sh.table.iter().map(|row| {
                    div().flex().flex_1().border_b_1().border_color(gpui::rgb(0xbbbbbb)).children(
                        row.iter().map(|c| div().flex_1().px(px(4. * scale)).text_size(px(fs)).text_color(gpui::rgb(if dark_bg { 0xffffff } else { 0x222222 })).overflow_hidden().child(c.clone())),
                    )
                }));
            }
            let editing = self.editing.as_ref().filter(|(sp, _)| Some(*sp) == sh.editable);
            if let Some((_, area)) = editing {
                d = d.child(
                    div()
                        .size_full()
                        .bg(theme::rgb(theme::EDITOR_BG))
                        .border_2()
                        .border_color(theme::rgb(theme::ACCENT))
                        .p(px(4.))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(area.clone()),
                );
            } else if !sh.paras.is_empty() {
                let fill_dark = sh.fill.map(|f| luma(f) < 0.45).unwrap_or(dark_bg);
                let default_color = if fill_dark { 0xffffff } else { 0x1f1f1f };
                d = d
                    .p(px(7. * scale))
                    .when(sh.anchor == Anchor::Middle, |d| d.justify_center())
                    .when(sh.anchor == Anchor::Bottom, |d| d.justify_end())
                    .children(sh.paras.iter().map(|p| {
                        let fs = (p.size * scale).max(4.);
                        let text = if p.text.is_empty() { " ".to_string() } else { p.text.replace('\u{b}', "\n") };
                        let text = if p.bullet { format!("•  {text}") } else { text };
                        div()
                            .w_full()
                            .pl(px(p.level as f32 * 24. * scale))
                            .text_size(px(fs))
                            .line_height(px(fs * 1.2))
                            .text_color(gpui::rgb(p.color.unwrap_or(default_color)))
                            .when(p.bold, |d| d.font_weight(FontWeight::BOLD))
                            .when(p.italic, |d| d.italic())
                            .map(|d| match p.align {
                                Align::Center => d.text_center(),
                                Align::Right => d.text_right(),
                                Align::Left => d,
                            })
                            .child(text)
                    }));
            }
            if let Some(sp) = sh.editable.filter(|_| editing.is_none()) {
                d = d.cursor_text().hover(|s| s.border_1().border_color(theme::rgb(theme::ACCENT))).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        if e.click_count >= 1 {
                            this.start_edit(sp, window, cx);
                        }
                        cx.stop_propagation();
                    }),
                );
            }
            canvas = canvas.child(d.id(("pptx-shape", ix)));
        }
        canvas.into_any_element()
    }
}

fn luma(c: u32) -> f32 {
    let (r, g, b) = (((c >> 16) & 0xff) as f32, ((c >> 8) & 0xff) as f32, (c & 0xff) as f32);
    (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.
}

fn btn(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(22.))
        .px(px(8.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(11.5))
        .text_color(theme::rgb(theme::TEXT_MUTED))
        .hover(|s| s.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT)))
        .child(label)
}

impl Render for PptxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(e) = &self.error {
            return div().flex_1().flex().items_center().justify_center().text_color(theme::rgb(theme::TEXT_MUTED)).child(e.clone()).into_any_element();
        }
        let Some(m) = &self.model else { return div().into_any_element() };
        let n = m.len();
        // Fit the slide into the area right of the list.
        let vw = f32::from(window.viewport_size().width) * 0.62;
        let scale = (vw / m.width).clamp(0.2, 2.5);
        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .px(px(10.))
            .py(px(4.))
            .flex_none()
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .child(btn("pp-present", "▶ Present").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.present(w, cx))))
            .child(div().w(px(1.)).h(px(16.)).mx(px(4.)).bg(theme::rgb(theme::BORDER)))
            .child(btn("pp-dup", "Duplicate slide").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.slide_op("dup", w, cx))))
            .child(btn("pp-del", "Delete slide").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.slide_op("del", w, cx))))
            .child(btn("pp-up", "Move up").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.slide_op("up", w, cx))))
            .child(btn("pp-dn", "Move down").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.slide_op("dn", w, cx))))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child(format!("Click a text box to edit · ⌘↩ apply · Esc cancel{}", if self.is_dirty() { " · unsaved (⌘S)" } else { self.status.as_deref().map(|s| s).unwrap_or("") }.to_string().replace("Saved", " · Saved"))),
            );
        let list = div().id("pp-list").w(px(190.)).flex_none().h_full().overflow_y_scroll().track_scroll(&self.list_scroll).border_r_1().border_color(theme::rgb(theme::BORDER)).bg(theme::rgb(theme::SIDEBAR_BG)).py(px(6.)).children(
            self.titles.iter().enumerate().map(|(i, t)| {
                let on = i == self.current;
                div()
                    .id(("pp-slide", i))
                    .flex()
                    .gap(px(8.))
                    .mx(px(6.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(on, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
                    .when(!on, |d| d.text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.bg(theme::rgb(theme::HOVER))))
                    .child(div().w(px(18.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)).child((i + 1).to_string()))
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().child(if t.is_empty() { "(untitled)".to_string() } else { t.replace('\u{b}', " ") }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.go(i, window, cx)))
            }),
        );
        if self.presenting {
            let vp = window.viewport_size();
            let s = (f32::from(vp.width) / m.width).min(f32::from(vp.height) / m.height);
            let slide = self.render_slide(s, cx);
            let show = div()
                .id("pp-show")
                .w(vp.width)
                .h(vp.height)
                .bg(gpui::black())
                .flex()
                .items_center()
                .justify_center()
                .occlude()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, w, cx| this.step(1, w, cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(|this, _: &MouseDownEvent, w, cx| this.step(-1, w, cx)))
                .child(slide)
                .child(
                    div()
                        .absolute()
                        .bottom(px(10.))
                        .right(px(16.))
                        .text_size(px(12.))
                        .text_color(gpui::rgba(0xffffff66))
                        .child(format!("{} / {n} · ← → · esc", self.current + 1)),
                );
            return div()
                .flex_1()
                .child(gpui::deferred(gpui::anchored().position(gpui::point(px(0.), px(0.))).child(show)).with_priority(10))
                .into_any_element();
        }
        let slide = self.render_slide(scale, cx);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .child(
                        div()
                            .id("pp-stage")
                            .flex_1()
                            .min_w_0()
                            .overflow_scroll()
                            .bg(theme::rgb(0x2a2d32))
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, window, cx| this.commit(window, cx)))
                            .child(div().min_w_full().min_h_full().flex().items_center().justify_center().p(px(24.)).child(slide)),
                    ),
            )
            .child(
                div()
                    .px(px(12.))
                    .py(px(4.))
                    .border_t_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .text_size(px(11.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child(format!("Slide {} of {n}", self.current + 1)),
            )
            .into_any_element()
    }
}
