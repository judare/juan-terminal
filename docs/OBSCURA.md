# Obscura in juan-terminal

[Obscura](https://github.com/h4ckf0r0day/obscura) (Apache-2.0) is a headless browser written in
Rust. juan-terminal does not embed it and never starts it. It is exposed to **Claude Code** as a
stdio MCP server, so Claude launches `obscura mcp` only when it needs a browser.

## Setup (sidebar → Tools → Browser for Claude)

1. **Download & install**: fetches `obscura-aarch64-macos.tar.gz` (render-enabled build, ≈67 MB)
   from the latest GitHub release into `~/Library/Application Support/juan-terminal/bin/`.
   If `obscura` is already on `$PATH`, that one is used.
2. **Add to Claude Code**: runs
   `claude mcp add -s user -e OBSCURA_ALLOW_PRIVATE_NETWORK=1 obscura -- <path>/obscura mcp`.
   Remove with `claude mcp remove -s user obscura` (button in the same panel).

Both steps ask for confirmation. Nothing else in your Claude Code config is touched.

## What Claude can do with it

Navigate first, then read or act (the MCP session keeps one live page):

| Tool | Use |
|---|---|
| `browser_navigate` | Open a URL (`waitUntil`: load / domcontentloaded / networkidle0) |
| `browser_snapshot` | URL, title, visible text, element references for interaction |
| `browser_screenshot` | Viewport PNG (render-enabled build) |
| `browser_click`, `browser_fill`, `browser_type`, `browser_press_key`, `browser_select_option` | Interact |
| `browser_evaluate` | Run JavaScript and return the result |
| `browser_console_messages`, `browser_network_requests` | Debug the page |
| `browser_markdown`, `browser_links`, `browser_extract` | Read content |
| `browser_tab_*` | Tabs |
| `browser_pdf` | PDF of the page |

Local dev servers (`localhost`, LAN): Obscura blocks private networks by default (SSRF
protection). juan-terminal registers it with `OBSCURA_ALLOW_PRIVATE_NETWORK=1` because the main
use is inspecting your own frontends running locally.

Source: Obscura `README.md` and `docs/Use-the-MCP-server.md`, release v0.2.7.
