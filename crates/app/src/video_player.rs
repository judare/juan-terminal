//! In-app video/audio playback: a native AVKit `AVPlayerView` (play/pause, scrubbing, volume,
//! fullscreen, PiP) laid over the GPUI window at the element's bounds.
//!
//! GPUI can't host native views, so the player is a sibling NSView above GPUI's Metal view:
//! - `player_slot` paints an empty area and moves/shows the native view there every frame.
//! - `frame_end` (painted last by the workspace root) hides players not painted in that frame
//!   (tab switched, panel hidden…).
//! The player sits above all GPUI content in its rect, so it's hidden while overlays are open.
#![allow(unexpected_cfgs)] // objc 0.2 macros

use gpui::{canvas, App, Bounds, IntoElement, Pixels, Styled, Window};
use objc::runtime::{Class, Object, Sel, BOOL, NO, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};

#[link(name = "AVKit", kind = "framework")]
unsafe extern "C" {}
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

pub type Id = *mut Object;

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

#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}
unsafe impl objc::Encode for NSPoint {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str("{CGPoint=dd}") }
    }
}

/// Controls drawn natively over the picture (GPUI can't paint above the player). The view lets
/// clicks through, so GPUI handles them with invisible targets at the same spots (see
/// `overlay_layout`).
pub const OVERLAY_PLAY: f32 = 40.;
pub const OVERLAY_SKIP: f32 = 30.;
pub const OVERLAY_SKIP_GAP: f32 = 14.;
pub const OVERLAY_BUTTON: f32 = 24.;
pub const OVERLAY_MARGIN: f32 = 6.;
pub const OVERLAY_GAP: f32 = 4.;
/// Side inset of the timeline and the text.
pub const OVERLAY_PAD: f32 = 10.;
/// Distance from the bottom edge to the timeline's center while hovered.
pub const OVERLAY_BAR_BOTTOM: f32 = 10.;
/// Volume slider at the right of the time row (hover only): its width, and the speaker
/// icon's box at its left (icon click = mute).
pub const OVERLAY_VOL_W: f32 = 52.;
pub const OVERLAY_VOL_ICON: f32 = 16.;
/// Distance from the bottom edge to the center of the time / volume row.
pub const OVERLAY_ROW_BOTTOM: f32 = OVERLAY_BAR_BOTTOM + 11.5;
const TOP_SHADE: f64 = 54.;
const BOTTOM_SHADE: f64 = 46.;

/// What the overlay shows. `hover` false: only a thin timeline along the bottom edge.
#[derive(Clone, PartialEq)]
pub struct OverlayInfo {
    pub hover: bool,
    pub playing: bool,
    pub title: String,
    pub channel: String,
    /// 0…1
    pub progress: f32,
    /// "4:56 / 7:22" (or a status like "Cargando…").
    pub time: String,
    /// Timeline color (sRGB 0…1).
    pub accent: (f32, f32, f32),
    /// 0…1
    pub volume: f32,
}

struct Overlay {
    top_shade: Id,
    bottom_shade: Id,
    title: Id,
    channel: Id,
    time: Id,
    track: Id,
    fill: Id,
    play: Id,
    play_icon: Id,
    prev: Id,
    next: Id,
    expand: Id,
    audio: Id,
    close: Id,
    /// Speaker: its (bare) circle and the image inside.
    vol_box: Id,
    vol_icon: Id,
    vol_track: Id,
    vol_fill: Id,
}

impl Overlay {
    fn hover_views(&self) -> [Id; 14] {
        [self.top_shade, self.bottom_shade, self.title, self.channel, self.time, self.play, self.prev, self.next, self.expand, self.audio, self.close, self.vol_box, self.vol_track, self.vol_fill]
    }
}

