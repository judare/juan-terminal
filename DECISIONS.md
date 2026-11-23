# Decisions

## D1 — GPUI from crates.io (`gpui = "=0.2.2"`), not the Zed git repository
Zed publishes GPUI to crates.io. Using the published crate avoids cloning the Zed monorepo
(>1 GB), gives reproducible pinned builds, and keeps download size small. Revisit if a needed
fix only exists on Zed `main`. License: Apache-2.0.

## D2 — `runtime_shaders` feature
The Xcode Metal Toolchain (~700 MB) is not installed and bandwidth was limited. GPUI compiles
its Metal shaders at startup instead. Cost: part of the ~0.9 s cold start. To switch to
precompiled shaders later: `xcodebuild -downloadComponent MetalToolchain` and remove the
feature in `Cargo.toml`.

## D3 — `alacritty_terminal = "=0.26.0"` for VT emulation and PTY
Mature, maintained, used by Alacritty and Zed's terminal. Provides PTY spawning (openpty,
setsid, controlling tty), a kqueue-based reader thread, VT parser (vte), grid with bounded
scrollback, selection, alt screen, bracketed paste, OSC 52. Apache-2.0. We only write the
GPUI rendering and input mapping.

## D4 — Shell launched as `$SHELL -l`
Not through `/usr/bin/login` (Alacritty's default). Same login-shell semantics (`.zprofile`),
and the child pid is the shell itself. Env inherited from the launching terminal plus
`TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=juan-terminal` and project env.

## D5 — Option key produces characters, not Meta
Spanish and other layouts need Option for `@ # [ ] { } \ |`. Text goes through the macOS input
method (IME), so dead keys (´ + e = é) and Option characters work like Terminal.app.
Exceptions: Option+←/→ send `ESC b` / `ESC f` (word jump), Option+Backspace deletes a word.

## D6 — Single global state file
Project config lives in the app's state file instead of writing `.workspace/config.toml` into
user folders (would show up as untracked files in repositories). Reading/writing a project-local
`.workspace/config.toml` can be added later as an opt-in.

## D7 — Transparent title bar, closing the window quits
Unified title bar with traffic lights over the sidebar. Single-window app for now; closing the
window quits after saving state. Multiple windows are a later milestone.

## D8 — No file watchers, no polling
Branch labels refresh on activation events only. Future Git status uses explicit refresh +
FSEvents on `.git` only when a Git panel is visible.

## D9 — Format on save through stdin
`oxfmt --stdin-filepath` output is applied to the buffer as one undoable edit before writing,
instead of formatting the file on disk and reloading (which would wipe undo history). Unsupported
file types (oxfmt exit 1 "Unsupported file type") are saved untouched; syntax errors keep the
user's text and the lint pass reports them.

## D10 — Assets
Images render with GPUI's `img` (PNG/JPEG/GIF/WebP/BMP/TIFF). GPUI has no video/audio playback,
so media and PDFs show a file card and open in the macOS default app. Anything over 100 MB is
refused, text or not.

## D11 — Markdown live editing
The buffer is always plain Markdown. Rendering hides markers on lines not being edited (Obsidian
style). Files with ≥ 5000 lines open in Source mode to keep layout cost bounded.

## D12 — Stay awake
`IOPMAssertionCreateWithName(PreventUserIdleDisplaySleep)` while the window is key; released on
deactivation and on quit. Same mechanism as `caffeinate -d`.

## Dependency policy
Only crates that are already pulled by GPUI or are essential: `alacritty_terminal`, `ropey`,
`tree-sitter` + grammars, `serde`, `serde_json`, `toml`, `anyhow`, `futures`, `libc`,
`core-foundation`, `log`. All pinned in `Cargo.lock`.
