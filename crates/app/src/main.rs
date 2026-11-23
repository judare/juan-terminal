//! juan-terminal: native macOS development workspace.

mod actions_panel;
mod asset_view;
mod claude_bridge;
mod claude_sessions;
mod assets;
mod command_palette;
mod diff_view;
mod editor_view;
mod git_panel;
mod hotkey;
mod hover_preview;
mod language_servers;
mod launcher;
mod markdown_live;
mod moli;
mod ports;
mod power;
mod quick_open;
mod search_panel;
mod servers_panel;
mod shell_env;
mod shell_integration;
mod slash_menu;
mod terminal_view;
mod text_area;
mod text_input;
mod theme;
mod tools_panel;
mod totp;
mod website_block;
mod workspace;

use std::time::Instant;

use gpui::{
    App, Application, Bounds, KeyBinding, Menu, MenuItem, SystemMenuType, TitlebarOptions,
    WindowBounds, WindowOptions, point, prelude::*, px, size,
};
use workspace::Workspace;
use workspace_state::StateStore;

/// Append to `<state>/logs/startup.log` (diagnosing launches that never show a window).
pub(crate) fn startup_log(line: &str) {
    use std::io::Write;
    let dir = workspace_state::state_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("startup.log")) {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{ts} pid={} {line}", std::process::id());
    }
}

/// Write `<state>/mcp/<project>.json` registering our `--mcp-servers` stdio server for Claude
/// (dev servers of the project, SSH servers, ssh_exec). Returns its path.
pub(crate) fn servers_mcp_config(project: &workspace_state::Project) -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = workspace_state::state_dir().join("mcp");
    std::fs::create_dir_all(&dir).ok()?;
    let safe: String = project.id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let path = dir.join(format!("{safe}.json"));
    let body = serde_json::json!({"mcpServers": {"juan-terminal": {
        "type": "stdio",
        "command": exe.display().to_string(),
        "args": ["--mcp-servers"],
        "env": {"JT_PROJECT_ID": project.id, "JT_APP_PID": std::process::id().to_string()},
    }}})
    .to_string();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
        std::fs::write(&path, body).ok()?;
    }
    Some(path)
}

