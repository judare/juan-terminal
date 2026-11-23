//! juan-terminal: native macOS development workspace.

mod asset_view;
mod claude_bridge;
mod claude_sessions;
mod assets;
mod command_palette;
mod diff_view;
mod editor_view;
mod git_panel;
mod hover_preview;
mod language_servers;
mod markdown_live;
mod obscura;
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
mod workspace;

use std::time::Instant;

use gpui::{
    App, Application, Bounds, KeyBinding, Menu, MenuItem, SystemMenuType, TitlebarOptions,
    WindowBounds, WindowOptions, point, prelude::*, px, size,
};
use workspace::Workspace;
use workspace_state::StateStore;

/// Append to `<state>/logs/startup.log` (diagnosing launches that never show a window).
fn startup_log(line: &str) {
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
    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
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
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    MenuItem::action("Quit juan-terminal", workspace::Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("New Terminal", workspace::NewTerminal),
                    MenuItem::action("Open Project…", workspace::OpenProject),
                    MenuItem::action("Add Repository…", workspace::AddRepository),
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

        let window = cx.open_window(
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
            |window, cx| cx.new(|cx| Workspace::new(store, state, window, cx)),
        );
        if let Err(err) = window {
            eprintln!("juan-terminal: failed to open window: {err:#}");
            startup_log(&format!("failed to open window: {err:#}"));
            cx.quit();
            return;
        }

        // Single-window app for now: closing it quits (state is saved on quit).
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

fn bind_keys(cx: &mut App) {
    use terminal_view as t;
    use workspace as w;
    cx.bind_keys([
        KeyBinding::new("cmd-q", w::Quit, None),
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
        KeyBinding::new("shift-pageup", t::ScrollPageUp, Some("Terminal")),
        KeyBinding::new("shift-pagedown", t::ScrollPageDown, Some("Terminal")),
        KeyBinding::new("cmd-home", t::ScrollToTop, Some("Terminal")),
        KeyBinding::new("cmd-end", t::ScrollToBottom, Some("Terminal")),
    ]);
}