pub struct NativePlayer {
    view: Id,
    overlay: RefCell<Option<Overlay>>,
    overlay_info: RefCell<Option<OverlayInfo>>,
    /// Last laid-out (w, h, fill width, volume width): skips relayout when nothing moved.
    overlay_size: Cell<Option<(f64, f64, f64, f64)>>,
    /// GPUI's NSView of the window the player was attached to (null until first paint).
    host: Cell<Id>,
    painted_gen: Cell<u64>,
    last: Cell<Option<(f64, f64, f64, f64)>>,
    hidden: Cell<bool>,
}

thread_local! {
    static PLAYERS: RefCell<Vec<Weak<NativePlayer>>> = const { RefCell::new(Vec::new()) };
    static GEN: Cell<u64> = const { Cell::new(1) };
}

fn ns_string(s: &str) -> Id {
    let bytes = std::ffi::CString::new(s).unwrap_or_default();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: bytes.as_ptr()] }
}

impl NativePlayer {
    /// `audio`: compact inline controls instead of floating ones over the picture.
    pub fn new(path: &Path, audio: bool) -> Rc<Self> {
        let view: Id = unsafe {
            let url: Id = msg_send![class!(NSURL), fileURLWithPath: ns_string(&path.to_string_lossy())];
            let player: Id = msg_send![class!(AVPlayer), playerWithURL: url];
            let view: Id = msg_send![class!(AVPlayerView), alloc];
            let view: Id = msg_send![view, initWithFrame: NSRect { x: 0., y: 0., w: 1., h: 1. }];
            let _: () = msg_send![view, setPlayer: player];
            // AVPlayerViewControlsStyleInline = 1, Floating = 2
            let _: () = msg_send![view, setControlsStyle: if audio { 1isize } else { 2isize }];
            let _: () = msg_send![view, setShowsFullScreenToggleButton: YES];
            let _: () = msg_send![view, setAllowsPictureInPicturePlayback: YES];
            let _: () = msg_send![view, setHidden: YES];
            view
        };
        let p = Rc::new(Self { view, overlay: RefCell::new(None), overlay_info: RefCell::new(None), overlay_size: Cell::new(None), host: Cell::new(std::ptr::null_mut()), painted_gen: Cell::new(0), last: Cell::new(None), hidden: Cell::new(true) });
        PLAYERS.with(|l| {
            let mut l = l.borrow_mut();
            l.retain(|w| w.strong_count() > 0);
            l.push(Rc::downgrade(&p));
        });
        p
    }

    /// View for an existing `AVPlayer` owned elsewhere (the music player's video), with no
    /// native controls: the caller draws its own. Hiding it pauses that player.
    pub fn for_player(player: Id) -> Rc<Self> {
        let view: Id = unsafe {
            let view: Id = msg_send![passthrough_class(), alloc];
            let view: Id = msg_send![view, initWithFrame: NSRect { x: 0., y: 0., w: 1., h: 1. }];
            let _: () = msg_send![view, setPlayer: player];
            // AVPlayerViewControlsStyleNone = 0
            let _: () = msg_send![view, setControlsStyle: 0isize];
            let _: () = msg_send![view, setHidden: YES];
            view
        };
        let p = Rc::new(Self { view, overlay: RefCell::new(None), overlay_info: RefCell::new(None), overlay_size: Cell::new(None), host: Cell::new(std::ptr::null_mut()), painted_gen: Cell::new(0), last: Cell::new(None), hidden: Cell::new(true) });
        PLAYERS.with(|l| {
            let mut l = l.borrow_mut();
            l.retain(|w| w.strong_count() > 0);
            l.push(Rc::downgrade(&p));
        });
        p
    }

