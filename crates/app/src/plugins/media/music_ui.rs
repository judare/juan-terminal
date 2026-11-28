//! Music player UI: the search overlay (⌘⇧M), the "now playing" card at the bottom of the
//! sidebar, and the large video view (theater).

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Bounds, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton, MouseDownEvent,
    Pixels, Render, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, Window, actions, canvas, div,
    prelude::*, px, uniform_list,
};

use crate::music::{self, Mode, Music, SKIP_SECONDS, Track, fmt_ago, fmt_time, fmt_views};
use crate::text_input::{TextInput, TextInputEvent};
use crate::glass::GlassExt;
use crate::theme;

actions!(music, [SelectNext, SelectPrevious, NextMode, PreviousMode, ModeMusic, ModePodcast, ModeVideos, TogglePlay, Forward, Back, NextTrack, PreviousTrack, CloseTheater]);

pub fn bind_keys(cx: &mut App) {
    let s = Some("MusicSearch");
    let t = Some("MusicTheater");
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, s),
        KeyBinding::new("up", SelectPrevious, s),
        KeyBinding::new("ctrl-n", SelectNext, s),
        KeyBinding::new("ctrl-p", SelectPrevious, s),
        KeyBinding::new("tab", NextMode, s),
        KeyBinding::new("shift-tab", PreviousMode, s),
        KeyBinding::new("cmd-1", ModeMusic, s),
        KeyBinding::new("cmd-2", ModePodcast, s),
        KeyBinding::new("cmd-3", ModeVideos, s),
        KeyBinding::new("space", TogglePlay, t),
        KeyBinding::new("k", TogglePlay, t),
        KeyBinding::new("right", Forward, t),
        KeyBinding::new("left", Back, t),
        KeyBinding::new("l", Forward, t),
        KeyBinding::new("j", Back, t),
        KeyBinding::new("shift-right", NextTrack, t),
        KeyBinding::new("shift-left", PreviousTrack, t),
        KeyBinding::new("escape", CloseTheater, t),
        KeyBinding::new("f", CloseTheater, t),
    ]);
}

const ROW_H: f32 = 56.;
const PANEL_W: f32 = 680.;
/// Panel width minus its 1 px borders and the rows' 6 px side margins.
const ROW_W: f32 = PANEL_W - 2. - 12.;
/// Videos / podcasts: a grid of big thumbnails with the whole title.
const GRID_COLS: usize = 3;
const GRID_GAP: f32 = 8.;
const CELL_W: f32 = (ROW_W - GRID_GAP * (GRID_COLS as f32 - 1.)) / GRID_COLS as f32;
const CELL_THUMB_W: f32 = CELL_W - 12.;
const CELL_THUMB_H: f32 = CELL_THUMB_W * 9. / 16.;
const GRID_ROW_H: f32 = CELL_THUMB_H + 12. + 4. * 16. + 6. + 2. * 15. + 10.;

// ---- shared bits ---------------------------------------------------------------------------------

fn icon(path: &'static str, size: f32, color: u32) -> gpui::Svg {
    gpui::svg().path(path).size(px(size)).flex_none().text_color(theme::rgb(color))
}

fn thumb_el(music: &Music, track: &Track, w: f32, h: f32, radius: f32) -> gpui::AnyElement {
    let base = div().w(px(w)).h(px(h)).flex_none().rounded(px(radius)).overflow_hidden().bg(theme::rgb(theme::SELECTED));
    match music.thumb(&track.id) {
        Some(p) => base.child(gpui::img(p).size_full().object_fit(gpui::ObjectFit::Cover)).into_any_element(),
        None => base.flex().items_center().justify_center().child(icon(track.mode.icon(), (h * 0.4).min(16.), theme::TEXT_FAINT)).into_any_element(),
    }
}

fn round_button(id: impl Into<gpui::ElementId>, path: &'static str, size: f32, icon_size: f32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(size / 2.))
        .cursor_pointer()
        .hover(|s| s.glass_hover())
        .child(icon(path, icon_size, theme::TEXT))
}

/// Filter chip: glass pill, accent-tinted when on.
fn chip(id: &'static str, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(10.))
        .h(px(24.))
        .rounded_full()
        .cursor_pointer()
        .text_size(px(11.5))
        .when(on, |d| d.glass_active().text_color(theme::rgb(theme::ACCENT)))
        .when(!on, |d| d.glass_button().text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.glass_hover().text_color(theme::rgb(theme::TEXT))))
}

/// Selected list row / grid cell: soft brand-red glass.
fn selected_glass<E: Styled>(d: E) -> E {
    d.bg(theme::rgb(theme::ACCENT).opacity(0.12)).border_color(gpui::Hsla { a: 0.35, ..theme::rgb(theme::ACCENT) })
}

fn tip(label: &'static str) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    move |_, cx| cx.new(|_| crate::git_panel::Tooltip(label)).into()
}

/// ⟲15 ⏮ ⏯ ⏭ ⟳15
fn transport(music: &Entity<Music>, m: &Music, big: bool, prefix: &'static str) -> gpui::Div {
    let s = if big { 1.35 } else { 1. };
    let on = |f: fn(&mut Music, &mut Context<Music>)| {
        let music = music.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| music.update(cx, |m, cx| f(m, cx))
    };
    let play_icon = if m.playing { "icons/pause.svg" } else { "icons/play.svg" };
    let accent = m.current().map(|t| t.mode.color()).unwrap_or(theme::ACCENT);
    div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(8. * s))
        .child(round_button(SharedString::from(format!("{prefix}-back")), "icons/back-15.svg", 26. * s, 15. * s).tooltip(tip("−15 s (⌃⌥←)")).on_click(on(|m, cx| m.seek_by(-SKIP_SECONDS, cx))))
        .child(round_button(SharedString::from(format!("{prefix}-prev")), "icons/skip-prev.svg", 26. * s, 13. * s).tooltip(tip("Anterior (⌃⌥⇧←)")).on_click(on(|m, cx| m.previous(cx))))
        .child(
            div()
                .id(SharedString::from(format!("{prefix}-play")))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(38. * s))
                .rounded(px(19. * s))
                .cursor_pointer()
                .bg(gpui::linear_gradient(
                    180.,
                    gpui::linear_color_stop(gpui::Hsla { l: (theme::rgb(accent).l + 0.06).min(1.), ..theme::rgb(accent) }, 0.),
                    gpui::linear_color_stop(theme::rgb(accent), 1.),
                ))
                .border_1()
                .border_color(gpui::hsla(0., 0., 1., 0.18))
                .shadow(vec![gpui::BoxShadow { color: gpui::Hsla { a: 0.35, ..theme::rgb(accent) }, offset: gpui::point(px(0.), px(3.)), blur_radius: px(10.), spread_radius: px(0.) }])
                .hover(|st| st.opacity(0.9))
                .child(gpui::svg().path(play_icon).size(px(15. * s)).text_color(gpui::white()))
                .tooltip(tip("Reproducir / pausa (⌃⌥Space)"))
                .on_click(on(|m, cx| m.toggle(cx))),
        )
        .child(
            round_button(SharedString::from(format!("{prefix}-next")), "icons/skip-next.svg", 26. * s, 13. * s)
                .when(!m.has_next(), |d| d.opacity(0.4))
                .tooltip(tip("Siguiente (⌃⌥⇧→)"))
                .on_click(on(|m, cx| m.next(cx))),
        )
        .child(round_button(SharedString::from(format!("{prefix}-fwd")), "icons/fwd-15.svg", 26. * s, 15. * s).tooltip(tip("+15 s (⌃⌥→)")).on_click(on(|m, cx| m.seek_by(SKIP_SECONDS, cx))))
}

/// Status line for the player: error or loading.
fn player_status(m: &Music) -> Option<String> {
    if let Some(e) = &m.error {
        Some(e.clone())
    } else if m.resolving || m.buffering {
        Some("Cargando…".into())
    } else {
        None
    }
}

