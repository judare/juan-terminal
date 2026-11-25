//! Native playback for the music player: `AVPlayer` streaming remote URLs (it starts as soon as
//! a few seconds are buffered, nothing is downloaded to disk), plus the system "Now Playing"
//! integration (`MPNowPlayingInfoCenter`) and media keys / Control Center commands
//! (`MPRemoteCommandCenter`). Everything here runs on the main thread.
#![allow(unexpected_cfgs)] // objc 0.2 macros

use std::path::Path;

use futures::channel::mpsc::UnboundedSender;
use objc::runtime::{Object, Sel, YES};
use objc::{class, msg_send, sel, sel_impl};

type Id = *mut Object;

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}
#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMTimeGetSeconds(t: CMTime) -> f64;
    fn CMTimeMakeWithSeconds(seconds: f64, timescale: i32) -> CMTime;
}
#[link(name = "MediaPlayer", kind = "framework")]
unsafe extern "C" {
    static MPMediaItemPropertyTitle: Id;
    static MPMediaItemPropertyArtist: Id;
    static MPMediaItemPropertyPlaybackDuration: Id;
    static MPMediaItemPropertyArtwork: Id;
    static MPNowPlayingInfoPropertyElapsedPlaybackTime: Id;
    static MPNowPlayingInfoPropertyPlaybackRate: Id;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}
unsafe impl objc::Encode for CMTime {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str("{?=qiIq}") }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    w: f64,
    h: f64,
}
unsafe impl objc::Encode for NSSize {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str("{CGSize=dd}") }
    }
}

fn ns_string(s: &str) -> Id {
    let bytes = std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: bytes.as_ptr()] }
}