    /// Rounded corners for the picture (the native view ignores GPUI's clipping).
    pub fn set_corner_radius(&self, r: f64) {
        unsafe {
            let _: () = msg_send![self.view, setWantsLayer: YES];
            let layer: Id = msg_send![self.view, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: r];
                let _: () = msg_send![layer, setMasksToBounds: if r > 0. { YES } else { NO }];
            }
        }
    }

    /// Show the overlay (`Some`) or hide it. Cheap when unchanged.
    pub fn set_overlay(&self, info: Option<OverlayInfo>) {
        if *self.overlay_info.borrow() == info && (info.is_none() || self.overlay.borrow().is_some()) {
            return;
        }
        let hover_changed = self.overlay_info.borrow().as_ref().map(|i| i.hover) != info.as_ref().map(|i| i.hover);
        *self.overlay_info.borrow_mut() = info.clone();
        if info.is_some() && self.overlay.borrow().is_none() {
            unsafe {
                let top_shade = shade(self.view, true);
                let bottom_shade = shade(self.view, false);
                let title = label(self.view, 12., 0.3, 1.);
                let channel = label(self.view, 10.5, 0., 0.75);
                let time = label(self.view, 10.5, 0.2, 0.9);
                let track = bar(self.view, (1., 1., 1.), 0.3);
                let fill = bar(self.view, (1., 1., 1.), 1.);
                let (prev, _) = circle(self.view, OVERLAY_SKIP as f64, "backward.fill", 11., 0.);
                let (play, play_icon) = circle(self.view, OVERLAY_PLAY as f64, "play.fill", 15., 0.45);
                let (next, _) = circle(self.view, OVERLAY_SKIP as f64, "forward.fill", 11., 0.);
                let (expand, _) = circle(self.view, OVERLAY_BUTTON as f64, "arrow.up.left.and.arrow.down.right", 10., 0.45);
                let (audio, _) = circle(self.view, OVERLAY_BUTTON as f64, "video.slash", 10., 0.45);
                let (close, _) = circle(self.view, OVERLAY_BUTTON as f64, "xmark", 10., 0.45);
                let (vol_box, vol_icon) = circle(self.view, OVERLAY_VOL_ICON as f64, "speaker.wave.2.fill", 9., 0.);
                let vol_track = bar(self.view, (1., 1., 1.), 0.3);
                let vol_fill = bar(self.view, (1., 1., 1.), 0.9);
                *self.overlay.borrow_mut() = Some(Overlay { top_shade, bottom_shade, title, channel, time, track, fill, play, play_icon, prev, next, expand, audio, close, vol_box, vol_icon, vol_track, vol_fill });
            }
            self.overlay_size.set(None);
        }
        {
            let Some(o) = &*self.overlay.borrow() else { return };
            unsafe {
                let hide: BOOL = if info.as_ref().is_some_and(|i| i.hover) { NO } else { YES };
                for v in o.hover_views() {
                    let _: () = msg_send![v, setHidden: hide];
                }
                let bars: BOOL = if info.is_some() { NO } else { YES };
                let _: () = msg_send![o.track, setHidden: bars];
                let _: () = msg_send![o.fill, setHidden: bars];
                if let Some(i) = &info {
                    let _: () = msg_send![o.play_icon, setImage: symbol(if i.playing { "pause.fill" } else { "play.fill" }, 15.)];
                    let _: () = msg_send![o.title, setStringValue: ns_string(&i.title)];
                    let _: () = msg_send![o.channel, setStringValue: ns_string(&i.channel)];
                    let _: () = msg_send![o.time, setStringValue: ns_string(&i.time)];
                    set_layer_color(o.fill, i.accent, 1.);
                    let _: () = msg_send![o.vol_icon, setImage: symbol(crate::music_engine::volume_symbol(i.volume), 9.)];
                }
            }
        }
        if hover_changed {
            self.overlay_size.set(None);
        }
        if let Some((_, _, w, h)) = self.last.get() {
            self.layout_overlay(w, h);
        }
    }

    fn layout_overlay(&self, w: f64, h: f64) {
        let info = self.overlay_info.borrow();
        let Some(info) = &*info else { return };
        let Some(o) = &*self.overlay.borrow() else { return };
        let progress = info.progress.clamp(0., 1.) as f64;
        let volume = info.volume.clamp(0., 1.) as f64;
        let vol_w = OVERLAY_VOL_W as f64;
        let key = (w, h, (progress * w).round(), (volume * vol_w).round());
        if self.overlay_size.replace(Some(key)) == Some(key) {
            return;
        }
        unsafe {
            let flipped: BOOL = msg_send![self.view, isFlipped];
            // Rect from top-left coordinates.
            let r = |x: f64, y: f64, rw: f64, rh: f64| NSRect { x, y: if flipped != NO { y } else { h - y - rh }, w: rw.max(0.), h: rh.max(0.) };
            let (p, s, sg, b, m, g, pad) = (OVERLAY_PLAY as f64, OVERLAY_SKIP as f64, OVERLAY_SKIP_GAP as f64, OVERLAY_BUTTON as f64, OVERLAY_MARGIN as f64, OVERLAY_GAP as f64, OVERLAY_PAD as f64);
            let _: () = msg_send![o.top_shade, setFrame: r(0., 0., w, TOP_SHADE)];
            let _: () = msg_send![o.bottom_shade, setFrame: r(0., h - BOTTOM_SHADE, w, BOTTOM_SHADE)];
            // Top-right: expand, audio only, close.
            let _: () = msg_send![o.close, setFrame: r(w - m - b, m, b, b)];
            let _: () = msg_send![o.audio, setFrame: r(w - m - 2. * b - g, m, b, b)];
            let _: () = msg_send![o.expand, setFrame: r(w - m - 3. * b - 2. * g, m, b, b)];
            let text_w = w - pad - (3. * b + 2. * g + m + 6.);
            let _: () = msg_send![o.title, setFrame: r(pad, 7., text_w, 16.)];
            let _: () = msg_send![o.channel, setFrame: r(pad, 23., text_w, 14.)];
            // Center: prev, play, next.
            let cy = h / 2.;
            let _: () = msg_send![o.play, setFrame: r((w - p) / 2., cy - p / 2., p, p)];
            let _: () = msg_send![o.prev, setFrame: r((w - p) / 2. - sg - s, cy - s / 2., s, s)];
            let _: () = msg_send![o.next, setFrame: r((w + p) / 2. + sg, cy - s / 2., s, s)];
            // Timeline: inset with the time while hovered, a hairline on the bottom edge otherwise.
            let (bx, bw, by, bh) = if info.hover { (pad, w - 2. * pad, h - OVERLAY_BAR_BOTTOM as f64 - 1.5, 3.) } else { (0., w, h - 2., 2.) };
            let _: () = msg_send![o.track, setFrame: r(bx, by, bw, bh)];
            let _: () = msg_send![o.fill, setFrame: r(bx, by, bw * progress, bh)];
            for v in [o.track, o.fill] {
                let layer: Id = msg_send![v, layer];
                let _: () = msg_send![layer, setCornerRadius: if info.hover { 1.5f64 } else { 0f64 }];
            }
            // Time row: time at the left, speaker + volume slider at the right.
            let row_cy = h - OVERLAY_ROW_BOTTOM as f64;
            let vi = OVERLAY_VOL_ICON as f64;
            let vx = w - pad - vol_w;
            let _: () = msg_send![o.vol_box, setFrame: r(vx - 4. - vi, row_cy - vi / 2., vi, vi)];
            let _: () = msg_send![o.vol_track, setFrame: r(vx, row_cy - 1.5, vol_w, 3.)];
            let _: () = msg_send![o.vol_fill, setFrame: r(vx, row_cy - 1.5, vol_w * volume, 3.)];
            for v in [o.vol_track, o.vol_fill] {
                let layer: Id = msg_send![v, layer];
                let _: () = msg_send![layer, setCornerRadius: 1.5f64];
            }
            let _: () = msg_send![o.time, setFrame: r(pad, by - 17., (w - 2. * pad - vol_w - vi - 8.).max(0.), 14.)];
        }
    }

    /// Whether `window` shows this player (or nobody has it yet). The native view is shared by
    /// every window, so only that window may drive its overlay: others painting their own hover
    /// state would toggle the controls each frame (flicker).
    pub fn shown_in(&self, window: &Window) -> bool {
        let host = self.host.get();
        host.is_null() || gpui_view(window) == Some(host)
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden.get()
    }

    fn set_hidden(&self, hide: bool) {
        if self.hidden.replace(hide) != hide {
            let v: BOOL = if hide { YES } else { NO };
            unsafe {
                let _: () = msg_send![self.view, setHidden: v];
                if hide {
                    restore_responder(self.host.get(), self.view);
                    // Don't keep playing (sound) in a tab nobody sees.
                    let player: Id = msg_send![self.view, player];
                    if !player.is_null() {
                        let _: () = msg_send![player, pause];
                    }
                }
            }
        }
    }

    fn place(&self, window: &Window, bounds: Bounds<Pixels>) {
        let Some(host) = gpui_view(window) else { return };
        unsafe {
            let content: Id = msg_send![host, superview];
            if content.is_null() {
                return;
            }
            if self.host.get() != host {
                // One native view shared by every window: only the focused window may take it
                // from another, or two windows painting the slot bounce it each frame (flicker).
                if !self.host.get().is_null() && !window.is_window_active() {
                    return;
                }
                let sup: Id = msg_send![self.view, superview];
                if !sup.is_null() {
                    let _: () = msg_send![self.view, removeFromSuperview];
                }
                // NSWindowAbove = 1
                let _: () = msg_send![content, addSubview: self.view positioned: 1isize relativeTo: host];
                self.host.set(host);
                self.last.set(None);
            }
            let cb: NSRect = msg_send![content, bounds];
            let (x, y, w, h) = (f64::from(f32::from(bounds.origin.x)), f64::from(f32::from(bounds.origin.y)), f64::from(f32::from(bounds.size.width)), f64::from(f32::from(bounds.size.height)));
            let flipped: BOOL = msg_send![content, isFlipped];
            let fy = if flipped != NO { y } else { cb.h - y - h };
            let r = (x, fy, w.max(1.), h.max(1.));
            if self.last.replace(Some(r)) != Some(r) {
                let _: () = msg_send![self.view, setFrame: NSRect { x: r.0, y: r.1, w: r.2, h: r.3 }];
            }
            self.layout_overlay(r.2, r.3);
        }
        self.painted_gen.set(GEN.with(|g| g.get()));
        self.set_hidden(false);
    }
}

