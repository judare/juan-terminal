//! A real PTY-backed terminal session using `alacritty_terminal` for VT emulation.

use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use anyhow::{Context as _, Result};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::colors::Palette;

/// Grid geometry in cells plus cell size in pixels (used for TIOCSWINSZ pixel fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermDims {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl TermDims {
    pub fn new(cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> Self {
        Self { cols: cols.max(2), rows: rows.max(1), cell_width, cell_height }
    }

    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows,
            num_cols: self.cols,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        }
    }
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    /// Program and args. `None` = the user's `$SHELL` as a login shell.
    pub program: Option<(String, Vec<String>)>,
    /// Scrollback lines; bounded on purpose.
    pub scrollback: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { cwd: None, env: HashMap::new(), program: None, scrollback: 10_000 }
    }
}

/// Events that the UI layer must react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// New content is available; re-render.
    Redraw,
    Title(String),
    ResetTitle,
    Bell,
    /// Application asked to store text in the clipboard (OSC 52).
    ClipboardStore(String),
    /// The child process exited.
    Exited(Option<i32>),
}

/// Forwards alacritty events to the UI, coalescing redraw notifications.
#[derive(Clone)]
pub struct Listener {
    tx: UnboundedSender<Event>,
    redraw_pending: Arc<AtomicBool>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if matches!(event, Event::Wakeup) && self.redraw_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.tx.unbounded_send(event);
    }
}

/// One visible cell, already resolved to concrete colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapCell {
    pub c: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: Flags,
    pub selected: bool,
    /// Shell-integration marker on this cell (OSC 8 link with a `jt://` URI).
    pub mark: Mark,
}

/// Block markers emitted by juan-terminal's zsh integration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mark {
    #[default]
    None,
    /// The prompt line (`jt://prompt?…`): chips or the finished block's header.
    Prompt,
    /// Text typed as the command (`jt://cmd`).
    Cmd,
    /// End of a command block (`jt://end?code=…&ms=…`).
    End,
}

/// Parsed `jt://name?k=v&…` (values percent-decoded).
pub fn parse_mark_uri(uri: &str) -> Option<(String, HashMap<String, String>)> {
    let rest = uri.strip_prefix("jt://")?;
    let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut map = HashMap::new();
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        map.insert(k.to_string(), percent_decode(v));
    }
    Some((name.to_string(), map))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapCursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
}

/// Copy of the visible viewport. Reused across frames to avoid reallocation.
#[derive(Default, Debug)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<SnapCell>,
    /// Combining characters attached to a cell index (common with NFD file names on macOS).
    pub zerowidth: Vec<(usize, Vec<char>)>,
    pub cursor: Option<SnapCursor>,
    pub display_offset: usize,
    pub history_size: usize,
    pub mode: TermModeFlags,
    /// Visible rows carrying a prompt/end marker, top to bottom, with the marker URI.
    pub marks: Vec<(usize, Mark, String)>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct TermModeFlags {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub alt_screen: bool,
    pub alternate_scroll: bool,
    pub focus_in_out: bool,
}

impl Snapshot {
    pub fn row(&self, row: usize) -> &[SnapCell] {
        &self.cells[row * self.cols..(row + 1) * self.cols]
    }

    /// Plain text of a row with trailing spaces trimmed (for tests and copy).
    pub fn row_text(&self, row: usize) -> String {
        let mut s: String = self
            .row(row)
            .iter()
            .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER))
            .map(|c| c.c)
            .collect();
        s.truncate(s.trim_end().len());
        s
    }
}

pub struct TerminalSession {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    redraw_pending: Arc<AtomicBool>,
    palette: Palette,
    dims: TermDims,
    master_fd: i32,
    child_pid: u32,
    exited: bool,
}

impl TerminalSession {
    pub fn spawn(
        config: SessionConfig,
        dims: TermDims,
        palette: Palette,
    ) -> Result<(Self, UnboundedReceiver<Event>)> {
        let mut env = config.env;
        env.entry("TERM".into()).or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into()).or_insert_with(|| "truecolor".into());
        env.insert("TERM_PROGRAM".into(), "juan-terminal".into());
        env.insert("TERM_PROGRAM_VERSION".into(), env!("CARGO_PKG_VERSION").into());
        // Inherit a UTF-8 locale when launched without one (e.g. from Finder).
        if std::env::var_os("LANG").is_none() {
            env.entry("LANG".into()).or_insert_with(|| "en_US.UTF-8".into());
        }

