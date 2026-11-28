//! Music / podcast / video player backed by YouTube through `yt-dlp`.
//!
//! * Search and suggestions: `yt-dlp --flat-playlist -J` (one request, no video pages).
//! * Playback: `yt-dlp -g` resolves a direct audio URL that `AVPlayer` streams (starts after a
//!   few seconds of buffer). Video is only resolved and streamed on demand, in a second muted
//!   `AVPlayer` kept in sync with the audio one, so audio-only listening never pulls video.
//! * Tuned for instant start over quality: 128 kb/s AAC audio, ≤480p video, URLs resolved
//!   ahead of time for the top results and the next track.
//! * Suggestions come from `preferences.json` (search topics per mode, refreshed weekly by a
//!   Claude task). Podcasts and videos remember progress: finished ones are never suggested
//!   again and unfinished ones resume where they were left.
//! * One player for the whole app (a gpui `Global`); every window shows the same state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::music_engine::{self, AvPlayer, NowPlaying, Remote};
use crate::video_player::NativePlayer;

pub const SKIP_SECONDS: f64 = 15.;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Mode {
    Music,
    Podcast,
    Videos,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Music, Mode::Podcast, Mode::Videos];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Music => "Música",
            Mode::Podcast => "Podcast",
            Mode::Videos => "Videos",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Mode::Music => "icons/music.svg",
            Mode::Podcast => "icons/podcast.svg",
            Mode::Videos => "icons/video.svg",
        }
    }

    pub fn color(self) -> u32 {
        match self {
            // Fragua brand red (was pink).
            Mode::Music => crate::theme::ACCENT,
            Mode::Podcast => 0xe0af68,
            Mode::Videos => 0x7aa2f7,
        }
    }

    /// Automatic popularity filter applied to search results.
    pub fn min_views(self) -> Option<u64> {
        match self {
            Mode::Music => Some(1_000_000),
            Mode::Podcast => Some(5_000),
            Mode::Videos => None,
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Mode::Music => "Busca canciones, artistas, álbumes…",
            Mode::Podcast => "Busca podcasts, episodios, invitados…",
            Mode::Videos => "Busca cualquier video de YouTube…",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub channel: String,
    #[serde(default)]
    pub channel_url: Option<String>,
    pub duration: Option<f64>,
    pub views: Option<u64>,
    pub mode: Mode,
    /// Preferences topic (search) a suggestion came from.
    #[serde(default)]
    pub topic: Option<String>,
    /// Upload time (unix seconds, approximate: YouTube's "3 months ago").
    #[serde(default)]
    pub published: Option<i64>,
}

impl Track {
    pub fn thumb_path(&self) -> PathBuf {
        thumbs_dir().join(format!("{}.jpg", self.id))
    }
}

/// Suggestions shown before typing anything.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    pub track: Track,
    pub pos: f64,
    pub dur: f64,
    pub finished: bool,
    /// Unix seconds of the last update.
    pub at: i64,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.dur > 0. { (self.pos / self.dur).clamp(0., 1.) as f32 } else { 0. }
    }
}

/// A podcast / video kept on disk to play offline: downloaded by hand, saved for later
/// ("Ver después", downloaded too) or cached while it played. Gone once it's finished.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub track: Track,
    /// Saved to watch / listen later.
    #[serde(default)]
    pub later: bool,
    /// Unix seconds it was added.
    pub at: i64,
}

/// Local copy of a podcast / video.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Offline {
    None,
    /// Downloading, 0..1.
    Downloading(f32),
    Ready,
    Failed,
}

