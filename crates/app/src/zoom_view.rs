//! Zoom + pan for a rendered image, shared by the image viewer and the SVG preview:
//! ⌘/Ctrl + wheel and trackpad pinch zoom around the pointer, click-drag pans,
//! and a toolbar (− % + · 1:1 · Fit).

use gpui::{
    App, ClickEvent, Context, Global, ImageSource, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, Pixels, Point,
    ScrollHandle, ScrollWheelEvent, WeakEntity, div, img, point, prelude::*, px,
};

use crate::asset_view::tool;
use crate::theme;

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 32.;
const PAD: f32 = 16.;

/// A view that shows one zoomable image.
pub trait Zoomable: 'static + Sized {
    fn zoom_pan(&mut self) -> &mut ZoomPan;
}

pub struct ZoomPan {
    /// `None` = fit; `Some(z)` = explicit zoom factor.
    pub zoom: Option<f32>,
    /// Natural size of the image (pixels, or SVG user units).
    pub natural: Option<(f32, f32)>,
    /// Fit also scales small images up (vectors stay sharp); bitmaps only scale down.
    pub fit_upscale: bool,
    scroll: ScrollHandle,
    hovered: bool,
    /// Last mouse position in window coordinates (zoom anchor for pinch).
    mouse: Option<Point<Pixels>>,
    /// Click-drag panning: (mouse at press, scroll offset at press).
    pan: Option<(Point<Pixels>, Point<Pixels>)>,
}

impl ZoomPan {
    pub fn new(natural: Option<(f32, f32)>, fit_upscale: bool) -> Self {
        Self { zoom: None, natural, fit_upscale, scroll: ScrollHandle::new(), hovered: false, mouse: None, pan: None }
    }

    /// Scale currently on screen: the explicit zoom, or the fit scale.
    pub fn current_scale(&self) -> f32 {
        if let Some(z) = self.zoom {
            return z;
        }
        let Some((w, h)) = self.natural else { return 1. };
        let b = self.scroll.bounds();
        let (vw, vh) = (f32::from(b.size.width) - 2. * PAD, f32::from(b.size.height) - 2. * PAD);
        if vw <= 0. || vh <= 0. {
            return 1.;
        }
        let fit = (vw / w).min(vh / h);
        if self.fit_upscale { fit } else { fit.min(1.) }
    }

