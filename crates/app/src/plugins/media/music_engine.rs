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

    /// 0…1 (AVPlayer's own gain, independent of the system volume).
    pub fn set_volume(&self, v: f32) {
        unsafe { let _: () = msg_send![self.player, setVolume: v.clamp(0., 1.)]; }
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

// ---- menu bar item: a mini player ------------------------------------------------------------

/// What the menu bar item's menu asks for.
#[derive(Clone, Copy, Debug)]
pub enum MenuCommand {
    Remote(Remote),
    OpenSearch,
    Stop,
    /// 👍 (true) / 👎 (false) on what's playing.
    Rate(bool),
    /// 0…1 from the volume slider.
    Volume(f32),
}

/// Indexed by the controls' `tag`.
const MENU_COMMANDS: [MenuCommand; 9] = [
    MenuCommand::Remote(Remote::Toggle),
    MenuCommand::Remote(Remote::SkipBackward),
    MenuCommand::Remote(Remote::SkipForward),
    MenuCommand::Remote(Remote::Previous),
    MenuCommand::Remote(Remote::Next),
    MenuCommand::OpenSearch,
    MenuCommand::Stop,
    MenuCommand::Rate(true),
    MenuCommand::Rate(false),
];

static MENU_TX: std::sync::Mutex<Option<UnboundedSender<MenuCommand>>> = std::sync::Mutex::new(None);

/// What the mini player shows.
pub struct StatusInfo<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub playing: bool,
    pub elapsed: f64,
    pub duration: f64,
    pub artwork: Option<&'a Path>,
    pub rating: Option<bool>,
    /// 0xRRGGBB of the mode (progress bar, chosen rating).
    pub accent: u32,
    /// 0…1
    pub volume: f32,
}

struct StatusItem {
    item: Id,
    art: Id,
    art_path: Option<String>,
    title: Id,
    artist: Id,
    fill: Id,
    elapsed: Id,
    total: Id,
    toggle: Id,
    up: Id,
    down: Id,
    /// Volume row: speaker icon (always) + slider (faded in while the mouse is over the row).
    vol_icon: Id,
    slider: Id,
}

thread_local! {
    static STATUS: std::cell::RefCell<Option<StatusItem>> = const { std::cell::RefCell::new(None) };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}
unsafe impl objc::Encode for NSRect {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect { x, y, w, h }
}

// Layout of the mini player (flipped view: y grows downwards).
const MP_W: f64 = 320.;
const MP_PAD: f64 = 14.;
const MP_ART_W: f64 = 112.;
const MP_ART_H: f64 = 63.;
const MP_TRACK_Y: f64 = 92.;
const MP_TRACK_W: f64 = MP_W - 2. * MP_PAD;
const MP_BUTTONS_Y: f64 = 122.;
const MP_VOLUME_Y: f64 = 160.;
const MP_H: f64 = 188.;
/// `tag` of the volume slider inside its row.
const SLIDER_TAG: isize = 99;

extern "C" fn on_menu_item(_: &Object, _: Sel, sender: Id) {
    let tag: isize = unsafe { msg_send![sender, tag] };
    let Some(cmd) = usize::try_from(tag).ok().and_then(|i| MENU_COMMANDS.get(i)).copied() else { return };
    if let Some(tx) = MENU_TX.lock().ok().and_then(|t| t.clone()) {
        let _ = tx.unbounded_send(cmd);
    }
}

extern "C" fn on_volume(_: &Object, _: Sel, sender: Id) {
    let v: f64 = unsafe { msg_send![sender, doubleValue] };
    if let Some(tx) = MENU_TX.lock().ok().and_then(|t| t.clone()) {
        let _ = tx.unbounded_send(MenuCommand::Volume(v as f32));
    }
}

/// Menu closed: fade the slider out again (no `mouseExited:` arrives once it's gone).
extern "C" fn on_menu_closed(_: &Object, _: Sel, _menu: Id) {
    STATUS.with(|s| {
        if let Ok(s) = s.try_borrow() {
            if let Some(st) = s.as_ref() {
                unsafe { let _: () = msg_send![st.slider, setAlphaValue: 0f64]; }
            }
        }
    });
}

