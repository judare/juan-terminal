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

/// Hover controls drawn natively over the picture (GPUI can't paint above the player). The
/// view lets clicks through, so GPUI handles them with invisible targets at the same spots.
pub const OVERLAY_PLAY: f32 = 44.;
pub const OVERLAY_BUTTON: f32 = 28.;
pub const OVERLAY_MARGIN: f32 = 6.;
pub const OVERLAY_GAP: f32 = 6.;

struct Overlay {
    play: Id,
    play_icon: Id,
    expand: Id,
    audio: Id,
}

pub struct NativePlayer {
    view: Id,
    overlay: RefCell<Option<Overlay>>,
    /// None: hidden; Some(playing).
    overlay_state: Cell<Option<bool>>,
    overlay_size: Cell<Option<(f64, f64)>>,
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
        let p = Rc::new(Self { view, overlay: RefCell::new(None), overlay_state: Cell::new(None), overlay_size: Cell::new(None), host: Cell::new(std::ptr::null_mut()), painted_gen: Cell::new(0), last: Cell::new(None), hidden: Cell::new(true) });
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
        let p = Rc::new(Self { view, overlay: RefCell::new(None), overlay_state: Cell::new(None), overlay_size: Cell::new(None), host: Cell::new(std::ptr::null_mut()), painted_gen: Cell::new(0), last: Cell::new(None), hidden: Cell::new(true) });
        PLAYERS.with(|l| {
            let mut l = l.borrow_mut();
            l.retain(|w| w.strong_count() > 0);
            l.push(Rc::downgrade(&p));
        });
        p
    }

    /// Show the hover controls (`Some(playing)`) or hide them. Cheap when unchanged.
    pub fn set_overlay(&self, state: Option<bool>) {
        if self.overlay_state.replace(state) == state && (state.is_none() || self.overlay.borrow().is_some()) {
            return;
        }
        if state.is_some() && self.overlay.borrow().is_none() {
            unsafe {
                let (play, play_icon) = circle(self.view, OVERLAY_PLAY as f64, "play.fill", 17.);
                let (expand, _) = circle(self.view, OVERLAY_BUTTON as f64, "arrow.up.left.and.arrow.down.right", 11.);
                let (audio, _) = circle(self.view, OVERLAY_BUTTON as f64, "video.slash", 11.);
                *self.overlay.borrow_mut() = Some(Overlay { play, play_icon, expand, audio });
            }
            self.overlay_size.set(None);
            if let Some((_, _, w, h)) = self.last.get() {
                self.layout_overlay(w, h);
            }
        }
        let Some(o) = &*self.overlay.borrow() else { return };
        unsafe {
            let hide: BOOL = if state.is_some() { NO } else { YES };
            for v in [o.play, o.expand, o.audio] {
                let _: () = msg_send![v, setHidden: hide];
            }
            if let Some(playing) = state {
                let _: () = msg_send![o.play_icon, setImage: symbol(if playing { "pause.fill" } else { "play.fill" }, 17.)];
            }
        }
    }

    fn layout_overlay(&self, w: f64, h: f64) {
        if self.overlay_size.replace(Some((w, h))) == Some((w, h)) {
            return;
        }
        let Some(o) = &*self.overlay.borrow() else { return };
        unsafe {
            let flipped: BOOL = msg_send![self.view, isFlipped];
            let top = |y: f64, size: f64| if flipped != NO { y } else { h - y - size };
            let (p, b, m, g) = (OVERLAY_PLAY as f64, OVERLAY_BUTTON as f64, OVERLAY_MARGIN as f64, OVERLAY_GAP as f64);
            let _: () = msg_send![o.play, setFrame: NSRect { x: (w - p) / 2., y: (h - p) / 2., w: p, h: p }];
            let _: () = msg_send![o.audio, setFrame: NSRect { x: w - m - b, y: top(m, b), w: b, h: b }];
            let _: () = msg_send![o.expand, setFrame: NSRect { x: w - m - 2. * b - g, y: top(m, b), w: b, h: b }];
        }
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

/// A dark translucent circle with a white SF Symbol, added (hidden) to `parent`.
unsafe fn circle(parent: Id, size: f64, name: &str, points: f64) -> (Id, Id) {
    unsafe {
        let v: Id = msg_send![class!(NSView), alloc];
        let v: Id = msg_send![v, initWithFrame: NSRect { x: 0., y: 0., w: size, h: size }];
        let _: () = msg_send![v, setWantsLayer: YES];
        let layer: Id = msg_send![v, layer];
        let color: Id = msg_send![class!(NSColor), colorWithWhite: 0f64 alpha: 0.6f64];
        let cg: *const std::ffi::c_void = msg_send![color, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cg];
        let _: () = msg_send![layer, setCornerRadius: size / 2.];
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