impl Drop for NativePlayer {
    fn drop(&mut self) {
        unsafe {
            let player: Id = msg_send![self.view, player];
            if !player.is_null() {
                let _: () = msg_send![player, pause];
            }
            let _: () = msg_send![self.view, setPlayer: std::ptr::null_mut::<Object>()];
            restore_responder(self.host.get(), self.view);
            let _: () = msg_send![self.view, removeFromSuperview];
            let _: () = msg_send![self.view, release];
        }
    }
}

/// `AVPlayerView` that never takes the mouse: hovers and clicks reach GPUI underneath.
fn passthrough_class() -> &'static Class {
    const NAME: &str = "JTPassthroughPlayerView";
    if let Some(c) = Class::get(NAME) {
        return c;
    }
    extern "C" fn hit_test(_: &Object, _: Sel, _: NSPoint) -> Id {
        std::ptr::null_mut()
    }
    let mut decl = objc::declare::ClassDecl::new(NAME, class!(AVPlayerView)).expect("player view class");
    unsafe { decl.add_method(sel!(hitTest:), hit_test as extern "C" fn(&Object, Sel, NSPoint) -> Id) };
    decl.register()
}

unsafe fn symbol(name: &str, points: f64) -> Id {
    unsafe {
        let image: Id = msg_send![class!(NSImage), imageWithSystemSymbolName: ns_string(name) accessibilityDescription: std::ptr::null_mut::<Object>()];
        if image.is_null() {
            return image;
        }
        // NSFontWeightSemibold ≈ 0.3
        let config: Id = msg_send![class!(NSImageSymbolConfiguration), configurationWithPointSize: points weight: 0.3f64];
        msg_send![image, imageWithSymbolConfiguration: config]
    }
}