fn menu_target() -> Id {
    use objc::declare::ClassDecl;
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cls = *CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("JTMusicMenuTarget", class!(NSObject)).expect("JTMusicMenuTarget");
        unsafe { decl.add_method(sel!(musicMenuItem:), on_menu_item as extern "C" fn(&Object, Sel, Id)) };
        unsafe { decl.add_method(sel!(musicVolume:), on_volume as extern "C" fn(&Object, Sel, Id)) };
        unsafe { decl.add_method(sel!(menuDidClose:), on_menu_closed as extern "C" fn(&Object, Sel, Id)) };
        decl.register() as *const objc::runtime::Class as usize
    });
    unsafe {
        let obj: Id = msg_send![cls as *const objc::runtime::Class, new];
        obj
    }
}

extern "C" fn is_flipped(_: &Object, _: Sel) -> objc::runtime::BOOL {
    YES
}

/// An `NSView` with a top-left origin.
fn flipped_view(frame: NSRect) -> Id {
    use objc::declare::ClassDecl;
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cls = *CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("JTMusicFlippedView", class!(NSView)).expect("JTMusicFlippedView");
        unsafe { decl.add_method(sel!(isFlipped), is_flipped as extern "C" fn(&Object, Sel) -> objc::runtime::BOOL) };
        decl.register() as *const objc::runtime::Class as usize
    });
    unsafe {
        let v: Id = msg_send![cls as *const objc::runtime::Class, alloc];
        msg_send![v, initWithFrame: frame]
    }
}

fn ns_color(rgb: u32, alpha: f64) -> Id {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.;
    unsafe { msg_send![class!(NSColor), colorWithSRGBRed: c(16) green: c(8) blue: c(0) alpha: alpha] }
}

unsafe fn set_layer_bg(view: Id, color: Id) {
    let layer: Id = msg_send![view, layer];
    if !layer.is_null() {
        let cg: *const std::ffi::c_void = msg_send![color, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cg];
    }
}

fn reveal_slider(row: &Object, alpha: f64) {
    unsafe {
        let slider: Id = msg_send![row, viewWithTag: SLIDER_TAG];
        if !slider.is_null() {
            let anim: Id = msg_send![slider, animator];
            let _: () = msg_send![anim, setAlphaValue: alpha];
        }
    }
}
extern "C" fn row_entered(this: &Object, _: Sel, _: Id) {
    reveal_slider(this, 1.);
}
extern "C" fn row_exited(this: &Object, _: Sel, _: Id) {
    reveal_slider(this, 0.);
}

/// Row that shows its slider (`SLIDER_TAG`) only while the mouse is over it.
unsafe fn hover_row(parent: Id, frame: NSRect) -> Id {
    use objc::declare::ClassDecl;
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cls = *CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("JTMusicHoverRow", class!(NSView)).expect("JTMusicHoverRow");
        unsafe {
            decl.add_method(sel!(mouseEntered:), row_entered as extern "C" fn(&Object, Sel, Id));
            decl.add_method(sel!(mouseExited:), row_exited as extern "C" fn(&Object, Sel, Id));
            decl.add_method(sel!(isFlipped), is_flipped as extern "C" fn(&Object, Sel) -> objc::runtime::BOOL);
        }
        decl.register() as *const objc::runtime::Class as usize
    });
    let v: Id = msg_send![cls as *const objc::runtime::Class, alloc];
    let v: Id = msg_send![v, initWithFrame: frame];
    // NSTrackingMouseEnteredAndExited | NSTrackingActiveAlways | NSTrackingInVisibleRect
    let opts: usize = 0x01 | 0x80 | 0x200;
    let area: Id = msg_send![class!(NSTrackingArea), alloc];
    let area: Id = msg_send![area, initWithRect: rect(0., 0., frame.w, frame.h) options: opts owner: v userInfo: std::ptr::null_mut::<Object>()];
    let _: () = msg_send![v, addTrackingArea: area];
    let _: () = msg_send![parent, addSubview: v];
    v
}

pub fn volume_symbol(v: f32) -> &'static str {
    if v <= 0.001 {
        "speaker.slash.fill"
    } else if v < 0.34 {
        "speaker.wave.1.fill"
    } else if v < 0.67 {
        "speaker.wave.2.fill"
    } else {
        "speaker.wave.3.fill"
    }
}