/// Just the seek bar (click to seek), thicker on hover.
fn seek_bar(music: &Entity<Music>, m: &Music, id: &'static str) -> gpui::Stateful<gpui::Div> {
    let frac = if m.duration > 0. { (m.position / m.duration).clamp(0., 1.) as f32 } else { 0. };
    let accent = m.current().map(|t| t.mode.color()).unwrap_or(theme::ACCENT);
    let bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
    let b2 = bounds.clone();
    let music = music.clone();
    let duration = m.duration;
    div()
        .id(id)
        .relative()
        .h(px(12.))
        .flex()
        .items_center()
        .cursor_pointer()
        .group("seek")
        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            if let Some(b) = b2.get() {
                let x = f32::from(e.position.x - b.origin.x) / f32::from(b.size.width).max(1.);
                if duration > 0. {
                    music.update(cx, |m, cx| m.seek_to(duration * x.clamp(0., 1.) as f64, cx));
                }
            }
        })
        .child(canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {}).absolute().size_full())
        .child(
            div()
                .w_full()
                .h(px(4.))
                .group_hover("seek", |s| s.h(px(6.)))
                .rounded(px(3.))
                .bg(gpui::hsla(0., 0., 1., 0.12))
                .child(div().h_full().rounded(px(3.)).bg(theme::rgb(accent)).w(gpui::relative(frac))),
        )
}

// ---- search overlay ------------------------------------------------------------------------------

pub enum MusicSearchEvent {
    Dismiss,
    /// Something started playing (the overlay closes).
    Played,
    /// Open a file in the editor (the preferences).
    OpenFile(std::path::PathBuf),
}

