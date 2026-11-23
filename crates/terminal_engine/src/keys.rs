//! Translate UI-level key presses into the byte sequences a PTY application expects.
//!
//! The encoder is UI-agnostic: the GPUI layer converts its `Keystroke` into a [`KeyInput`].
//! Plain printable text (including characters produced with Option on non-US layouts, and dead
//! key compositions) is *not* handled here: it goes through the IME text-input path so that
//! layouts such as Spanish work exactly like in Terminal.app.

use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default)]
pub struct KeyInput<'a> {
    /// GPUI key name: "a", "enter", "up", "f5", ...
    pub key: &'a str,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub cmd: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KeyModes {
    /// DECCKM: application cursor keys.
    pub app_cursor: bool,
}

impl KeyInput<'_> {
    /// xterm modifier parameter: 1 + (shift=1 | alt=2 | ctrl=4).
    fn modifier_param(&self) -> u8 {
        1 + (self.shift as u8) + ((self.alt as u8) << 1) + ((self.ctrl as u8) << 2)
    }

    fn has_mods(&self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

/// Returns the bytes to send for a non-text key press, or `None` when the key should be handled
/// as text input (or ignored, e.g. Cmd shortcuts that were not bound).
pub fn encode_key(k: &KeyInput, modes: KeyModes) -> Option<Cow<'static, [u8]>> {
    if k.cmd {
        return None;
    }
    let s = |b: &'static [u8]| Some(Cow::Borrowed(b));

    match k.key {
        "enter" => {
            return if k.alt { s(b"\x1b\r") } else { s(b"\r") };
        }
        "backspace" => {
            return match (k.ctrl, k.alt) {
                (true, _) => s(b"\x08"),
                (false, true) => s(b"\x1b\x7f"),
                _ => s(b"\x7f"),
            };
        }
        "tab" => return if k.shift { s(b"\x1b[Z") } else { s(b"\t") },
        "escape" => return s(b"\x1b"),
        _ => {}
    }

    // Cursor keys: SS3 form only applies without modifiers.
    if let Some(c) = match k.key {
        "up" => Some(b'A'),
        "down" => Some(b'B'),
        "right" => Some(b'C'),
        "left" => Some(b'D'),
        "home" => Some(b'H'),
        "end" => Some(b'F'),
        _ => None,
    } {
        // macOS convention: Option+Left/Right move by word (readline/zsh `\eb`, `\ef`).
        if k.alt && !k.ctrl && !k.shift {
            match c {
                b'D' => return s(b"\x1bb"),
                b'C' => return s(b"\x1bf"),
                _ => {}
            }
        }
        if k.has_mods() {
            return Some(Cow::Owned(format!("\x1b[1;{}{}", k.modifier_param(), c as char).into_bytes()));
        }
        let prefix: &[u8] = if modes.app_cursor { b"\x1bO" } else { b"\x1b[" };
        let mut v = prefix.to_vec();
        v.push(c);
        return Some(Cow::Owned(v));
    }

    // Tilde keys.
    if let Some(n) = match k.key {
        "insert" => Some(2),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    } {
        let seq = if k.has_mods() {
            format!("\x1b[{n};{}~", k.modifier_param())
        } else {
            format!("\x1b[{n}~")
        };
        return Some(Cow::Owned(seq.into_bytes()));
    }

    // F1-F4 use SS3.
    if let Some(c) = match k.key {
        "f1" => Some('P'),
        "f2" => Some('Q'),
        "f3" => Some('R'),
        "f4" => Some('S'),
        _ => None,
    } {
        let seq = if k.has_mods() {
            format!("\x1b[1;{}{c}", k.modifier_param())
        } else {
            format!("\x1bO{c}")
        };
        return Some(Cow::Owned(seq.into_bytes()));
    }

    // Control characters.
    if k.ctrl {
        let byte = match k.key {
            "space" | "@" | "2" => Some(0x00),
            "[" | "3" => Some(0x1b),
            "\\" | "4" => Some(0x1c),
            "]" | "5" => Some(0x1d),
            "^" | "6" => Some(0x1e),
            "_" | "-" | "7" => Some(0x1f),
            "/" => Some(0x1f),
            "8" => Some(0x7f),
            key if key.len() == 1 => {
                let c = key.as_bytes()[0].to_ascii_lowercase();
                c.is_ascii_lowercase().then(|| c & 0x1f)
            }
            _ => None,
        }?;
        return Some(if k.alt { Cow::Owned(vec![0x1b, byte]) } else { Cow::Owned(vec![byte]) });
    }

    None
}

