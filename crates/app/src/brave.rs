//! MCP tools that drive the user's real Brave (their profile, sessions and logins) from Claude:
//! list tabs, open URLs, run JavaScript and screenshot a window. Control goes through
//! AppleScript (Automation permission); screenshots use `screencapture -l <CGWindowID>`, which
//! needs juan-terminal to have the Screen Recording permission.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use serde_json::{Value, json};

const APP: &str = "Brave Browser";

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
}

pub const INSTRUCTIONS: &str = " The user's own Brave browser (with their logins) is reachable through brave_tabs, brave_open, brave_screenshot and brave_eval_js: use them when the user asks to look at, check or screenshot something in their browser or a page that needs their session. Prefer the local dev server URL for their own app as said above.";

pub fn tools() -> Vec<Value> {
    let target = json!({
        "window": {"type": "integer", "description": "Window index from brave_tabs (1 = frontmost). Default 1."},
        "tab": {"type": "integer", "description": "Tab index inside the window (from brave_tabs). Default: the active tab."},
    });
    let mut shot = target.clone();
    shot["wait_ms"] = json!({"type": "integer", "description": "Extra wait before capturing, after the page finished loading. Default 300."});
    let mut eval = target.clone();
    eval["code"] = json!({"type": "string", "description": "JavaScript expression/statements; the value of the last expression is returned (as a string)."});
    vec![
        json!({
            "name": "brave_tabs",
            "description": "List the open windows and tabs of the user's Brave browser (window index, tab index, active flag, title, URL).",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "brave_open",
            "description": "Open a URL in the user's Brave: in a new tab of the front window (default) or in the current tab. Waits until it loads.",
            "inputSchema": {"type": "object", "properties": {
                "url": {"type": "string"},
                "new_tab": {"type": "boolean", "description": "Default true. false = navigate the active tab of the front window."},
            }, "required": ["url"]},
        }),
        json!({
            "name": "brave_screenshot",
            "description": "Screenshot a Brave window (what the user sees: visible viewport plus browser chrome). Selecting a tab makes it the active one first. Returns the image and the saved PNG path.",
            "inputSchema": {"type": "object", "properties": shot},
        }),
        json!({
            "name": "brave_eval_js",
            "description": "Run JavaScript in a Brave tab (active tab of the front window by default) and return the result. Useful to read text, check the DOM or scroll before a screenshot.",
            "inputSchema": {"type": "object", "properties": eval, "required": ["code"]},
        }),
    ]
}

pub fn call(name: &str, args: &Value) -> Option<(Vec<Value>, bool)> {
    let result = match name {
        "brave_tabs" => list_tabs().map(|t| vec![text(t)]),
        "brave_open" => open(args).map(|t| vec![text(t)]),
        "brave_screenshot" => screenshot(args),
        "brave_eval_js" => eval_js(args).map(|t| vec![text(t)]),
        _ => return None,
    };
    Some(match result {
        Ok(content) => (content, false),
        Err(e) => (vec![text(e)], true),
    })
}

fn text(s: impl Into<String>) -> Value {
    json!({"type": "text", "text": s.into()})
}

/// Runs an AppleScript; `argv` reaches the script as `item n of argv` (no quoting needed).
fn osascript(script: &str, argv: &[&str]) -> Result<String, String> {
    let out = Command::new("osascript").arg("-e").arg(script).args(argv).output().map_err(|e| format!("osascript: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if err.contains("-1743") || err.contains("Not authorized") {
            Err(format!("macOS blocked controlling Brave: allow juan-terminal → Brave Browser in System Settings › Privacy & Security › Automation. ({err})"))
        } else {
            Err(err)
        }
    }
}

fn ensure_running() -> Result<(), String> {
    match osascript(&format!("return application \"{APP}\" is running"), &[])?.as_str() {
        "true" => Ok(()),
        _ => Err("Brave is not running. Ask the user to open it (or use brave_open, which launches it).".into()),
    }
}

fn window_tab(args: &Value) -> (i64, Option<i64>) {
    (args.get("window").and_then(|v| v.as_i64()).unwrap_or(1).max(1), args.get("tab").and_then(|v| v.as_i64()).filter(|t| *t > 0))
}

fn list_tabs() -> Result<String, String> {
    ensure_running()?;
    let script = format!(
        r#"set sep to character id 9
set out to ""
tell application "{APP}"
  set wi to 0
  repeat with w in windows
    set wi to wi + 1
    set act to active tab index of w
    set ti to 0
    repeat with t in tabs of w
      set ti to ti + 1
      set out to out & wi & sep & ti & sep & (ti = act) & sep & (title of t) & sep & (URL of t) & linefeed
    end repeat
  end repeat
end tell
return out"#
    );
    let raw = osascript(&script, &[])?;
    let mut lines = Vec::new();
    for l in raw.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = l.splitn(5, '\t').collect();
        if let [w, t, active, title, url] = f[..] {
            lines.push(format!("[{w}:{t}]{} {title} — {url}", if active == "true" { " (active)" } else { "" }));
        }
    }
    Ok(if lines.is_empty() { "Brave has no open windows.".into() } else { lines.join("\n") })
}

