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

## D13 — Project schedules and blocked websites live in the app (replaces focus-time)
`settings.toml` is separate from `state.toml` because the user edits it while the app keeps
rewriting the state file. Schedules only lock projects inside juan-terminal: no root daemon, no
moving or hiding folders. Website blocking needs root, so the managed `/etc/hosts` block is
written through `osascript … with administrator privileges`, only at launch or on save and only
when it differs; no timed (per-schedule) website blocking, which would need a privileged daemon.
Friction is asymmetric on purpose: a site removed from the list stays blocked for 24 h
(`unblock-pending.json` records when the removal was first seen), while a locked project can be
opened at once for 1 or 4 hours, or extended by an hour (`unlocked_until` in state.toml).

## D14 — Music, podcasts and videos (YouTube via yt-dlp)
Search uses `yt-dlp --flat-playlist -J` (gives view counts for the automatic filters: music ≥ 1M
views, podcasts ≥ 5k and ≥ 10 min, videos unfiltered). Playback favors instant start over
quality: `yt-dlp -g` (android_vr client, fallback default) resolves 128 kb/s AAC, already done in
the background for the top results and the next track, and plays in a native `AVPlayer` that
doesn't wait for a buffer. googlevideo refuses ranges over 1 MiB, so AVPlayer reads through a
loopback proxy (`music_proxy.rs`) that fetches chunks on demand (256 KiB first): streaming,
nothing written to disk. Video (≤480p) is only resolved and streamed when asked, in a second
muted `AVPlayer` synced to the audio clock. No browser cookies or scraping (anonymous, so the
user's account is never at risk). YouTube only enables a stream URL ~5 s after issuing it, so
playback waits out that warm-up (usually already elapsed thanks to prefetching). Songs ≤ 15 min
are kept in `music/cache/` and replayed offline; unplayed for 30 days → deleted.
Suggestions come from `music/preferences.json` (search topics per mode, `avoid`, `used`): a few
topics a day, searched in parallel; each result shows its topic with ✕ to drop it (→ `avoid`).
A LaunchAgent (`com.juan-terminal.music-preferences`, Mondays 09:00) runs
`update-preferences.sh`, where `claude -p` rewrites the topics from history/progress without
repeating `used`. Podcasts/videos keep progress (`progress.json`): finished ones are never
suggested again, unfinished ones resume. Media keys / Now Playing via MediaPlayer.framework;
global ⌃⌥Space / ⌃⌥←→ / ⌃⌥⇧←→ via Carbon hotkeys. `ureq` is reused from the WhatsApp crate's tree.

## D15 — Site blocking in Brave instead of DNS (except adult/gambling)
DNS blocking breaks everything on the Mac: YouTube in /etc/hosts would kill juan-terminal's
music (yt-dlp), and work.example would break work tools. So only `blocked_websites` (adult,
gambling) stays in /etc/hosts; schedules, `brave_blocked_websites` and project `websites` are
enforced only in Brave: a `brave-guard` thread lists Brave's tabs through AppleScript (1 s while
Brave is frontmost, 3 s otherwise, nothing while it isn't running, checked in-process) and sets
blocked tabs to `about:blank`. A guard script injected into each window's active tab blanks pages
with ≥ 6 adult-keyword hits of ≥ 3 different words (or an adult rating meta) and hides videos on
`brave_no_video_websites`. Trade-offs: a page can show for up to ~1 s, it only works while
juan-terminal runs, and Brave tab ids overflow AppleScript integers, so tabs are matched by id
as text. Moving a host from /etc/hosts to Brave releases it from the hosts file at once (it stays
blocked); Brave lists have no 24 h removal delay.

## Dependency policy
Only crates that are already pulled by GPUI or are essential: `alacritty_terminal`, `ropey`,
`tree-sitter` + grammars, `serde`, `serde_json`, `toml`, `anyhow`, `futures`, `libc`,
`core-foundation`, `log`. All pinned in `Cargo.lock`.
