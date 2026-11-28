//! Clipboard history: watches the general pasteboard (its change count, polled twice a
//! second on the main thread: one integer read, no copying) and keeps the text copied, newest
//! first, in `<state>/clipboard.json`. Copies that password managers mark as concealed or
//! transient are skipped. Pinned entries (snippets) are never pruned.

#![allow(unexpected_cfgs)] // objc 0.2 macros

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::plugins::{self, CLIPBOARD};

/// Longest text kept (bigger copies are skipped: logs, whole files).
const MAX_TEXT: usize = 200_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Clip {
    pub text: String,
    /// Unix seconds of the last copy.
    pub at: i64,
    /// App in front when it was copied.
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub pinned: bool,
}

pub struct History {
    pub entries: Vec<Clip>,
    last_change: i64,
    _poll: Option<Task<()>>,
    save_task: Option<Task<()>>,
}

struct HistoryGlobal(Entity<History>);
impl Global for HistoryGlobal {}

pub fn global(cx: &App) -> Option<Entity<History>> {
    cx.try_global::<HistoryGlobal>().map(|g| g.0.clone())
}

fn file() -> PathBuf {
    workspace_state::state_dir().join("clipboard.json")
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Start watching (once; it stops by itself while the plugin is off).
pub fn init(cx: &mut App) {
    if cx.has_global::<HistoryGlobal>() {
        return;
    }
    let entries: Vec<Clip> = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let history = cx.new(|cx| {
        let mut h = History { entries, last_change: unsafe { change_count() }, _poll: None, save_task: None };
        h.prune();
        h._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let Ok(()) = this.update(cx, |h: &mut History, cx| h.poll(cx)) else { break };
            }
        }));
        h
    });
    cx.set_global(HistoryGlobal(history));
}

impl History {
    fn poll(&mut self, cx: &mut Context<Self>) {
        let count = unsafe { change_count() };
        if count == self.last_change {
            return;
        }
        self.last_change = count;
        if !plugins::enabled(CLIPBOARD) {
            return;
        }
        let Some((text, app)) = (unsafe { read_pasteboard() }) else { return };
        self.add(text, app, cx);
    }

    fn add(&mut self, text: String, app: String, cx: &mut Context<Self>) {
        if text.trim().is_empty() || text.len() > MAX_TEXT {
            return;
        }
        let pinned = match self.entries.iter().position(|e| e.text == text) {
            Some(ix) => self.entries.remove(ix).pinned,
            None => false,
        };
        self.entries.insert(0, Clip { text, at: now(), app, pinned });
        self.prune();
        self.save(cx);
        cx.notify();
    }

    /// Keep the configured number of entries / days (pinned ones always stay).
    fn prune(&mut self) {
        let max = plugins::choice_number(CLIPBOARD, "history_size").unwrap_or(500) as usize;
        let oldest = plugins::choice_number(CLIPBOARD, "history_days").map(|d| now() - d as i64 * 86_400);
        let mut kept = 0;
        self.entries.retain(|e| {
            if e.pinned {
                return true;
            }
            if oldest.is_some_and(|o| e.at < o) || kept >= max {
                return false;
            }
            kept += 1;
            true
        });
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.entries.clone();
        // Coalesce bursts of copies into one write.
        let exec = cx.background_executor().clone();
        self.save_task = Some(cx.background_executor().spawn(async move {
            exec.timer(Duration::from_millis(400)).await;
            let path = file();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, serde_json::to_vec(&snapshot).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }));
    }

    pub fn toggle_pin(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.text == text) {
            e.pinned = !e.pinned;
        }
        self.save(cx);
        cx.notify();
    }

    pub fn remove(&mut self, text: &str, cx: &mut Context<Self>) {
        self.entries.retain(|e| e.text != text);
        self.save(cx);
        cx.notify();
    }

    /// Delete the history; pinned entries stay.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.entries.retain(|e| e.pinned);
        self.save(cx);
        cx.notify();
    }

    /// Settings changed: apply the new limits.
    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let before = self.entries.len();
        self.prune();
        if self.entries.len() != before {
            self.save(cx);
            cx.notify();
        }
    }
}