pub struct MusicSearch {
    music: Entity<Music>,
    input: Entity<TextInput>,
    mode: Mode,
    /// Popularity filter on (per mode).
    filtered: HashMap<Mode, bool>,
    cache: HashMap<(Mode, String, bool), Result<Vec<Track>, String>>,
    searching: Option<(Mode, String, bool)>,
    visible: Vec<Track>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _search: Option<Task<()>>,
    _debounce: Option<Task<()>>,
    /// First ids of the list whose audio URLs were last resolved ahead of time.
    prefetched: Vec<String>,
    /// "Borrar descargas" clicked once: the next click deletes.
    confirm_clear: bool,
    /// Per mode: list only what plays offline (the query filters it locally).
    only_downloaded: HashMap<Mode, bool>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<MusicSearchEvent> for MusicSearch {}

/// Mode picked last (kept for the session).
static LAST_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

impl MusicSearch {
    pub fn new(music: Entity<Music>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mode = Mode::ALL[LAST_MODE.load(std::sync::atomic::Ordering::Relaxed) as usize % 3];
        let input = cx.new(|cx| TextInput::new(mode.placeholder(), cx));
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
                TextInputEvent::Changed => this.schedule_search(cx),
                TextInputEvent::Submit { .. } => this.submit(cx),
                TextInputEvent::Cancel => cx.emit(MusicSearchEvent::Dismiss),
            }),
            cx.observe(&music, |_, _, cx| cx.notify()),
        ];
        window.focus(&input.focus_handle(cx));
        music.update(cx, |m, cx| {
            m.ensure_suggestions(mode, false, cx);
            m.refresh_disk_usage(cx);
        });
        Self {
            music,
            input,
            mode,
            filtered: HashMap::new(),
            cache: HashMap::new(),
            searching: None,
            visible: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _search: None,
            _debounce: None,
            prefetched: Vec::new(),
            confirm_clear: false,
            only_downloaded: HashMap::new(),
            _subs: subs,
        }
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).content.trim().to_string()
    }

    fn grid(&self) -> bool {
        self.mode != Mode::Music
    }

    /// The list row holding result `ix` (grid rows hold several).
    fn row_of(&self, ix: usize) -> usize {
        if self.grid() { ix / GRID_COLS } else { ix }
    }

    fn is_filtered(&self, mode: Mode) -> bool {
        mode.min_views().is_some() && self.filtered.get(&mode).copied().unwrap_or(true)
    }

    fn key(&self, cx: &App) -> (Mode, String, bool) {
        (self.mode, self.query(cx).to_lowercase(), self.is_filtered(self.mode))
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        LAST_MODE.store(Mode::ALL.iter().position(|m| *m == mode).unwrap_or(0) as u8, std::sync::atomic::Ordering::Relaxed);
        self.input.update(cx, |i, cx| i.set_placeholder(mode.placeholder(), cx));
        self.music.update(cx, |m, cx| m.ensure_suggestions(mode, false, cx));
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.run_search(cx);
        cx.notify();
    }

    fn cycle_mode(&mut self, delta: isize, cx: &mut Context<Self>) {
        let i = Mode::ALL.iter().position(|m| *m == self.mode).unwrap_or(0) as isize;
        self.set_mode(Mode::ALL[(i + delta).rem_euclid(3) as usize], cx);
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.selected = 0;
        self._debounce = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(Duration::from_millis(450)).await;
            let _ = this.update(cx, |s, cx| s.run_search(cx));
        }));
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        self._debounce = None;
        let key = self.key(cx);
        if self.is_only_downloaded(self.mode) || key.1.is_empty() || self.cache.contains_key(&key) || self.searching.as_ref() == Some(&key) {
            return;
        }
        self.searching = Some(key.clone());
        let query = self.query(cx);
        let music = self.music.clone();
        self._search = Some(cx.spawn(async move |this, cx| {
            let (mode, filtered) = (key.0, key.2);
            let res = cx.background_executor().spawn(async move { music::search(mode, &query, filtered) }).await;
            let _ = this.update(cx, |s, cx| {
                if let Ok(tracks) = &res {
                    music.update(cx, |m, cx| m.ensure_thumbs(tracks, cx));
                }
                s.cache.insert(key.clone(), res);
                if s.searching.as_ref() == Some(&key) {
                    s.searching = None;
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let key = self.key(cx);
        if !self.is_only_downloaded(self.mode) && !key.1.is_empty() && !self.cache.contains_key(&key) {
            self.run_search(cx);
            return;
        }
        self.play(self.selected, cx);
    }

    fn play(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.visible.len() {
            return;
        }
        let queue = self.visible.clone();
        let videos = self.mode == Mode::Videos;
        self.music.update(cx, |m, cx| {
            m.play_queue(queue, ix, cx);
            if videos {
                m.set_theater(true, cx);
            }
        });
        cx.emit(MusicSearchEvent::Played);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let n = self.visible.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
        self.scroll.scroll_to_item(self.row_of(self.selected), ScrollStrategy::Center);
        let ahead: Vec<Track> = self.visible.iter().skip(self.selected).take(2).cloned().collect();
        self.music.update(cx, |m, cx| m.prefetch(&ahead, cx));
        cx.notify();
    }

    fn is_only_downloaded(&self, mode: Mode) -> bool {
        self.only_downloaded.get(&mode).copied().unwrap_or(false)
    }

    fn toggle_only_downloaded(&mut self, cx: &mut Context<Self>) {
        let on = !self.is_only_downloaded(self.mode);
        self.only_downloaded.insert(self.mode, on);
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        if on {
            self.music.update(cx, |m, cx| m.refresh_disk_usage(cx));
        } else {
            self.run_search(cx);
        }
        cx.notify();
    }

    fn toggle_filter(&mut self, cx: &mut Context<Self>) {
        let on = !self.is_filtered(self.mode);
        self.filtered.insert(self.mode, on);
        self.selected = 0;
        self.run_search(cx);
        cx.notify();
    }
}

impl Focusable for MusicSearch {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for MusicSearch {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key = self.key(cx);
        let music_entity = self.music.clone();
        let music = music_entity.read(cx);
        // What the list shows: search results, or suggestions while the query is empty.
        let mut header: Option<String> = None;
        let mut status: Option<(String, bool)> = None;
        let only_downloaded = self.is_only_downloaded(self.mode);
        if only_downloaded {
            let all = music.downloaded_tracks(self.mode);
            let total = all.len();
            let q = key.1.clone();
            self.visible = all.into_iter().filter(|t| q.is_empty() || t.title.to_lowercase().contains(&q) || t.channel.to_lowercase().contains(&q)).collect();
            header = Some(if q.is_empty() { format!("{total} descargados") } else { format!("{} de {total} descargados", self.visible.len()) });
            if self.visible.is_empty() {
                status = Some((if total == 0 { "Nada descargado en esta categoría todavía." } else { "Nada descargado coincide con la búsqueda." }.into(), false));
            }
        } else if key.1.is_empty() {
            // Saved / downloaded first (they also play offline), then the suggestions.
            let library = music.library_tracks(self.mode);
            let saved: std::collections::HashSet<String> = library.iter().map(|t| t.id.clone()).collect();
            self.visible = library;
            match music.suggestions(self.mode) {
                Some(s) => {
                    let failed = s.header.starts_with("Sin sugerencias");
                    header = Some(match (failed, saved.is_empty()) {
                        (true, false) => "Sin conexión · tus descargas y guardados".into(),
                        (false, false) => format!("{} guardados · {}", saved.len(), s.header),
                        _ => s.header.clone(),
                    });
                    self.visible.extend(s.tracks.iter().filter(|t| !saved.contains(&t.id)).cloned());
                }
                None if !saved.is_empty() => header = Some(format!("{} guardados", saved.len())),
                None => {}
            }
            if music.is_suggesting(self.mode) && self.visible.is_empty() {
                status = Some(("Preparando sugerencias…".into(), false));
            }
        } else {
            // Downloads that match show from the first keystroke (they play offline), on top
            // of the search results once those arrive.
            let q = key.1.clone();
            let local: Vec<Track> = music.downloaded_tracks(self.mode).into_iter().filter(|t| t.title.to_lowercase().contains(&q) || t.channel.to_lowercase().contains(&q)).collect();
            let local_ids: std::collections::HashSet<String> = local.iter().map(|t| t.id.clone()).collect();
            let n_local = local.len();
            match self.cache.get(&key) {
                Some(Ok(tracks)) => {
                    self.visible = local;
                    self.visible.extend(tracks.iter().filter(|t| !local_ids.contains(&t.id)).cloned());
                    if self.visible.is_empty() {
                        status = Some((
                            if key.2 { "Nada supera el filtro de reproducciones. Desactívalo para ver todo." } else { "Sin resultados." }.into(),
                            false,
                        ));
                    }
                }
                Some(Err(e)) => {
                    self.visible = local;
                    if self.visible.is_empty() {
                        status = Some((e.clone(), true));
                    }
                }
                None => {
                    // Only the matching downloads until the search answers.
                    self.visible = local;
                }
            }
            let downloaded = if n_local > 0 { format!(" · {n_local} descargados") } else { String::new() };
            if self.searching.is_some() || self._debounce.is_some() {
                header = Some(format!("Buscando…{downloaded}"));
            } else if !self.visible.is_empty() {
                header = Some(format!("{} resultados{downloaded}", self.visible.len()));
            }
        }
        if self.selected >= self.visible.len() {
            self.selected = 0;
        }
        let grid = self.grid();
        let count = if grid { self.visible.len().div_ceil(GRID_COLS) } else { self.visible.len() };
        let current_id = music.current().map(|t| t.id.clone());
        let renewing = music.renewing;
        let usage = music.disk_usage();
        let suggesting = music.is_suggesting(self.mode);
        // Resolve the top results' audio URLs now, so a click starts at once.
        let top: Vec<String> = self.visible.iter().take(3).map(|t| t.id.clone()).collect();
        if top != self.prefetched {
            self.prefetched = top;
            let ahead: Vec<Track> = self.visible.iter().take(3).cloned().collect();
            let m = self.music.clone();
            cx.defer(move |cx| m.update(cx, |m, cx| m.prefetch(&ahead, cx)));
        }
        let updated = music::preferences_updated();
        let filtered = self.is_filtered(self.mode);
        let mode = self.mode;

        let mode_tabs = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .p(px(3.))
            .rounded_full()
            .bg(gpui::hsla(0., 0., 0., if theme::is_light() { 0.05 } else { 0.25 }))
            .border_1()
            .border_color(crate::glass::edge())
            .children(Mode::ALL.iter().enumerate().map(|(i, &m)| {
            let active = m == mode;
            div()
                .id(("music-mode", i))
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(12.))
                .h(px(24.))
                .rounded_full()
                .cursor_pointer()
                .text_size(px(12.5))
                .when(active, |d| d.glass_button().text_color(theme::rgb(theme::TEXT)).font_weight(gpui::FontWeight::MEDIUM))
                .when(!active, |d| d.text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.text_color(theme::rgb(theme::TEXT))))
                .child(gpui::svg().path(m.icon()).size(px(12.)).text_color(theme::rgb(if active { theme::ACCENT } else { theme::TEXT_FAINT })))
                .child(m.label())
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_mode(m, cx)))
        }));

        let downloaded_chip = chip("music-only-downloaded", only_downloaded)
            .child(icon(if only_downloaded { "icons/check.svg" } else { "icons/pull.svg" }, 11., if only_downloaded { theme::ACCENT } else { theme::TEXT_MUTED }))
            .child("Descargados")
            .tooltip(tip("Ver solo lo que suena sin internet en esta categoría"))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_only_downloaded(cx)));

        let filter_chip = (!only_downloaded).then_some(()).and(mode.min_views()).map(|min| {
            let label = if min >= 1_000_000 { "+1M reproducciones" } else { "+5k reproducciones" };
            chip("music-filter", filtered)
                .child(icon(if filtered { "icons/check.svg" } else { "icons/close.svg" }, 10., if filtered { theme::ACCENT } else { theme::TEXT_FAINT }))
                .child(label)
                .tooltip(tip("Filtro automático por popularidad (clic para quitarlo)"))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_filter(cx)))
        });

        div()
            .key_context("MusicSearch")
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &NextMode, _, cx| this.cycle_mode(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousMode, _, cx| this.cycle_mode(-1, cx)))
            .on_action(cx.listener(|this, _: &ModeMusic, _, cx| this.set_mode(Mode::Music, cx)))
            .on_action(cx.listener(|this, _: &ModePodcast, _, cx| this.set_mode(Mode::Podcast, cx)))
            .on_action(cx.listener(|this, _: &ModeVideos, _, cx| this.set_mode(Mode::Videos, cx)))
            .w(px(PANEL_W))
            .flex()
            .flex_col()
            .bg({
                let base = theme::rgb(theme::SIDEBAR_BG);
                gpui::linear_gradient(180., gpui::linear_color_stop(gpui::Hsla { l: (base.l + 0.03).min(1.), a: 0.97, ..base }, 0.), gpui::linear_color_stop(gpui::Hsla { a: 0.97, ..base }, 1.))
            })
            .border_1()
            .border_color(crate::glass::edge())
            .rounded(px(18.))
            .shadow(vec![
                gpui::BoxShadow { color: gpui::hsla(0., 0., 0., 0.45), offset: gpui::point(px(0.), px(18.)), blur_radius: px(48.), spread_radius: px(0.) },
                gpui::BoxShadow { color: gpui::hsla(0., 0., 0., 0.25), offset: gpui::point(px(0.), px(2.)), blur_radius: px(6.), spread_radius: px(0.) },
            ])
            .overflow_hidden()
            .child(
                div().px(px(12.)).pt(px(12.)).pb(px(10.)).child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(14.))
                        .h(px(40.))
                        .rounded_full()
                        .glass()
                        .child(icon("icons/search.svg", 15., theme::TEXT_MUTED))
                        .child(div().flex_1().text_size(px(14.)).child(self.input.clone())),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .pb(px(10.))
                    .child(mode_tabs)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(downloaded_chip)
                            .children(filter_chip)
                            .child(
                                round_button("music-refresh", "icons/undo.svg", 24., 12.)
                                    .when(suggesting, |d| d.opacity(0.4))
                                    .tooltip(tip("Rehacer sugerencias con otros temas"))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.input.update(cx, |i, cx| i.set_text("", cx));
                                        this.music.update(cx, |m, cx| m.ensure_suggestions(mode, true, cx));
                                    })),
                            )
                            .child(
                                round_button("music-prefs", "icons/gear.svg", 24., 12.)
                                    .tooltip(tip("Editar preferencias (temas por modo)"))
                                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(MusicSearchEvent::OpenFile(music::ensure_preferences_file())))),
                            )
                            .child(
                                round_button("music-claude", "icons/sparkle.svg", 24., 12.)
                                    .when(renewing, |d| d.opacity(0.4))
                                    .tooltip(tip("Renovar preferencias con Claude (la tarea semanal, ahora)"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.music.update(cx, |m, cx| m.renew_preferences(cx)))),
                            ),
                    ),
            )
            .child({
                // "Para ti · tema · tema…": the label, then each topic as a small glass chip.
                let text = header.unwrap_or_else(|| if key.1.is_empty() { "Sugerencias".into() } else { String::new() });
                let suggesting_row = key.1.is_empty() && !only_downloaded;
                let mut parts = text.split(" · ").map(str::to_string);
                let first = parts.next().unwrap_or_default();
                let rest: Vec<String> = if suggesting_row { parts.collect() } else { Vec::new() };
                let label = if suggesting_row { first } else { text.clone() };
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(16.))
                    .pt(px(8.))
                    .pb(px(6.))
                    .border_t_1()
                    .border_color(crate::glass::edge())
                    .overflow_hidden()
                    .text_size(px(11.5))
                    .text_color(theme::rgb(theme::TEXT_MUTED))
                    .when(suggesting_row, |d| d.child(icon("icons/sparkle.svg", 11., theme::ACCENT)))
                    .child(div().flex_none().font_weight(gpui::FontWeight::MEDIUM).text_color(theme::rgb(theme::TEXT)).child(label))
                    .children(rest.into_iter().map(|t| div().flex_none().px(px(8.)).py(px(1.)).rounded_full().glass_button().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(t)))
            })
            .when_some(status, |d, (st, err)| {
                d.child(div().px(px(16.)).py(px(14.)).text_size(px(12.5)).text_color(theme::rgb(if err { theme::DELETED } else { theme::TEXT_MUTED })).child(st))
            })
            .child(
                uniform_list(
                    "music-results",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        if grid {
                            return range
                                .map(|row| {
                                    let cells: Vec<gpui::AnyElement> = (row * GRID_COLS..((row + 1) * GRID_COLS).min(this.visible.len())).map(|ix| grid_cell(this, ix, current_id.as_deref(), cx)).collect();
                                    div().id(("music-grid-row", row)).flex().gap(px(GRID_GAP)).w(px(ROW_W)).h(px(GRID_ROW_H)).mx(px(6.)).children(cells).into_any_element()
                                })
                                .collect::<Vec<_>>();
                        }
                        let music = this.music.read(cx);
                        range
                            .filter_map(|ix| {
                                let t = this.visible.get(ix)?;
                                let selected = ix == this.selected;
                                let playing = current_id.as_deref() == Some(t.id.as_str());
                                let progress = music.progress_of(&t.id).cloned();
                                let rating = music.rating(&t.id);
                                let local = music.has_local_copy(&t.id);
                                let mut meta: Vec<String> = Vec::new();
                                if let Some(p) = progress.as_ref().filter(|p| !p.finished) {
                                    meta.push(format!("Continuar · quedan {}", fmt_time(p.dur - p.pos)));
                                }
                                if !t.channel.is_empty() {
                                    meta.push(t.channel.clone());
                                }
                                if let Some(v) = t.views {
                                    meta.push(format!("{} reproducciones", fmt_views(v)));
                                }
                                if !playing && progress.as_ref().is_some_and(|p| p.finished) {
                                    meta.push("Visto".into());
                                }
                                // The preferences topic it came from, folded into the same line.
                                if let Some(topic) = &t.topic {
                                    meta.push(topic.clone());
                                }
                                Some(
                                    div()
                                        .id(ix)
                                        .group("music-row")
                                        .flex()
                                        .items_center()
                                        .gap(px(12.))
                                        // Fixed width: the list's rows would otherwise size to their content.
                                        .w(px(ROW_W))
                                        .h(px(ROW_H))
                                        .px(px(8.))
                                        .mx(px(6.))
                                        .rounded(px(12.))
                                        .border_1()
                                        .border_color(gpui::transparent_black())
                                        .cursor_pointer()
                                        .when(selected, selected_glass)
                                        .when(!selected, |d| d.hover(|s| s.glass_hover()))
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.play(ix, cx)))
                                        .child(
                                            div()
                                                .relative()
                                                .rounded(px(8.))
                                                .overflow_hidden()
                                                .shadow(crate::glass::shadow_sm())
                                                .child(thumb_el(music, t, 76., 43., 8.))
                                                .when_some(progress.as_ref().filter(|p| !p.finished).map(|p| p.fraction()), |d, f| {
                                                    d.child(
                                                        div()
                                                            .absolute()
                                                            .bottom_0()
                                                            .left_0()
                                                            .w(px(76.))
                                                            .h(px(3.))
                                                            .bg(gpui::hsla(0., 0., 1., 0.25))
                                                            .child(div().h_full().w(gpui::relative(f)).bg(theme::rgb(t.mode.color()))),
                                                    )
                                                })
                                                .when_some(t.duration, |d, dur| {
                                                    d.child(
                                                        div()
                                                            .absolute()
                                                            .bottom(px(3.))
                                                            .right(px(3.))
                                                            .px(px(4.))
                                                            .rounded(px(3.))
                                                            .bg(gpui::hsla(0., 0., 0., 0.75))
                                                            .text_size(px(9.5))
                                                            .text_color(gpui::white())
                                                            .child(fmt_time(dur)),
                                                    )
                                                }),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .gap(px(2.))
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap(px(6.))
                                                        .min_w_0()
                                                        .when(playing, |d| d.child(icon("icons/music.svg", 11., theme::ACCENT)))
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .truncate()
                                                                .text_size(px(13.))
                                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                                .text_color(theme::rgb(if playing { theme::ACCENT } else { theme::TEXT }))
                                                                .child(t.title.clone()),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .overflow_hidden()
                                                        .whitespace_nowrap()
                                                        .text_ellipsis()
                                                        .text_size(px(11.5))
                                                        .text_color(theme::rgb(theme::TEXT_FAINT))
                                                        .child(meta.join(" · ")),
                                                ),
                                        )
                                        .child({
                                            // 👍 / 👎 (the chosen one stays visible) and 🗑 for local copies.
                                            let rate = [true, false].map(|good| {
                                                let music = this.music.clone();
                                                let track = t.clone();
                                                let (path, label) = rate_icon(good, rating);
                                                let color = if rating == Some(good) { theme::ACCENT } else { theme::TEXT_MUTED };
                                                row_action((if good { "music-row-up" } else { "music-row-down" }, ix), path, label, color, rating == Some(good)).on_click(move |_: &ClickEvent, _, cx| {
                                                    cx.stop_propagation();
                                                    music.update(cx, |m, cx| m.rate(&track, good, cx));
                                                })
                                            });
                                            let drop_topic = t.topic.clone().map(|topic| {
                                                let (mode, music) = (t.mode, this.music.clone());
                                                row_action(("music-drop-topic", ix), "icons/close.svg", "No sugerir más este tema", theme::TEXT_MUTED, false).on_click(move |_: &ClickEvent, _, cx| {
                                                    cx.stop_propagation();
                                                    music.update(cx, |m, cx| m.drop_topic(mode, &topic, cx));
                                                })
                                            });
                                            let music = this.music.clone();
                                            let id = t.id.clone();
                                            div().flex().flex_none().items_center().gap(px(2.)).children(rate).children(drop_topic).when(local, |d| {
                                                d.child(row_action(("music-row-trash", ix), "icons/trash.svg", "Borrar la descarga (libera espacio)", theme::TEXT_MUTED, false).on_click(move |_: &ClickEvent, _, cx| {
                                                    cx.stop_propagation();
                                                    music.update(cx, |m, cx| m.remove_download(&id, cx));
                                                }))
                                            })
                                        })
                                        .into_any_element(),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .h(px(if grid { 2. * GRID_ROW_H + 30. } else { 420. }))
                .pb(px(6.)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(14.))
                    .h(px(32.))
                    .border_t_1()
                    .border_color(crate::glass::edge())
                    .text_size(px(10.5))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(10.))
                            .when_some(usage.filter(|b| *b > 0), |d, bytes| {
                                let confirming = self.confirm_clear;
                                d.child(
                                    div()
                                        .id("music-clear")
                                        .flex()
                                        .items_center()
                                        .gap(px(4.))
                                        .px(px(6.))
                                        .h(px(20.))
                                        .rounded_full()
                                        .cursor_pointer()
                                        .hover(|s| s.glass_hover())
                                        .when(confirming, |d| d.text_color(theme::rgb(theme::DELETED)))
                                        .child(icon("icons/trash.svg", 11., if confirming { theme::DELETED } else { theme::TEXT_FAINT }))
                                        .child(if confirming { format!("¿Borrar {}? (no toca «Ver después»)", fmt_bytes(bytes)) } else { format!("{} descargados", fmt_bytes(bytes)) })
                                        .tooltip(tip("Borrar las descargas y la caché de canciones (lo guardado en «Ver después» se queda)"))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            if this.confirm_clear {
                                                this.confirm_clear = false;
                                                this.music.update(cx, |m, cx| m.clear_downloads(cx));
                                            } else {
                                                this.confirm_clear = true;
                                            }
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(if renewing {
                                "Claude está renovando tus preferencias…".to_string()
                            } else {
                                updated.map(|u| format!("Preferencias del {u}")).unwrap_or_default()
                            }),
                    ),
            )
    }
}