/// A circle (dark translucent, or bare with `alpha` 0) with a white SF Symbol, added hidden.
unsafe fn circle(parent: Id, size: f64, name: &str, points: f64, alpha: f64) -> (Id, Id) {
    unsafe {
        let v: Id = msg_send![class!(NSView), alloc];
        let v: Id = msg_send![v, initWithFrame: NSRect { x: 0., y: 0., w: size, h: size }];
        let _: () = msg_send![v, setWantsLayer: YES];
        let layer: Id = msg_send![v, layer];
        let color: Id = msg_send![class!(NSColor), colorWithWhite: 0f64 alpha: alpha];
        let cg: *const std::ffi::c_void = msg_send![color, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cg];
        let _: () = msg_send![layer, setCornerRadius: size / 2.];
        if alpha == 0. {
            // Bare icons stay readable over bright frames.
            let _: () = msg_send![layer, setShadowOpacity: 0.6f32];
            let _: () = msg_send![layer, setShadowRadius: 3f64];
            let _: () = msg_send![layer, setShadowOffset: NSPoint { x: 0., y: 0. }];
        }
        let iv: Id = msg_send![class!(NSImageView), alloc];
        let iv: Id = msg_send![iv, initWithFrame: NSRect { x: 0., y: 0., w: size, h: size }];
        let _: () = msg_send![iv, setImage: symbol(name, points)];
        let white: Id = msg_send![class!(NSColor), whiteColor];
        let _: () = msg_send![iv, setContentTintColor: white];
        // NSImageScaleNone = 2: keep the symbol's point size, centered.
        let _: () = msg_send![iv, setImageScaling: 2isize];
        let _: () = msg_send![v, addSubview: iv];
        let _: () = msg_send![iv, release];
        let _: () = msg_send![v, setHidden: YES];
        let _: () = msg_send![parent, addSubview: v];
        let _: () = msg_send![v, release];
        (v, iv)
    }
}

