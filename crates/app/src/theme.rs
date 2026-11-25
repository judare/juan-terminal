//! Theme tokens. Colors are written as dark-theme hex constants; `rgb()` translates every one of
//! them to its light-theme counterpart while light mode is on, so call sites stay theme-agnostic.
//! Every UI color must go through `rgb()` (or `map()` for non-gpui color types).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use gpui::{Hsla, Rgba};

static LIGHT: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU32 = AtomicU32::new(0);

pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// Switch between light and dark. Callers refresh the windows afterwards.
pub fn set_light(light: bool) {
    if LIGHT.swap(light, Ordering::Relaxed) != light {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    set_native_appearance(light);
}

/// Bumped on every theme change: part of the key of caches that hold resolved colors.
pub fn generation() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

/// Native chrome (menus, context menus, file dialogs, scrollers) follows the app theme.
#[allow(unexpected_cfgs)] // objc 0.2 macros
fn set_native_appearance(light: bool) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    let name: &[u8] = if light { b"NSAppearanceNameAqua\0" } else { b"NSAppearanceNameDarkAqua\0" };
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let ns_name: *mut Object = msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
        let appearance: *mut Object = msg_send![class!(NSAppearance), appearanceNamed: ns_name];
        let _: () = msg_send![app, setAppearance: appearance];
    }
}

pub const MONO_FONT: &str = "Menlo";
pub const UI_FONT: &str = ".SystemUIFont";
pub const TERM_LINE_HEIGHT: f32 = 1.3;
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
/// Code editor text renders this much larger than the base font size.
pub const EDITOR_FONT_BONUS: f32 = 2.0;
/// Terminals render 1px larger than the base font size.
pub const TERM_FONT_BONUS: f32 = 1.0;

pub const TERM_BG: u32 = 0x141518;
pub const APP_BG: u32 = 0x141518;
pub const SIDEBAR_BG: u32 = 0x1a1b1f;
pub const TABBAR_BG: u32 = 0x1a1b1f;
pub const BORDER: u32 = 0x26282d;
pub const HOVER: u32 = 0x24262b;
pub const SELECTED: u32 = 0x2a2d34;
pub const TEXT: u32 = 0xd7dae0;
pub const TEXT_MUTED: u32 = 0x8a8f98;
pub const TEXT_FAINT: u32 = 0x5c6168;
pub const ACCENT: u32 = 0x7aa2f7;
pub const SELECTION: u32 = 0x2f3d55;
pub const EDITOR_BG: u32 = 0x141518;
pub const CURRENT_LINE: u32 = 0x1a1c20;
pub const GUTTER_TEXT: u32 = 0x4b5058;
pub const GUTTER_TEXT_ACTIVE: u32 = 0xa9aeb6;
pub const FIND_MATCH: u32 = 0x3d3a22;
pub const FIND_MATCH_CURRENT: u32 = 0x6b5d1c;
pub const MODIFIED: u32 = 0xe5c07b;
pub const ADDED: u32 = 0x98c379;
pub const DELETED: u32 = 0xe06c75;

/// Syntax color for a tree-sitter highlight name (see `editor::HIGHLIGHT_NAMES`).
pub fn syntax_color(name: &str) -> u32 {
    let base = name.split('.').next().unwrap_or(name);
    match name {
        "variable.builtin" | "constant.builtin" | "boolean" => 0xd19a66,
        "variable.parameter" => 0xe2c08d,
        "function.builtin" => 0x56b6c2,
        "type.builtin" => 0x56b6c2,
        "punctuation.special" | "escape" | "string.special" => 0x56b6c2,
        "text.title" => 0xe5c07b,
        "text.literal" => 0x98c379,
        "text.uri" | "text.reference" => 0x61afef,
        "text.emphasis" => 0xc8ccd4,
        "text.strong" => 0xe8eaee,
        _ => match base {
            "keyword" => 0xc678dd,
            "function" | "constructor" => 0x61afef,
            "string" => 0x98c379,
            "number" | "constant" => 0xd19a66,
            "comment" => 0x6b717c,
            "type" | "module" => 0xe5c07b,
            "tag" => 0xe06c75,
            "attribute" => 0xd19a66,
            "property" => 0xe06c75,
            "operator" => 0x56b6c2,
            "punctuation" => 0x8a8f98,
            "label" => 0xc678dd,
            "embedded" => TEXT,
            _ => TEXT,
        },
    }
}

