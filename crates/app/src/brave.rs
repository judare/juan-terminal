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

pub const INSTRUCTIONS: &str = " The user's own Brave browser (with their logins) is reachable through brave_tabs, brave_open, brave_screenshot, brave_eval_js and brave_pick_element (the user points at an element): use them when the user asks to look at, check or screenshot something in their browser or a page that needs their session. Prefer the local dev server URL for their own app as said above.";

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
        "brave_pick_element" => pick_for_tool(),
        _ => return None,
    };
    Some(match result {
        Ok(content) => (content, false),
        Err(e) => (vec![text(e)], true),
    })
}

fn pick_for_tool() -> Result<Vec<Value>, String> {
    let pick = pick_element(Duration::from_secs(180));
    activate_self();
    let Some(pick) = pick? else { return Err("The user cancelled the selection (Esc) or didn't pick anything.".into()) };
    let mut out = vec![text(pick.summary)];
    if let Some(bytes) = pick.shot.and_then(|p| std::fs::read(p).ok()) {
        out.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType": "image/png"}));
    }
    Ok(out)
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

// ---------------------------------------------------------------------------------------------
// Element picker: an overlay in the page highlights what's under the mouse (box + label); a
// click captures the element's context (selector, component + source file, styles, HTML) and
// a screenshot of it. The page stores the result in `window.__jtPick`, which we poll.

static PICK_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const PICK_JS: &str = r#"(() => {
  if (window.__jtPickCleanup) window.__jtPickCleanup();
  window.__jtPick = '';
  const Z = 2147483647, ACC = '#7aa2f7';
  const mk = (css) => { const d = document.createElement('div'); d.style.cssText = css; document.documentElement.appendChild(d); return d; };
  const box = mk(`position:fixed;pointer-events:none;z-index:${Z};border:2px solid ${ACC};background:rgba(122,162,247,.16);border-radius:3px;display:none;box-sizing:border-box`);
  const pad = mk(`position:fixed;pointer-events:none;z-index:${Z};outline:1px dashed rgba(122,162,247,.55);display:none`);
  const tag = mk(`position:fixed;pointer-events:none;z-index:${Z};background:#1a1b1f;color:#d7dae0;font:11px/1.5 Menlo,monospace;padding:3px 7px;border-radius:5px;white-space:nowrap;box-shadow:0 2px 10px rgba(0,0,0,.35);display:none`);
  const bar = mk(`position:fixed;left:50%;top:10px;transform:translateX(-50%);z-index:${Z};background:#1a1b1f;color:#d7dae0;font:12px/1.4 -apple-system,system-ui,sans-serif;padding:7px 14px;border-radius:8px;border:1px solid ${ACC};box-shadow:0 4px 18px rgba(0,0,0,.4);pointer-events:none`);
  bar.innerHTML = '<b style="color:#7aa2f7">juan-terminal</b> · Click an element to send it to Claude · <b>Esc</b> to cancel';
  const ours = new Set([box, pad, tag, bar]);
  const comp = (el) => {
    const out = { names: [], source: null };
    const fk = Object.keys(el).find(k => k.startsWith('__reactFiber$') || k.startsWith('__reactInternalInstance$'));
    if (fk) {
      for (let f = el[fk], n = 0; f && n < 60; f = f.return, n++) {
        const t = f.type;
        if (t && typeof t !== 'string') { const nm = t.displayName || t.name || (t.render && (t.render.displayName || t.render.name)); if (nm && out.names.length < 4 && !out.names.includes(nm)) out.names.push(nm); }
        if (!out.source && f._debugSource) out.source = f._debugSource.fileName + ':' + f._debugSource.lineNumber;
      }
    }
    for (let e = el; e && !out.names.length; e = e.parentElement) {
      const v = e.__vueParentComponent;
      if (v) { for (let c = v; c && out.names.length < 4; c = c.parent) { const t = c.type || {}; const nm = t.__name || t.name || (t.__file && t.__file.split('/').pop()); if (nm) out.names.push(nm); if (!out.source && t.__file) out.source = t.__file; } }
      else if (e.__vue__) { const o = e.__vue__.$options; out.names.push(o.name || o._componentTag || 'Vue'); out.source = o.__file || null; }
    }
    for (let e = el; e && !out.source; e = e.parentElement) if (e.__svelte_meta) out.source = e.__svelte_meta.loc.file + ':' + (e.__svelte_meta.loc.line + 1);
    return out;
  };
  const sel = (el) => {
    const parts = [];
    for (let e = el; e && e.nodeType === 1 && e !== document.documentElement; e = e.parentElement) {
      if (e.id && /^[A-Za-z][\w-]*$/.test(e.id)) { parts.unshift('#' + e.id); break; }
      let s = e.localName;
      const tid = e.getAttribute('data-testid');
      if (tid) { parts.unshift(`${s}[data-testid="${tid}"]`); break; }
      const cls = [...e.classList].filter(c => /^[A-Za-z_-][\w-]*$/.test(c) && c.length < 30).slice(0, 2);
      if (cls.length) s += '.' + cls.join('.');
      const p = e.parentElement;
      if (p) { const same = [...p.children].filter(c => c.localName === e.localName); if (same.length > 1) s += `:nth-of-type(${same.indexOf(e) + 1})`; }
      parts.unshift(s);
      try { if (document.querySelectorAll(parts.join(' > ')).length === 1) break; } catch (_) {}
    }
    return parts.join(' > ');
  };
  const label = (el) => {
    const r = el.getBoundingClientRect(), c = comp(el);
    let s = el.localName + (el.id ? '#' + el.id : '') + [...el.classList].slice(0, 2).map(c => '.' + c).join('');
    return `${c.names[0] ? '<' + c.names[0] + '> ' : ''}${s}  ${Math.round(r.width)}×${Math.round(r.height)}`;
  };
  let cur = null;
  const place = () => {
    if (!cur) return;
    const r = cur.getBoundingClientRect(), cs = getComputedStyle(cur);
    Object.assign(box.style, { display: 'block', left: r.left + 'px', top: r.top + 'px', width: r.width + 'px', height: r.height + 'px' });
    const ml = parseFloat(cs.marginLeft) || 0, mt = parseFloat(cs.marginTop) || 0, mr = parseFloat(cs.marginRight) || 0, mb = parseFloat(cs.marginBottom) || 0;
    Object.assign(pad.style, { display: (ml || mt || mr || mb) ? 'block' : 'none', left: (r.left - ml) + 'px', top: (r.top - mt) + 'px', width: (r.width + ml + mr) + 'px', height: (r.height + mt + mb) + 'px' });
    tag.textContent = label(cur);
    tag.style.display = 'block';
    const th = 22, above = r.top - th - 6;
    tag.style.left = Math.max(4, Math.min(r.left, innerWidth - tag.offsetWidth - 4)) + 'px';
    tag.style.top = (above > 4 ? above : Math.min(r.bottom + 6, innerHeight - th - 4)) + 'px';
  };
  const move = (e) => {
    const el = document.elementFromPoint(e.clientX, e.clientY);
    if (!el || ours.has(el) || el === cur) return;
    cur = el; place();
  };
  const block = (e) => { e.preventDefault(); e.stopPropagation(); e.stopImmediatePropagation(); };
  const describe = (el) => {
    const r = el.getBoundingClientRect(), cs = getComputedStyle(el), c = comp(el);
    const keys = ['display','position','width','height','margin','padding','color','background-color','background-image','font-family','font-size','font-weight','line-height','text-align','border','border-radius','box-shadow','gap','flex-direction','justify-content','align-items','grid-template-columns','opacity','z-index','overflow'];
    const def = { 'background-image': 'none', 'box-shadow': 'none', opacity: '1', 'z-index': 'auto', overflow: 'visible', gap: 'normal', 'grid-template-columns': 'none', position: 'static' };
    const styles = keys.map(k => [k, cs.getPropertyValue(k)]).filter(([k, v]) => v && def[k] !== v && !(k === 'border' && /^0px/.test(v)) && !(k === 'margin' && v === '0px') && !(k === 'padding' && v === '0px') && !(k === 'border-radius' && v === '0px')).map(([k, v]) => `${k}: ${v}`);
    const attrs = [...el.attributes].filter(a => /^(aria-|data-|role$|href$|type$|name$|alt$|title$|placeholder$)/.test(a.name)).map(a => `${a.name}="${a.value.slice(0, 80)}"`);
    let html = el.outerHTML.replace(/\s+/g, ' ');
    if (html.length > 1200) html = html.slice(0, 1200) + '…';
    const zoom = (() => { const z = outerWidth / innerWidth; return z > 0.25 && z < 5 ? z : 1; })();
    const chrome = outerHeight - innerHeight * zoom;
    const x0 = Math.max(0, r.left - 6), y0 = Math.max(0, r.top - 6), x1 = Math.min(innerWidth, r.right + 6), y1 = Math.min(innerHeight, r.bottom + 6);
    return {
      url: location.href, title: document.title, selector: sel(el), tag: el.localName,
      text: (el.innerText || el.value || '').trim().replace(/\s+/g, ' ').slice(0, 200),
      components: c.names, source: c.source, styles, attrs, html,
      rect: { x: Math.round(r.left), y: Math.round(r.top), w: Math.round(r.width), h: Math.round(r.height) },
      viewport: { w: innerWidth, h: innerHeight, dpr: devicePixelRatio },
      shot: x1 > x0 && y1 > y0 ? { x: Math.round(screenX + x0 * zoom), y: Math.round(screenY + chrome + y0 * zoom), w: Math.round((x1 - x0) * zoom), h: Math.round((y1 - y0) * zoom) } : null,
    };
  };
  const finish = (r) => { cleanup(); window.__jtPick = JSON.stringify(r); };
  const click = (e) => { block(e); if (cur) finish(describe(cur)); };
  const key = (e) => { if (e.key === 'Escape') { block(e); finish({ cancelled: true }); } };
  const opts = { capture: true, passive: false };
  const evs = [['mousemove', move], ['click', click], ['mousedown', block], ['mouseup', block], ['pointerdown', block], ['pointerup', block], ['auxclick', block], ['contextmenu', block], ['keydown', key], ['scroll', place], ['resize', place]];
  const cleanup = () => { evs.forEach(([t, f]) => window.removeEventListener(t, f, opts)); ours.forEach(d => d.remove()); delete window.__jtPickCleanup; };
  evs.forEach(([t, f]) => window.addEventListener(t, f, opts));
  window.__jtPickCleanup = () => { cleanup(); window.__jtPick = JSON.stringify({ cancelled: true }); };
  return 'ok';
})()"#;