/// One video / podcast in the grid: big thumbnail, whole title, channel, views and age.
fn grid_cell(this: &MusicSearch, ix: usize, current_id: Option<&str>, cx: &mut Context<MusicSearch>) -> gpui::AnyElement {
    let music = this.music.read(cx);
    let Some(t) = this.visible.get(ix) else { return div().into_any_element() };
    let selected = ix == this.selected;
    let playing = current_id == Some(t.id.as_str());
    let progress = music.progress_of(&t.id).cloned();
    let mut meta: Vec<String> = Vec::new();
    if let Some(p) = progress.as_ref().filter(|p| !p.finished) {
        meta.push(format!("quedan {}", fmt_time(p.dur - p.pos)));
    }
    if let Some(v) = t.views {
        meta.push(format!("{} reproducciones", fmt_views(v)));
    }
    if let Some(ts) = t.published {
        meta.push(fmt_ago(ts));
    }
    let badge = |label: &'static str, path: &'static str, bg: gpui::Hsla| {
        div()
            .absolute()
            .top(px(4.))
            .left(px(4.))
            .flex()
            .items_center()
            .gap(px(3.))
            .px(px(5.))
            .h(px(17.))
            .rounded(px(4.))
            .bg(bg)
            .text_size(px(10.))
            .text_color(gpui::white())
            .child(gpui::svg().path(path).size(px(10.)).text_color(gpui::white()))
            .child(label)
    };
    let thumb = div()
        .relative()
        .child(thumb_el(music, t, CELL_THUMB_W, CELL_THUMB_H, 7.))
        .when_some(progress.as_ref().filter(|p| !p.finished).map(|p| p.fraction()), |d, f| {
            d.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w(px(CELL_THUMB_W))
                    .h(px(3.))
                    .bg(gpui::hsla(0., 0., 1., 0.25))
                    .child(div().h_full().w(gpui::relative(f)).bg(theme::rgb(t.mode.color()))),
            )
        })
        .when_some(t.duration, |d, dur| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(5.))
                    .right(px(5.))
                    .px(px(4.))
                    .rounded(px(3.))
                    .bg(gpui::hsla(0., 0., 0., 0.75))
                    .text_size(px(10.5))
                    .text_color(gpui::white())
                    .child(fmt_time(dur)),
            )
        })
        .when(playing, |d| d.child(badge("Sonando", "icons/music.svg", theme::rgb(t.mode.color()))))
        .when(!playing && progress.as_ref().is_some_and(|p| p.finished), |d| d.child(badge("Visto", "icons/check.svg", gpui::hsla(0., 0., 0., 0.7))))
        .when_some(offline_badge(music.offline(&t.id)), |d, b| d.child(b))
        .child(cell_actions(this, t, ix, music.is_later(&t.id), music.offline(&t.id), music.rating(&t.id)));
    div()
        .id(("music-cell", ix))
        .group("music-cell")
        .flex()
        .flex_col()
        .gap(px(6.))
        .w(px(CELL_W))
        .h(px(GRID_ROW_H - 6.))
        .p(px(6.))
        .rounded(px(12.))
        .border_1()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .when(selected, selected_glass)
        .when(!selected, |d| d.hover(|s| s.glass_hover()))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.play(ix, cx)))
        .child(thumb)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(2.))
                .child(
                    div()
                        .text_size(px(12.5))
                        .line_height(px(16.))
                        .line_clamp(4)
                        .text_color(theme::rgb(if playing { t.mode.color() } else { theme::TEXT }))
                        .when(playing, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                        .child(t.title.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .text_size(px(11.))
                        .line_height(px(15.))
                        .text_color(theme::rgb(theme::TEXT_FAINT))
                        .when(!t.channel.is_empty(), |d| d.child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().child(t.channel.clone())))
                        .when(!meta.is_empty(), |d| d.child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().child(meta.join(" · ")))),
                ),
        )
        .into_any_element()
}