/// Waits until the active tab of `window` stops loading (max 20 s).
fn wait_loaded(window: i64) {
    let start = Instant::now();
    let script = format!("tell application \"{APP}\" to return loading of active tab of window {window}");
    while start.elapsed() < Duration::from_secs(20) {
        match osascript(&script, &[]) {
            Ok(s) if s == "true" => std::thread::sleep(Duration::from_millis(200)),
            _ => break,
        }
    }
}

fn open(args: &Value) -> Result<String, String> {
    let url = args.get("url").and_then(|v| v.as_str()).unwrap_or_default().trim();
    if url.is_empty() {
        return Err("url is required".into());
    }
    let new_tab = args.get("new_tab").and_then(|v| v.as_bool()).unwrap_or(true);
    let script = format!(
        r#"on run argv
  set u to item 1 of argv
  tell application "{APP}"
    if (count of windows) = 0 then
      make new window
      set URL of active tab of front window to u
    else if (item 2 of argv) = "new" then
      tell front window to make new tab with properties {{URL:u}}
    else
      set URL of active tab of front window to u
    end if
  end tell
end run"#
    );
    osascript(&script, &[url, if new_tab { "new" } else { "same" }])?;
    std::thread::sleep(Duration::from_millis(300));
    wait_loaded(1);
    let info = osascript(&format!("tell application \"{APP}\" to return (title of active tab of front window) & \" — \" & (URL of active tab of front window)"), &[])?;
    Ok(format!("Opened in Brave: {info}"))
}

fn select_tab(window: i64, tab: Option<i64>) -> Result<(), String> {
    if let Some(tab) = tab {
        osascript(&format!("tell application \"{APP}\" to set active tab index of window {window} to {tab}"), &[])?;
    }
    Ok(())
}

fn eval_js(args: &Value) -> Result<String, String> {
    ensure_running()?;
    let code = args.get("code").and_then(|v| v.as_str()).unwrap_or_default();
    if code.trim().is_empty() {
        return Err("code is required".into());
    }
    let (window, tab) = window_tab(args);
    select_tab(window, tab)?;
    let script = format!("on run argv\n tell application \"{APP}\" to return execute active tab of window {window} javascript (item 1 of argv)\nend run");
    osascript(&script, &[code]).map_err(|e| {
        if e.contains("JavaScript") {
            format!("Brave blocks JavaScript from AppleScript. The user must enable Brave menu › View › Developer › Allow JavaScript from Apple Events. ({e})")
        } else {
            e
        }
    })
}

/// CGWindowID of the Brave window titled `title` (falls back to the frontmost normal Brave window).
fn brave_window_id(title: &str) -> Option<(u32, bool)> {
    let raw = unsafe { CGWindowListCopyWindowInfo(0, 0) };
    if raw.is_null() {
        return None;
    }
    let windows: CFArray<CFDictionary<CFString, CFType>> = unsafe { CFArray::wrap_under_create_rule(raw) };
    let key = |s: &'static str| CFString::from_static_string(s);
    let mut fallback = None;
    for item in windows.iter() {
        let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(item.as_concrete_TypeRef() as CFDictionaryRef) };
        let get_str = |k| dict.find(key(k)).and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string());
        let get_num = |k| dict.find(key(k)).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i64());
        if get_str("kCGWindowOwnerName").as_deref() != Some(APP) || get_num("kCGWindowLayer") != Some(0) {
            continue;
        }
        let Some(id) = get_num("kCGWindowNumber") else { continue };
        let onscreen = dict.find(key("kCGWindowIsOnscreen")).and_then(|v| v.downcast::<CFBoolean>()).map(bool::from).unwrap_or(false);
        let name = get_str("kCGWindowName");
        if name.as_deref() == Some(title) {
            return Some((id as u32, true));
        }
        // Without Screen Recording permission names are hidden: front-to-back order still helps.
        if fallback.is_none() && onscreen {
            fallback = Some((id as u32, name.is_some()));
        }
    }
    fallback
}

fn screenshot(args: &Value) -> Result<Vec<Value>, String> {
    ensure_running()?;
    let (window, tab) = window_tab(args);
    select_tab(window, tab)?;
    wait_loaded(window);
    std::thread::sleep(Duration::from_millis(args.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(300).min(30_000)));
    let info = osascript(&format!("set sep to character id 9\ntell application \"{APP}\"\n set w to window {window}\n return (name of w) & sep & (URL of active tab of w)\nend tell"), &[])?;
    let (title, url) = info.split_once('\t').unwrap_or((info.as_str(), ""));
    let (id, _) = brave_window_id(title).ok_or("Couldn't find the Brave window on screen (minimized or on another Space?).")?;

    let dir = std::env::temp_dir().join("juan-terminal-shots");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let path: PathBuf = dir.join(format!("brave-{ts}.png"));
    let out = Command::new("screencapture").args(["-x", "-o", &format!("-l{id}")]).arg(&path).output().map_err(|e| format!("screencapture: {e}"))?;
    if !out.status.success() || !path.exists() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "Screenshot failed: macOS needs the Screen Recording permission for juan-terminal (System Settings › Privacy & Security › Screen & System Audio Recording › enable juan-terminal, then restart juan-terminal). ({err})"
        ));
    }
    // Retina captures are huge; Claude downsizes anyway, so shrink to a long edge of 1600 px.
    let _ = Command::new("sips").args(["-Z", "1600"]).arg(&path).output();
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(vec![
        text(format!("Brave window {window}: {title} — {url}\nSaved: {}", path.display())),
        json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType": "image/png"}),
    ])
}