/// One-line white label (truncates with …), added hidden.
unsafe fn label(parent: Id, size: f64, weight: f64, alpha: f64) -> Id {
    unsafe {
        let l: Id = msg_send![class!(NSTextField), labelWithString: ns_string("")];
        let font: Id = msg_send![class!(NSFont), monospacedDigitSystemFontOfSize: size weight: weight];
        let _: () = msg_send![l, setFont: font];
        let color: Id = msg_send![class!(NSColor), colorWithWhite: 1f64 alpha: alpha];
        let _: () = msg_send![l, setTextColor: color];
        // NSLineBreakByTruncatingTail = 4
        let _: () = msg_send![l, setLineBreakMode: 4isize];
        let _: () = msg_send![l, setMaximumNumberOfLines: 1isize];
        let _: () = msg_send![l, setWantsLayer: YES];
        let layer: Id = msg_send![l, layer];
        let _: () = msg_send![layer, setShadowOpacity: 0.5f32];
        let _: () = msg_send![layer, setShadowRadius: 2f64];
        let _: () = msg_send![layer, setShadowOffset: NSPoint { x: 0., y: 0. }];
        let _: () = msg_send![l, setHidden: YES];
        let _: () = msg_send![parent, addSubview: l];
        l
    }
}

/// Plain colored rectangle (timeline track / fill), added hidden.
unsafe fn bar(parent: Id, rgb: (f32, f32, f32), alpha: f64) -> Id {
    unsafe {
        let v: Id = msg_send![class!(NSView), alloc];
        let v: Id = msg_send![v, initWithFrame: NSRect { x: 0., y: 0., w: 1., h: 1. }];
        let _: () = msg_send![v, setWantsLayer: YES];
        set_layer_color(v, rgb, alpha);
        let _: () = msg_send![v, setHidden: YES];
        let _: () = msg_send![parent, addSubview: v];
        let _: () = msg_send![v, release];
        v
    }
}