/// Bottom-left of a thumbnail: "Offline" when downloaded, the % while downloading.
fn offline_badge(state: music::Offline) -> Option<gpui::Div> {
    let (label, path, bg) = match state {
        music::Offline::None => return None,
        music::Offline::Ready => ("Offline".to_string(), "icons/check.svg", gpui::hsla(0.39, 0.55, 0.32, 0.92)),
        music::Offline::Downloading(f) => (format!("{:.0}%", f * 100.), "icons/pull.svg", gpui::hsla(0., 0., 0., 0.75)),
        music::Offline::Failed => ("Falló".to_string(), "icons/close.svg", gpui::hsla(0.98, 0.6, 0.4, 0.9)),
    };
    Some(
        div()
            .absolute()
            .bottom(px(7.))
            .left(px(5.))
            .flex()
            .items_center()
            .gap(px(3.))
            .px(px(5.))
            .h(px(17.))
            .rounded(px(4.))
            .bg(bg)
            .text_size(px(10.))
            .text_color(gpui::white())
            .child(gpui::svg().path(path).size(px(10.)).text_color(gpui::white()))
            .child(label),
    )
}

/// Top-right of a thumbnail: ✕ topic, ⬇ download / remove, 🔖 "Ver después" (shown on hover;
/// the bookmark stays visible while saved).
fn cell_actions(this: &MusicSearch, t: &Track, ix: usize, later: bool, offline: music::Offline, rating: Option<bool>) -> gpui::Div {
    let button = |id: (&'static str, usize), path: &'static str, label: &'static str, always: bool| {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(22.))
            .rounded(px(11.))
            .cursor_pointer()
            .bg(gpui::hsla(0., 0., 0., 0.7))
            .when(!always, |d| d.invisible().group_hover("music-cell", |s| s.visible()))
            .hover(|s| s.bg(gpui::hsla(0., 0., 0., 0.92)))
            .child(gpui::svg().path(path).size(px(11.)).text_color(gpui::white()))
            .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(label)).into())
    };
    let music = this.music.clone();
    let track = t.clone();
    let later_btn = button(("music-later", ix), if later { "icons/bookmark-fill.svg" } else { "icons/bookmark.svg" }, if later { "Quitar de «Ver después»" } else { "Ver después (se descarga para verlo sin internet)" }, later)
        .on_click(move |_: &ClickEvent, _, cx| {
            cx.stop_propagation();
            music.update(cx, |m, cx| m.toggle_later(&track, cx));
        });
    let music = this.music.clone();
    let track = t.clone();
    let (path, label) = match offline {
        music::Offline::Ready => ("icons/trash.svg", "Borrar la descarga"),
        music::Offline::Downloading(_) => ("icons/close.svg", "Cancelar la descarga"),
        music::Offline::None | music::Offline::Failed => ("icons/pull.svg", "Descargar para ver sin internet"),
    };
    let download_btn = button(("music-download", ix), path, label, false).on_click(move |_: &ClickEvent, _, cx| {
        cx.stop_propagation();
        music.update(cx, |m, cx| match offline {
            music::Offline::Ready | music::Offline::Downloading(_) => m.remove_download(&track.id, cx),
            _ => m.download(&track, cx),
        });
    });
    div()
        .absolute()
        .top(px(4.))
        .right(px(4.))
        .flex()
        .gap(px(4.))
        .when_some(t.topic.clone(), |d, topic| {
            let mode = t.mode;
            let music = this.music.clone();
            d.child(button(("music-drop-topic", ix), "icons/close.svg", "No sugerir más este tema", false).on_click(move |_: &ClickEvent, _, cx| {
                cx.stop_propagation();
                music.update(cx, |m, cx| m.drop_topic(mode, &topic, cx));
            }))
        })
        .children([true, false].map(|good| {
            let music = this.music.clone();
            let track = t.clone();
            let (path, label) = rate_icon(good, rating);
            button((if good { "music-rate-up" } else { "music-rate-down" }, ix), path, label, rating == Some(good)).on_click(move |_: &ClickEvent, _, cx| {
                cx.stop_propagation();
                music.update(cx, |m, cx| m.rate(&track, good, cx));
            })
        }))
        .child(download_btn)
        .child(later_btn)
}