    /// Zoom by `factor`, keeping the image point under `anchor` (window coords) fixed.
    pub fn zoom_by(&mut self, factor: f32, anchor: Option<Point<Pixels>>) -> bool {
        let Some((w, h)) = self.natural else { return false };
        let z0 = self.current_scale();
        let z1 = (z0 * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (z1 - z0).abs() < f32::EPSILON {
            return false;
        }
        let b = self.scroll.bounds();
        let (vw, vh) = (f32::from(b.size.width), f32::from(b.size.height));
        let anchor = anchor.filter(|p| b.contains(p)).unwrap_or_else(|| b.center());
        let (ax, ay) = (f32::from(anchor.x - b.origin.x), f32::from(anchor.y - b.origin.y));
        let off = if self.zoom.is_some() { self.scroll.offset() } else { point(px(0.), px(0.)) };
        // Image origin inside the scroll content (centered when smaller than the viewport).
        let origin = |z: f32, view: f32, natural: f32| ((view - natural * z) / 2.).max(PAD);
        let (ix0, iy0) = (origin(z0, vw, w), origin(z0, vh, h));
        // Image coordinate under the anchor.
        let cx_img = (ax - f32::from(off.x) - ix0) / z0;
        let cy_img = (ay - f32::from(off.y) - iy0) / z0;
        let (ix1, iy1) = (origin(z1, vw, w), origin(z1, vh, h));
        let max_x = (w * z1 + 2. * PAD - vw).max(0.);
        let max_y = (h * z1 + 2. * PAD - vh).max(0.);
        let nx = (ax - ix1 - cx_img * z1).clamp(-max_x, 0.);
        let ny = (ay - iy1 - cy_img * z1).clamp(-max_y, 0.);
        self.zoom = Some(z1);
        self.scroll.set_offset(point(px(nx), px(ny)));
        true
    }

    pub fn set_zoom(&mut self, zoom: Option<f32>) {
        self.zoom = zoom;
        self.scroll.set_offset(point(px(0.), px(0.)));
    }

    /// − % + · 1:1 · Fit.
    pub fn toolbar<V: Zoomable>(&self, cx: &mut Context<V>) -> gpui::Div {
        let pct = format!("{:.0}%", self.current_scale() * 100.);
        let fit = self.zoom.is_none();
        div()
            .flex()
            .items_center()
            .gap(px(1.))
            .p(px(2.))
            .rounded(px(6.))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .child(tool("zoom-out", Some("icons/minus.svg"), "", "Zoom out (⌘ + wheel)").on_click(cx.listener(|this: &mut V, _: &ClickEvent, _, cx| {
                if this.zoom_pan().zoom_by(1. / 1.25, None) {
                    cx.notify();
                }
            })))
            .child(div().w(px(40.)).flex().justify_center().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT)).child(pct))
            .child(tool("zoom-in", Some("icons/plus.svg"), "", "Zoom in (⌘ + wheel)").on_click(cx.listener(|this: &mut V, _: &ClickEvent, _, cx| {
                if this.zoom_pan().zoom_by(1.25, None) {
                    cx.notify();
                }
            })))
            .child(div().w(px(1.)).h(px(14.)).mx(px(2.)).bg(theme::rgb(theme::BORDER)))
            .child(tool("zoom-100", None, "1:1", "Actual size").on_click(cx.listener(|this: &mut V, _: &ClickEvent, _, cx| {
                this.zoom_pan().set_zoom(Some(1.));
                cx.notify();
            })))
            .child(tool("zoom-fit", None, "Fit", "Fit to window").when(fit, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT))).on_click(cx.listener(
                |this: &mut V, _: &ClickEvent, _, cx| {
                    this.zoom_pan().set_zoom(None);
                    cx.notify();
                },
            )))
    }

    /// Scrollable, pannable viewport showing `source` at the current zoom.
    pub fn viewport<V: Zoomable>(&self, source: impl Into<ImageSource>, cx: &mut Context<V>) -> gpui::AnyElement {
        let source = source.into();
        let base = div()
            .id("zoom-scroll")
            .flex_1()
            .min_h_0()
            .size_full()
            .overflow_scroll()
            .track_scroll(&self.scroll)
            // Neutral background so transparent images are readable.
            .bg(theme::rgb(0x1d1f23))
            .on_hover(cx.listener(|this: &mut V, h: &bool, _, _| this.zoom_pan().hovered = *h))
            .when(self.zoom.is_some(), |d| d.cursor(if self.pan.is_some() { gpui::CursorStyle::ClosedHand } else { gpui::CursorStyle::OpenHand }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut V, e: &MouseDownEvent, _, cx| {
                    let zp = this.zoom_pan();
                    if zp.zoom.is_some() {
                        zp.pan = Some((e.position, zp.scroll.offset()));
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this: &mut V, e: &MouseMoveEvent, _, cx| {
                let zp = this.zoom_pan();
                zp.mouse = Some(e.position);
                zp.hovered = true;
                if let Some((start, off)) = zp.pan {
                    if e.pressed_button != Some(MouseButton::Left) {
                        zp.pan = None;
                        cx.notify();
                        return;
                    }
                    let max = zp.scroll.max_offset();
                    let d = e.position - start;
                    let x = (off.x + d.x).clamp(-max.width, px(0.));
                    let y = (off.y + d.y).clamp(-max.height, px(0.));
                    zp.scroll.set_offset(point(x, y));
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut V, _, _, cx| {
                if this.zoom_pan().pan.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this: &mut V, _, _, cx| {
                if this.zoom_pan().pan.take().is_some() {
                    cx.notify();
                }
            }))
            // ⌘/Ctrl + wheel zooms (plain wheel scrolls the zoomed image).
            .on_scroll_wheel(cx.listener(|this: &mut V, e: &ScrollWheelEvent, _, cx| {
                if !(e.modifiers.platform || e.modifiers.control) {
                    return;
                }
                let dy = f32::from(e.delta.pixel_delta(px(20.)).y);
                if this.zoom_pan().zoom_by((dy * 0.005).exp(), Some(e.position)) {
                    cx.notify();
                }
                cx.stop_propagation();
            }));
        match (self.zoom, self.natural) {
            (Some(z), Some((w, h))) => base
                .child(
                    // Explicit size so the scroll area measures the whole image (with only
                    // min_w_full the row stayed viewport-wide: no horizontal scroll/pan).
                    div()
                        .w(px(w * z + 2. * PAD))
                        .h(px(h * z + 2. * PAD))
                        .min_w_full()
                        .min_h_full()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(PAD))
                        .child(img(source).flex_none().w(px(w * z)).h(px(h * z)).object_fit(ObjectFit::Fill)),
                )
                .into_any_element(),
            _ => base
                .flex()
                .items_center()
                .justify_center()
                .p(px(PAD))
                .child(img(source).size_full().object_fit(if self.fit_upscale { ObjectFit::Contain } else { ObjectFit::ScaleDown }))
                .into_any_element(),
        }
    }
}

/// Trackpad pinch (NSEvent magnify). GPUI doesn't surface it, so we add
/// `magnifyWithEvent:` to GPUI's NSView class and forward the deltas to the hovered zoomable view.
pub fn register_pinch<V: Zoomable>(view: WeakEntity<V>, cx: &mut App) {
    pinch::register(
        Box::new(move |cx: &mut App, factor: f32| {
            let e = view.upgrade()?;
            Some(e.update(cx, |v, cx| {
                let zp = v.zoom_pan();
                if !zp.hovered {
                    return false;
                }
                let anchor = zp.mouse;
                if zp.zoom_by(factor, anchor) {
                    cx.notify();
                }
                true
            }))
        }),
        cx,
    );
}

mod pinch {
    use super::*;
    use futures::{StreamExt, channel::mpsc};
    use std::sync::OnceLock;

    static TX: OnceLock<mpsc::UnboundedSender<f64>> = OnceLock::new();

    /// Applies a zoom factor to its view if hovered: `None` = view dropped, `Some(handled)`.
    pub type Target = Box<dyn Fn(&mut App, f32) -> Option<bool>>;

    #[derive(Default)]
    struct Targets(Vec<Target>);
    impl Global for Targets {}

    pub fn register(target: Target, cx: &mut App) {
        if !cx.has_global::<Targets>() {
            cx.set_global(Targets::default());
            let (tx, mut rx) = mpsc::unbounded::<f64>();
            if TX.set(tx).is_ok() {
                install_magnify_hook();
            }
            cx.spawn(async move |cx| {
                while let Some(mut m) = rx.next().await {
                    // Coalesce queued deltas into one zoom step.
                    while let Ok(more) = rx.try_recv() {
                        m += more;
                    }
                    let factor = (1. + m as f32).max(0.1);
                    let _ = cx.update(|cx| {
                        let mut targets = std::mem::take(&mut cx.global_mut::<Targets>().0);
                        let mut alive = vec![true; targets.len()];
                        for (i, t) in targets.iter().enumerate().rev() {
                            match t(cx, factor) {
                                None => alive[i] = false,
                                Some(true) => break,
                                Some(false) => {}
                            }
                        }
                        let mut i = 0;
                        targets.retain(|_| (alive[i], i += 1).0);
                        // Keep any registered while the targets ran.
                        let added = std::mem::take(&mut cx.global_mut::<Targets>().0);
                        targets.extend(added);
                        cx.global_mut::<Targets>().0 = targets;
                    });
                }
            })
            .detach();
        }
        cx.global_mut::<Targets>().0.push(target);
    }

    #[cfg(target_os = "macos")]
    #[allow(unexpected_cfgs)]
    fn install_magnify_hook() {
        use objc::runtime::{Class, Object, Sel, class_addMethod};
        use objc::{msg_send, sel, sel_impl};

        extern "C" fn magnify(_this: &Object, _sel: Sel, event: *mut Object) {
            let m: f64 = unsafe { msg_send![event, magnification] };
            if let Some(tx) = TX.get() {
                let _ = tx.unbounded_send(m);
            }
        }

        let Some(cls) = Class::get("GPUIView") else { return };
        unsafe {
            let imp: extern "C" fn(&Object, Sel, *mut Object) = magnify;
            class_addMethod(cls as *const Class as *mut Class, sel!(magnifyWithEvent:), std::mem::transmute(imp), c"v@:@".as_ptr());
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn install_magnify_hook() {}
}