fn main() {
    // Stdio MCP server for Claude (spawned by `claude`, not a GUI launch).
    if std::env::args().any(|a| a == "--mcp-servers") {
        let project_id = std::env::var("JT_PROJECT_ID").unwrap_or_default();
        let app_pid: i32 = std::env::var("JT_APP_PID").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        ssh_remote::mcp::serve_stdio_with(&|| {
            let roots: Vec<(String, std::path::PathBuf)> = workspace_state::StateStore::default_location()
                .load()
                .projects
                .into_iter()
                .find(|p| p.id == project_id)
                .map(|p| p.repositories.iter().map(|r| (r.name.clone(), r.path.clone())).chain(p.root.clone().map(|r| (p.name.clone(), r))).collect())
                .unwrap_or_default();
            ports::dev_servers_report(app_pid, &roots)
        });
        return;
    }
    let started = Instant::now();
    let env_result = if shell_env::needs_import() { shell_env::import_login_env() } else { "skipped (terminal)" };
    startup_log(&format!("env import: {env_result} in {:?}", started.elapsed()));
    // If launched from inside a Claude Code session (e.g. `open` run by Claude), don't pass
    // its session markers on to our terminals: they'd make `claude` there act as a child
    // session (no IDE auto-connect, transcript saving off).
    for (k, _) in std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(|k| k == "CLAUDECODE" || k.starts_with("CLAUDE_CODE_") || k == "CLAUDE_PID" || k == "CLAUDE_EFFORT")).collect::<Vec<_>>() {
        // SAFETY: still single-threaded here.
        unsafe { std::env::remove_var(k) };
    }
    let app = Application::new().with_assets(assets::Assets);
    // Files opened from Finder ("Open With", dropped on the Dock icon, `open -a`).
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    app.on_open_urls(move |urls| {
        let _ = open_tx.unbounded_send(urls);
    });
    app.run(move |cx: &mut App| {
        cx.spawn(async move |cx| {
            use futures::StreamExt;
            while let Some(urls) = open_rx.next().await {
                let paths: Vec<std::path::PathBuf> = urls.iter().filter_map(|u| file_url_to_path(u)).collect();
                if !paths.is_empty() && cx.update(|cx| open_paths_in_front_window(paths, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        bind_keys(cx);
        text_input::bind_keys(cx);
        text_area::bind_keys(cx);
        editor_view::bind_keys(cx);
        search_panel::bind_keys(cx);
        quick_open::bind_keys(cx);
        diff_view::bind_keys(cx);
        // Fallback when no window handles Quit (e.g. no window open).
        cx.on_action(|_: &workspace::Quit, cx| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "juan-terminal".into(),
                items: vec![
                    MenuItem::action("Settings…", workspace::OpenSettings),
                    MenuItem::separator(),
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    MenuItem::action("Quit juan-terminal", workspace::Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("New Window", workspace::NewWindow),
                    MenuItem::action("New Terminal", workspace::NewTerminal),
                    MenuItem::action("Open Project…", workspace::OpenProject),
                    MenuItem::action("Add Repository…", workspace::AddRepository),
                    MenuItem::action("Keep Project Open 1 More Hour", workspace::ExtendProjectHours),
                    MenuItem::separator(),
                    MenuItem::action("Save", editor_view::Save),
                    MenuItem::action("Save All", workspace::SaveAll),
                    MenuItem::action("Toggle Autosave", workspace::ToggleAutosave),
                    MenuItem::separator(),
                    MenuItem::action("Close Editor", workspace::CloseEditor),
                    MenuItem::action("Close Terminal", workspace::CloseTerminal),
                ],
            },
            Menu {
                name: "Edit".into(),
                items: vec![
                    MenuItem::action("Undo", editor_view::Undo),
                    MenuItem::action("Redo", editor_view::Redo),
                    MenuItem::separator(),
                    MenuItem::action("Cut", editor_view::Cut),
                    MenuItem::action("Copy", editor_view::Copy),
                    MenuItem::action("Paste", editor_view::Paste),
                    MenuItem::action("Select All", editor_view::SelectAll),
                    MenuItem::separator(),
                    MenuItem::action("Find", editor_view::Find),
                    MenuItem::action("Go to Line", editor_view::GoToLine),
                    MenuItem::action("Toggle Comment", editor_view::ToggleComment),
                    MenuItem::separator(),
                    MenuItem::action("Clear Terminal", terminal_view::Clear),
                ],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Toggle Sidebar", workspace::ToggleSidebar),
                    MenuItem::action("Toggle Terminal", workspace::ToggleTerminalPanel),
                    MenuItem::action("Command Palette…", workspace::ToggleCommandPalette),
                    MenuItem::action("Go to File…", workspace::ToggleQuickOpen),
                    MenuItem::action("Search in Project", workspace::SearchInProject),
                    MenuItem::action("Files", workspace::ShowFiles),
                    MenuItem::action("Git", workspace::ShowGit),
                    MenuItem::action("Tools", workspace::ShowTools),
                    MenuItem::separator(),
                    MenuItem::action("Lint Project (oxlint)", workspace::LintProject),
                    MenuItem::action("Format File (oxfmt)", workspace::FormatFile),
                    MenuItem::separator(),
                    MenuItem::action("New Claude Session", workspace::OpenClaude),
                    MenuItem::separator(),
                    MenuItem::action("Bigger", workspace::IncreaseFontSize),
                    MenuItem::action("Smaller", workspace::DecreaseFontSize),
                    MenuItem::action("Actual Size", workspace::ResetFontSize),
                ],
            },
            Menu {
                name: "Window".into(),
                items: vec![
                    MenuItem::action("Next Terminal", workspace::NextTerminal),
                    MenuItem::action("Previous Terminal", workspace::PreviousTerminal),
                ],
            },
        ]);

        let store = StateStore::default_location();
        let state = store.load();
        let bounds = state
            .window
            .filter(|w| w.width >= 400. && w.height >= 300.)
            .map(|w| Bounds::new(point(px(w.x), px(w.y)), size(px(w.width), px(w.height))))
            .unwrap_or_else(|| Bounds::centered(None, size(px(1200.), px(780.)), cx));

        let window = match open_workspace_window(store, state, bounds, true, cx) {
            Ok(w) => w,
            Err(err) => {
                eprintln!("juan-terminal: failed to open window: {err:#}");
                startup_log(&format!("failed to open window: {err:#}"));
                cx.quit();
                return;
            }
        };
        // ⌘Space anywhere: apps, URLs, projects, files (replaces Raycast/Spotlight).
        launcher::init(window, cx);

        // Closing the last window quits (state is saved on quit).
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);

        startup_log(&format!("window opened in {:?}", started.elapsed()));
        if std::env::var_os("JUAN_TERMINAL_TRACE_STARTUP").is_some() {
            eprintln!("startup: window opened in {:?}", started.elapsed());
        }
    });
}

/// `file:///a/b%20c` → `/a/b c`. Non-file URLs are ignored.
fn file_url_to_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let path = String::from_utf8(out).ok()?;
    let path = path.strip_suffix('/').filter(|p| !p.is_empty()).unwrap_or(&path);
    Some(std::path::PathBuf::from(path))
}