/// Copy `text` and, when allowed (setting + Accessibility permission), paste it into the app
/// in front with a synthetic ⌘V.
pub fn paste(text: String, cx: &mut App) {
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    if !plugins::setting(CLIPBOARD, "paste_on_pick") || !accessibility_trusted() {
        return;
    }
    std::thread::spawn(|| {
        // Let the launcher close and the previous app take the keyboard back.
        std::thread::sleep(Duration::from_millis(120));
        unsafe { post_cmd_v() };
    });
}

// ---- macOS -------------------------------------------------------------------------------

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn CGEventCreateKeyboardEvent(source: *const std::ffi::c_void, key: u16, down: bool) -> *mut std::ffi::c_void;
    fn CGEventSetFlags(event: *mut std::ffi::c_void, flags: u64);
    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CFRelease(cf: *const std::ffi::c_void);
}

pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// System Settings › Privacy & Security › Accessibility.
pub fn open_accessibility_settings() {
    let _ = std::process::Command::new("/usr/bin/open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").spawn();
}

unsafe fn post_cmd_v() {
    const KVK_V: u16 = 9;
    const CMD: u64 = 1 << 20;
    const HID_TAP: u32 = 0;
    unsafe {
        for down in [true, false] {
            let e = CGEventCreateKeyboardEvent(std::ptr::null(), KVK_V, down);
            if e.is_null() {
                return;
            }
            CGEventSetFlags(e, CMD);
            CGEventPost(HID_TAP, e);
            CFRelease(e);
        }
    }
}

unsafe fn general() -> *mut Object {
    unsafe { msg_send![class!(NSPasteboard), generalPasteboard] }
}

unsafe fn change_count() -> i64 {
    unsafe {
        let pb = general();
        if pb.is_null() {
            return 0;
        }
        msg_send![pb, changeCount]
    }
}

unsafe fn ns_string(s: &str) -> *mut Object {
    let c = std::ffi::CString::new(s).unwrap_or_default();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
}

unsafe fn rust_string(ns: *mut Object) -> Option<String> {
    if ns.is_null() {
        return None;
    }
    unsafe {
        let p: *const std::os::raw::c_char = msg_send![ns, UTF8String];
        (!p.is_null()).then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

/// (text, app in front), or None for non-text or concealed copies (password managers).
unsafe fn read_pasteboard() -> Option<(String, String)> {
    unsafe {
        let pb = general();
        if pb.is_null() {
            return None;
        }
        let types: *mut Object = msg_send![pb, types];
        if types.is_null() {
            return None;
        }
        let n: usize = msg_send![types, count];
        for i in 0..n {
            let t: *mut Object = msg_send![types, objectAtIndex: i];
            let name = rust_string(t).unwrap_or_default();
            if name.starts_with("org.nspasteboard.Concealed") || name.starts_with("org.nspasteboard.Transient") || name.starts_with("org.nspasteboard.AutoGenerated") || name.starts_with("com.agilebits") {
                return None;
            }
        }
        let text = rust_string(msg_send![pb, stringForType: ns_string("public.utf8-plain-text")])?;
        let ws: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        let front: *mut Object = if ws.is_null() { std::ptr::null_mut() } else { msg_send![ws, frontmostApplication] };
        let app = if front.is_null() { String::new() } else { rust_string(msg_send![front, localizedName]).unwrap_or_default() };
        Some((text, app))
    }
}

/// "hace 5 min", "ayer"…
pub fn ago(t: i64) -> String {
    let d = (now() - t).max(0);
    match d {
        0..60 => "ahora".into(),
        60..3600 => format!("hace {} min", d / 60),
        3600..86_400 => format!("hace {} h", d / 3600),
        86_400..172_800 => "ayer".into(),
        _ => format!("hace {} días", d / 86_400),
    }
}