/// 👍 / 👎 icon and tooltip given the current rating.
fn rate_icon(good: bool, rating: Option<bool>) -> (&'static str, &'static str) {
    match (good, rating == Some(good)) {
        (true, false) => ("icons/thumb-up.svg", "Me gusta (se anota en personal brain)"),
        (true, true) => ("icons/thumb-up-fill.svg", "Quitar «me gusta»"),
        (false, false) => ("icons/thumb-down.svg", "No me gusta (no se vuelve a sugerir)"),
        (false, true) => ("icons/thumb-down-fill.svg", "Quitar «no me gusta»"),
    }
}

/// Small round action at the right of a list row: shown on hover, or always when `always`.
fn row_action(id: (&'static str, usize), path: &'static str, label: &'static str, color: u32, always: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded(px(11.))
        .cursor_pointer()
        .when(!always, |d| d.invisible().group_hover("music-row", |s| s.visible()))
        .hover(|s| s.glass_hover())
        .child(icon(path, 12., color))
        .tooltip(tip(label))
}

fn fmt_bytes(b: u64) -> String {
    let mb = b as f64 / 1_048_576.;
    if mb >= 1024. { format!("{:.1} GB", mb / 1024.) } else { format!("{mb:.0} MB") }
}

// ---- sidebar card -------------------------------------------------------------------------------

pub enum MusicCardEvent {
    OpenSearch,
    OpenTheater,
}

pub struct MusicCard {
    music: Entity<Music>,
    /// Sidebar width (for the 16:9 video).
    pub width: f32,
    /// Mouse over the video (shows its controls).
    video_hover: bool,
    _sub: Subscription,
}

impl EventEmitter<MusicCardEvent> for MusicCard {}

impl MusicCard {
    pub fn new(music: Entity<Music>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&music, |_, _, cx| cx.notify());
        Self { music, width: 260., video_hover: false, _sub: sub }
    }
}

/// Invisible click target under one of the player's native hover buttons.
fn overlay_target(id: &'static str, size: f32) -> gpui::Stateful<gpui::Div> {
    div().id(id).size(px(size)).rounded(px(size / 2.)).cursor_pointer()
}

impl Render for MusicCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let music = self.music.clone();
        let m = self.music.read(cx);
        let Some(track) = m.current().cloned() else { return div().into_any_element() };
        let show_video = m.video_on && !m.theater;
        let on = |f: fn(&mut Music, &mut Context<Music>)| {
            let music = music.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| music.update(cx, |m, cx| f(m, cx))
        };
        if show_video {
            return self.render_video(&music, &track, window, cx);
        }
        m.video_view.set_overlay(None);
        const ART: f32 = 76.;
        const BADGE: f32 = 24.;
        let mode_color = theme::rgb(track.mode.color());
        let play_icon = if m.playing { "icons/pause.svg" } else { "icons/play.svg" };
        let subtitle = player_status(m).unwrap_or_else(|| {
            let total = if m.duration > 0. { fmt_time(m.duration) } else { "--:--".into() };
            format!("{} · {} / {}", track.mode.label(), fmt_time(m.position), total)
        });
        let plain_button = |id: &'static str, path: &'static str, size: f32| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(28.))
                .rounded(px(14.))
                .cursor_pointer()
                .hover(|s| s.glass_hover())
                .child(icon(path, size, theme::TEXT))
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(8.))
            .mx(px(8.))
            .mb(px(8.))
            .px(px(10.))
            .pt(px(10.))
            .pb(px(8.))
            .relative()
            .rounded(px(18.))
            .glass()
            .group("music-card")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        // Artwork with the mode badge (music / podcast / video) on its corner.
                        div()
                            .id("music-card-thumb")
                            .relative()
                            .flex_none()
                            .size(px(ART + BADGE / 3.))
                            .cursor_pointer()
                            .group("thumb")
                            .child(div().rounded(px(14.)).shadow(crate::glass::shadow()).child(thumb_el(m, &track, ART, ART, 14.)))
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size(px(ART))
                                    .rounded(px(14.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(gpui::hsla(0., 0., 0., 0.55))
                                    .invisible()
                                    .group_hover("thumb", |s| s.visible())
                                    .child(gpui::svg().path(if m.video_on { "icons/close.svg" } else { "icons/video.svg" }).size(px(18.)).text_color(gpui::white())),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .right_0()
                                    .bottom_0()
                                    .size(px(BADGE))
                                    .rounded(px(6.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(gpui::linear_gradient(
                                        180.,
                                        gpui::linear_color_stop(gpui::Hsla { l: (mode_color.l + 0.08).min(1.), ..mode_color }, 0.),
                                        gpui::linear_color_stop(mode_color, 1.),
                                    ))
                                    .border_1()
                                    .border_color(gpui::hsla(0., 0., 1., 0.22))
                                    .shadow(vec![gpui::BoxShadow { color: gpui::hsla(0., 0., 0., 0.35), offset: gpui::point(px(0.), px(2.)), blur_radius: px(6.), spread_radius: px(0.) }])
                                    .child(gpui::svg().path(track.mode.icon()).size(px(14.)).text_color(gpui::white())),
                            )
                            .tooltip(tip("Ver / ocultar video"))
                            .on_click(on(|m, cx| m.toggle_video(cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(14.)).font_weight(gpui::FontWeight::BOLD).text_color(theme::rgb(theme::TEXT)).child(track.title.clone()))
                            .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme::rgb(theme::TEXT_MUTED)).child(track.channel.clone()))
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(px(11.))
                                    .text_color(theme::rgb(if m.error.is_some() { theme::DELETED } else { theme::TEXT_FAINT }))
                                    .child(subtitle),
                            )
                            .child(
                                div()
                                    .mt(px(4.))
                                    .ml(px(-6.))
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(plain_button("card-prev", "icons/skip-prev.svg", 13.).tooltip(tip("Anterior (⌃⌥⇧←)")).on_click(on(|m, cx| m.previous(cx))))
                                    .child(plain_button("card-play", play_icon, 17.).tooltip(tip("Reproducir / pausa (⌃⌥Space)")).on_click(on(|m, cx| m.toggle(cx))))
                                    .child(plain_button("card-next", "icons/skip-next.svg", 13.).when(!m.has_next(), |d| d.opacity(0.4)).tooltip(tip("Siguiente (⌃⌥⇧→)")).on_click(on(|m, cx| m.next(cx)))),
                            ),
                    ),
            )
            .child(seek_bar(&music, m, "music-card-progress"))
            // Rate / search / close: top-right, shown on hover.
            .child(
                div()
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .flex()
                    .items_center()
                    .invisible()
                    .group_hover("music-card", |s| s.visible())
                    .children([true, false].map(|good| {
                        let rating = m.rating(&track.id);
                        let (path, label) = rate_icon(good, rating);
                        let music = music.clone();
                        let t = track.clone();
                        div()
                            .id(if good { "music-card-up" } else { "music-card-down" })
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(22.))
                            .rounded(px(11.))
                            .cursor_pointer()
                            .hover(|s| s.glass_hover())
                            .child(icon(path, 12., if rating == Some(good) { theme::ACCENT } else { theme::TEXT }))
                            .tooltip(tip(label))
                            .on_click(move |_: &ClickEvent, _, cx| music.update(cx, |m, cx| m.rate(&t, good, cx)))
                    }))
                    .child(round_button("music-card-search", "icons/search.svg", 22., 12.).tooltip(tip("Buscar (⌘⇧M)")).on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(MusicCardEvent::OpenSearch))))
                    .child(round_button("music-card-close", "icons/close.svg", 22., 12.).tooltip(tip("Cerrar reproductor")).on_click(on(|m, cx| m.stop(cx)))),
            )
            .into_any_element()
    }
}