fn rust_string(ns: Id) -> String {
    if ns.is_null() {
        return String::new();
    }
    unsafe {
        let p: *const std::os::raw::c_char = msg_send![ns, UTF8String];
        if p.is_null() { String::new() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}

/// One `AVPlayer`, kept for the whole session.
pub struct AvPlayer {
    player: Id,
}

impl AvPlayer {
    pub fn new(muted: bool) -> Self {
        unsafe {
            let player: Id = msg_send![class!(AVPlayer), alloc];
            let player: Id = msg_send![player, init];
            if muted {
                let _: () = msg_send![player, setMuted: YES];
            }
            Self { player }
        }
    }

    pub fn raw(&self) -> Id {
        self.player
    }

    /// Replace the current item with a remote stream (progressive: plays while it downloads).
    pub fn load(&self, url: &str) {
        unsafe {
            let u: Id = msg_send![class!(NSURL), URLWithString: ns_string(url)];
            if u.is_null() {
                return;
            }
            let item: Id = msg_send![class!(AVPlayerItem), playerItemWithURL: u];
            // Small forward buffer: start fast, don't pull the whole file ahead of time.
            let _: () = msg_send![item, setPreferredForwardBufferDuration: 15f64];
            let _: () = msg_send![self.player, replaceCurrentItemWithPlayerItem: item];
        }
    }

    /// Play a local file (cached song).
    pub fn load_file(&self, path: &Path) {
        unsafe {
            let u: Id = msg_send![class!(NSURL), fileURLWithPath: ns_string(&path.to_string_lossy())];
            let item: Id = msg_send![class!(AVPlayerItem), playerItemWithURL: u];
            let _: () = msg_send![self.player, replaceCurrentItemWithPlayerItem: item];
        }
    }

    pub fn clear(&self) {
        unsafe {
            let _: () = msg_send![self.player, pause];
            let _: () = msg_send![self.player, replaceCurrentItemWithPlayerItem: std::ptr::null_mut::<Object>()];
        }
    }

    pub fn has_item(&self) -> bool {
        unsafe {
            let item: Id = msg_send![self.player, currentItem];
            !item.is_null()
        }
    }

    pub fn play(&self) {
        unsafe { let _: () = msg_send![self.player, play]; }
    }

    /// Start now with whatever is buffered (fast start); AVPlayer still waits out later stalls.
    pub fn play_now(&self) {
        unsafe { let _: () = msg_send![self.player, playImmediatelyAtRate: 1f32]; }
    }

    /// Play at `rate` (1.0 normal); used to keep the video on the audio clock.
    pub fn set_rate(&self, rate: f32) {
        unsafe {
            let current: f32 = msg_send![self.player, rate];
            if (current - rate).abs() > 0.01 {
                let _: () = msg_send![self.player, setRate: rate];
            }
        }
    }

    pub fn pause(&self) {
        unsafe { let _: () = msg_send![self.player, pause]; }
    }

    /// AVPlayerTimeControlStatus: 0 paused, 1 waiting (buffering), 2 playing.
    pub fn time_control(&self) -> isize {
        unsafe { msg_send![self.player, timeControlStatus] }
    }

    pub fn time(&self) -> f64 {
        let t: CMTime = unsafe { msg_send![self.player, currentTime] };
        let s = unsafe { CMTimeGetSeconds(t) };
        if s.is_finite() { s.max(0.) } else { 0. }
    }

    pub fn duration(&self) -> Option<f64> {
        unsafe {
            let item: Id = msg_send![self.player, currentItem];
            if item.is_null() {
                return None;
            }
            let t: CMTime = msg_send![item, duration];
            let s = CMTimeGetSeconds(t);
            (s.is_finite() && s > 0.).then_some(s)
        }
    }

    pub fn seek(&self, secs: f64) {
        unsafe {
            let t = CMTimeMakeWithSeconds(secs.max(0.), 600);
            let _: () = msg_send![self.player, seekToTime: t];
        }
    }

    /// The item's error once it failed (AVPlayerItemStatusFailed = 2).
    pub fn failure(&self) -> Option<String> {
        unsafe {
            let item: Id = msg_send![self.player, currentItem];
            if item.is_null() {
                return None;
            }
            let status: isize = msg_send![item, status];
            if status != 2 {
                return None;
            }
            let err: Id = msg_send![item, error];
            let desc: Id = if err.is_null() { std::ptr::null_mut() } else { msg_send![err, localizedDescription] };
            Some(rust_string(desc))
        }
    }
}

/// Media keys (⏯ ⏮ ⏭), headphones, Control Center and the menu bar Now Playing widget.
#[derive(Clone, Copy, Debug)]
pub enum Remote {
    Toggle,
    Play,
    Pause,
    Next,
    Previous,
    SkipForward,
    SkipBackward,
    Seek(f64),
}

pub fn install_remote_commands(tx: UnboundedSender<Remote>) {
    unsafe {
        let center: Id = msg_send![class!(MPRemoteCommandCenter), sharedCommandCenter];
        if center.is_null() {
            return;
        }
        let simple: [(&str, Remote); 6] = [
            ("togglePlayPauseCommand", Remote::Toggle),
            ("playCommand", Remote::Play),
            ("pauseCommand", Remote::Pause),
            ("nextTrackCommand", Remote::Next),
            ("previousTrackCommand", Remote::Previous),
            ("skipForwardCommand", Remote::SkipForward),
        ];
        for (name, ev) in simple.into_iter().chain([("skipBackwardCommand", Remote::SkipBackward)]) {
            let cmd: Id = msg_send![center, performSelector: Sel::register(name)];
            if cmd.is_null() {
                continue;
            }
            if name.starts_with("skip") {
                let n: Id = msg_send![class!(NSNumber), numberWithDouble: 15f64];
                let arr: Id = msg_send![class!(NSArray), arrayWithObject: n];
                let _: () = msg_send![cmd, setPreferredIntervals: arr];
            }
            let tx = tx.clone();
            let block = block::ConcreteBlock::new(move |_ev: Id| -> isize {
                let _ = tx.unbounded_send(ev);
                0 // MPRemoteCommandHandlerStatusSuccess
            })
            .copy();
            let _: Id = msg_send![cmd, addTargetWithHandler: &*block];
            let _: () = msg_send![cmd, setEnabled: YES];
            std::mem::forget(block);
        }
        let cmd: Id = msg_send![center, changePlaybackPositionCommand];
        if !cmd.is_null() {
            let tx = tx.clone();
            let block = block::ConcreteBlock::new(move |ev: Id| -> isize {
                let t: f64 = msg_send![ev, positionTime];
                let _ = tx.unbounded_send(Remote::Seek(t));
                0
            })
            .copy();
            let _: Id = msg_send![cmd, addTargetWithHandler: &*block];
            let _: () = msg_send![cmd, setEnabled: YES];
            std::mem::forget(block);
        }
    }
}

/// What the system shows in Control Center / the Now Playing menu bar item.
pub struct NowPlaying<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub duration: f64,
    pub elapsed: f64,
    pub playing: bool,
    pub artwork: Option<&'a Path>,
}

pub fn set_now_playing(info: Option<NowPlaying>) {
    unsafe {
        let center: Id = msg_send![class!(MPNowPlayingInfoCenter), defaultCenter];
        if center.is_null() {
            return;
        }
        let Some(info) = info else {
            let _: () = msg_send![center, setNowPlayingInfo: std::ptr::null_mut::<Object>()];
            // MPNowPlayingPlaybackStateStopped = 3
            let _: () = msg_send![center, setPlaybackState: 3usize];
            return;
        };
        let dict: Id = msg_send![class!(NSMutableDictionary), dictionary];
        let _: () = msg_send![dict, setObject: ns_string(info.title) forKey: MPMediaItemPropertyTitle];
        let _: () = msg_send![dict, setObject: ns_string(info.artist) forKey: MPMediaItemPropertyArtist];
        let num = |v: f64| -> Id { msg_send![class!(NSNumber), numberWithDouble: v] };
        if info.duration > 0. {
            let _: () = msg_send![dict, setObject: num(info.duration) forKey: MPMediaItemPropertyPlaybackDuration];
        }
        let _: () = msg_send![dict, setObject: num(info.elapsed) forKey: MPNowPlayingInfoPropertyElapsedPlaybackTime];
        let _: () = msg_send![dict, setObject: num(if info.playing { 1. } else { 0. }) forKey: MPNowPlayingInfoPropertyPlaybackRate];
        if let Some(path) = info.artwork {
            let image: Id = msg_send![class!(NSImage), alloc];
            let image: Id = msg_send![image, initWithContentsOfFile: ns_string(&path.to_string_lossy())];
            if !image.is_null() {
                let size: NSSize = msg_send![image, size];
                let handler = block::ConcreteBlock::new(move |_s: NSSize| -> Id { image }).copy();
                let art: Id = msg_send![class!(MPMediaItemArtwork), alloc];
                let art: Id = msg_send![art, initWithBoundsSize: size requestHandler: &*handler];
                if !art.is_null() {
                    let _: () = msg_send![dict, setObject: art forKey: MPMediaItemPropertyArtwork];
                    let _: () = msg_send![art, release];
                }
                // The artwork keeps its own copy of the block; the image lives as long as it.
            }
        }
        let _: () = msg_send![center, setNowPlayingInfo: dict];
        // MPNowPlayingPlaybackStatePlaying = 1, Paused = 2
        let _: () = msg_send![center, setPlaybackState: if info.playing { 1usize } else { 2usize }];
    }
}

// ---- menu bar item -----------------------------------------------------------------------------

/// What the menu bar item's menu asks for.
#[derive(Clone, Copy, Debug)]
pub enum MenuCommand {
    Remote(Remote),
    OpenSearch,
    Stop,
}

const MENU_COMMANDS: [MenuCommand; 7] = [
    MenuCommand::Remote(Remote::Toggle),
    MenuCommand::Remote(Remote::SkipBackward),
    MenuCommand::Remote(Remote::SkipForward),
    MenuCommand::Remote(Remote::Previous),
    MenuCommand::Remote(Remote::Next),
    MenuCommand::OpenSearch,
    MenuCommand::Stop,
];

static MENU_TX: std::sync::Mutex<Option<UnboundedSender<MenuCommand>>> = std::sync::Mutex::new(None);

struct StatusItem {
    item: Id,
    header: Id,
    toggle: Id,
}

thread_local! {
    static STATUS: std::cell::RefCell<Option<StatusItem>> = const { std::cell::RefCell::new(None) };
}

extern "C" fn on_menu_item(_: &Object, _: Sel, sender: Id) {
    let tag: isize = unsafe { msg_send![sender, tag] };
    let Some(cmd) = usize::try_from(tag).ok().and_then(|i| MENU_COMMANDS.get(i)).copied() else { return };
    if let Some(tx) = MENU_TX.lock().ok().and_then(|t| t.clone()) {
        let _ = tx.unbounded_send(cmd);
    }
}

fn menu_target() -> Id {
    use objc::declare::ClassDecl;
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cls = *CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("JTMusicMenuTarget", class!(NSObject)).expect("JTMusicMenuTarget");
        unsafe { decl.add_method(sel!(musicMenuItem:), on_menu_item as extern "C" fn(&Object, Sel, Id)) };
        decl.register() as *const objc::runtime::Class as usize
    });
    unsafe {
        let obj: Id = msg_send![cls as *const objc::runtime::Class, new];
        obj
    }
}