        let shell = match config.program {
            Some((program, args)) => tty::Shell::new(program, args),
            None => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
                tty::Shell::new(shell, vec!["-l".into()])
            }
        };

        let cwd = config.cwd.filter(|p| p.is_dir());
        let options = tty::Options {
            shell: Some(shell),
            working_directory: cwd,
            drain_on_exit: true,
            env,
        };

        let pty = tty::new(&options, dims.window_size(), 0).context("failed to open PTY")?;
        let master_fd = pty.file().as_raw_fd();
        let child_pid = pty.child().id();

        let (tx, rx) = unbounded();
        let redraw_pending = Arc::new(AtomicBool::new(false));
        let listener = Listener { tx, redraw_pending: redraw_pending.clone() };

        let term_config = Config { scrolling_history: config.scrollback, ..Config::default() };
        let term = Arc::new(FairMutex::new(Term::new(term_config, &dims, listener.clone())));

        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)
            .context("failed to create PTY event loop")?;
        let sender = event_loop.channel();
        // The reader thread blocks in kqueue; no polling.
        let _ = event_loop.spawn();

        Ok((
            Self {
                term,
                sender,
                redraw_pending,
                palette,
                dims,
                master_fd,
                child_pid,
                exited: false,
            },
            rx,
        ))
    }

    pub fn dims(&self) -> TermDims {
        self.dims
    }

    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    pub fn has_exited(&self) -> bool {
        self.exited
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        if self.exited {
            return;
        }
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        let _ = self.sender.send(Msg::Input(bytes));
    }

    pub fn resize(&mut self, dims: TermDims) {
        if dims == self.dims {
            return;
        }
        self.dims = dims;
        let _ = self.sender.send(Msg::Resize(dims.window_size()));
        self.term.lock().resize(dims);
    }

    /// Translate a raw alacritty event into a UI event, answering terminal queries internally.
    pub fn process_event(&mut self, event: Event) -> Option<SessionEvent> {
        match event {
            Event::Wakeup => {
                self.redraw_pending.store(false, Ordering::Release);
                Some(SessionEvent::Redraw)
            }
            Event::Title(t) => Some(SessionEvent::Title(t)),
            Event::ResetTitle => Some(SessionEvent::ResetTitle),
            Event::Bell => Some(SessionEvent::Bell),
            Event::ClipboardStore(_, text) => Some(SessionEvent::ClipboardStore(text)),
            Event::PtyWrite(text) => {
                self.write(text.into_bytes());
                None
            }
            Event::ColorRequest(idx, fmt) => {
                let color = self.term.lock().colors()[idx].unwrap_or_else(|| self.palette.for_index(idx));
                self.write(fmt(color).into_bytes());
                None
            }
            Event::TextAreaSizeRequest(fmt) => {
                self.write(fmt(self.dims.window_size()).into_bytes());
                None
            }
            // Reading the clipboard from an application (OSC 52 load) is disabled by default
            // in alacritty's config (`Osc52::OnlyCopy`); nothing to do.
            Event::ClipboardLoad(..) => None,
            Event::ChildExit(status) => {
                self.exited = true;
                Some(SessionEvent::Exited(status.code()))
            }
            Event::Exit => {
                self.exited = true;
                Some(SessionEvent::Exited(None))
            }
            Event::MouseCursorDirty | Event::CursorBlinkingChange => Some(SessionEvent::Redraw),
        }
    }

    /// Copy the visible viewport into `out`. Holds the terminal lock only for the copy.
    pub fn snapshot(&self, out: &mut Snapshot) {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cols = term.columns();
        let rows = term.screen_lines();
        let offset = content.display_offset as i32;
        let overrides = content.colors;
        let selection = content.selection;

        out.cols = cols;
        out.rows = rows;
        out.display_offset = content.display_offset;
        out.history_size = term.history_size();
        out.zerowidth.clear();
        out.cells.clear();
        out.cells.resize(
            cols * rows,
            SnapCell {
                c: ' ',
                fg: self.palette.foreground,
                bg: self.palette.background,
                flags: Flags::empty(),
                selected: false,
                mark: Mark::None,
            },
        );
        out.marks.clear();

        let mode = content.mode;
        out.mode = TermModeFlags {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            focus_in_out: mode.contains(TermMode::FOCUS_IN_OUT),
        };

        let cursor = content.cursor;
        let cursor_row = cursor.point.line.0 + offset;
        out.cursor = (cursor.shape != CursorShape::Hidden && cursor_row >= 0 && (cursor_row as usize) < rows)
            .then(|| SnapCursor { row: cursor_row as usize, col: cursor.point.column.0, shape: cursor.shape });

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let col = indexed.point.column.0;
            if col >= cols {
                continue;
            }
            let cell = indexed.cell;
            let mut fg = self.palette.resolve(cell.fg, overrides, cell.flags);
            let mut bg = self.palette.resolve(cell.bg, overrides, Flags::empty());
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let idx = row as usize * cols + col;
            let mut mark = Mark::None;
            if let Some(link) = cell.hyperlink() {
                let uri = link.uri();
                if let Some(rest) = uri.strip_prefix("jt://") {
                    mark = if rest.starts_with("cmd") {
                        Mark::Cmd
                    } else if rest.starts_with("prompt") {
                        Mark::Prompt
                    } else if rest.starts_with("end") {
                        Mark::End
                    } else {
                        Mark::None
                    };
                    if matches!(mark, Mark::Prompt | Mark::End) && !out.marks.iter().any(|(r, _, _)| *r == row as usize) {
                        out.marks.push((row as usize, mark, uri.to_string()));
                    }
                }
            }
            out.cells[idx] = SnapCell {
                mark,
                c: if cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c },
                fg,
                bg,
                flags: cell.flags,
                selected: selection.is_some_and(|s| s.contains(indexed.point)),
            };
            if let Some(zw) = cell.zerowidth() {
                out.zerowidth.push((idx, zw.to_vec()));
            }
        }
        out.marks.sort_by_key(|m| m.0);
    }

    pub fn scroll(&self, lines: i32) {
        if lines != 0 {
            self.term.lock().scroll_display(Scroll::Delta(lines));
        }
    }

    pub fn scroll_page(&self, up: bool) {
        self.term.lock().scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
    }

    pub fn scroll_to_top(&self) {
        self.term.lock().scroll_display(Scroll::Top);
    }

    /// Drop scrollback history (Cmd-K).
    pub fn clear_history(&self) {
        let mut term = self.term.lock();
        term.scroll_display(Scroll::Bottom);
        term.grid_mut().clear_history();
    }

    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    fn grid_point(&self, term: &Term<Listener>, row: usize, col: usize) -> Point {
        let offset = term.grid().display_offset() as i32;
        let row = row.min(term.screen_lines().saturating_sub(1));
        let col = col.min(term.columns().saturating_sub(1));
        Point::new(Line(row as i32 - offset), Column(col))
    }

    /// Begin a selection at a viewport cell. `clicks`: 1 = simple, 2 = word, 3 = line.
    pub fn start_selection(&self, row: usize, col: usize, right_half: bool, clicks: usize) {
        let mut term = self.term.lock();
        let point = self.grid_point(&term, row, col);
        let ty = match clicks {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        let side = if right_half { Side::Right } else { Side::Left };
        term.selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&self, row: usize, col: usize, right_half: bool) {
        let mut term = self.term.lock();
        let point = self.grid_point(&term, row, col);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn set_focused(&self, focused: bool) {
        let mut term = self.term.lock();
        let notify = term.mode().contains(TermMode::FOCUS_IN_OUT) && term.is_focused != focused;
        term.is_focused = focused;
        drop(term);
        if notify {
            self.write(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
        }
    }

    /// Executable path of the foreground process (e.g. to know `claude` is running).
    pub fn foreground_exe(&self) -> Option<PathBuf> {
        if self.exited {
            return None;
        }
        // SAFETY: see foreground_cwd.
        let pgrp = unsafe { libc::tcgetpgrp(self.master_fd) };
        if pgrp <= 0 {
            return None;
        }
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: buffer sized as the API requires.
        let n = unsafe { libc::proc_pidpath(pgrp, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
        (n > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned()))
    }

    /// Working directory of the foreground process in this terminal (macOS `proc_pidinfo`).
    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        if self.exited {
            return None;
        }
        // SAFETY: master_fd stays open for as long as the event loop owns the PTY,
        // which outlives `self` (shutdown is sent on drop).
        let pgrp = unsafe { libc::tcgetpgrp(self.master_fd) };
        let pid = if pgrp > 0 { pgrp } else { self.child_pid as i32 };
        pid_cwd(pid).or_else(|| pid_cwd(self.child_pid as i32))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Order matters. If the reader stops first, programs that print on exit (claude, vim…)
        // block forever closing the TTY with unread output, stuck in the kernel ("E" state)
        // and keeping their ports. So: hang up the shell's and the foreground job's process
        // groups while the reader keeps draining, escalate to SIGKILL, and only then shut the
        // event loop down (which drops the PTY and reaps the shell).
        let shell = self.child_pid as i32;
        // SAFETY: master_fd is owned by the still-running event loop.
        let fg = unsafe { libc::tcgetpgrp(self.master_fd) };
        let groups: Vec<i32> = [shell, fg].into_iter().filter(|p| *p > 1).collect();
        signal_groups(&groups, libc::SIGHUP);
        let sender = self.sender.clone();
        std::thread::Builder::new()
            .name("pty-teardown".into())
            .spawn(move || {
                let done = |groups: &[i32]| groups.iter().all(|g| !group_alive(*g));
                let mut ok = false;
                for _ in 0..30 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if done(&groups) {
                        ok = true;
                        break;
                    }
                }
                if !ok {
                    signal_groups(&groups, libc::SIGKILL);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                let _ = sender.send(Msg::Shutdown);
            })
            .ok();
    }
}

fn signal_groups(groups: &[i32], sig: i32) {
    for g in groups {
        // SAFETY: plain kill(2) on a process group we created (setsid in the PTY child).
        unsafe { libc::kill(-g, sig) };
    }
}

/// `<libproc.h>`: list only the members of a process group.
const PROC_PGRP_ONLY: u32 = 2;

/// True while any member of process group `pgid` is alive and not a zombie.
fn group_alive(pgid: i32) -> bool {
    // Count members via proc_listpids(PROC_PGRP_ONLY) and skip zombies.
    let mut pids = vec![0i32; 256];
    // SAFETY: buffer size is passed in bytes.
    let n = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, pgid as u32, pids.as_mut_ptr() as *mut _, (pids.len() * 4) as i32) };
    if n <= 0 {
        return false;
    }
    pids.truncate(n as usize / 4);
    pids.into_iter().filter(|p| *p > 0).any(|p| !is_zombie(p))
}

