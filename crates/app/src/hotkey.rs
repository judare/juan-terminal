//! System-wide hotkey (⌘Space by default) via Carbon's `RegisterEventHotKey`: works while
//! another app is frontmost, needs no Accessibility permission. Spotlight's own ⌘Space must be
//! turned off in System Settings › Keyboard › Keyboard Shortcuts › Spotlight.

use std::ffi::c_void;
use std::sync::Mutex;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventHandlerUPP = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(target: EventTargetRef, handler: EventHandlerUPP, num_types: usize, list: *const EventTypeSpec, user_data: *mut c_void, out: *mut EventHandlerRef) -> OSStatus;
    fn RegisterEventHotKey(key_code: u32, modifiers: u32, id: EventHotKeyID, target: EventTargetRef, options: u32, out: *mut EventHotKeyRef) -> OSStatus;
}

const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const CMD_KEY: u32 = 1 << 8;
const KVK_SPACE: u32 = 49;

static SENDER: Mutex<Option<UnboundedSender<()>>> = Mutex::new(None);

extern "C" fn on_hotkey(_: EventHandlerCallRef, _: EventRef, _: *mut c_void) -> OSStatus {
    if let Some(tx) = SENDER.lock().ok().and_then(|s| s.clone()) {
        let _ = tx.unbounded_send(());
    }
    0
}

/// Register ⌘Space. Each press arrives as `()` on the returned channel (main thread).
pub fn register_cmd_space() -> Result<UnboundedReceiver<()>, String> {
    let (tx, rx) = unbounded();
    *SENDER.lock().map_err(|e| e.to_string())? = Some(tx);
    unsafe {
        let target = GetApplicationEventTarget();
        let spec = EventTypeSpec { event_class: K_EVENT_CLASS_KEYBOARD, event_kind: K_EVENT_HOT_KEY_PRESSED };
        let mut handler: EventHandlerRef = std::ptr::null_mut();
        let st = InstallEventHandler(target, on_hotkey, 1, &spec, std::ptr::null_mut(), &mut handler);
        if st != 0 {
            return Err(format!("InstallEventHandler failed ({st})"));
        }
        let mut hk: EventHotKeyRef = std::ptr::null_mut();
        let id = EventHotKeyID { signature: u32::from_be_bytes(*b"jtlc"), id: 1 };
        let st = RegisterEventHotKey(KVK_SPACE, CMD_KEY, id, target, 0, &mut hk);
        if st != 0 {
            return Err(format!("RegisterEventHotKey(⌘Space) failed ({st}); another app may own it"));
        }
    }
    Ok(rx)
}