pub fn rgb(hex: u32) -> Hsla {
    let hex = map(hex);
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.,
        g: ((hex >> 8) & 0xff) as f32 / 255.,
        b: (hex & 0xff) as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// The color to paint for a (dark-theme) hex value under the current theme.
pub fn map(hex: u32) -> u32 {
    if is_light() { light(hex) } else { hex }
}

/// Light-theme counterpart of a dark-theme color.
fn light(hex: u32) -> u32 {
    match hex {
        // Surfaces.
        0x141518 => 0xfcfcfd, // APP_BG / EDITOR_BG / TERM_BG
        0x1a1b1f => 0xf3f4f6, // SIDEBAR_BG / TABBAR_BG
        0x26282d => 0xe1e3e8, // BORDER
        0x24262b => 0xe8eaee, // HOVER
        0x2a2d34 => 0xdde2ea, // SELECTED
        0x1a1c20 => 0xf1f3f6, // CURRENT_LINE
        0x2f3d55 => 0xc8daf6, // SELECTION
        0x3d3a22 => 0xfbe9a6, // FIND_MATCH
        0x6b5d1c => 0xf3c33f, // FIND_MATCH_CURRENT
        // Text.
        0xd7dae0 => 0x24292f, // TEXT
        0x8a8f98 => 0x5d636c, // TEXT_MUTED
        0x5c6168 => 0x878d96, // TEXT_FAINT
        0x4b5058 => 0xa4a9b1, // GUTTER_TEXT
        0xa9aeb6 => 0x3b4149, // GUTTER_TEXT_ACTIVE
        0xc3c7ce => 0x3b4149,
        0xeef0f4 => 0x1b1f24,
        0xd5d8de => 0x2b3036,
        0xc8ccd4 => 0x383a42,
        0xe8eaee => 0x16191d,
        0xffffff => 0x000000,
        // Accent / status (also syntax colors sharing the same hex).
        0x7aa2f7 => 0x2f6fdf, // ACCENT
        0xe5c07b => 0x9a6700, // MODIFIED / type
        0x98c379 => 0x2f8a3c, // ADDED / string
        0xe06c75 => 0xcf2f3d, // DELETED / tag
        0xc678dd => 0xa626a4, // keyword
        0x61afef => 0x2f6fdf, // function
        0x56b6c2 => 0x0b7f8f, // operator / builtin
        0xd19a66 => 0x9a5a00, // number / constant
        0xe2c08d => 0x8a5d12, // parameter
        0x6b717c => 0x8b919a, // comment
        0xe6b673 => 0x8f5f00,
        0xb3a06a => 0x7d6a2e,
        0xff6b6b => 0xd02424,
        0xf0b3b8 => 0xb4202d, // error text
        0xa8d8a0 => 0x23743a, // success text
        0x7f8fa9 => 0x4d5d78,
        0xe0714f => 0xc4501f, // repository icon
        0x8fa3bf => 0x5878a6, // folder icon
        // Tinted backgrounds.
        0x3a1f22 => 0xfbe4e6, // error
        0x4a2226 => 0xf6cfd4,
        0x1d2b22 => 0xe1f3e4, // success
        0x23402a => 0xcbeacf,
        0x3a3220 => 0xfbf0d4, // warning
        0x0e4429 => 0x9be9a8, // commit heat map
        0x006d32 => 0x40c463,
        0x26a641 => 0x30a14e,
        0x39d353 => 0x216e39,
        0x1e2a3d => 0xe2ebfa, // info
        0x1f2a40 => 0xdde8fb,
        0x2b4a7a => 0xc7daf7, // primary button / hover
        0x355a92 => 0xb3cdf3,
        0x3d6bb0 => 0x6d9be0,
        0x1b2230 => 0xe7eef9, // diff hunk header
        0x1c2b20 => 0xe3f5e6, // diff added line
        0x2e1d20 => 0xfce6e8, // diff removed line
        0x2d5a36 => 0xb4e2bc, // diff added words
        0x6a2a32 => 0xf5bcc3, // diff removed words
        // Popovers, chips, tracks.
        0x202227 => 0xffffff,
        0x1e2025 => 0xffffff,
        0x33363d => 0xd6d9df,
        0x363a42 => 0xd6d9df,
        0x3a3d44 => 0xc3c7ce,
        0x2e3138 => 0xdadde3,
        0x2a2c31 => 0xe4e6ea,
        0x2a2d33 => 0xe0e3e8,
        0x2a2c32 => 0xe0e3e8,
        0x34373e => 0xd3d7dd,
        0x1c1e23 => 0xeef0f3,
        0x1d1f23 => 0xf1f2f5,
        0x1f2125 => 0xe3e5e9,
        0x17181c => 0xf0f1f4,
        0x18191c => 0xf6f7f9,
        // Markdown live preview.
        0x24272d => 0xebedf1, // inline code
        0x1b1d22 => 0xf2f3f5, // code block
        0x1e2127 => 0xf4f5f7, // table header row
        0x343841 => 0xd3d7dd, // table grid
        0x3b4252 => 0xc5cad3, // quote bar
        // Brand colors that already read on both backgrounds.
        0xd97757 | 0xfbf1ec => hex,
        _ => invert_lightness(hex),
    }
}

/// Fallback for colors without an explicit light value: keep hue and saturation, mirror lightness,
/// so dark tints become light tints and bright foregrounds become deep ones.
fn invert_lightness(hex: u32) -> u32 {
    let c: Hsla = Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.,
        g: ((hex >> 8) & 0xff) as f32 / 255.,
        b: (hex & 0xff) as f32 / 255.,
        a: 1.,
    }
    .into();
    let l = (1. - c.l).clamp(0.22, 0.97);
    let o: Rgba = Hsla { l, ..c }.into();
    let ch = |v: f32| ((v.clamp(0., 1.) * 255.).round() as u32) & 0xff;
    (ch(o.r) << 16) | (ch(o.g) << 8) | ch(o.b)
}

/// Terminal palette for the current theme.
pub fn terminal_palette() -> terminal_engine::Palette {
    if is_light() { terminal_engine::Palette::light() } else { terminal_engine::Palette::dark() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_mapping_inverts_surfaces_and_text() {
        assert_eq!(light(TEXT), 0x24292f);
        assert_eq!(light(EDITOR_BG), 0xfcfcfd);
        // Unlisted dark tint becomes a light tint, unlisted bright color becomes darker.
        let tint = invert_lightness(0x202a20);
        assert!(((tint >> 8) & 0xff) > 0xc0);
        let deep = invert_lightness(0x61dafb);
        assert!(((deep >> 8) & 0xff) < 0xda);
    }
}