unsafe fn set_layer_color(v: Id, rgb: (f32, f32, f32), alpha: f64) {
    unsafe {
        let layer: Id = msg_send![v, layer];
        let color: Id = msg_send![class!(NSColor), colorWithSRGBRed: rgb.0 as f64 green: rgb.1 as f64 blue: rgb.2 as f64 alpha: alpha];
        let cg: *const std::ffi::c_void = msg_send![color, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cg];
    }
}

/// Black → clear gradient from the top (`top`) or bottom edge, added hidden.
unsafe fn shade(parent: Id, top: bool) -> Id {
    unsafe {
        let v: Id = msg_send![class!(NSView), alloc];
        let v: Id = msg_send![v, initWithFrame: NSRect { x: 0., y: 0., w: 1., h: 1. }];
        let g: Id = msg_send![class!(CAGradientLayer), layer];
        let dark: Id = msg_send![class!(NSColor), colorWithWhite: 0f64 alpha: 0.7f64];
        let clear: Id = msg_send![class!(NSColor), colorWithWhite: 0f64 alpha: 0f64];
        let dark: Id = msg_send![dark, CGColor];
        let clear: Id = msg_send![clear, CGColor];
        // Layer space is y-up: y 0 is the bottom edge.
        let colors = if top { [clear, dark] } else { [dark, clear] };
        let arr: Id = msg_send![class!(NSArray), arrayWithObjects: colors.as_ptr() count: 2usize];
        let _: () = msg_send![g, setColors: arr];
        let _: () = msg_send![g, setStartPoint: NSPoint { x: 0.5, y: 0. }];
        let _: () = msg_send![g, setEndPoint: NSPoint { x: 0.5, y: 1. }];
        // Layer-hosting view: set the layer first.
        let _: () = msg_send![v, setLayer: g];
        let _: () = msg_send![v, setWantsLayer: YES];
        let _: () = msg_send![v, setHidden: YES];
        let _: () = msg_send![parent, addSubview: v];
        let _: () = msg_send![v, release];
        v
    }
}

/// GPUI's view refuses first responder on click (it only gets it at window creation), so once
/// the player took it (a click on its controls) and is hidden or removed, the window keeps the
/// focus: GPUI stops getting keys and every event beeps. Hand it back to GPUI when the player,
/// or nothing (the window itself), holds it.
fn restore_responder(host: Id, player: Id) {
    if host.is_null() {
        return;
    }
    unsafe {
        let window: Id = msg_send![host, window];
        if window.is_null() {
            return;
        }
        let fr: Id = msg_send![window, firstResponder];
        let in_player = !player.is_null() && !fr.is_null() && {
            let is_view: BOOL = msg_send![fr, isKindOfClass: class!(NSView)];
            is_view != NO && {
                let d: BOOL = msg_send![fr, isDescendantOf: player];
                d != NO
            }
        };
        if fr.is_null() || fr == window || in_player {
            let _: BOOL = msg_send![window, makeFirstResponder: host];
        }
    }
}

fn gpui_view(window: &Window) -> Option<Id> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr() as Id),
        _ => None,
    }
}

/// Empty element filling its parent; the native player is shown over it.
pub fn player_slot(player: Rc<NativePlayer>) -> impl IntoElement {
    canvas(|_, _, _| {}, move |bounds, _, window: &mut Window, _: &mut App| player.place(window, bounds)).size_full()
}

/// Paint last in each window: hides that window's players that weren't painted this frame
/// (or all of them while `suppress`, e.g. an overlay is open on top).
pub fn frame_end(suppress: bool) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, window: &mut Window, _: &mut App| {
            let Some(host) = gpui_view(window) else { return };
            // Catch-all: the window must never be left as its own first responder.
            restore_responder(host, std::ptr::null_mut());
            let g = GEN.with(|g| g.get());
            PLAYERS.with(|l| {
                for p in l.borrow().iter().filter_map(Weak::upgrade) {
                    if p.host.get() == host && (suppress || p.painted_gen.get() != g) {
                        p.set_hidden(true);
                    }
                }
            });
            GEN.with(|c| c.set(g + 1));
        },
    )
    .absolute()
    .size_0()
}