/// Open files in the frontmost workspace window (or the first one) and bring it forward.
fn open_paths_in_front_window(paths: Vec<std::path::PathBuf>, cx: &mut App) {
    let target = cx
        .active_window()
        .and_then(|w| w.downcast::<Workspace>())
        .or_else(|| workspace::workspace_windows(cx).into_iter().next());
    let Some(target) = target else { return };
    let _ = target.update(cx, |ws, window, cx| {
        ws.open_dropped(&paths, window, cx);
        window.activate_window();
    });
    cx.activate(true);
}

/// Open a workspace window. `primary` windows also save the window geometry and the active
/// project used at the next launch; other windows only save their own project.
pub(crate) fn open_workspace_window(
    store: StateStore,
    state: workspace_state::WorkspaceState,
    bounds: Bounds<gpui::Pixels>,
    primary: bool,
    cx: &mut App,
) -> anyhow::Result<gpui::WindowHandle<Workspace>> {
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("juan-terminal".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(13.))),
            }),
            window_min_size: Some(size(px(480.), px(320.))),
            app_id: Some("dev.juan.terminal".into()),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Workspace::new(store, state, primary, window, cx)),
    )
}

fn bind_keys(cx: &mut App) {
    use terminal_view as t;
    use workspace as w;
    cx.bind_keys([
        KeyBinding::new("cmd-q", w::Quit, None),
        KeyBinding::new("cmd-,", w::OpenSettings, Some("Workspace")),
        KeyBinding::new("cmd-j", w::ToggleTerminalPanel, Some("Workspace")),
        KeyBinding::new("cmd-p", w::ToggleQuickOpen, Some("Workspace")),
        KeyBinding::new("cmd-shift-p", w::ToggleCommandPalette, Some("Workspace")),
        KeyBinding::new("cmd-shift-f", w::SearchInProject, Some("Workspace")),
        KeyBinding::new("cmd-shift-e", w::ShowFiles, Some("Workspace")),
        KeyBinding::new("cmd-shift-c", w::OpenClaude, Some("Workspace")),
        KeyBinding::new("cmd-shift-g", w::ShowGit, Some("Workspace")),
        KeyBinding::new("cmd-shift-l", w::LintProject, Some("Workspace")),
        KeyBinding::new("alt-shift-f", w::FormatFile, Some("Workspace")),
        KeyBinding::new("cmd-w", w::CloseDiff, Some("DiffView")),
        KeyBinding::new("ctrl-`", w::ToggleTerminalPanel, Some("Workspace")),
        KeyBinding::new("cmd-alt-s", w::SaveAll, Some("Workspace")),
        KeyBinding::new("cmd-w", w::CloseEditor, Some("Editor")),
        KeyBinding::new("cmd-shift-]", w::NextEditor, Some("Editor")),
        KeyBinding::new("cmd-shift-[", w::PreviousEditor, Some("Editor")),
        KeyBinding::new("ctrl-tab", w::NextEditor, Some("Editor")),
        KeyBinding::new("ctrl-shift-tab", w::PreviousEditor, Some("Editor")),
        KeyBinding::new("cmd-t", w::NewTerminal, Some("Workspace")),
        KeyBinding::new("cmd-shift-n", w::NewWindow, Some("Workspace")),
        KeyBinding::new("cmd-w", w::CloseTerminal, Some("Workspace")),
        KeyBinding::new("cmd-shift-]", w::NextTerminal, Some("Workspace")),
        KeyBinding::new("cmd-shift-[", w::PreviousTerminal, Some("Workspace")),
        KeyBinding::new("ctrl-tab", w::NextTerminal, Some("Workspace")),
        KeyBinding::new("ctrl-shift-tab", w::PreviousTerminal, Some("Workspace")),
        KeyBinding::new("cmd-o", w::OpenProject, Some("Workspace")),
        KeyBinding::new("cmd-shift-o", w::AddRepository, Some("Workspace")),
        KeyBinding::new("cmd-b", w::ToggleSidebar, Some("Workspace")),
        KeyBinding::new("cmd-=", w::IncreaseFontSize, Some("Workspace")),
        KeyBinding::new("cmd-+", w::IncreaseFontSize, Some("Workspace")),
        KeyBinding::new("cmd--", w::DecreaseFontSize, Some("Workspace")),
        KeyBinding::new("cmd-0", w::ResetFontSize, Some("Workspace")),
        KeyBinding::new("cmd-c", t::Copy, Some("Terminal")),
        KeyBinding::new("cmd-v", t::Paste, Some("Terminal")),
        KeyBinding::new("cmd-k", t::Clear, Some("Terminal")),
        KeyBinding::new("cmd-f", t::Find, Some("Terminal")),
        KeyBinding::new("cmd-g", t::FindNext, Some("Terminal")),
        KeyBinding::new("cmd-shift-g", t::FindPrevious, Some("Terminal")),
        KeyBinding::new("shift-pageup", t::ScrollPageUp, Some("Terminal")),
        KeyBinding::new("shift-pagedown", t::ScrollPageDown, Some("Terminal")),
        KeyBinding::new("cmd-home", t::ScrollToTop, Some("Terminal")),
        KeyBinding::new("cmd-end", t::ScrollToBottom, Some("Terminal")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_decode() {
        assert_eq!(file_url_to_path("file:///Users/a/b%20c.txt"), Some("/Users/a/b c.txt".into()));
        assert_eq!(file_url_to_path("file://localhost/tmp/x/"), Some("/tmp/x".into()));
        assert_eq!(file_url_to_path("file:///%C3%B1.md"), Some("/ñ.md".into()));
        assert_eq!(file_url_to_path("https://x.com"), None);
    }

    #[test]
    fn shell_quoting() {
        use terminal_view::shell_quote;
        assert_eq!(shell_quote("/a/b.txt"), "/a/b.txt");
        assert_eq!(shell_quote("/a/b c.txt"), "'/a/b c.txt'");
        assert_eq!(shell_quote("/a/it's"), "'/a/it'\\''s'");
    }
}