/// Wrap pasted text for the PTY, honoring bracketed paste mode.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // Strip any embedded end-of-paste marker so pasted content can't escape the bracket.
        let clean = text.replace("\x1b[201~", "");
        let mut v = Vec::with_capacity(clean.len() + 12);
        v.extend_from_slice(b"\x1b[200~");
        v.extend_from_slice(clean.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        // Terminals send CR for newlines when pasting.
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Encode a mouse report for an app that enabled mouse tracking.
/// `button`: 0 left, 1 middle, 2 right, 3 release (legacy), 64/65 wheel up/down; add 32 for
/// motion. `col`/`row` are 0-based grid cells. `pressed` = false sends a release.
pub fn encode_mouse(button: u8, col: usize, row: usize, pressed: bool, sgr: bool, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    let mods = (shift as u8) * 4 + (alt as u8) * 8 + (ctrl as u8) * 16;
    let (x, y) = (col + 1, row + 1);
    if sgr {
        format!("\x1b[<{};{x};{y}{}", button + mods, if pressed { 'M' } else { 'm' }).into_bytes()
    } else {
        // X10/normal encoding: values offset by 32, coordinates capped at 223.
        let b = if pressed { button } else { 3 } + mods + 32;
        vec![0x1b, b'[', b'M', b, (x.min(223) + 32) as u8, (y.min(223) + 32) as u8]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_reports() {
        assert_eq!(encode_mouse(0, 4, 9, true, true, false, false, false), b"\x1b[<0;5;10M");
        assert_eq!(encode_mouse(0, 4, 9, false, true, false, false, false), b"\x1b[<0;5;10m");
        assert_eq!(encode_mouse(64, 0, 0, true, true, false, false, false), b"\x1b[<64;1;1M");
        assert_eq!(encode_mouse(32, 1, 1, true, true, true, false, false), b"\x1b[<36;2;2M");
        assert_eq!(encode_mouse(65, 2, 3, true, false, false, false, false), vec![0x1b, b'[', b'M', 65 + 32, 3 + 32, 4 + 32]);
    }

    fn enc(key: &str, f: impl FnOnce(&mut KeyInput)) -> Option<Vec<u8>> {
        let mut k = KeyInput { key, ..Default::default() };
        f(&mut k);
        encode_key(&k, KeyModes::default()).map(|c| c.into_owned())
    }

    #[test]
    fn basic_keys() {
        assert_eq!(enc("enter", |_| {}), Some(b"\r".to_vec()));
        assert_eq!(enc("backspace", |_| {}), Some(b"\x7f".to_vec()));
        assert_eq!(enc("escape", |_| {}), Some(b"\x1b".to_vec()));
        assert_eq!(enc("tab", |k| k.shift = true), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn plain_text_is_not_encoded() {
        assert_eq!(enc("a", |_| {}), None);
        assert_eq!(enc("space", |_| {}), None);
        // Option+2 on a Spanish layout produces "@": must go through text input.
        assert_eq!(enc("2", |k| k.alt = true), None);
    }

    #[test]
    fn ctrl_keys() {
        assert_eq!(enc("c", |k| k.ctrl = true), Some(vec![0x03]));
        assert_eq!(enc("d", |k| k.ctrl = true), Some(vec![0x04]));
        assert_eq!(enc("space", |k| k.ctrl = true), Some(vec![0x00]));
        assert_eq!(enc("[", |k| k.ctrl = true), Some(vec![0x1b]));
    }

    #[test]
    fn cursor_keys_and_modes() {
        assert_eq!(enc("up", |_| {}), Some(b"\x1b[A".to_vec()));
        let k = KeyInput { key: "up", ..Default::default() };
        assert_eq!(
            encode_key(&k, KeyModes { app_cursor: true }).unwrap().as_ref(),
            b"\x1bOA"
        );
        assert_eq!(enc("right", |k| k.ctrl = true), Some(b"\x1b[1;5C".to_vec()));
        assert_eq!(enc("left", |k| k.alt = true), Some(b"\x1bb".to_vec()));
    }

    #[test]
    fn tilde_and_function_keys() {
        assert_eq!(enc("delete", |_| {}), Some(b"\x1b[3~".to_vec()));
        assert_eq!(enc("pageup", |k| k.shift = true), Some(b"\x1b[5;2~".to_vec()));
        assert_eq!(enc("f1", |_| {}), Some(b"\x1bOP".to_vec()));
        assert_eq!(enc("f12", |_| {}), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn cmd_is_never_sent() {
        assert_eq!(enc("c", |k| k.cmd = true), None);
    }

    #[test]
    fn paste() {
        assert_eq!(encode_paste("a\nb", false), b"a\rb".to_vec());
        assert_eq!(encode_paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~".to_vec());
    }
}
