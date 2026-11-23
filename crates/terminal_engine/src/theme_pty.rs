//! Light/dark theme reporting for programs in the terminal (contour/ghostty "color palette update
//! notifications", DEC mode 2031): alacritty_terminal ignores it, so the PTY output is scanned here.
//!
//! - `CSI ? 996 n` (query) is answered with `CSI ? 997 ; 1 n` (dark) or `CSI ? 997 ; 2 n` (light).
//! - After `CSI ? 2031 h`, every theme switch sends that report unprompted (Claude Code's "auto"
//!   theme, neovim, … repaint in place). `CSI ? 2031 l` turns it off.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite, Pty};
use polling::{Event, PollMode, Poller};

/// Theme state shared between the session (UI thread) and the PTY reader thread.
#[derive(Default)]
pub struct ThemeState {
    pub light: AtomicBool,
    /// The program enabled DEC mode 2031.
    pub notify: AtomicBool,
}

impl ThemeState {
    pub fn report(&self) -> &'static [u8] {
        if self.light.load(Ordering::Relaxed) { b"\x1b[?997;2n" } else { b"\x1b[?997;1n" }
    }
}

/// `Pty` whose reader watches the output for the sequences above.
pub struct ThemedPty {
    inner: Pty,
    reader: ScanReader,
}

impl ThemedPty {
    pub fn new(mut inner: Pty, state: Arc<ThemeState>) -> io::Result<Self> {
        let fd = inner.reader().as_raw_fd();
        // Same open file description (and O_NONBLOCK) as the PTY master.
        let read_fd = unsafe { libc::dup(fd) };
        let write_fd = unsafe { libc::dup(fd) };
        if read_fd < 0 || write_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let reader = ScanReader {
            file: unsafe { File::from_raw_fd(read_fd) },
            reply: unsafe { File::from_raw_fd(write_fd) },
            scan: Scanner::default(),
            state,
        };
        Ok(Self { inner, reader })
    }
}

pub struct ScanReader {
    file: File,
    reply: File,
    scan: Scanner,
    state: Arc<ThemeState>,
}

impl Read for ScanReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        for &b in &buf[..n] {
            match self.scan.feed(b) {
                Some(Found::Query) => {
                    let _ = self.reply.write_all(self.state.report());
                }
                Some(Found::Notify(on)) => self.state.notify.store(on, Ordering::Relaxed),
                None => {}
            }
        }
        Ok(n)
    }
}

impl EventedReadWrite for ThemedPty {
    type Reader = ScanReader;
    type Writer = File;

    unsafe fn register(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        unsafe { self.inner.register(poll, interest, mode) }
    }

    fn reregister(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        self.inner.reregister(poll, interest, mode)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.inner.deregister(poll)
    }

    fn reader(&mut self) -> &mut ScanReader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut File {
        self.inner.writer()
    }
}

impl EventedPty for ThemedPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.inner.next_child_event()
    }
}

impl OnResize for ThemedPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.inner.on_resize(size)
    }
}

#[derive(Debug, PartialEq)]
enum Found {
    Query,
    Notify(bool),
}

/// Minimal CSI recognizer for `ESC [ ? params final`; survives sequences split across reads.
#[derive(Default)]
struct Scanner {
    state: u8, // 0 ground, 1 after ESC, 2 after ESC [, 3 in private params
    params: Vec<u16>,
    cur: Option<u16>,
}

impl Scanner {
    fn feed(&mut self, b: u8) -> Option<Found> {
        match (self.state, b) {
            (_, 0x1b) => {
                self.state = 1;
                None
            }
            (1, b'[') => {
                self.state = 2;
                None
            }
            (2, b'?') => {
                self.state = 3;
                self.params.clear();
                self.cur = None;
                None
            }
            (3, b'0'..=b'9') => {
                let d = (b - b'0') as u16;
                self.cur = Some(self.cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
                None
            }
            (3, b';') => {
                self.params.push(self.cur.take().unwrap_or(0));
                None
            }
            (3, b'h' | b'l' | b'n') => {
                self.state = 0;
                if let Some(c) = self.cur.take() {
                    self.params.push(c);
                }
                match b {
                    b'n' if self.params == [996] => Some(Found::Query),
                    b'h' | b'l' if self.params.contains(&2031) => Some(Found::Notify(b == b'h')),
                    _ => None,
                }
            }
            _ => {
                self.state = 0;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8]) -> Vec<Found> {
        let mut s = Scanner::default();
        bytes.iter().filter_map(|&b| s.feed(b)).collect()
    }

    #[test]
    fn recognizes_query_and_mode_2031() {
        assert_eq!(scan(b"hi\x1b[?996n"), vec![Found::Query]);
        assert_eq!(scan(b"\x1b[?2031h\x1b[?2031l"), vec![Found::Notify(true), Found::Notify(false)]);
        assert_eq!(scan(b"\x1b[?2004;2031h"), vec![Found::Notify(true)]);
        assert_eq!(scan(b"\x1b[?25h\x1b[?1049l\x1b[6n"), vec![]);
    }
}