struct DownloadJob {
    /// f32 bits, 0..1.
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct Suggestions {
    pub header: String,
    pub tracks: Vec<Track>,
}

pub struct Music {
    audio: AvPlayer,
    video: AvPlayer,
    pub video_view: Rc<NativePlayer>,
    pub queue: Vec<Track>,
    pub index: Option<usize>,
    /// The user wants it playing (even while resolving / buffering).
    pub playing: bool,
    pub resolving: bool,
    pub buffering: bool,
    pub position: f64,
    pub duration: f64,
    /// Exact length (googlevideo's `dur=`, else search metadata). AVPlayer's own estimate
    /// for fragmented MP4 can be far off, so it's only a fallback.
    known_duration: Option<f64>,
    pub video_on: bool,
    pub video_loading: bool,
    video_loaded: Option<String>,
    /// Last drift-correcting seek of the video (they're spaced out).
    video_seeked: Instant,
    /// Large video over the window.
    pub theater: bool,
    pub error: Option<String>,
    history: Vec<Track>,
    /// Position to restore once the stream is loaded again (retry after an error).
    resume_at: Option<f64>,
    urls: HashMap<(String, bool), (String, Instant)>,
    generation: u64,
    thumbs_ready: HashSet<String>,
    thumbs_pending: HashSet<String>,
    suggestions: HashMap<Mode, (Instant, Suggestions)>,
    suggesting: HashSet<Mode>,
    /// Podcast / video progress by id (finished or where it was left).
    progress: HashMap<String, Progress>,
    progress_saved: Instant,
    /// Audio URLs being resolved ahead of time.
    prefetching: HashSet<String>,
    /// The weekly "refresh preferences" Claude task, run by hand.
    pub renewing: bool,
    /// Podcasts / videos kept on disk (offline), newest first.
    library: Vec<LibraryEntry>,
    /// Ids whose local copy is complete.
    downloaded: HashSet<String>,
    downloads: HashMap<String, DownloadJob>,
    download_failed: HashSet<String>,
    /// 👍 (true) / 👎 (false) by id; logged to the personal brain too.
    ratings: HashMap<String, bool>,
    /// Bytes used by the local copies (songs, podcasts, videos); None while measuring.
    disk_usage: Option<u64>,
    /// Player volume 0…1 (persisted).
    pub volume: f32,
    /// Volume to go back to when unmuting.
    volume_before_mute: f32,
    /// Ids with a complete `.m4a` in the song cache (refreshed with `disk_usage`).
    cached_songs: HashSet<String>,
    _downloads_tick: Option<Task<()>>,
    _tick: Option<Task<()>>,
    _resolve: Option<Task<()>>,
    _video: Option<Task<()>>,
}

pub struct MusicGlobal(pub Entity<Music>);
impl Global for MusicGlobal {}

pub fn global(cx: &App) -> Option<Entity<Music>> {
    cx.try_global::<MusicGlobal>().map(|g| g.0.clone())
}

pub fn init(cx: &mut App) {
    std::thread::spawn(prune_song_cache);
    let music = cx.new(|cx| {
        let mut m = Music::new();
        m.downloaded = m.library.iter().filter(|e| is_complete(&e.track)).map(|e| e.track.id.clone()).collect();
        m.refresh_disk_usage(cx);
        // Library thumbnails are on disk already: offline lists show them too.
        let saved: Vec<Track> = m.library.iter().map(|e| e.track.clone()).collect();
        m.ensure_thumbs(&saved, cx);
        m
    });
    cx.set_global(MusicGlobal(music.clone()));

    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Remote>();
    music_engine::install_remote_commands(tx.clone());
    // System-wide shortcuts (work from any app): ⌃⌥Space, ⌃⌥←/→, ⌃⌥⇧←/→.
    use crate::hotkey::{CONTROL_KEY, KVK_LEFT, KVK_RIGHT, KVK_SPACE, OPTION_KEY, SHIFT_KEY, register};
    let keys = [
        (KVK_SPACE, CONTROL_KEY | OPTION_KEY, Remote::Toggle, "⌃⌥Space"),
        (KVK_LEFT, CONTROL_KEY | OPTION_KEY, Remote::SkipBackward, "⌃⌥←"),
        (KVK_RIGHT, CONTROL_KEY | OPTION_KEY, Remote::SkipForward, "⌃⌥→"),
        (KVK_LEFT, CONTROL_KEY | OPTION_KEY | SHIFT_KEY, Remote::Previous, "⌃⌥⇧←"),
        (KVK_RIGHT, CONTROL_KEY | OPTION_KEY | SHIFT_KEY, Remote::Next, "⌃⌥⇧→"),
    ];
    for (key, mods, ev, label) in keys {
        match register(key, mods, label) {
            Ok(mut keys_rx) => {
                let tx = tx.clone();
                cx.spawn(async move |_| {
                    while keys_rx.next().await.is_some() {
                        let _ = tx.unbounded_send(ev);
                    }
                })
                .detach();
            }
            Err(e) => crate::startup_log(&format!("music hotkey: {e}")),
        }
    }
    // Menu bar item (shown while something is loaded).
    let (menu_tx, mut menu_rx) = futures::channel::mpsc::unbounded::<music_engine::MenuCommand>();
    music_engine::install_status_item(menu_tx);
    {
        let music = music.clone();
        cx.spawn(async move |cx| {
            while let Some(cmd) = menu_rx.next().await {
                let ok = match cmd {
                    music_engine::MenuCommand::Remote(ev) => music.update(cx, |m, cx| m.remote(ev, cx)).is_ok(),
                    music_engine::MenuCommand::Volume(v) => music.update(cx, |m, cx| m.set_volume(v, cx)).is_ok(),
                    music_engine::MenuCommand::Stop => music.update(cx, |m, cx| m.stop(cx)).is_ok(),
                    music_engine::MenuCommand::OpenSearch => cx.update(crate::launcher::open_music).is_ok(),
                    music_engine::MenuCommand::Rate(good) => music
                        .update(cx, |m, cx| {
                            if let Some(t) = m.current().cloned() {
                                m.rate(&t, good, cx);
                            }
                        })
                        .is_ok(),
                };
                if !ok {
                    break;
                }
            }
        })
        .detach();
    }
    cx.spawn(async move |cx| {
        while let Some(ev) = rx.next().await {
            if music.update(cx, |m, cx| m.remote(ev, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

impl Music {
    fn new() -> Self {
        let video = AvPlayer::new(true);
        let video_view = NativePlayer::for_player(video.raw());
        let audio = AvPlayer::new(false);
        let volume = load_volume();
        audio.set_volume(volume);
        Self {
            audio,
            video,
            video_view,
            queue: Vec::new(),
            index: None,
            playing: false,
            resolving: false,
            buffering: false,
            position: 0.,
            duration: 0.,
            known_duration: None,
            video_on: false,
            video_loading: false,
            video_loaded: None,
            video_seeked: Instant::now(),
            theater: false,
            error: None,
            history: load_history(),
            resume_at: None,
            urls: HashMap::new(),
            generation: 0,
            thumbs_ready: HashSet::new(),
            thumbs_pending: HashSet::new(),
            suggestions: HashMap::new(),
            suggesting: HashSet::new(),
            progress: load_progress(),
            progress_saved: Instant::now(),
            prefetching: HashSet::new(),
            renewing: false,
            downloaded: HashSet::new(),
            cached_songs: HashSet::new(),
            library: load_library(),
            downloads: HashMap::new(),
            download_failed: HashSet::new(),
            ratings: load_ratings(),
            disk_usage: None,
            volume,
            volume_before_mute: if volume > 0.01 { volume } else { 1. },
            _downloads_tick: None,
            _tick: None,
            _resolve: None,
            _video: None,
        }
    }

    pub fn current(&self) -> Option<&Track> {
        self.index.and_then(|i| self.queue.get(i))
    }

    pub fn has_next(&self) -> bool {
        self.index.is_some_and(|i| i + 1 < self.queue.len())
    }

    fn remote(&mut self, ev: Remote, cx: &mut Context<Self>) {
        if self.index.is_none() {
            return;
        }
        match ev {
            Remote::Toggle => self.toggle(cx),
            Remote::Play => self.set_playing(true, cx),
            Remote::Pause => self.set_playing(false, cx),
            Remote::Next => self.next(cx),
            Remote::Previous => self.previous(cx),
            Remote::SkipForward => self.seek_by(SKIP_SECONDS, cx),
            Remote::SkipBackward => self.seek_by(-SKIP_SECONDS, cx),
            Remote::Seek(t) => self.seek_to(t, cx),
        }
    }

    // ---- playback ---------------------------------------------------------------------

    /// Play `queue[index]`; the rest of the queue plays next.
    pub fn play_queue(&mut self, queue: Vec<Track>, index: usize, cx: &mut Context<Self>) {
        if index >= queue.len() {
            return;
        }
        self.record_progress(false);
        self.queue = queue;
        self.index = Some(index);
        self.start(cx);
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.current().cloned() else { return };
        if self.resume_at.is_none() {
            // Unfinished podcast / video: continue where it was left.
            if let Some(p) = self.progress.get(&track.id).filter(|p| track.mode != Mode::Music && !p.finished && p.pos > 20.) {
                self.resume_at = Some(p.pos - 3.);
            }
        }
        self.generation += 1;
        let generation = self.generation;
        self.audio.clear();
        self.video.clear();
        self.video_loaded = None;
        self.video_loading = false;
        self.position = 0.;
        self.duration = track.duration.unwrap_or(0.);
        self.known_duration = track.duration;
        self.playing = true;
        self.resolving = true;
        self.buffering = false;
        self.error = None;
        if track.mode == Mode::Videos {
            self.video_on = true;
        }
        self.remember(&track);
        self.ensure_thumbs(std::slice::from_ref(&track), cx);
        self.sync_now_playing();
        self.ensure_tick(cx);
        let resume = self.resume_at.take();
        // Local copy (song played in the last 30 days, downloaded podcast / video): no network.
        let local = if track.mode == Mode::Music || self.downloaded.contains(&track.id) { cached_song(&track.id) } else { None };
        {
            if let Some((path, dur)) = local {
                self.resolving = false;
                if let Some(d) = dur {
                    self.known_duration = Some(d);
                    self.duration = d;
                }
                self.audio.load_file(&path);
                if let Some(t) = resume {
                    self.audio.seek(t);
                    self.position = t;
                }
                self.audio.play_now();
                if self.video_on {
                    self.load_video(cx);
                }
                self.prefetch_next(cx);
                self.sync_now_playing();
                cx.notify();
                return;
            }
        }
        let cached = self.cached_url(&track.id, false);
        let id = track.id.clone();
        // Short songs are cached while they play (replays need no network); podcasts are
        // downloaded into the library, to resume offline.
        let keep_song = track.mode == Mode::Music && track.duration.is_none_or(|d| d <= 15. * 60.);
        let keep_podcast = track.mode == Mode::Podcast;
        self._resolve = Some(cx.spawn(async move |this, cx| {
            let url = match cached {
                Some(u) => Ok(u),
                None => {
                    let id = id.clone();
                    cx.background_executor().spawn(async move { stream_url(&id, false).map(|u| (u, Instant::now())) }).await
                }
            };
            if let Ok((_, at)) = &url {
                wait_until_valid(*at, cx).await;
            }
            let _ = this.update(cx, |m, cx| {
                if m.generation != generation {
                    return;
                }
                m.resolving = false;
                match url {
                    Ok((url, at)) => {
                        m.urls.insert((id.clone(), false), (url.clone(), at));
                        if let Some(d) = url_duration(&url) {
                            m.known_duration = Some(d);
                            m.duration = d;
                        }
                        if keep_song {
                            let (url, id, dur) = (url.clone(), id.clone(), m.known_duration);
                            std::thread::spawn(move || cache_song(&id, &url, dur));
                        }
                        if keep_podcast {
                            if let Some(t) = m.current().cloned().filter(|t| t.id == id) {
                                m.start_download(t, false, Some((url.clone(), at)), cx);
                            }
                        }
                        let local = crate::music_proxy::local_url(&format!("{id}-a"), &url, false).unwrap_or(url);
                        m.audio.load(&local);
                        if let Some(t) = resume {
                            m.audio.seek(t);
                            m.position = t;
                        }
                        if m.playing {
                            m.audio.play_now();
                        }
                        if m.video_on {
                            m.load_video(cx);
                        }
                        m.prefetch_next(cx);
                    }
                    Err(e) => {
                        m.playing = false;
                        m.error = Some(e);
                    }
                }
                m.sync_now_playing();
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Resolve the next track's audio URL ahead of time so "next" starts instantly.
    fn prefetch_next(&mut self, cx: &mut Context<Self>) {
        let Some(next) = self.index.and_then(|i| self.queue.get(i + 1)).cloned() else { return };
        if self.cached_url(&next.id, false).is_some() {
            return;
        }
        self.ensure_thumbs(std::slice::from_ref(&next), cx);
        cx.spawn(async move |this, cx| {
            let id = next.id.clone();
            let url = cx.background_executor().spawn(async move { stream_url(&id, false).map(|u| (u, Instant::now())) }).await;
            if let Ok((url, at)) = url {
                let _ = this.update(cx, |m, _| {
                    m.urls.insert((next.id.clone(), false), (url, at));
                });
            }
        })
        .detach();
    }

    /// A resolved stream URL and when it was resolved (googlevideo URLs expire after ~6 h).
    fn cached_url(&self, id: &str, video: bool) -> Option<(String, Instant)> {
        self.urls.get(&(id.to_string(), video)).filter(|(_, at)| at.elapsed() < Duration::from_secs(4 * 3600)).cloned()
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        let p = !self.playing;
        self.set_playing(p, cx);
    }

    pub fn set_playing(&mut self, playing: bool, cx: &mut Context<Self>) {
        if self.index.is_none() {
            return;
        }
        if playing && self.error.is_some() {
            // Retry (e.g. an expired URL after a long pause).
            let cur = self.current().map(|t| t.id.clone());
            self.urls.retain(|(id, _), _| Some(id) != cur.as_ref());
            self.resume_at = Some(self.position);
            self.start(cx);
            return;
        }
        self.playing = playing;
        if playing {
            if self.duration > 0. && self.position >= self.duration - 1. {
                self.audio.seek(0.);
            }
            self.audio.play();
        } else {
            self.audio.pause();
            self.video.pause();
            self.record_progress(false);
        }
        self.sync_now_playing();
        cx.notify();
    }

    pub fn set_volume(&mut self, v: f32, cx: &mut Context<Self>) {
        let v = v.clamp(0., 1.);
        if (v - self.volume).abs() < 0.001 {
            return;
        }
        self.volume = v;
        if v > 0.01 {
            self.volume_before_mute = v;
        }
        self.audio.set_volume(v);
        self.sync_status_item();
        save_json(&volume_file(), &v);
        cx.notify();
    }

    pub fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        let v = if self.volume > 0.01 { 0. } else { self.volume_before_mute.max(0.1) };
        self.set_volume(v, cx);
    }

    pub fn seek_by(&mut self, delta: f64, cx: &mut Context<Self>) {
        let t = self.audio.time() + delta;
        self.seek_to(t, cx);
    }

    pub fn seek_to(&mut self, t: f64, cx: &mut Context<Self>) {
        if self.index.is_none() {
            return;
        }
        let max = if self.duration > 0. { (self.duration - 0.5).max(0.) } else { f64::MAX };
        let t = t.clamp(0., max);
        self.audio.seek(t);
        if self.video_loaded.is_some() {
            self.video.seek(t);
        }
        self.position = t;
        self.sync_now_playing();
        cx.notify();
    }

    pub fn next(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.index.filter(|i| i + 1 < self.queue.len()) {
            self.record_progress(false);
            self.index = Some(i + 1);
            self.start(cx);
        }
    }

    pub fn previous(&mut self, cx: &mut Context<Self>) {
        match self.index {
            Some(i) if i > 0 && self.audio.time() < 4. => {
                self.record_progress(false);
                self.index = Some(i - 1);
                self.start(cx);
            }
            Some(_) => self.seek_to(0., cx),
            None => {}
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.record_progress(false);
        self.generation += 1;
        self.audio.clear();
        self.video.clear();
        self.video_loaded = None;
        self.video_on = false;
        self.theater = false;
        self.index = None;
        self.playing = false;
        self.resolving = false;
        self.error = None;
        self._tick = None;
        music_engine::set_now_playing(None);
        music_engine::set_status_item(None);
        cx.notify();
    }

    // ---- video (on demand) -------------------------------------------------------------

    pub fn toggle_video(&mut self, cx: &mut Context<Self>) {
        self.set_video(!self.video_on, cx);
    }

    pub fn set_video(&mut self, on: bool, cx: &mut Context<Self>) {
        self.video_on = on;
        if on {
            self.load_video(cx);
        } else {
            // Stop pulling video data altogether.
            self.video.clear();
            self.video_loaded = None;
            self.video_loading = false;
            self.theater = false;
            self._video = None;
        }
        cx.notify();
    }

    pub fn set_theater(&mut self, on: bool, cx: &mut Context<Self>) {
        if on && !self.video_on {
            self.set_video(true, cx);
        }
        self.theater = on;
        cx.notify();
    }

    fn load_video(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.current().cloned() else { return };
        if self.resolving || self.video_loaded.as_deref() == Some(track.id.as_str()) {
            return;
        }
        let file = video_file(&track.id);
        if self.downloaded.contains(&track.id) && file.exists() {
            self.video.load_file(&file);
            self.video.seek(self.audio.time());
            self.video_seeked = Instant::now();
            if self.playing {
                self.video.play();
            }
            self.video_loaded = Some(track.id.clone());
            self.video_loading = false;
            cx.notify();
            return;
        }
        let generation = self.generation;
        self.video_loading = true;
        let cached = self.cached_url(&track.id, true);
        self._video = Some(cx.spawn(async move |this, cx| {
            let url = match cached {
                Some(u) => Ok(u),
                None => {
                    let id = track.id.clone();
                    cx.background_executor().spawn(async move { stream_url(&id, true).map(|u| (u, Instant::now())) }).await
                }
            };
            if let Ok((_, at)) = &url {
                wait_until_valid(*at, cx).await;
            }
            let _ = this.update(cx, |m, cx| {
                if m.generation != generation || !m.video_on {
                    return;
                }
                m.video_loading = false;
                match url {
                    Ok((url, at)) => {
                        m.urls.insert((track.id.clone(), true), (url.clone(), at));
                        let local = crate::music_proxy::local_url(&format!("{}-v", track.id), &url, true).unwrap_or(url);
                        m.video.load(&local);
                        m.video.seek(m.audio.time());
                        m.video_seeked = Instant::now();
                        if m.playing {
                            m.video.play();
                        }
                        m.video_loaded = Some(track.id.clone());
                    }
                    Err(e) => m.error = Some(format!("Video: {e}")),
                }
                cx.notify();
            });
        }));
    }

    // ---- clock ---------------------------------------------------------------------------

    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if self._tick.is_some() {
            return;
        }
        self._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(250)).await;
                let Ok(alive) = this.update(cx, |m, cx| m.tick(cx)) else { break };
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Returns false when nothing is loaded anymore (the clock stops).
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        if self.index.is_none() {
            self._tick = None;
            return false;
        }
        if self.resolving {
            return true;
        }
        let before = (self.position as i64, self.buffering, self.duration as i64);
        if let Some(err) = self.audio.failure() {
            if self.error.is_none() {
                self.error = Some(if err.is_empty() { "No se pudo reproducir".into() } else { err });
                self.playing = false;
                cx.notify();
            }
            return true;
        }
        if !self.audio.has_item() {
            return true;
        }
        self.position = self.audio.time();
        if let Some(d) = self.known_duration.or_else(|| self.audio.duration()) {
            self.duration = d;
        }
        self.buffering = self.playing && self.audio.time_control() == 1;
        // Ended: advance through the queue.
        if self.playing && self.duration > 0. && self.position >= self.duration - 1. && self.audio.time_control() != 2 {
            self.record_progress(true);
            if self.has_next() {
                self.next(cx);
            } else {
                self.playing = false;
                self.sync_now_playing();
                cx.notify();
            }
            return true;
        }
        // Playing but AVPlayer paused itself mid-track (stall, interruption): resume.
        if self.playing && self.audio.time_control() == 0 && (self.duration <= 0. || self.position < self.duration - 1.) {
            self.audio.play();
        }
        if self.playing && self.progress_saved.elapsed() > Duration::from_secs(10) {
            self.record_progress(false);
        }
        // Keep the muted video on the audio clock.
        if self.video_on && self.video_loaded.is_some() && self.video_loaded.as_deref() == self.current().map(|t| t.id.as_str()) {
            let visible = !self.video_view.is_hidden();
            let vstatus = self.video.time_control();
            if self.playing && visible && vstatus == 0 {
                self.video.play();
            } else if (!self.playing || !visible) && vstatus != 0 {
                self.video.pause();
            }
            // Small drifts: nudge the video's rate (invisible, it's muted). Seeking for every
            // few hundred ms made it jump back and forth while it buffered after each seek.
            if visible && vstatus != 1 {
                let drift = self.video.time() - self.position;
                if drift.abs() > 1.5 {
                    if self.video_seeked.elapsed() > Duration::from_secs(3) {
                        self.video.seek(self.position + if self.playing { 0.3 } else { 0. });
                        self.video_seeked = Instant::now();
                    }
                } else if self.playing && vstatus == 2 {
                    let rate = if drift.abs() < 0.06 { 1. } else { (1. - drift * 0.6).clamp(0.75, 1.25) };
                    self.video.set_rate(rate as f32);
                }
            }
        }
        if before != (self.position as i64, self.buffering, self.duration as i64) {
            self.sync_status_item();
            cx.notify();
        }
        true
    }

    fn sync_now_playing(&self) {
        let Some(t) = self.current() else {
            music_engine::set_now_playing(None);
            music_engine::set_status_item(None);
            return;
        };
        self.sync_status_item();
        let thumb = t.thumb_path();
        music_engine::set_now_playing(Some(NowPlaying {
            title: &t.title,
            artist: &t.channel,
            duration: self.duration,
            elapsed: self.position,
            playing: self.playing,
            artwork: thumb.exists().then_some(thumb.as_path()),
        }));
    }

    /// The menu bar mini player (cheap: called every second while playing).
    fn sync_status_item(&self) {
        let Some(t) = self.current() else { return };
        let thumb = t.thumb_path();
        music_engine::set_status_item(Some(music_engine::StatusInfo {
            title: &t.title,
            artist: &t.channel,
            playing: self.playing,
            elapsed: self.position,
            duration: self.duration,
            artwork: thumb.exists().then_some(thumb.as_path()),
            rating: self.rating(&t.id),
            accent: t.mode.color(),
            volume: self.volume,
        }));
    }

    // ---- thumbnails ------------------------------------------------------------------------

    pub fn thumb(&self, id: &str) -> Option<PathBuf> {
        self.thumbs_ready.contains(id).then(|| thumbs_dir().join(format!("{id}.jpg")))
    }

    /// Download missing thumbnails in the background (small JPEGs, cached on disk).
    pub fn ensure_thumbs(&mut self, tracks: &[Track], cx: &mut Context<Self>) {
        let mut missing = Vec::new();
        for t in tracks {
            if self.thumbs_ready.contains(&t.id) || self.thumbs_pending.contains(&t.id) {
                continue;
            }
            if t.thumb_path().exists() {
                self.thumbs_ready.insert(t.id.clone());
            } else {
                self.thumbs_pending.insert(t.id.clone());
                missing.push(t.id.clone());
            }
        }
        if missing.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let ids = missing.clone();
            let done: Vec<String> = cx.background_executor().spawn(async move { download_thumbs(&ids) }).await;
            let _ = this.update(cx, |m, cx| {
                for id in &missing {
                    m.thumbs_pending.remove(id);
                }
                let current = m.current().map(|t| t.id.clone());
                let refresh_np = current.as_ref().is_some_and(|c| done.contains(c));
                m.thumbs_ready.extend(done);
                if refresh_np {
                    m.sync_now_playing();
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- history & suggestions ---------------------------------------------------------------

    fn remember(&mut self, track: &Track) {
        self.history.retain(|t| t.id != track.id);
        self.history.insert(0, track.clone());
        self.history.truncate(100);
        let history = self.history.clone();
        std::thread::spawn(move || save_history(&history));
    }

    pub fn suggestions(&self, mode: Mode) -> Option<&Suggestions> {
        self.suggestions.get(&mode).map(|(_, s)| s)
    }

    pub fn is_suggesting(&self, mode: Mode) -> bool {
        self.suggesting.contains(&mode)
    }

    pub fn progress_of(&self, id: &str) -> Option<&Progress> {
        self.progress.get(id)
    }

    /// Save where the current podcast / video is (music isn't tracked: it may repeat).
    fn record_progress(&mut self, finished: bool) {
        let Some(track) = self.current().cloned() else { return };
        if track.mode == Mode::Music || self.resolving || self.duration <= 0. {
            return;
        }
        let pos = self.position;
        if pos < 5. && !finished && !self.progress.contains_key(&track.id) {
            return;
        }
        // Finished = stopped less than 15 s before the end.
        let finished = finished || self.duration - pos < 15.;
        if finished {
            // Done with it: out of the "continue" suggestions and its local copy deleted.
            for (_, s) in self.suggestions.values_mut() {
                s.tracks.retain(|t| t.id != track.id);
            }
            self.forget_download(&track.id);
        }
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        self.progress.insert(track.id.clone(), Progress { track, pos, dur: self.duration, finished, at });
        self.progress_saved = Instant::now();
        let snapshot: Vec<Progress> = self.progress.values().cloned().collect();
        std::thread::spawn(move || save_json(&progress_file(), &snapshot));
    }

    /// Resolve the audio URL of the first few `tracks` ahead of time, so a click plays at once.
    pub fn prefetch(&mut self, tracks: &[Track], cx: &mut Context<Self>) {
        for t in tracks.iter().take(3) {
            if self.prefetching.contains(&t.id) || self.cached_url(&t.id, false).is_some() {
                continue;
            }
            self.prefetching.insert(t.id.clone());
            let id = t.id.clone();
            cx.spawn(async move |this, cx| {
                let id2 = id.clone();
                let url = cx.background_executor().spawn(async move { stream_url(&id2, false).map(|u| (u, Instant::now())) }).await;
                let _ = this.update(cx, |m, _| {
                    m.prefetching.remove(&id);
                    if let Ok((url, at)) = url {
                        m.urls.insert((id, false), (url, at));
                    }
                });
            })
            .detach();
        }
    }

    // ---- library: "Ver después" and downloads ------------------------------------------------

    pub fn offline(&self, id: &str) -> Offline {
        if self.downloaded.contains(id) {
            Offline::Ready
        } else if let Some(j) = self.downloads.get(id) {
            Offline::Downloading(f32::from_bits(j.progress.load(Ordering::Relaxed)))
        } else if self.download_failed.contains(id) {
            Offline::Failed
        } else {
            Offline::None
        }
    }

    pub fn is_later(&self, id: &str) -> bool {
        self.library.iter().any(|e| e.track.id == id && e.later)
    }

    pub fn disk_usage(&self) -> Option<u64> {
        self.disk_usage
    }

    /// Podcasts / videos of `mode` playable offline or on their way: the unfinished ones first
    /// (by last played), then "Ver después", then the rest, newest first.
    pub fn library_tracks(&self, mode: Mode) -> Vec<Track> {
        let mut entries: Vec<&LibraryEntry> = self
            .library
            .iter()
            .filter(|e| e.track.mode == mode && (e.later || self.downloaded.contains(&e.track.id) || self.downloads.contains_key(&e.track.id)))
            .collect();
        let rank = |e: &LibraryEntry| {
            let started = self.progress.get(&e.track.id).filter(|p| !p.finished && p.pos > 20.).map(|p| p.at);
            (started.is_none(), !e.later, -started.unwrap_or(e.at))
        };
        entries.sort_by_key(|e| rank(e));
        entries.into_iter().map(|e| e.track.clone()).collect()
    }

    /// What of `mode` plays offline: songs played lately whose copy is in the cache, or
    /// podcasts / videos downloaded (or on their way).
    pub fn downloaded_tracks(&self, mode: Mode) -> Vec<Track> {
        if mode == Mode::Music {
            return self.history.iter().filter(|t| t.mode == Mode::Music && self.cached_songs.contains(&t.id)).cloned().collect();
        }
        self.library_tracks(mode).into_iter().filter(|t| self.downloaded.contains(&t.id) || self.downloads.contains_key(&t.id)).collect()
    }

    /// "Ver después": saving also downloads it, so it plays offline.
    pub fn toggle_later(&mut self, track: &Track, cx: &mut Context<Self>) {
        let later = !self.is_later(&track.id);
        self.add_to_library(track, Some(later));
        if later {
            self.start_download(track.clone(), true, None, cx);
        }
        cx.notify();
    }

    /// ⬇ on a podcast / video: download it (no-op if it's there or on its way).
    pub fn download(&mut self, track: &Track, cx: &mut Context<Self>) {
        self.start_download(track.clone(), true, None, cx);
    }

    /// Whether `id` has a local copy (downloaded podcast / video or cached song) or one on its way.
    pub fn has_local_copy(&self, id: &str) -> bool {
        self.downloaded.contains(id) || self.downloads.contains_key(id) || self.cached_songs.contains(id)
    }

    /// Cancel a download in progress or delete the local copy (and its library entry).
    pub fn remove_download(&mut self, id: &str, cx: &mut Context<Self>) {
        self.forget_download(id);
        self.cached_songs.remove(id);
        self.refresh_disk_usage(cx);
        cx.notify();
    }

    /// Delete every local copy except "Ver después" (songs cache included).
    pub fn clear_downloads(&mut self, cx: &mut Context<Self>) {
        let keep: HashSet<String> = self.library.iter().filter(|e| e.later).map(|e| e.track.id.clone()).collect();
        for (id, job) in &self.downloads {
            if !keep.contains(id) {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        self.downloads.retain(|id, _| keep.contains(id));
        self.downloaded.retain(|id| keep.contains(id));
        self.library.retain(|e| e.later);
        self.download_failed.clear();
        self.save_library();
        self.disk_usage = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().spawn(async move { delete_cache_except(&keep) }).await;
            let _ = this.update(cx, |m, cx| m.refresh_disk_usage(cx));
        })
        .detach();
    }

    fn add_to_library(&mut self, track: &Track, later: Option<bool>) {
        if let Some(e) = self.library.iter_mut().find(|e| e.track.id == track.id) {
            if let Some(l) = later {
                e.later = l;
            }
        } else {
            self.library.insert(0, LibraryEntry { track: track.clone(), later: later.unwrap_or(false), at: now_secs() });
        }
        self.save_library();
    }

    fn forget_download(&mut self, id: &str) {
        if let Some(job) = self.downloads.remove(id) {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.downloaded.remove(id);
        self.download_failed.remove(id);
        let before = self.library.len();
        self.library.retain(|e| e.track.id != id);
        if self.library.len() != before {
            self.save_library();
        }
        remove_cached_song(id);
    }

    fn save_library(&self) {
        let snapshot = self.library.clone();
        std::thread::spawn(move || save_json(&library_file(), &snapshot));
    }

    /// Download `track` (audio; plus video for videos) into the library. `soon`: right away;
    /// otherwise a few seconds later, so playback takes the first chunks first. `url`: an audio
    /// URL already resolved.
    fn start_download(&mut self, track: Track, soon: bool, url: Option<(String, Instant)>, cx: &mut Context<Self>) {
        if track.mode == Mode::Music || self.downloaded.contains(&track.id) || self.downloads.contains_key(&track.id) {
            return;
        }
        self.add_to_library(&track, None);
        if is_complete(&track) {
            self.downloaded.insert(track.id.clone());
            return;
        }
        self.download_failed.remove(&track.id);
        let progress = Arc::new(AtomicU32::new(0f32.to_bits()));
        let cancel = Arc::new(AtomicBool::new(false));
        self.downloads.insert(track.id.clone(), DownloadJob { progress: progress.clone(), cancel: cancel.clone() });
        self.ensure_thumbs(std::slice::from_ref(&track), cx);
        self.ensure_downloads_tick(cx);
        let id = track.id.clone();
        cx.spawn(async move |this, cx| {
            if !soon {
                cx.background_executor().timer(Duration::from_secs(3)).await;
            }
            let res = cx.background_executor().spawn(async move { run_download(&track, url, &progress, &cancel) }).await;
            let _ = this.update(cx, |m, cx| {
                // Cancelled / removed meanwhile: nothing to record.
                if m.downloads.remove(&id).is_none() {
                    return;
                }
                match res {
                    Ok(()) => {
                        m.downloaded.insert(id.clone());
                    }
                    Err(e) => {
                        crate::startup_log(&format!("music download {id}: {e}"));
                        m.download_failed.insert(id.clone());
                    }
                }
                m.refresh_disk_usage(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Repaint download progress while any download runs.
    fn ensure_downloads_tick(&mut self, cx: &mut Context<Self>) {
        if self._downloads_tick.is_some() {
            return;
        }
        self._downloads_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(700)).await;
                let Ok(busy) = this.update(cx, |m, cx| {
                    let busy = !m.downloads.is_empty();
                    if busy {
                        cx.notify();
                    } else {
                        m._downloads_tick = None;
                    }
                    busy
                }) else {
                    break;
                };
                if !busy {
                    break;
                }
            }
        }));
    }

    /// Measure the local copies' size (shown in the search footer).
    pub fn refresh_disk_usage(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let (bytes, songs) = cx.background_executor().spawn(async move { (dir_size(&song_cache_dir()), cached_song_ids()) }).await;
            let _ = this.update(cx, |m, cx| {
                m.disk_usage = Some(bytes);
                m.cached_songs = songs;
                cx.notify();
            });
        })
        .detach();
    }

    /// Compute suggestions for `mode` unless fresh ones exist (30 min). `force` re-runs them
    /// now with other topics from the preferences.
    pub fn ensure_suggestions(&mut self, mode: Mode, force: bool, cx: &mut Context<Self>) {
        if self.suggesting.contains(&mode) || (!force && self.suggestions.get(&mode).is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(1800))) {
            return;
        }
        self.suggesting.insert(mode);
        // Podcasts / videos: never suggest what was finished; unfinished ones go first.
        let mut continuing: Vec<&Progress> = self.progress.values().filter(|p| p.track.mode == mode && !p.finished && p.pos > 20.).collect();
        continuing.sort_by(|a, b| b.at.cmp(&a.at));
        let continuing: Vec<Track> = continuing.into_iter().take(5).map(|p| p.track.clone()).collect();
        let mut exclude: HashSet<String> = if mode == Mode::Music { HashSet::new() } else { self.progress.keys().cloned().collect() };
        // Never suggest what got a 👎.
        exclude.extend(self.ratings.iter().filter(|(_, g)| !**g).map(|(id, _)| id.clone()));
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        // Same topics all day unless refreshed by hand.
        let seed = if force { nanos } else { nanos / 86_400_000_000_000 };
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { suggest(mode, &exclude, seed) }).await;
            let _ = this.update(cx, |m, cx| {
                m.suggesting.remove(&mode);
                match res {
                    Ok(mut s) => {
                        if !continuing.is_empty() {
                            let ids: HashSet<String> = continuing.iter().map(|t| t.id.clone()).collect();
                            s.tracks.retain(|t| !ids.contains(&t.id));
                            s.tracks.splice(0..0, continuing);
                        }
                        m.ensure_thumbs(&s.tracks, cx);
                        m.suggestions.insert(mode, (Instant::now(), s));
                    }
                    Err(e) => {
                        m.suggestions.insert(mode, (Instant::now() - Duration::from_secs(1700), Suggestions { header: format!("Sin sugerencias: {e}"), tracks: continuing }));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn rating(&self, id: &str) -> Option<bool> {
        self.ratings.get(id).copied()
    }

    /// 👍 / 👎 on a track; the same one again clears it. Saved locally and appended to the
    /// personal brain's log (committed there), which the preferences task reads.
    pub fn rate(&mut self, track: &Track, good: bool, cx: &mut Context<Self>) {
        let rating = if self.ratings.get(&track.id) == Some(&good) { None } else { Some(good) };
        match rating {
            Some(g) => {
                self.ratings.insert(track.id.clone(), g);
            }
            None => {
                self.ratings.remove(&track.id);
            }
        }
        // 👎: out of the current suggestions right away.
        if rating == Some(false) {
            for (_, s) in self.suggestions.values_mut() {
                s.tracks.retain(|t| t.id != track.id);
            }
        }
        self.sync_status_item();
        let snapshot = self.ratings.clone();
        let track = track.clone();
        std::thread::spawn(move || {
            save_json(&ratings_file(), &snapshot);
            if let Err(e) = log_rating_to_brain(&track, rating) {
                crate::startup_log(&format!("music rating → personal brain: {e}"));
            }
        });
        cx.notify();
    }

    /// ✕ on a suggestion's topic: never suggest it again (moved to `avoid` in the preferences,
    /// so the weekly task won't bring it back either) and drop its results from the list.
    pub fn drop_topic(&mut self, mode: Mode, topic: &str, cx: &mut Context<Self>) {
        if let Err(e) = remove_topic(mode, topic) {
            self.error = Some(format!("Preferencias: {e}"));
        }
        if let Some((_, s)) = self.suggestions.get_mut(&mode) {
            s.tracks.retain(|t| t.topic.as_deref() != Some(topic));
            s.header = s.header.replace(&format!(" · {topic}"), "");
        }
        cx.notify();
    }

    /// Run the weekly Claude task now (rewrites preferences.json), then refresh suggestions.
    pub fn renew_preferences(&mut self, cx: &mut Context<Self>) {
        if self.renewing {
            return;
        }
        self.renewing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { run_renew_script() }).await;
            let _ = this.update(cx, |m, cx| {
                m.renewing = false;
                if let Err(e) = res {
                    m.error = Some(format!("Claude: {e}"));
                }
                m.suggestions.clear();
                for mode in Mode::ALL {
                    m.ensure_suggestions(mode, true, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}

// ---- yt-dlp -------------------------------------------------------------------------------------

fn music_dir() -> PathBuf {
    workspace_state::state_dir().join("music")
}

fn thumbs_dir() -> PathBuf {
    music_dir().join("thumbs")
}

fn history_file() -> PathBuf {
    music_dir().join("history.json")
}

fn load_history() -> Vec<Track> {
    std::fs::read(history_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_history(history: &[Track]) {
    let path = history_file();
    let _ = std::fs::create_dir_all(music_dir());
    if let Ok(json) = serde_json::to_vec(history) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

fn ytdlp() -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .map(|d| Path::new(d).join("yt-dlp"))
        .chain(["/opt/homebrew/bin/yt-dlp", "/usr/local/bin/yt-dlp"].map(PathBuf::from))
        .find(|p| p.is_file())
}

fn run_ytdlp(args: &[&str]) -> Result<String, String> {
    let bin = ytdlp().ok_or("yt-dlp no está instalado (brew install yt-dlp)")?;
    let out = Command::new(bin)
        .args(["--no-update", "--no-warnings", "--ignore-config", "--socket-timeout", "15"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("yt-dlp: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().rev().find(|l| l.starts_with("ERROR")).or_else(|| err.lines().last()).unwrap_or("falló");
        Err(line.trim_start_matches("ERROR: ").chars().take(200).collect())
    }
}

/// YouTube only enables a freshly issued stream URL a few seconds after handing it out (yt-dlp:
/// "Sleeping 5 seconds as required by the site"); before that every range is 403. URLs resolved
/// ahead of time (top results, next track) are usually past this by the time they're played.
const URL_WARMUP: Duration = Duration::from_millis(5500);

async fn wait_until_valid(resolved: Instant, cx: &mut gpui::AsyncApp) {
    let wait = URL_WARMUP.saturating_sub(resolved.elapsed());
    if !wait.is_zero() {
        cx.background_executor().timer(wait).await;
    }
}

fn stream_url(id: &str, video: bool) -> Result<String, String> {
    let url = format!("https://www.youtube.com/watch?v={id}");
    // AVPlayer plays MP4/AAC/H.264 only. Low bitrates on purpose: starts sooner, less data.
    let format = if video { "bestvideo[vcodec^=avc1][height<=480][ext=mp4]/18" } else { "140/139/bestaudio[ext=m4a]/18" };
    let out = run_ytdlp(&["-f", format, "-g", &url])?;
    out.lines().map(str::trim).find(|l| l.starts_with("http")).map(str::to_string).ok_or_else(|| "sin URL de audio".to_string())
}

/// Exact stream length from googlevideo's `dur=` query parameter.
fn url_duration(url: &str) -> Option<f64> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.strip_prefix("dur=")).and_then(|v| v.parse().ok()).filter(|d: &f64| *d > 0.)
}

fn flat(target: &str, limit: Option<usize>) -> Result<Vec<Value>, String> {
    flat_lang(target, limit, false)
}

/// `dates`: English pages (the only language yt-dlp parses "3 months ago" from) with
/// approximate upload timestamps; titles come back auto-translated, so only dates are used.
fn flat_lang(target: &str, limit: Option<usize>, dates: bool) -> Result<Vec<Value>, String> {
    let end = limit.map(|n| n.to_string());
    // Original / Spanish titles instead of YouTube's English auto-translations.
    let mut args = vec!["--flat-playlist", "-J", "--extractor-args", if dates { "youtube:lang=en" } else { "youtube:lang=es" }];
    if dates {
        args.extend(["--extractor-args", "youtubetab:approximate_date"]);
    }
    if let Some(end) = end.as_deref() {
        args.extend(["--playlist-end", end]);
    }
    args.push(target);
    let out = run_ytdlp(&args)?;
    let v: Value = serde_json::from_str(&out).map_err(|e| format!("respuesta inválida: {e}"))?;
    Ok(v.get("entries").and_then(Value::as_array).cloned().unwrap_or_default())
}

fn parse(e: &Value, mode: Mode) -> Option<Track> {
    let id = e.get("id")?.as_str()?.to_string();
    if id.len() != 11 || e.get("live_status").and_then(Value::as_str) == Some("is_live") {
        return None;
    }
    let title = e.get("title")?.as_str()?.to_string();
    if title == "[Private video]" || title == "[Deleted video]" {
        return None;
    }
    let channel = e.get("channel").or_else(|| e.get("uploader")).and_then(Value::as_str).unwrap_or("").to_string();
    let channel_url = e.get("channel_url").or_else(|| e.get("uploader_url")).and_then(Value::as_str).map(str::to_string);
    Some(Track {
        id,
        title,
        channel,
        channel_url,
        duration: e.get("duration").and_then(Value::as_f64),
        views: e.get("view_count").and_then(Value::as_u64),
        mode,
        topic: None,
        published: e.get("timestamp").and_then(Value::as_i64),
    })
}

/// Search results with the mode's automatic filter (`filtered`). Shorts never qualify.
pub fn search(mode: Mode, query: &str, filtered: bool) -> Result<Vec<Track>, String> {
    let q = query.trim();
    let (q, n) = match mode {
        Mode::Music => (q.to_string(), 40),
        Mode::Podcast if q.to_lowercase().contains("podcast") => (q.to_string(), 30),
        Mode::Podcast => (format!("{q} podcast"), 30),
        Mode::Videos => (q.to_string(), 25),
    };
    let target = format!("ytsearch{n}:{q}");
    // Videos / podcasts show how old they are: the dates come from a parallel English request.
    let dates = (mode != Mode::Music).then(|| {
        let target = target.clone();
        std::thread::spawn(move || flat_lang(&target, None, true).unwrap_or_default())
    });
    let entries = flat(&target, None)?;
    let published: HashMap<String, i64> = dates
        .and_then(|h| h.join().ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|e| Some((e.get("id")?.as_str()?.to_string(), e.get("timestamp")?.as_i64()?)))
        .collect();
    Ok(entries
        .iter()
        .filter_map(|e| parse(e, mode))
        .map(|mut t| {
            t.published = t.published.or_else(|| published.get(&t.id).copied());
            t
        })
        .filter(|t| keep(t, mode, filtered))
        .collect())
}

fn keep(t: &Track, mode: Mode, filtered: bool) -> bool {
    let dur = t.duration.unwrap_or(0.);
    if dur > 0. && dur < 61. {
        return false; // Shorts
    }
    if !filtered {
        return true;
    }
    let views_ok = mode.min_views().is_none_or(|min| t.views.is_some_and(|v| v >= min));
    let length_ok = mode != Mode::Podcast || dur >= 600.;
    views_ok && length_ok
}

/// What to show before typing: a few topics from `preferences.json` (rotating by `seed`),
/// searched in parallel and interleaved, minus `exclude` (finished / started podcasts & videos).
fn suggest(mode: Mode, exclude: &HashSet<String>, seed: u64) -> Result<Suggestions, String> {
    let topics = preference_topics(mode);
    if topics.is_empty() {
        return Err("no hay temas en preferences.json".into());
    }
    let n = topics.len().min(4);
    let start = (seed % topics.len() as u64) as usize;
    let picked: Vec<String> = (0..n).map(|i| topics[(start + i) % topics.len()].clone()).collect();
    let lists: Vec<Vec<Track>> = std::thread::scope(|s| {
        let handles: Vec<_> = picked
            .iter()
            .map(|q| {
                s.spawn(move || {
                    let mut list = search(mode, q, true).unwrap_or_default();
                    for t in &mut list {
                        t.topic = Some(q.clone());
                    }
                    list
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    let mut seen: HashSet<String> = exclude.clone();
    let mut tracks = Vec::new();
    for i in 0..12 {
        for l in &lists {
            if let Some(t) = l.get(i) {
                if seen.insert(t.id.clone()) {
                    tracks.push(t.clone());
                }
            }
        }
    }
    if tracks.is_empty() {
        return Err("las búsquedas no devolvieron nada".into());
    }
    Ok(Suggestions { header: format!("Para ti · {}", picked.join(" · ")), tracks })
}

// ---- preferences & progress files ------------------------------------------------------------

const DEFAULT_PREFERENCES: &str = include_str!("../assets/music-preferences.json");
const RENEW_SCRIPT: &str = include_str!("../assets/music-update-preferences.sh");

pub fn preferences_file() -> PathBuf {
    music_dir().join("preferences.json")
}

fn volume_file() -> PathBuf {
    music_dir().join("volume.json")
}

fn load_volume() -> f32 {
    std::fs::read(volume_file()).ok().and_then(|b| serde_json::from_slice::<f32>(&b).ok()).map(|v| v.clamp(0., 1.)).unwrap_or(1.)
}

fn progress_file() -> PathBuf {
    music_dir().join("progress.json")
}

/// Writes the default preferences the first time; returns the path (to open it in the editor).
pub fn ensure_preferences_file() -> PathBuf {
    let path = preferences_file();
    if !path.exists() {
        let _ = std::fs::create_dir_all(music_dir());
        let _ = std::fs::write(&path, DEFAULT_PREFERENCES);
    }
    path
}

fn preference_topics(mode: Mode) -> Vec<String> {
    let path = ensure_preferences_file();
    let v: Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).or_else(|| serde_json::from_str(DEFAULT_PREFERENCES).ok()).unwrap_or_default();
    let key = match mode {
        Mode::Music => "music",
        Mode::Podcast => "podcast",
        Mode::Videos => "videos",
    };
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).filter(|s| !s.trim().is_empty()).collect()).unwrap_or_default()
}

fn remove_topic(mode: Mode, topic: &str) -> Result<(), String> {
    let path = ensure_preferences_file();
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| format!("JSON inválido: {e}"))?;
    let key = match mode {
        Mode::Music => "music",
        Mode::Podcast => "podcast",
        Mode::Videos => "videos",
    };
    if let Some(list) = v.get_mut(key).and_then(Value::as_array_mut) {
        list.retain(|x| x.as_str() != Some(topic));
    }
    let obj = v.as_object_mut().ok_or("preferences.json no es un objeto")?;
    let avoid = obj.entry("avoid").or_insert_with(|| Value::Array(Vec::new()));
    if let Some(a) = avoid.as_array_mut() {
        if !a.iter().any(|x| x.as_str() == Some(topic)) {
            a.push(Value::String(topic.to_string()));
        }
    }
    let json = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    std::fs::write(&path, json + "\n").map_err(|e| e.to_string())
}

/// "updated" date of the preferences (shown in the search footer).
pub fn preferences_updated() -> Option<String> {
    let v: Value = std::fs::read(preferences_file()).ok().and_then(|b| serde_json::from_slice(&b).ok())?;
    v.get("updated").and_then(Value::as_str).map(str::to_string)
}

// ---- song cache (30 days) ------------------------------------------------------------------------

const SONG_CACHE_DAYS: u64 = 30;

fn song_cache_dir() -> PathBuf {
    music_dir().join("cache")
}

/// Local copy of a song and its exact length, refreshing its 30-day clock.
fn cached_song(id: &str) -> Option<(PathBuf, Option<f64>)> {
    let path = song_cache_dir().join(format!("{id}.m4a"));
    let file = std::fs::File::options().append(true).open(&path).ok()?;
    let _ = file.set_modified(std::time::SystemTime::now());
    let dur = std::fs::read_to_string(path.with_extension("dur")).ok().and_then(|s| s.trim().parse().ok());
    Some((path, dur))
}

fn cache_song(id: &str, url: &str, dur: Option<f64>) {
    let dir = song_cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{id}.m4a"));
    if path.exists() {
        return;
    }
    // Let playback take the first chunks before downloading the whole song next to it.
    std::thread::sleep(Duration::from_secs(3));
    if crate::music_proxy::download_to(url, &path) {
        if let Some(d) = dur {
            let _ = std::fs::write(path.with_extension("dur"), d.to_string());
        }
    }
}

fn audio_file(id: &str) -> PathBuf {
    song_cache_dir().join(format!("{id}.m4a"))
}

/// Video-only stream of a downloaded video (its audio is the `.m4a` next to it).
fn video_file(id: &str) -> PathBuf {
    song_cache_dir().join(format!("{id}.v.mp4"))
}

fn is_complete(track: &Track) -> bool {
    audio_file(&track.id).exists() && (track.mode != Mode::Videos || video_file(&track.id).exists())
}

fn remove_cached_song(id: &str) {
    let path = audio_file(id);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("dur"));
    let _ = std::fs::remove_file(video_file(id));
}

fn library_file() -> PathBuf {
    music_dir().join("library.json")
}

fn load_library() -> Vec<LibraryEntry> {
    std::fs::read(library_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

// ---- ratings (👍 / 👎) ----------------------------------------------------------------------------

fn ratings_file() -> PathBuf {
    music_dir().join("ratings.json")
}

fn load_ratings() -> HashMap<String, bool> {
    std::fs::read(ratings_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The personal brain repo (`$JUAN_BRAIN_DIR`, else ~/Desktop/repositories/personal brain).
pub fn brain_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("JUAN_BRAIN_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Desktop/repositories/personal brain")
}

/// Log of ratings inside the brain (read by the preferences task).
pub const BRAIN_RATINGS_FILE: &str = "calificaciones-media.md";

const BRAIN_RATINGS_HEADER: &str = "# Calificaciones de música, podcasts y videos\n\n\
Log automático desde el reproductor de juan-terminal: una línea por calificación (👍 gustó, 👎 no gustó, ⊘ se quitó). \
La tarea semanal de preferencias lo lee para sugerir más de lo que gusta y nada de lo que no.\n\n";

/// Append one line to the brain's ratings log, commit it there (only that file) and push.
fn log_rating_to_brain(track: &Track, rating: Option<bool>) -> Result<(), String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = brain_dir();
    if !dir.join(".git").exists() {
        return Err(format!("{} no es un repo git", dir.display()));
    }
    let path = dir.join(BRAIN_RATINGS_FILE);
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.is_empty() {
        text.push_str(BRAIN_RATINGS_HEADER);
    } else if !text.ends_with('\n') {
        text.push('\n');
    }
    let mark = match rating {
        Some(true) => "👍",
        Some(false) => "👎",
        None => "⊘",
    };
    let clean = |s: &str| s.replace(['\n', '\r'], " ").trim().to_string();
    let mut line = format!("- {} · {mark} · {} · {}", chrono::Local::now().format("%Y-%m-%d %H:%M"), track.mode.label(), clean(&track.title));
    if !track.channel.is_empty() {
        line.push_str(&format!(" — {}", clean(&track.channel)));
    }
    if let Some(topic) = &track.topic {
        line.push_str(&format!(" · tema: {}", clean(topic)));
    }
    line.push_str(&format!(" · https://youtu.be/{}\n", track.id));
    text.push_str(&line);
    std::fs::write(&path, text).map_err(|e| e.to_string())?;

    let git = |args: &[&str]| -> Result<(), String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            // Never block on a password / host-key prompt (runs from the GUI, no terminal).
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=10")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect()) }
    };
    git(&["add", "--", BRAIN_RATINGS_FILE])?;
    let title: String = clean(&track.title).chars().take(50).collect();
    let msg = format!("{} {}: {title}", if rating.is_some() { "Calificar" } else { "Quitar calificación" }, mark);
    git(&["commit", "-q", "-m", &msg, "--", BRAIN_RATINGS_FILE])?;
    // Push; if the remote moved ahead, rebase onto it and retry once.
    if git(&["push", "-q"]).is_err() {
        git(&["pull", "-q", "--rebase", "--autostash"]).map_err(|e| format!("push: pull --rebase falló: {e}"))?;
        git(&["push", "-q"]).map_err(|e| format!("push falló: {e}"))?;
    }
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Ids of the songs with an `.m4a` in the cache.
fn cached_song_ids() -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(song_cache_dir()) else { return HashSet::new() };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".m4a")).map(str::to_string))
        .collect()
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map(|it| it.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()).unwrap_or(0)
}

/// Delete every cached file whose id isn't in `keep`.
fn delete_cache_except(keep: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(song_cache_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let id = name.split('.').next().unwrap_or("");
        if !keep.contains(id) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Blocking: fetch the audio (and, for videos, the video stream) of `track` into the cache,
/// reporting 0..1 on `progress`.
fn run_download(track: &Track, url: Option<(String, Instant)>, progress: &AtomicU32, cancel: &AtomicBool) -> Result<(), String> {
    let _ = std::fs::create_dir_all(song_cache_dir());
    let video = track.mode == Mode::Videos;
    // Audio is a small part of a video's bytes.
    let audio_share = if video { 0.15 } else { 1. };
    let set = |f: f32| progress.store(f.clamp(0., 1.).to_bits(), Ordering::Relaxed);
    let fetch = |video: bool, known: Option<(String, Instant)>| -> Result<String, String> {
        let (url, at) = match known.filter(|(_, at)| at.elapsed() < Duration::from_secs(4 * 3600)) {
            Some(u) => u,
            None => (stream_url(&track.id, video)?, Instant::now()),
        };
        std::thread::sleep(URL_WARMUP.saturating_sub(at.elapsed()));
        Ok(url)
    };
    let audio = audio_file(&track.id);
    if !audio.exists() {
        let url = fetch(false, url)?;
        let ok = crate::music_proxy::download_with_progress(&url, &audio, |done, total| {
            if total > 0 {
                set(audio_share * done as f32 / total as f32);
            }
            !cancel.load(Ordering::Relaxed)
        });
        if !ok {
            return Err(if cancel.load(Ordering::Relaxed) { "cancelada".into() } else { "falló el audio".into() });
        }
        if let Some(d) = url_duration(&url).or(track.duration) {
            let _ = std::fs::write(audio.with_extension("dur"), d.to_string());
        }
    }
    if video {
        let file = video_file(&track.id);
        if !file.exists() {
            let url = fetch(true, None)?;
            let ok = crate::music_proxy::download_with_progress(&url, &file, |done, total| {
                if total > 0 {
                    set(audio_share + (1. - audio_share) * done as f32 / total as f32);
                }
                !cancel.load(Ordering::Relaxed)
            });
            if !ok {
                return Err(if cancel.load(Ordering::Relaxed) { "cancelada".into() } else { "falló el video".into() });
            }
        }
    }
    set(1.);
    Ok(())
}

/// Drop songs not played for 30 days and leftovers of interrupted downloads. Podcasts /
/// videos in the library stay until they're finished or removed.
fn prune_song_cache() {
    let Ok(entries) = std::fs::read_dir(song_cache_dir()) else { return };
    let keep: HashSet<String> = load_library().into_iter().map(|e| e.track.id).collect();
    let max_age = Duration::from_secs(SONG_CACHE_DAYS * 86_400);
    for e in entries.flatten() {
        let path = e.path();
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        let name = e.file_name().to_string_lossy().into_owned();
        let kept = keep.contains(name.split('.').next().unwrap_or(""));
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > max_age);
        if ext == "part" || (!kept && old && matches!(ext, "m4a" | "mp4")) {
            let _ = std::fs::remove_file(video_file(name.split('.').next().unwrap_or("")));
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("dur"));
        }
    }
}

fn load_progress() -> HashMap<String, Progress> {
    let list: Vec<Progress> = std::fs::read(progress_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    list.into_iter().map(|p| (p.track.id.clone(), p)).collect()
}

fn save_json<T: Serialize>(path: &Path, value: &T) {
    let _ = std::fs::create_dir_all(music_dir());
    if let Ok(json) = serde_json::to_vec_pretty(value) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

/// The weekly task's script (also installed as a LaunchAgent); kept in sync with the binary.
pub fn renew_script() -> PathBuf {
    let path = music_dir().join("update-preferences.sh");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(RENEW_SCRIPT) {
        let _ = std::fs::create_dir_all(music_dir());
        let _ = std::fs::write(&path, RENEW_SCRIPT);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn run_renew_script() -> Result<(), String> {
    ensure_preferences_file();
    let out = Command::new("/bin/zsh").arg(renew_script()).stdin(std::process::Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().last().unwrap_or("falló").chars().take(200).collect())
    }
}

/// Fetch `mqdefault.jpg` (320×180) for each id in parallel; returns the ids that succeeded.
fn download_thumbs(ids: &[String]) -> Vec<String> {
    let dir = thumbs_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    let children: Vec<(String, PathBuf, Option<std::process::Child>)> = ids
        .iter()
        .map(|id| {
            let tmp = dir.join(format!("{id}.part"));
            let child = Command::new("curl")
                .args(["-sfL", "-m", "15", "-o"])
                .arg(&tmp)
                .arg(format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"))
                .stdin(std::process::Stdio::null())
                .spawn()
                .ok();
            (id.clone(), tmp, child)
        })
        .collect();
    let mut done = Vec::new();
    for (id, tmp, child) in children {
        let ok = child.and_then(|mut c| c.wait().ok()).is_some_and(|s| s.success());
        if ok && std::fs::rename(&tmp, dir.join(format!("{id}.jpg"))).is_ok() {
            done.push(id);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    done
}

// ---- formatting ---------------------------------------------------------------------------------

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// "hace 3 meses" from a unix timestamp.
pub fn fmt_ago(ts: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let s = (now - ts).max(0);
    let (n, one, many) = match s {
        s if s < 3600 => return "hace un momento".into(),
        s if s < 86_400 => (s / 3600, "hora", "horas"),
        s if s < 7 * 86_400 => (s / 86_400, "día", "días"),
        s if s < 30 * 86_400 => (s / (7 * 86_400), "semana", "semanas"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "mes", "meses"),
        s => (s / (365 * 86_400), "año", "años"),
    };
    format!("hace {n} {}", if n == 1 { one } else { many })
}

pub fn fmt_views(v: u64) -> String {
    if v >= 1_000_000_000 {
        format!("{:.1} mil M", v as f64 / 1e9)
    } else if v >= 1_000_000 {
        let m = v as f64 / 1e6;
        if m >= 10. { format!("{m:.0} M") } else { format!("{m:.1} M") }
    } else if v >= 1_000 {
        format!("{:.0} k", v as f64 / 1e3)
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(views: Option<u64>, duration: f64) -> Track {
        Track { id: "aaaaaaaaaaa".into(), title: "x".into(), channel: String::new(), channel_url: None, duration: Some(duration), views, mode: Mode::Music, topic: None, published: None }
    }

    #[test]
    fn filters_by_mode() {
        assert!(keep(&t(Some(2_000_000), 200.), Mode::Music, true));
        assert!(!keep(&t(Some(900_000), 200.), Mode::Music, true));
        assert!(keep(&t(Some(900_000), 200.), Mode::Music, false));
        assert!(!keep(&t(Some(9_000_000), 30.), Mode::Music, false), "shorts never");
        assert!(keep(&t(Some(6_000), 3600.), Mode::Podcast, true));
        assert!(!keep(&t(Some(4_000), 3600.), Mode::Podcast, true));
        assert!(!keep(&t(Some(60_000), 300.), Mode::Podcast, true), "too short for a podcast");
        assert!(keep(&t(None, 300.), Mode::Videos, true));
    }

    /// Manual: `cargo test -p juan-terminal music_live -- --ignored --nocapture` (network).
    #[test]
    #[ignore]
    fn music_live() {
        for (mode, q) in [(Mode::Music, "soda stereo"), (Mode::Podcast, "inteligencia artificial"), (Mode::Videos, "rust programming")] {
            let r = search(mode, q, true).unwrap();
            println!("{mode:?} '{q}': {} results", r.len());
            for t in r.iter().take(3) {
                println!("   {} | {} | {:?} views | {:?}s", t.title, t.channel, t.views, t.duration);
            }
            assert!(r.iter().all(|t| keep(t, mode, true)));
        }
        for mode in Mode::ALL {
            let t = std::time::Instant::now();
            let s = suggest(mode, &HashSet::new(), 0).unwrap();
            println!("{} → {} en {:?} ({})", s.header, s.tracks.len(), t.elapsed(), s.tracks.first().map(|t| t.title.as_str()).unwrap_or(""));
        }
        let t = std::time::Instant::now();
        let url = stream_url("xw9cgCr0VmE", false).unwrap();
        println!("stream_url en {:?}: {}", t.elapsed(), &url[..60]);
    }

    #[test]
    fn duration_from_url() {
        assert_eq!(url_duration("https://x.googlevideo.com/videoplayback?a=1&dur=210.094&b=2"), Some(210.094));
        assert_eq!(url_duration("https://x/y?a=1"), None);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_time(65.), "1:05");
        assert_eq!(fmt_time(3725.), "1:02:05");
        assert_eq!(fmt_views(1_234_567), "1.2 M");
        assert_eq!(fmt_views(311_097_654), "311 M");
        assert_eq!(fmt_views(5_400), "5 k");
    }
}
