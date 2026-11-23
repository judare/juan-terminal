# Architecture

juan-terminal is a native macOS (Apple Silicon) development workspace written in Rust on top of
GPUI. One process, no Electron, no embedded browser, no background services.

## Crates (Milestone 1)

```
crates/
  app/              binary `juan-terminal`: GPUI window, menus, keymap, views
    main.rs           entry point, menus, key bindings, window creation
    workspace.rs      root view: project sidebar, terminal tabs, persistence glue
    terminal_view.rs  terminal grid element, keyboard/IME/mouse/scroll handling
    theme.rs          dark theme tokens
  terminal_engine/  PTY + VT emulation, UI-agnostic, headless-testable
    session.rs        TerminalSession (alacritty_terminal Term + EventLoop), Snapshot
    keys.rs           key → escape-sequence encoder, bracketed paste
    colors.rs         palette + color resolution (16/256/truecolor, OSC overrides)
  workspace_state/  projects, repositories, state file, repo discovery
```

Planned crates (see ROADMAP.md): `editor`, `git_engine`, `diff_engine`, `code_quality`,
`claude_integration`, `obscura_integration`, `settings`. Each one only depends on what it needs;
the `app` crate composes them. No IPC or serialization between in-process modules.

## Data flow: terminal

```
keyboard ─► GPUI key dispatch ─► bindings (Cmd-*) ─► actions (copy, paste, new tab…)
                              └► TerminalView::key_down ─► keys::encode_key ─► PTY
IME / text input (dead keys, Option chars, emoji) ─► EntityInputHandler ─► PTY

PTY ─► alacritty EventLoop thread (kqueue, blocking) ─► Term (VT parser, grid)
    └► Listener: Wakeup (coalesced via AtomicBool) ─► futures channel ─► GPUI task ─► cx.notify()
frame ─► TerminalElement::prepaint: lock Term, copy viewport into reused Snapshot, unlock
      ─► build background quads + positioned text runs ─► paint
```

* One reader thread per terminal (alacritty's `PTY reader`), blocked in the kernel when idle.
* Redraw notifications are coalesced: at most one pending `Wakeup` per session at a time, and
  GPUI coalesces `notify()` calls into one frame.
* The terminal lock is held only while copying the visible cells (not while shaping/painting).
* Text runs are placed at exact cell positions, so alignment never depends on font shaping;
  wide chars are isolated in their own run; combining marks (NFD file names) are attached.
* Scrollback is bounded (10 000 lines default).

## Projects and repositories

* A Project has an optional root folder plus any number of repositories anywhere on disk.
  Repositories are never moved or cloned.
* Creating a project from a folder runs a one-shot, bounded discovery (depth 2, skipping
  hidden/dependency/build folders). No file watchers.
* Branch labels in the sidebar come from reading `HEAD` (no subprocess), refreshed on project
  activation and window activation. All real Git operations will use the `git` executable.
* Terminals are per project, started lazily when the project is first activated.

## Persistence

`~/Library/Application Support/juan-terminal/state.toml` (override with
`JUAN_TERMINAL_STATE_DIR`). Atomic write (tmp + rename). Corrupt files are backed up to
`state.toml.bak` and the app starts with defaults. Saved: projects, repositories, project env
vars and commands, per-project terminal tabs (current working directory of the foreground
process via `proc_pidinfo`), active tab, sidebar width, window frame.