/// Menu bar item for the player: what's playing, and play / pause, ±15 s, prev / next.
pub fn install_status_item(tx: UnboundedSender<MenuCommand>) {
    if let Ok(mut t) = MENU_TX.lock() {
        *t = Some(tx);
    }
}

fn build_status_item() -> StatusItem {
    unsafe {
        let bar: Id = msg_send![class!(NSStatusBar), systemStatusBar];
        // NSVariableStatusItemLength = -1
        let item: Id = msg_send![bar, statusItemWithLength: -1f64];
        let _: Id = msg_send![item, retain];
        let menu: Id = msg_send![class!(NSMenu), alloc];
        let menu: Id = msg_send![menu, initWithTitle: ns_string("Música")];
        let _: () = msg_send![menu, setAutoenablesItems: objc::runtime::NO];
        let target = menu_target();
        let add = |title: &str, tag: Option<isize>| -> Id {
            let mi: Id = msg_send![class!(NSMenuItem), alloc];
            let action = if tag.is_some() { sel!(musicMenuItem:) } else { Sel::from_ptr(std::ptr::null()) };
            let mi: Id = msg_send![mi, initWithTitle: ns_string(title) action: action keyEquivalent: ns_string("")];
            if let Some(tag) = tag {
                let _: () = msg_send![mi, setTag: tag];
                let _: () = msg_send![mi, setTarget: target];
            } else {
                let _: () = msg_send![mi, setEnabled: objc::runtime::NO];
            }
            let _: () = msg_send![menu, addItem: mi];
            mi
        };
        let separator = || {
            let sep: Id = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![menu, addItem: sep];
        };
        let header = add("", None);
        separator();
        let toggle = add("Pausa", Some(0));
        add("−15 s", Some(1));
        add("+15 s", Some(2));
        add("Anterior", Some(3));
        add("Siguiente", Some(4));
        separator();
        add("Buscar música, podcasts y videos… (⌘⇧M)", Some(5));
        add("Cerrar reproductor", Some(6));
        let _: () = msg_send![item, setMenu: menu];
        StatusItem { item, header, toggle }
    }
}