fn is_zombie(pid: i32) -> bool {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: info is a properly sized, zeroed out-buffer.
    let r = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut _) as *mut _, size) };
    // SZOMB = 5; a failed lookup means it's gone.
    r != size || info.pbi_status == 5
}

fn pid_cwd(pid: i32) -> Option<PathBuf> {
    use std::ffi::CStr;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    // SAFETY: buffer is a properly sized, zeroed proc_vnodepathinfo.
    let ret = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&mut info as *mut _) as *mut _, size)
    };
    if ret != size {
        return None;
    }
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const _) };
    let path = PathBuf::from(path.to_str().ok()?);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::{Duration, Instant};

    fn run_until_exit(session: &mut TerminalSession, rx: &mut UnboundedReceiver<Event>) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for child exit");
            let ev = futures::executor::block_on(rx.next());
            let Some(ev) = ev else { return None };
            if let Some(SessionEvent::Exited(code)) = session.process_event(ev) {
                return code;
            }
        }
    }

    fn sh(script: &str) -> SessionConfig {
        SessionConfig {
            program: Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
            ..Default::default()
        }
    }

    #[test]
    fn mark_uris_parse_and_decode() {
        let (name, q) = parse_mark_uri("jt://prompt?cwd=~/my%20dir%26x&node=v22&dirty=3").unwrap();
        assert_eq!(name, "prompt");
        assert_eq!(q["cwd"], "~/my dir&x");
        assert_eq!(q["dirty"], "3");
        assert_eq!(parse_mark_uri("jt://cmd").unwrap().0, "cmd");
        assert!(parse_mark_uri("https://x").is_none());
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%e2%9c%93"), "✓");
    }

    #[test]
    fn osc8_jt_links_become_marks() {
        let (mut s, mut rx) = TerminalSession::spawn(
            sh("printf '\\033]8;;jt://prompt?cwd=%%7E\\033\\\\~\\033]8;;\\033\\\\\\n\\033]8;;jt://cmd\\033\\\\ls\\033]8;;\\033\\\\\\nout\\n\\033]8;;jt://end?code=1&ms=5\\033\\\\ \\033]8;;\\033\\\\\\n'"),
            TermDims::new(40, 8, 8, 16),
            Palette::dark(),
        )
        .unwrap();
        run_until_exit(&mut s, &mut rx);
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        let kinds: Vec<(usize, Mark)> = snap.marks.iter().map(|(r, m, _)| (*r, *m)).collect();
        assert_eq!(kinds, vec![(0, Mark::Prompt), (3, Mark::End)], "{:?}", snap.marks);
        assert_eq!(snap.row(1)[0].mark, Mark::Cmd);
        assert_eq!(snap.row(2)[0].mark, Mark::None);
        assert_eq!(parse_mark_uri(&snap.marks[0].2).unwrap().1["cwd"], "~");
    }

    #[test]
    fn closing_kills_processes_that_print_on_exit() {
        // Prints a lot when hung up: with the old teardown it blocked forever in tty drain.
        let (s, _rx) = TerminalSession::spawn(
            sh("trap 'yes exiting | head -c 200000; exit 0' HUP; sleep 100 & wait"),
            TermDims::new(80, 24, 8, 16),
            Palette::dark(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let pgid = s.child_pid() as i32;
        assert!(group_alive(pgid));
        drop(s);
        let deadline = Instant::now() + Duration::from_secs(6);
        while group_alive(pgid) {
            assert!(Instant::now() < deadline, "process group {pgid} still alive after close");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn runs_command_and_renders_output() {
        let (mut s, mut rx) =
            TerminalSession::spawn(sh("printf 'hello\\n\\033[31mred\\033[0m\\n'"), TermDims::new(40, 10, 8, 16), Palette::dark())
                .unwrap();
        let code = run_until_exit(&mut s, &mut rx);
        assert_eq!(code, Some(0));
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        assert_eq!(snap.row_text(0), "hello");
        assert_eq!(snap.row_text(1), "red");
        assert_eq!(snap.row(1)[0].fg, Palette::dark().ansi[1]);
    }

    #[test]
    fn passes_env_and_cwd() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let mut cfg = sh("printf \"$JT_TEST_VAR:$TERM:\"; pwd");
        cfg.env.insert("JT_TEST_VAR".into(), "ok".into());
        cfg.cwd = Some(dir.clone());
        let (mut s, mut rx) = TerminalSession::spawn(cfg, TermDims::new(200, 5, 8, 16), Palette::dark()).unwrap();
        run_until_exit(&mut s, &mut rx);
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        assert_eq!(snap.row_text(0), format!("ok:xterm-256color:{}", dir.display()));
    }

    #[test]
    fn input_reaches_child_and_exit_code_propagates() {
        let (mut s, mut rx) =
            TerminalSession::spawn(sh("read x; echo got:$x; exit 3"), TermDims::new(40, 5, 8, 16), Palette::dark()).unwrap();
        s.write(&b"abc\r"[..]);
        assert_eq!(run_until_exit(&mut s, &mut rx), Some(3));
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        assert!((0..snap.rows).any(|r| snap.row_text(r) == "got:abc"));
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut cfg = sh("i=0; while [ $i -lt 300 ]; do echo line$i; i=$((i+1)); done");
        cfg.scrollback = 100;
        let (mut s, mut rx) = TerminalSession::spawn(cfg, TermDims::new(20, 10, 8, 16), Palette::dark()).unwrap();
        run_until_exit(&mut s, &mut rx);
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        assert_eq!(snap.history_size, 100);
        s.scroll(5);
        s.snapshot(&mut snap);
        assert_eq!(snap.display_offset, 5);
    }

    #[test]
    fn resize_updates_grid() {
        let (mut s, mut rx) =
            TerminalSession::spawn(sh("sleep 0.2; stty size"), TermDims::new(40, 10, 8, 16), Palette::dark()).unwrap();
        s.resize(TermDims::new(60, 12, 8, 16));
        run_until_exit(&mut s, &mut rx);
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        assert_eq!(snap.cols, 60);
        assert!((0..snap.rows).any(|r| snap.row_text(r) == "12 60"));
    }

    #[test]
    fn foreground_cwd_reports_directory() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let mut cfg = sh("sleep 2");
        cfg.cwd = Some(dir.clone());
        let (s, _rx) = TerminalSession::spawn(cfg, TermDims::new(40, 5, 8, 16), Palette::dark()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(s.foreground_cwd().map(|p| p.canonicalize().unwrap()), Some(dir));
    }
}