/// Plain view with a rounded, colored layer.
unsafe fn layer_view(parent: Id, frame: NSRect, color: Id, radius: f64) -> Id {
    let v: Id = msg_send![class!(NSView), alloc];
    let v: Id = msg_send![v, initWithFrame: frame];
    let _: () = msg_send![v, setWantsLayer: YES];
    let layer: Id = msg_send![v, layer];
    let _: () = msg_send![layer, setCornerRadius: radius];
    let _: () = msg_send![layer, setMasksToBounds: YES];
    set_layer_bg(v, color);
    let _: () = msg_send![parent, addSubview: v];
    v
}

/// Label truncated with "…" after `lines` lines.
unsafe fn label(parent: Id, frame: NSRect, size: f64, weight: f64, color: Id, lines: isize, mono: bool) -> Id {
    let l: Id = msg_send![class!(NSTextField), wrappingLabelWithString: ns_string("")];
    let _: () = msg_send![l, setFrame: frame];
    let font: Id = if mono { msg_send![class!(NSFont), monospacedDigitSystemFontOfSize: size weight: weight] } else { msg_send![class!(NSFont), systemFontOfSize: size weight: weight] };
    let _: () = msg_send![l, setFont: font];
    let _: () = msg_send![l, setTextColor: color];
    let _: () = msg_send![l, setSelectable: objc::runtime::NO];
    let _: () = msg_send![l, setMaximumNumberOfLines: lines];
    let cell: Id = msg_send![l, cell];
    let _: () = msg_send![cell, setWraps: if lines > 1 { YES } else { objc::runtime::NO }];
    // NSLineBreakByWordWrapping = 0, ByTruncatingTail = 4
    let _: () = msg_send![cell, setLineBreakMode: if lines > 1 { 0isize } else { 4isize }];
    let _: () = msg_send![cell, setTruncatesLastVisibleLine: YES];
    let _: () = msg_send![parent, addSubview: l];
    l
}

fn symbol(name: &str, points: f64, weight: f64) -> Id {
    unsafe {
        let img: Id = msg_send![class!(NSImage), imageWithSystemSymbolName: ns_string(name) accessibilityDescription: std::ptr::null_mut::<Object>()];
        if img.is_null() {
            return img;
        }
        let conf: Id = msg_send![class!(NSImageSymbolConfiguration), configurationWithPointSize: points weight: weight];
        msg_send![img, imageWithSymbolConfiguration: conf]
    }
}

/// Borderless SF Symbol button that sends `tag`'s command.
unsafe fn symbol_button(parent: Id, target: Id, tag: isize, name: &str, points: f64, frame: NSRect, tip: &str) -> Id {
    let b: Id = msg_send![class!(NSButton), alloc];
    let b: Id = msg_send![b, initWithFrame: frame];
    let _: () = msg_send![b, setBordered: objc::runtime::NO];
    // NSImageOnly = 1
    let _: () = msg_send![b, setImagePosition: 1isize];
    let _: () = msg_send![b, setImage: symbol(name, points, 0.)];
    let _: () = msg_send![b, setTarget: target];
    let _: () = msg_send![b, setAction: sel!(musicMenuItem:)];
    let _: () = msg_send![b, setTag: tag];
    let _: () = msg_send![b, setToolTip: ns_string(tip)];
    let _: () = msg_send![parent, addSubview: b];
    b
}

