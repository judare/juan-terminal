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
    fn GetEventParameter(event: EventRef, name: u32, desired_type: u32, actual_type: *mut u32, buffer_size: usize, actual_size: *mut usize, data: *mut c_void) -> OSStatus;
}

const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");
const SIGNATURE: u32 = u32::from_be_bytes(*b"jtlc");

pub const CMD_KEY: u32 = 1 << 8;
pub const SHIFT_KEY: u32 = 1 << 9;
pub const OPTION_KEY: u32 = 1 << 11;
pub const CONTROL_KEY: u32 = 1 << 12;
pub const KVK_SPACE: u32 = 49;
pub const KVK_M: u32 = 46;
pub const KVK_LEFT: u32 = 123;
pub const KVK_RIGHT: u32 = 124;

/// One sender per registered hotkey; its index + 1 is the Carbon hotkey id.
static SENDERS: Mutex<Vec<UnboundedSender<()>>> = Mutex::new(Vec::new());
static INSTALL: std::sync::Once = std::sync::Once::new();
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_hotkey(_: EventHandlerCallRef, event: EventRef, _: *mut c_void) -> OSStatus {
    let mut id = EventHotKeyID { signature: 0, id: 0 };
    let st = unsafe {
        GetEventParameter(event, K_EVENT_PARAM_DIRECT_OBJECT, TYPE_EVENT_HOT_KEY_ID, std::ptr::null_mut(), std::mem::size_of::<EventHotKeyID>(), std::ptr::null_mut(), &mut id as *mut _ as *mut c_void)
    };
    if st != 0 || id.signature != SIGNATURE || id.id == 0 {
        return 0;
    }
    if let Some(tx) = SENDERS.lock().ok().and_then(|s| s.get(id.id as usize - 1).cloned()) {
        let _ = tx.unbounded_send(());
    }
    0
}

/// Register a system-wide hotkey (Carbon virtual key code + modifier mask). Each press
/// arrives as `()` on the returned channel (main thread).
pub fn register(key_code: u32, modifiers: u32, label: &str) -> Result<UnboundedReceiver<()>, String> {
    unsafe {
        let target = GetApplicationEventTarget();
        INSTALL.call_once(|| {
            let spec = EventTypeSpec { event_class: K_EVENT_CLASS_KEYBOARD, event_kind: K_EVENT_HOT_KEY_PRESSED };
            let mut handler: EventHandlerRef = std::ptr::null_mut();
            let st = InstallEventHandler(target, on_hotkey, 1, &spec, std::ptr::null_mut(), &mut handler);
            INSTALLED.store(st == 0, std::sync::atomic::Ordering::Relaxed);
        });
        if !INSTALLED.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("InstallEventHandler failed".into());
        }
        let (tx, rx) = unbounded();
        let id = {
            let mut senders = SENDERS.lock().map_err(|e| e.to_string())?;
            senders.push(tx);
            senders.len() as u32
        };
        let mut hk: EventHotKeyRef = std::ptr::null_mut();
        let st = RegisterEventHotKey(key_code, modifiers, EventHotKeyID { signature: SIGNATURE, id }, target, 0, &mut hk);
        if st != 0 {
            return Err(format!("RegisterEventHotKey({label}) failed ({st}); another app may own it"));
        }
        Ok(rx)
    }
}

/// Register ⌘Space. Each press arrives as `()` on the returned channel (main thread).
pub fn register_cmd_space() -> Result<UnboundedReceiver<()>, String> {
    register(KVK_SPACE, CMD_KEY, "⌘Space")
}