impl MusicCard {
    /// Video mode: just the picture, edge to edge in the sidebar. Title, controls and timeline
    /// are drawn natively over it (GPUI can't paint above the player); invisible GPUI targets
    /// at the same spots take the clicks.
    fn render_video(&mut self, music: &Entity<Music>, track: &Track, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let on_hover = cx.listener(|this, hovered: &bool, _, cx| {
            if this.video_hover != *hovered {
                this.video_hover = *hovered;
                cx.notify();
            }
        });
        let open_theater = cx.listener(|_, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            cx.emit(MusicCardEvent::OpenTheater)
        });
        let m = music.read(cx);
        use crate::video_player::{OverlayInfo, OVERLAY_BAR_BOTTOM, OVERLAY_BUTTON, OVERLAY_GAP, OVERLAY_MARGIN, OVERLAY_PAD, OVERLAY_PLAY, OVERLAY_ROW_BOTTOM, OVERLAY_SKIP, OVERLAY_SKIP_GAP, OVERLAY_VOL_ICON, OVERLAY_VOL_W};
        const SIDE: f32 = 6.;
        let video_w = (self.width - 2. * SIDE).max(80.);
        let video_h = video_w * 9. / 16.;
        let video_ready = !m.video_loading && !m.resolving;
        let hover = video_ready && self.video_hover;
        let accent = gpui::Rgba::from(theme::rgb(track.mode.color()));
        let time = if let Some(e) = &m.error {
            e.clone()
        } else if m.buffering {
            "Cargando…".into()
        } else {
            format!("{} / {}", fmt_time(m.position), if m.duration > 0. { fmt_time(m.duration) } else { "--:--".into() })
        };
        let owner = m.video_view.shown_in(window);
        if owner {
            m.video_view.set_corner_radius(8.);
        }
        if owner {
            m.video_view.set_overlay(video_ready.then(|| OverlayInfo {
            hover,
            playing: m.playing,
            title: track.title.to_string(),
            channel: track.channel.to_string(),
            progress: if m.duration > 0. { (m.position / m.duration) as f32 } else { 0. },
            time,
            accent: (accent.r, accent.g, accent.b),
            volume: m.volume,
            }));
        }
        let on = |f: fn(&mut Music, &mut Context<Music>)| {
            let music = music.clone();
            // Targets overlap (buttons sit over the click-to-pause area): handle once.
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                cx.stop_propagation();
                music.update(cx, |m, cx| f(m, cx))
            }
        };
        let bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let b2 = bounds.clone();
        let music_seek = music.clone();
        let duration = m.duration;
        let vol_bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let set_vol = {
            let vol_bounds = vol_bounds.clone();
            let music = music.clone();
            move |x: Pixels, cx: &mut App| {
                if let Some(b) = vol_bounds.get() {
                    let v = f32::from(x - b.origin.x) / f32::from(b.size.width).max(1.);
                    music.update(cx, |m, cx| m.set_volume(v, cx));
                }
            }
        };
        let set_vol_move = set_vol.clone();
        let music_wheel = music.clone();
        div()
            .id("music-card-video")
            .flex_none()
            .relative()
            .mx(px(SIDE))
            .mb(px(8.))
            .h(px(video_h))
            .rounded(px(8.))
            .overflow_hidden()
            .bg(gpui::black())
            .on_hover(on_hover)
            // Wheel / two-finger scroll over the picture: volume.
            .when(video_ready, |d| {
                d.on_scroll_wheel(move |e: &gpui::ScrollWheelEvent, _, cx| {
                    let dy = f32::from(e.delta.pixel_delta(px(16.)).y);
                    if dy != 0. {
                        cx.stop_propagation();
                        music_wheel.update(cx, |m, cx| {
                            let v = m.volume + dy * 0.004;
                            m.set_volume(v, cx)
                        });
                    }
                })
            })
            .when(video_ready, |d| d.child(crate::video_player::player_slot(m.video_view.clone())))
            .when(!video_ready, |d| {
                // No native view yet: GPUI paints the escape hatches itself.
                let btn = |id: &'static str, path: &'static str| {
                    overlay_target(id, OVERLAY_BUTTON).flex().items_center().justify_center().bg(gpui::hsla(0., 0., 0., 0.45)).hover(|s| s.bg(gpui::hsla(0., 0., 0., 0.7))).child(gpui::svg().path(path).size(px(11.)).text_color(gpui::white()))
                };
                d.flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(gpui::hsla(0., 0., 1., 0.6))
                    .child("Cargando video…")
                    .child(
                        div()
                            .absolute()
                            .top(px(OVERLAY_MARGIN))
                            .right(px(OVERLAY_MARGIN))
                            .flex()
                            .gap(px(OVERLAY_GAP))
                            .child(btn("music-card-loading-audio", "icons/music.svg").tooltip(tip("Solo audio")).on_click(on(|m, cx| m.set_video(false, cx))))
                            .child(btn("music-card-loading-close", "icons/close.svg").tooltip(tip("Cerrar reproductor")).on_click(on(|m, cx| m.stop(cx)))),
                    )
            })
            .when(hover, |d| {
                let cy = video_h / 2.;
                d.child(
                    // Click on the picture (outside the buttons) toggles play / pause.
                    div().id("music-card-video-toggle").absolute().top(px(44.)).left_0().w_full().h(px((video_h - 44. - 30.).max(0.))).on_click(on(|m, cx| m.toggle(cx))),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(cy - OVERLAY_PLAY / 2.))
                        .left_0()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(OVERLAY_SKIP_GAP))
                        .child(overlay_target("music-card-video-prev", OVERLAY_SKIP).on_click(on(|m, cx| m.previous(cx))))
                        .child(overlay_target("music-card-video-play", OVERLAY_PLAY).on_click(on(|m, cx| m.toggle(cx))))
                        .child(overlay_target("music-card-video-next", OVERLAY_SKIP).on_click(on(|m, cx| m.next(cx)))),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(OVERLAY_MARGIN))
                        .right(px(OVERLAY_MARGIN))
                        .flex()
                        .gap(px(OVERLAY_GAP))
                        .child(overlay_target("music-card-video-expand", OVERLAY_BUTTON).tooltip(tip("Pantalla grande")).on_click(open_theater))
                        .child(overlay_target("music-card-video-audio", OVERLAY_BUTTON).tooltip(tip("Solo audio")).on_click(on(|m, cx| m.set_video(false, cx))))
                        .child(overlay_target("music-card-video-close", OVERLAY_BUTTON).tooltip(tip("Cerrar reproductor")).on_click(on(|m, cx| m.stop(cx)))),
                )
                .child(
                    // Timeline: click / drag-free seek over a taller strip than the bar.
                    div()
                        .id("music-card-video-seek")
                        .absolute()
                        .left(px(OVERLAY_PAD))
                        .right(px(OVERLAY_PAD))
                        .bottom(px(OVERLAY_BAR_BOTTOM - 7.))
                        .h(px(14.))
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            if let Some(b) = b2.get() {
                                let x = f32::from(e.position.x - b.origin.x) / f32::from(b.size.width).max(1.);
                                if duration > 0. {
                                    music_seek.update(cx, |m, cx| m.seek_to(duration * x.clamp(0., 1.) as f64, cx));
                                }
                            }
                        })
                        .child(canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {}).absolute().size_full()),
                )
                .child(
                    // Speaker (mute) at the left of the volume slider.
                    overlay_target("music-card-video-mute", OVERLAY_VOL_ICON)
                        .absolute()
                        .right(px(OVERLAY_PAD + OVERLAY_VOL_W + 4.))
                        .bottom(px(OVERLAY_ROW_BOTTOM - OVERLAY_VOL_ICON / 2.))
                        .tooltip(tip("Silenciar"))
                        .on_click(on(|m, cx| m.toggle_mute(cx))),
                )
                .child(
                    // Volume slider: click or drag.
                    div()
                        .id("music-card-video-volume")
                        .absolute()
                        .right(px(OVERLAY_PAD))
                        .bottom(px(OVERLAY_ROW_BOTTOM - 7.))
                        .w(px(OVERLAY_VOL_W))
                        .h(px(14.))
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            set_vol(e.position.x, cx);
                        })
                        .on_mouse_move(move |e: &gpui::MouseMoveEvent, _, cx| {
                            if e.pressed_button == Some(MouseButton::Left) {
                                set_vol_move(e.position.x, cx);
                            }
                        })
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(canvas(move |b, _, _| vol_bounds.set(Some(b)), |_, _, _, _| {}).absolute().size_full()),
                )
            })
            .into_any_element()
    }
}