/// Menu bar item for the player: its menu is a mini player (artwork, title, progress and
/// controls, like the browsers' media popover) plus "Buscar…" and "Cerrar reproductor".
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

        let view = flipped_view(rect(0., 0., MP_W, MP_H));
        let art = layer_view(view, rect(MP_PAD, MP_PAD, MP_ART_W, MP_ART_H), msg_send![class!(NSColor), colorWithWhite: 0.5f64 alpha: 0.2f64], 7.);
        let art_layer: Id = msg_send![art, layer];
        let _: () = msg_send![art_layer, setContentsGravity: ns_string("resizeAspectFill")];
        let text_x = MP_PAD + MP_ART_W + 12.;
        let text_w = MP_W - text_x - MP_PAD;
        let primary: Id = msg_send![class!(NSColor), labelColor];
        let secondary: Id = msg_send![class!(NSColor), secondaryLabelColor];
        // NSFontWeightSemibold ≈ 0.3
        let title = label(view, rect(text_x, MP_PAD - 1., text_w, 36.), 13., 0.3, primary, 2, false);
        let artist = label(view, rect(text_x, MP_PAD + 38., text_w, 16.), 11.5, 0., secondary, 1, false);

        let _track = layer_view(view, rect(MP_PAD, MP_TRACK_Y, MP_TRACK_W, 4.), msg_send![class!(NSColor), colorWithWhite: 0.5f64 alpha: 0.3f64], 2.);
        let fill = layer_view(view, rect(MP_PAD, MP_TRACK_Y, 0., 4.), ns_color(0xf7768e, 1.), 2.);
        let elapsed = label(view, rect(MP_PAD - 2., MP_TRACK_Y + 7., 80., 14.), 10.5, 0., secondary, 1, true);
        let total = label(view, rect(MP_W - MP_PAD - 78., MP_TRACK_Y + 7., 80., 14.), 10.5, 0., secondary, 1, true);
        // NSTextAlignmentRight = 2
        let _: () = msg_send![total, setAlignment: 2isize];

        let cx = MP_W / 2.;
        let y = MP_BUTTONS_Y;
        let toggle = symbol_button(view, target, 0, "pause.fill", 22., rect(cx - 20., y - 4., 40., 40.), "Reproducir / pausa (⌃⌥Space)");
        symbol_button(view, target, 3, "backward.fill", 15., rect(cx - 64., y, 32., 32.), "Anterior (⌃⌥⇧←)");
        symbol_button(view, target, 4, "forward.fill", 15., rect(cx + 32., y, 32., 32.), "Siguiente (⌃⌥⇧→)");
        symbol_button(view, target, 1, "gobackward.15", 15., rect(cx - 106., y, 32., 32.), "−15 s (⌃⌥←)");
        symbol_button(view, target, 2, "goforward.15", 15., rect(cx + 74., y, 32., 32.), "+15 s (⌃⌥→)");
        let up = symbol_button(view, target, 7, "hand.thumbsup", 13., rect(MP_PAD - 6., y, 30., 32.), "Me gusta");
        let down = symbol_button(view, target, 8, "hand.thumbsdown", 13., rect(MP_W - MP_PAD - 24., y, 30., 32.), "No me gusta");

        // Volume: a dim speaker; the slider fades in while hovering the row.
        let row = hover_row(view, rect(MP_PAD - 4., MP_VOLUME_Y, MP_TRACK_W + 8., 22.));
        let vol_icon: Id = msg_send![class!(NSImageView), alloc];
        let vol_icon: Id = msg_send![vol_icon, initWithFrame: rect(2., 2., 22., 18.)];
        let _: () = msg_send![vol_icon, setImage: symbol("speaker.wave.2.fill", 11., 0.)];
        let _: () = msg_send![vol_icon, setContentTintColor: secondary];
        let _: () = msg_send![vol_icon, setToolTip: ns_string("Volumen")];
        let _: () = msg_send![row, addSubview: vol_icon];
        let slider: Id = msg_send![class!(NSSlider), sliderWithValue: 1f64 minValue: 0f64 maxValue: 1f64 target: target action: sel!(musicVolume:)];
        let _: () = msg_send![slider, setFrame: rect(28., 1., MP_TRACK_W + 8. - 34., 20.)];
        // NSControlSizeSmall = 1
        let _: () = msg_send![slider, setControlSize: 1isize];
        let _: () = msg_send![slider, setContinuous: YES];
        let _: () = msg_send![slider, setTag: SLIDER_TAG];
        let _: () = msg_send![slider, setAlphaValue: 0f64];
        let _: () = msg_send![row, addSubview: slider];
        let _: () = msg_send![menu, setDelegate: target];

        let player: Id = msg_send![class!(NSMenuItem), new];
        let _: () = msg_send![player, setView: view];
        let _: () = msg_send![menu, addItem: player];
        let sep: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep];
        let add = |title: &str, tag: isize, key: &str| {
            let mi: Id = msg_send![class!(NSMenuItem), alloc];
            let mi: Id = msg_send![mi, initWithTitle: ns_string(title) action: sel!(musicMenuItem:) keyEquivalent: ns_string(key)];
            if !key.is_empty() {
                // ⌘⇧
                let _: () = msg_send![mi, setKeyEquivalentModifierMask: (1usize << 20) | (1usize << 17)];
            }
            let _: () = msg_send![mi, setTag: tag];
            let _: () = msg_send![mi, setTarget: target];
            let _: () = msg_send![menu, addItem: mi];
        };
        add("Buscar música, podcasts y videos…", 5, "m");
        add("Cerrar reproductor", 6, "");
        let _: () = msg_send![item, setMenu: menu];
        StatusItem { item, art, art_path: None, title, artist, fill, elapsed, total, toggle, up, down, vol_icon, slider }
    }
}

