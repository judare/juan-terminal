//! "Liquid glass" surfaces for inputs, pills and toolbar buttons.
//!
//! GPUI can't blur what's behind a single element (only a whole window), so glass is emulated:
//! a translucent top-lit gradient fill, a hairline light edge and a soft drop shadow. Colors are
//! alpha values picked per theme (they can't go through the hex-keyed `theme::rgb` mapping).

use gpui::{BoxShadow, Background, Hsla, Styled, linear_color_stop, linear_gradient, point, px};

use crate::theme;

fn white(a: f32) -> Hsla {
    gpui::hsla(0., 0., 1., a)
}

fn black(a: f32) -> Hsla {
    gpui::hsla(0., 0., 0., a)
}

/// Resting glass fill: brighter at the top, like light catching the upper edge.
pub fn fill() -> Background {
    if theme::is_light() {
        linear_gradient(180., linear_color_stop(white(0.92), 0.), linear_color_stop(white(0.62), 1.))
    } else {
        linear_gradient(180., linear_color_stop(white(0.085), 0.), linear_color_stop(white(0.035), 1.))
    }
}

/// Hovered / pressed glass: a touch brighter.
pub fn fill_hover() -> Background {
    if theme::is_light() {
        linear_gradient(180., linear_color_stop(white(1.), 0.), linear_color_stop(white(0.78), 1.))
    } else {
        linear_gradient(180., linear_color_stop(white(0.13), 0.), linear_color_stop(white(0.06), 1.))
    }
}

/// Active glass (toggled-on pill): tinted with the brand accent.
pub fn fill_active() -> Background {
    let accent = theme::rgb(theme::ACCENT);
    let (top, bottom) = if theme::is_light() { (0.20, 0.12) } else { (0.28, 0.14) };
    linear_gradient(180., linear_color_stop(Hsla { a: top, ..accent }, 0.), linear_color_stop(Hsla { a: bottom, ..accent }, 1.))
}

/// Hairline edge.
pub fn edge() -> Hsla {
    if theme::is_light() { black(0.09) } else { white(0.11) }
}

/// Edge while focused / active.
pub fn edge_active() -> Hsla {
    let accent = theme::rgb(theme::ACCENT);
    Hsla { a: 0.55, ..accent }
}

fn shadow_with(alpha_dark: f32, alpha_light: f32, blur: f32, y: f32) -> Vec<BoxShadow> {
    let a = if theme::is_light() { alpha_light } else { alpha_dark };
    vec![
        BoxShadow { color: black(a), offset: point(px(0.), px(y)), blur_radius: px(blur), spread_radius: px(0.) },
        BoxShadow { color: black(a * 0.6), offset: point(px(0.), px(1.)), blur_radius: px(2.), spread_radius: px(0.) },
    ]
}

/// Soft floating shadow for inputs and composers.
pub fn shadow() -> Vec<BoxShadow> {
    shadow_with(0.32, 0.07, 14., 4.)
}

/// Smaller shadow for buttons and pills.
pub fn shadow_sm() -> Vec<BoxShadow> {
    shadow_with(0.25, 0.06, 6., 2.)
}

pub trait GlassExt: Styled + Sized {
    /// Glass surface for a text box / composer (caller sets rounding).
    fn glass(self) -> Self {
        self.bg(fill()).border_1().border_color(edge()).shadow(shadow())
    }

    /// Glass surface for a button or pill.
    fn glass_button(self) -> Self {
        self.bg(fill()).border_1().border_color(edge()).shadow(shadow_sm())
    }

    /// Accent-tinted glass for an "on" toggle.
    fn glass_active(self) -> Self {
        self.bg(fill_active()).border_1().border_color(edge_active()).shadow(shadow_sm())
    }

    /// Hover state for any glass surface (use inside `.hover(|s| s.glass_hover())`).
    fn glass_hover(self) -> Self {
        self.bg(fill_hover())
    }
}

impl<T: Styled + Sized> GlassExt for T {}
