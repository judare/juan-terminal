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

## Tests

```sh
cargo test --workspace
```

See ARCHITECTURE.md, DECISIONS.md, ROADMAP.md, PERFORMANCE.md.
