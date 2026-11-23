# juan-terminal

Native macOS (Apple Silicon) development workspace in Rust + GPUI: terminal, projects with
multiple Git repositories, and (next) editor, Git UI, Claude Code, Oxlint/Oxfmt, Obscura.

## Run

```sh
cargo run --release
```

The app appears in the Dock. State: `~/Library/Application Support/juan-terminal/state.toml`.

## Shortcuts

| | |
|---|---|
| ⌘, | Settings (`settings.toml`: project schedules, blocked websites) |
| ⌘O | Open folder as project (auto-detects Git repos) |
| ⌘⇧O | Add repository to current project |
| ⌘T / ⌘W | New / close terminal tab |
| ⌘⇧] ⌘⇧[ / ⌃Tab | Next / previous tab |
| ⌘C / ⌘V | Copy selection / paste |
| ⌘K | Clear terminal + scrollback |
| ⌘B | Toggle sidebar |
| ⌘= ⌘- ⌘0 | Font size |
| ⇧PgUp/PgDn, ⌘Home/End | Scroll |

Click a repository in the sidebar to open (or jump to) a terminal in it.

## Settings

`~/Library/Application Support/juan-terminal/settings.toml` (⌘, opens it; applied on save):

* `[projects.<id>] schedule = [{ days = [...], start = "HH:MM", end = "HH:MM" }]`: hours a project
  can be opened (Mac local time, or a fixed `utc_offset = "-05:00"`). Outside them it is locked in the sidebar and, if open, closed (files saved,
  5-minute warning). Folders on disk are never touched.
* `blocked_websites = [...]`: blocked system-wide via a managed block in `/etc/hosts` (macOS asks
  for the admin password when the list changes). Additions apply at once; removals only after
  24 h.
* Emergencies: opening a locked project offers "Open for 1 hour / 4 hours"; ⌘⇧P → “Keep Project
  Open 1 More Hour” extends the open one.

## Tests

```sh
cargo test --workspace
```

See ARCHITECTURE.md, DECISIONS.md, ROADMAP.md, PERFORMANCE.md.