fn fmt_clock(secs: f64) -> String {
    let s = secs.max(0.) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// Show / update the menu bar item and its mini player (`None` hides it).
pub fn set_status_item(info: Option<StatusInfo>) {
    STATUS.with(|s| {
        let mut s = s.borrow_mut();
        let Some(info) = info else {
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
                let image: Id = msg_send![class!(NSImage), imageWithSystemSymbolName: ns_string(if info.playing { "music.note" } else { "pause.fill" }) accessibilityDescription: ns_string("Música")];
                if !image.is_null() {
                    let _: () = msg_send![image, setTemplate: YES];
                    let _: () = msg_send![button, setImage: image];
                    // NSImageLeft = 2
                    let _: () = msg_send![button, setImagePosition: 2isize];
                }
                let t = info.title;
                let short: String = if t.chars().count() > 28 { format!(" {}…", t.chars().take(27).collect::<String>().trim_end()) } else { format!(" {t}") };
                let _: () = msg_send![button, setTitle: ns_string(&short)];
            }
            let _: () = msg_send![st.title, setStringValue: ns_string(info.title)];
            let _: () = msg_send![st.artist, setStringValue: ns_string(info.artist)];

            let path = info.artwork.map(|p| p.to_string_lossy().into_owned());
            if path != st.art_path {
                let layer: Id = msg_send![st.art, layer];
                let image: Id = match &path {
                    Some(p) => {
                        let img: Id = msg_send![class!(NSImage), alloc];
                        msg_send![img, initWithContentsOfFile: ns_string(p)]
                    }
                    None => std::ptr::null_mut(),
                };
                let _: () = msg_send![layer, setContents: image];
                if !image.is_null() {
                    let _: () = msg_send![image, release];
                }
                st.art_path = path;
            }

            let frac = if info.duration > 0. { (info.elapsed / info.duration).clamp(0., 1.) } else { 0. };
            let _: () = msg_send![st.fill, setFrame: rect(MP_PAD, MP_TRACK_Y, MP_TRACK_W * frac, 4.)];
            set_layer_bg(st.fill, ns_color(info.accent, 1.));
            let _: () = msg_send![st.elapsed, setStringValue: ns_string(&fmt_clock(info.elapsed))];
            let total = if info.duration > 0. { fmt_clock(info.duration) } else { "--:--".into() };
            let _: () = msg_send![st.total, setStringValue: ns_string(&total)];

            let _: () = msg_send![st.toggle, setImage: symbol(if info.playing { "pause.fill" } else { "play.fill" }, 22., 0.)];
            for (b, good, name) in [(st.up, true, "hand.thumbsup"), (st.down, false, "hand.thumbsdown")] {
                let on = info.rating == Some(good);
                let _: () = msg_send![b, setImage: symbol(&if on { format!("{name}.fill") } else { name.to_string() }, 13., 0.)];
                let tint: Id = if on { ns_color(info.accent, 1.) } else { std::ptr::null_mut() };
                let _: () = msg_send![b, setContentTintColor: tint];
            }
            let _: () = msg_send![st.vol_icon, setImage: symbol(volume_symbol(info.volume), 11., 0.)];
            let current: f64 = msg_send![st.slider, doubleValue];
            if (current - info.volume as f64).abs() > 0.001 {
                let _: () = msg_send![st.slider, setDoubleValue: info.volume as f64];
            }
            let _: () = msg_send![st.slider, setTrackFillColor: ns_color(info.accent, 1.)];
        }
    });
}
