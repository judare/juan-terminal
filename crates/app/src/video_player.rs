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
use objc::runtime::{Object, BOOL, NO, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};

#[link(name = "AVKit", kind = "framework")]
unsafe extern "C" {}
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

type Id = *mut Object;

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

pub struct NativePlayer {
    view: Id,
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
        let p = Rc::new(Self { view, host: Cell::new(std::ptr::null_mut()), painted_gen: Cell::new(0), last: Cell::new(None), hidden: Cell::new(true) });
        PLAYERS.with(|l| {
            let mut l = l.borrow_mut();
            l.retain(|w| w.strong_count() > 0);
            l.push(Rc::downgrade(&p));
        });
        p
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
