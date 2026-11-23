//! Terminal color palette and color resolution.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

const fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub ansi: [Rgb; 16],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
}

impl Palette {
    /// Default dark palette: low-saturation, high legibility.
    pub fn dark() -> Self {
        Self {
            ansi: [
                rgb(0x1d1f23), // black
                rgb(0xe06c75), // red
                rgb(0x98c379), // green
                rgb(0xe5c07b), // yellow
                rgb(0x61afef), // blue
                rgb(0xc678dd), // magenta
                rgb(0x56b6c2), // cyan
                rgb(0xc8ccd4), // white
                rgb(0x5c6370), // bright black
                rgb(0xef8891), // bright red
                rgb(0xb5e08f), // bright green
                rgb(0xf0d197), // bright yellow
                rgb(0x82c4ff), // bright blue
                rgb(0xd99cee), // bright magenta
                rgb(0x7fd1db), // bright cyan
                rgb(0xf2f4f8), // bright white
            ],
            foreground: rgb(0xd7dae0),
            background: rgb(0x141518),
            cursor: rgb(0xd7dae0),
            selection: rgb(0x2f3d55),
        }
    }

    /// Color for one of the 256 indexed colors.
    pub fn indexed(&self, idx: u8) -> Rgb {
        match idx {
            0..=15 => self.ansi[idx as usize],
            16..=231 => {
                let i = idx - 16;
                let step = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
                Rgb { r: step(i / 36), g: step((i / 6) % 6), b: step(i % 6) }
            }
            232..=255 => {
                let v = 8 + 10 * (idx - 232);
                Rgb { r: v, g: v, b: v }
            }
        }
    }

    fn named(&self, n: NamedColor) -> Rgb {
        match n {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::DimForeground => dim(self.foreground),
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimBlack
            | NamedColor::DimRed
            | NamedColor::DimGreen
            | NamedColor::DimYellow
            | NamedColor::DimBlue
            | NamedColor::DimMagenta
            | NamedColor::DimCyan
            | NamedColor::DimWhite => dim(self.ansi[n as usize - NamedColor::DimBlack as usize]),
            other => self.ansi[other as usize],
        }
    }

    /// Resolve a cell color, honoring palette overrides set by applications via OSC 4/10/11.
    pub fn resolve(&self, color: Color, overrides: &Colors, flags: Flags) -> Rgb {
        match color {
            Color::Spec(rgb) => {
                if flags.contains(Flags::DIM) { dim(rgb) } else { rgb }
            }
            Color::Indexed(idx) => {
                let idx = if flags.contains(Flags::DIM) && idx < 8 {
                    NamedColor::DimBlack as usize + idx as usize
                } else {
                    idx as usize
                };
                overrides[idx].unwrap_or_else(|| {
                    if idx >= 256 {
                        self.named(named_from_index(idx))
                    } else {
                        self.indexed(idx as u8)
                    }
                })
            }
            Color::Named(n) => {
                let n = if flags.contains(Flags::DIM) { n.to_dim() } else { n };
                overrides[n as usize].unwrap_or_else(|| self.named(n))
            }
        }
    }

    /// Color for an index used by OSC color queries.
    pub fn for_index(&self, idx: usize) -> Rgb {
        if idx < 256 { self.indexed(idx as u8) } else { self.named(named_from_index(idx)) }
    }
}

fn named_from_index(idx: usize) -> NamedColor {
    use NamedColor::*;
    match idx {
        256 => Foreground,
        257 => Background,
        258 => Cursor,
        259 => DimBlack,
        260 => DimRed,
        261 => DimGreen,
        262 => DimYellow,
        263 => DimBlue,
        264 => DimMagenta,
        265 => DimCyan,
        266 => DimWhite,
        267 => BrightForeground,
        _ => DimForeground,
    }
}

fn dim(c: Rgb) -> Rgb {
    Rgb { r: (c.r as u16 * 2 / 3) as u8, g: (c.g as u16 * 2 / 3) as u8, b: (c.b as u16 * 2 / 3) as u8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_grayscale() {
        let p = Palette::dark();
        assert_eq!(p.indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(p.indexed(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(p.indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(p.indexed(255), Rgb { r: 238, g: 238, b: 238 });
    }

    #[test]
    fn overrides_win() {
        let p = Palette::dark();
        let mut o = Colors::default();
        o[NamedColor::Foreground] = Some(rgb(0x123456));
        assert_eq!(p.resolve(Color::Named(NamedColor::Foreground), &o, Flags::empty()), rgb(0x123456));
    }
}
