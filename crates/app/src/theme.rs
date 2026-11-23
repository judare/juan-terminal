//! Dark theme tokens. Kept as plain constants: no runtime theme engine until it is needed.

use gpui::{Hsla, Rgba};

pub const MONO_FONT: &str = "Menlo";
pub const UI_FONT: &str = ".SystemUIFont";
pub const TERM_LINE_HEIGHT: f32 = 1.3;
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
/// Terminals render 1px larger than the editor.
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
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.,
        g: ((hex >> 8) & 0xff) as f32 / 255.,
        b: (hex & 0xff) as f32 / 255.,
        a: 1.,
    }
    .into()
}
