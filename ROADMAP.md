# Roadmap

Each milestone: compile, test, check regressions, measure (PERFORMANCE.md), then continue.

## M1 — Shell (current) ✅ implemented
- [x] Native GPUI window, Dock icon, macOS menus, standard shortcuts
- [x] Project sidebar: create from folder (⌘O), auto-detect repos, add repos (⌘⇧O), remove with confirmation
- [x] Real PTY terminal (alacritty_terminal): colors (16/256/truecolor), alt screen, scrollback, IME, selection, copy/paste, bracketed paste
- [x] Terminal tabs per project, restore with last cwd, clean-exit closes tab
- [x] Project env vars passed to terminals
- [x] Persistent workspace state
- [ ] Mouse reporting to TUI apps (vim/htop mouse) — M1.1
- [ ] Terminal splits — M1.1
- [ ] Search in scrollback — M1.1
- [ ] UI for project env vars / commands (stored, not yet editable in UI) — M1.1

## M2 — Editor ✅ mostly implemented
- [x] Layout: navigation | code | terminals (right), resizable
- [x] File tree per repo + project-root files, SVG file-type icons, lazy folder reads (no watchers)
- [x] Rope editor (`ropey`): edit, undo/redo, selection, auto-indent, comment toggle, find, go to line
- [x] tree-sitter highlighting: JS/JSX/TS/TSX/Vue/Rust/JSON/TOML/YAML/MD/sh/CSS/HTML (background, versioned)
- [x] Save, autosave on focus loss (opt-in), external change detection with conflict banner
- [x] Quick open (⌘P, fuzzy over `git ls-files`), global search (⌘⇧F)
- [x] Markdown live editing (WYSIWYG-style: markers hidden except on the edited line; Live/Source toggle ⌘⇧V; clickable task checkboxes)
- [x] Asset viewer: images, CSV/TSV table, media/PDF/binary card (open in default app); 100 MB limit
- [x] File operations: context menu (new file/folder, rename, trash, reveal in Finder, copy path, terminal/Claude here)
- [x] Lazy LSP (`lsp_client` crate): TypeScript Go (`tsgo --lsp --stdio`) + `oxlint --lsp`, live diagnostics on every change (pull), go to definition (F12 / ⌘-click); servers stop on project switch (process-group kill)
- [x] Markdown tables as an editable grid (Tab/⇧Tab/Enter between cells, toolbar: add/remove rows and columns, align)
- [x] SVG Preview/Source toggle (renders the unsaved buffer)
- [x] Virtual "others" folder per repository (git-ignored, dotfiles, config files)
- [ ] Find references

## M3 — Git ✅ implemented
- [x] `git_engine` crate over the `git` CLI: porcelain v2 status, stage/unstage, discard (confirmed), commit (hooks + signing respected), branches, switch/create, fetch/pull --ff-only/push, log, show
- [x] Diff viewer: unified + side-by-side, syntax highlighted, hunk navigation, stage/unstage single hunks (`git apply --cached`)
- [x] Branch compare (`base...head`), commit history, merge/rebase-in-progress + conflict detection

## M4 — Claude Code
- [x] Claude button per project/repo (and ⌘⇧C): opens a real terminal running `claude` in the user's login shell
- Claude panel: active sessions (detected via foreground process of each PTY), repo, status, jump to terminal, changed files (via git status)

## M5 — Code quality ✅ implemented
- [x] `code_quality` crate: locate oxlint/oxfmt (repo or package `node_modules/.bin`, then PATH), detect config
- [x] Lint project (⌘⇧L) with problems list; format changed files; format file (⌥⇧F)
- [x] On ⌘S: oxfmt via stdin applied as an undoable edit, then oxlint on the file; wavy underlines + hover tooltip

## M6 — Obscura ✅ implemented
- [x] Tools panel: detect binary, download latest arm64 release (confirmed), register/unregister stdio MCP server with Claude Code (`claude mcp add -s user`), docs/OBSCURA.md

## M7 — Polish
- [x] Keep Mac awake while the app is frontmost (IOKit power assertion)
- [x] Command palette (⌘⇧P)
- [ ] Settings file, themes, multiple windows (terminal splits: not wanted)