/// A picked element, ready to hand to Claude.
pub struct Pick {
    /// Markdown context (page, selector, component, styles, HTML).
    pub summary: String,
    /// Screenshot of the element, when Screen Recording is allowed.
    pub shot: Option<PathBuf>,
}

/// Cancel a running `pick_element` (removes the overlay from the page).
pub fn cancel_pick() {
    PICK_CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn js_in(target: &str, code: &str) -> Result<String, String> {
    let script = format!("on run argv\n tell application \"{APP}\" to return execute {target} javascript (item 1 of argv)\nend run");
    osascript(&script, &[code]).map_err(|e| {
        if e.contains("JavaScript") || e.contains("javascript") {
            format!("Brave blocks JavaScript from AppleScript. Enable Brave menu › View › Developer › Allow JavaScript from Apple Events. ({e})")
        } else {
            e
        }
    })
}

/// Bring juan-terminal back to the front (works from the MCP helper process too).
pub fn activate_self() {
    let _ = Command::new("osascript").args(["-e", "tell application id \"dev.juan-terminal.app\" to activate"]).output();
}

/// Let the user pick an element in the active Brave tab (blocks until click, Esc, cancel or
/// `timeout`). `Ok(None)` = cancelled.
pub fn pick_element(timeout: Duration) -> Result<Option<Pick>, String> {
    use std::sync::atomic::Ordering;
    ensure_running()?;
    PICK_CANCEL.store(false, Ordering::SeqCst);
    let ids = osascript(&format!("tell application \"{APP}\"\n if (count of windows) = 0 then error \"Brave has no open windows.\"\n return ((id of front window) as text) & \",\" & ((id of active tab of front window) as text)\nend tell"), &[])?;
    let (w, t) = ids.split_once(',').ok_or("Couldn't read the active Brave tab.")?;
    let target = format!("tab id {t} of window id {w}");
    js_in(&target, PICK_JS)?;
    let _ = osascript(&format!("tell application \"{APP}\" to activate"), &[]);

    let start = Instant::now();
    let raw = loop {
        std::thread::sleep(Duration::from_millis(150));
        if PICK_CANCEL.load(Ordering::SeqCst) || start.elapsed() > timeout {
            let _ = js_in(&target, "window.__jtPickCleanup && window.__jtPickCleanup(); 'ok'");
            return Ok(None);
        }
        match js_in(&target, "window.__jtPick || ''") {
            Ok(s) if !s.is_empty() => break s,
            Ok(_) => {}
            // Tab closed or navigated away from: give up.
            Err(_) => return Ok(None),
        }
    };
    let _ = js_in(&target, "delete window.__jtPick; 'ok'");
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("bad pick result: {e}"))?;
    if v.get("cancelled").and_then(|c| c.as_bool()) == Some(true) {
        return Ok(None);
    }
    let shot = v.get("shot").filter(|s| !s.is_null()).and_then(|s| {
        let n = |k: &str| s.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        let (x, y, w, h) = (n("x"), n("y"), n("w"), n("h"));
        if w < 1 || h < 1 {
            return None;
        }
        let dir = std::env::temp_dir().join("juan-terminal-shots");
        std::fs::create_dir_all(&dir).ok()?;
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let path = dir.join(format!("pick-{ts}.png"));
        // The overlay is gone already; give the page a frame to repaint.
        std::thread::sleep(Duration::from_millis(120));
        let ok = Command::new("screencapture").args(["-x", &format!("-R{x},{y},{w},{h}")]).arg(&path).status().is_ok_and(|s| s.success());
        (ok && path.exists()).then(|| {
            let _ = Command::new("sips").args(["-Z", "1600"]).arg(&path).output();
            path
        })
    });
    Ok(Some(Pick { summary: pick_summary(&v, shot.as_deref()), shot }))
}

