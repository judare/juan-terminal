# Moli in juan-terminal

[Moli](https://github.com/lexmount/moli) (Apache-2.0 / MIT) is a headless browser for AI agents
written in Rust (V8 + html5ever + libcurl, no Chromium). juan-terminal does not embed it and
never starts it. Moli has **no MCP server**: Claude Code drives it through its CLI and CDP
server, guided by Moli's official skills.

Replaces the previous Obscura MCP integration, which produced too many errors.

## Setup (sidebar → Tools → Browser)

1. **Download & install**: fetches `moli-<arch>-apple-darwin.tar.gz` (≈42 MB) from the latest
   GitHub release into `~/.local/bin/moli` — the same place the official installer uses, so the
   skills find it. If `moli` is already on `$PATH`, that one is used. "Update binary" re-downloads.
2. **Add skills to Claude**: downloads `skills/moli-webfetch`, `skills/moli-websearch` and
   `skills/moli-cdp-server` from `lexmount/moli` at the tag matching the installed binary
   (`v<version>`) into `~/.claude/skills/<name>/`. A `.juan-terminal-ref` file records the tag;
   the panel offers "Update skills" when the binary is newer.
3. **Remove old Obscura** (only shown if leftovers exist): `claude mcp remove -s user obscura`
   and deletes `obscura` / `obscura-worker` from `~/Library/Application Support/juan-terminal/bin/`.

Every step asks for confirmation.

## What Claude can do with it

| Skill | Use |
|---|---|
| `moli-webfetch` | `moli fetch --dump markdown\|semantic_tree_text\|json\|html --wait done\|networkidle\|domstable URL`; `--layout --dump screenshot\|screenshot_full\|pdf`; `--eval` JS |
| `moli-websearch` | Keyword / reverse-image search across engines, verifying sources with `moli fetch` |
| `moli-cdp-server` | `moli serve [--layout]` on `127.0.0.1:9222` for Playwright/Puppeteer `connectOverCDP` |

Local dev servers (`localhost`, LAN) work by default; Moli only blocks private networks with
`--block-private-networks`.
