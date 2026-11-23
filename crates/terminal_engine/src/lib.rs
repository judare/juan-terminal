//! Terminal engine: PTY process management and VT emulation (via `alacritty_terminal`).
//! Independent of the UI framework so it can be tested headless.

pub mod colors;
pub mod keys;
pub mod session;

pub use alacritty_terminal::event::Event as RawEvent;
pub use alacritty_terminal::term::cell::Flags as CellFlags;
pub use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
pub use colors::Palette;
pub use keys::{KeyInput, KeyModes, encode_key, encode_paste};
pub use session::{Mark, SessionConfig, SessionEvent, SnapCell, SnapCursor, Snapshot, TermDims, TerminalSession, parse_mark_uri};