// ---- theater ---------------------------------------------------------------------------------------

pub enum TheaterEvent {
    Closed,
}

pub struct MusicTheater {
    music: Entity<Music>,
    focus: FocusHandle,
    _sub: Subscription,
}

impl EventEmitter<TheaterEvent> for MusicTheater {}

impl MusicTheater {
    pub fn new(music: Entity<Music>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&music, |_, _, cx| cx.notify());
        Self { music, focus: cx.focus_handle(), _sub: sub }
    }

    pub fn is_open(&self, cx: &App) -> bool {
        self.music.read(cx).theater
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.music.update(cx, |m, cx| m.set_theater(false, cx));
        cx.emit(TheaterEvent::Closed);
    }
}

impl Focusable for MusicTheater {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for MusicTheater {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let music = self.music.clone();
        let m = self.music.read(cx);
        let Some(track) = m.current().cloned().filter(|_| m.theater) else { return div().into_any_element() };
        m.video_view.set_overlay(None);
        m.video_view.set_corner_radius(0.);
        let size = window.bounds().size;
        let (ww, wh) = (f32::from(size.width), f32::from(size.height));
        // Compact: ~60% of the window, never taller than it leaves room for the controls.
        let vw = (ww * 0.6).min((wh - 170.).max(160.) * 16. / 9.).clamp(320., 1100.);
        let vh = vw * 9. / 16.;
        let video_ready = !m.video_loading && !m.resolving;
        let mu = music.clone();
        let accent = track.mode.color();
        let status = player_status(m);
        let error = m.error.is_some();
        let time = if m.duration > 0. { format!("{} / {}", fmt_time(m.position), fmt_time(m.duration)) } else { fmt_time(m.position) };
        let subtle = |id: &'static str, path: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(28.))
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|s| s.bg(gpui::hsla(0., 0., 1., 0.08)))
                .child(gpui::svg().path(path).size(px(14.)).text_color(gpui::hsla(0., 0., 1., 0.7)))
                .tooltip(tip(label))
        };
        div()
            .id("music-theater")
            .key_context("MusicTheater")
            .track_focus(&self.focus)
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., 0.72))
            .on_action(cx.listener(|this, _: &TogglePlay, _, cx| this.music.update(cx, |m, cx| m.toggle(cx))))
            .on_action(cx.listener(|this, _: &Forward, _, cx| this.music.update(cx, |m, cx| m.seek_by(SKIP_SECONDS, cx))))
            .on_action(cx.listener(|this, _: &Back, _, cx| this.music.update(cx, |m, cx| m.seek_by(-SKIP_SECONDS, cx))))
            .on_action(cx.listener(|this, _: &NextTrack, _, cx| this.music.update(cx, |m, cx| m.next(cx))))
            .on_action(cx.listener(|this, _: &PreviousTrack, _, cx| this.music.update(cx, |m, cx| m.previous(cx))))
            .on_action(cx.listener(|this, _: &CloseTheater, _, cx| this.close(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.close(cx)))
            .child(
                // One card: the video and a slim control bar under it.
                div()
                    .w(px(vw))
                    .flex()
                    .flex_col()
                    .rounded(px(14.))
                    .overflow_hidden()
                    .bg(gpui::rgb(0x111216))
                    .border_1()
                    .border_color(gpui::hsla(0., 0., 1., 0.08))
                    .shadow_2xl()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .w(px(vw))
                            .h(px(vh))
                            .bg(gpui::black())
                            .when(video_ready, |d| d.child(crate::video_player::player_slot(m.video_view.clone())))
                            .when(!video_ready, |d| d.flex().items_center().justify_center().text_size(px(12.)).text_color(gpui::hsla(0., 0., 1., 0.5)).child("Cargando video…")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .px(px(16.))
                            .pt(px(8.))
                            .pb(px(12.))
                            .child(seek_bar(&music, m, "theater-seek"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(12.))
                                    // Left: what's playing.
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(1.))
                                            .child(
                                                div()
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_size(px(13.))
                                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                                    .text_color(gpui::hsla(0., 0., 1., 0.94))
                                                    .child(track.title.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(6.))
                                                    .text_size(px(11.5))
                                                    .text_color(gpui::hsla(0., 0., 1., 0.5))
                                                    .child(icon(track.mode.icon(), 11., accent))
                                                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(track.channel.clone())),
                                            ),
                                    )
                                    // Center: transport.
                                    .child(transport(&music, m, false, "theater"))
                                    // Right: time / status and actions.
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .justify_end()
                                            .gap(px(4.))
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .mr(px(6.))
                                                    .text_size(px(11.5))
                                                    .text_color(if error { theme::rgb(theme::DELETED) } else { gpui::hsla(0., 0., 1., 0.55) })
                                                    .child(status.unwrap_or(time)),
                                            )
                                            .child(subtle("theater-audio", "icons/music.svg", "Solo audio (deja de cargar video)").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                                mu.update(cx, |m, cx| m.set_video(false, cx));
                                                cx.emit(TheaterEvent::Closed);
                                            })))
                                            .child(subtle("theater-close", "icons/close.svg", "Cerrar (esc) · espacio pausa · ←/→ 15 s · ⇧←/⇧→ anterior/siguiente").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close(cx)))),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}