fn pick_summary(v: &Value, shot: Option<&std::path::Path>) -> String {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let list = |k: &str| v.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
    let n = |a: &str, b: &str| v.pointer(&format!("/{a}/{b}")).and_then(|x| x.as_f64()).unwrap_or(0.);
    let mut out = format!("Element selected in Brave: <{}>", s("tag"));
    let text = s("text");
    if !text.is_empty() {
        out.push_str(&format!(" \"{text}\""));
    }
    out.push_str(&format!("\n- page: {} ({})", s("url"), s("title")));
    out.push_str(&format!("\n- selector: {}", s("selector")));
    let comps = list("components");
    if !comps.is_empty() {
        out.push_str(&format!("\n- component: {}", comps.join(" ← ")));
    }
    if let Some(src) = v.get("source").and_then(|x| x.as_str()) {
        out.push_str(&format!("\n- source: {src}"));
    }
    out.push_str(&format!(
        "\n- box: {}×{} at ({}, {}) in a {}×{} viewport (dpr {})",
        n("rect", "w"),
        n("rect", "h"),
        n("rect", "x"),
        n("rect", "y"),
        n("viewport", "w"),
        n("viewport", "h"),
        n("viewport", "dpr")
    ));
    let attrs = list("attrs");
    if !attrs.is_empty() {
        out.push_str(&format!("\n- attributes: {}", attrs.join(" ")));
    }
    let styles = list("styles");
    if !styles.is_empty() {
        out.push_str(&format!("\n- computed styles: {}", styles.join("; ")));
    }
    out.push_str(&format!("\n- html: {}", s("html")));
    if let Some(p) = shot {
        out.push_str(&format!("\n- screenshot: {}", p.display()));
    }
    out
}

/// Put a PNG on the clipboard (Claude Code reads it on Ctrl-V).
pub fn png_to_clipboard(path: &std::path::Path) -> bool {
    let script = "on run argv\n set the clipboard to (read (POSIX file (item 1 of argv)) as «class PNGf»)\nend run";
    Command::new("osascript").arg("-e").arg(script).arg(path).status().is_ok_and(|s| s.success())
}