/// Show / update the menu bar item (`None` hides it).
pub fn set_status_item(info: Option<(&str, &str, bool)>) {
    STATUS.with(|s| {
        let mut s = s.borrow_mut();
        let Some((title, artist, playing)) = info else {
            if let Some(st) = s.as_ref() {
                unsafe { let _: () = msg_send![st.item, setVisible: objc::runtime::NO]; }
            }
            return;
        };
        let st = s.get_or_insert_with(build_status_item);
        unsafe {
            let _: () = msg_send![st.item, setVisible: YES];
            let button: Id = msg_send![st.item, button];
            if !button.is_null() {
                let symbol = if playing { "music.note" } else { "pause.fill" };
                let image: Id = msg_send![class!(NSImage), imageWithSystemSymbolName: ns_string(symbol) accessibilityDescription: ns_string("Música")];
                if !image.is_null() {
                    let _: () = msg_send![image, setTemplate: YES];
                    let _: () = msg_send![button, setImage: image];
                    // NSImageLeft = 2
                    let _: () = msg_send![button, setImagePosition: 2isize];
                }
                let short: String = if title.chars().count() > 28 { format!(" {}…", title.chars().take(27).collect::<String>().trim_end()) } else { format!(" {title}") };
                let _: () = msg_send![button, setTitle: ns_string(&short)];
            }
            let header = if artist.is_empty() { title.to_string() } else { format!("{title} — {artist}") };
            let _: () = msg_send![st.header, setTitle: ns_string(&header)];
            let _: () = msg_send![st.toggle, setTitle: ns_string(if playing { "Pausa" } else { "Reproducir" })];
        }
    });
}
