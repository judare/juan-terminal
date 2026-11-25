//! macOS notifications posted by the app itself (UNUserNotificationCenter), so Notification
//! Center shows juan-terminal's icon instead of Script Editor's, with a WhatsApp / Claude image
//! attached. Outside the .app bundle (cargo run) or without permission it falls back to
//! `osascript`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub enum Source {
    WhatsApp,
    Claude,
}

impl Source {
    fn png(self) -> (&'static str, &'static [u8]) {
        match self {
            Source::WhatsApp => ("whatsapp", include_bytes!("../assets/notify/whatsapp.png")),
            Source::Claude => ("claude", include_bytes!("../assets/notify/claude.png")),
        }
    }
}

/// Set only once macOS confirms permission. Until then (still asking, refused, or an ad-hoc
/// signed build that never gets registered) everything goes through osascript, since the
/// center drops requests from unauthorized apps silently.
static AUTHORIZED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn notify(source: Source, title: &str, subtitle: &str, body: &str) {
    if !native_ready() || !AUTHORIZED.load(Ordering::Relaxed) || !post(source, title, subtitle, body) {
        osascript(title, subtitle, body);
    }
}

fn osascript(title: &str, subtitle: &str, message: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\" subtitle \"{}\" sound name \"Glass\"", esc(message), esc(title), esc(subtitle));
    let _ = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

#[link(name = "UserNotifications", kind = "framework")]
unsafe extern "C" {}

/// Running from a bundle with an identifier (required by UNUserNotificationCenter, which
/// throws otherwise). Sets the delegate and asks for permission once.
#[allow(unexpected_cfgs)] // objc 0.2 macros
fn native_ready() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| unsafe {
        use objc::runtime::Object;
        use objc::{class, msg_send, sel, sel_impl};
        let bundle: *mut Object = msg_send![class!(NSBundle), mainBundle];
        let ident: *mut Object = msg_send![bundle, bundleIdentifier];
        if ident.is_null() {
            return false;
        }
        let center: *mut Object = msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
        if center.is_null() {
            return false;
        }
        if let Some(delegate) = delegate() {
            let _: () = msg_send![center, setDelegate: delegate];
        }
        let handler = block::ConcreteBlock::new(|granted: objc::runtime::BOOL, _err: *mut Object| {
            AUTHORIZED.store(granted != objc::runtime::NO, Ordering::Relaxed);
        })
        .copy();
        // alert | sound | badge
        let _: () = msg_send![center, requestAuthorizationWithOptions: 7usize completionHandler: &*handler];
        true
    })
}

/// Delegate that shows banners even while the app is in front (the default hides them).
#[allow(unexpected_cfgs)]
fn delegate() -> Option<*mut objc::runtime::Object> {
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, Object, Sel};
    use objc::{class, msg_send, sel, sel_impl};
    extern "C" fn will_present(_: &Object, _: Sel, _center: *mut Object, _n: *mut Object, handler: *mut Object) {
        // banner | list | sound
        let handler = handler as *mut block::Block<(usize,), ()>;
        if !handler.is_null() {
            unsafe { (*handler).call(((1 << 4) | (1 << 3) | (1 << 1),)) };
        }
    }
    let cls: &Class = match Class::get("JTNotifyDelegate") {
        Some(c) => c,
        None => {
            let mut decl = ClassDecl::new("JTNotifyDelegate", class!(NSObject))?;
            unsafe {
                decl.add_method(
                    sel!(userNotificationCenter:willPresentNotification:withCompletionHandler:),
                    will_present as extern "C" fn(&Object, Sel, *mut Object, *mut Object, *mut Object),
                );
            }
            decl.register()
        }
    };
    // Kept for the life of the app (the center holds it weakly).
    let obj: *mut Object = unsafe { msg_send![cls, new] };
    (!obj.is_null()).then_some(obj)
}

#[allow(unexpected_cfgs)]
fn post(source: Source, title: &str, subtitle: &str, body: &str) -> bool {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    let ns = |s: &str| -> *mut Object {
        let c = std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    };
    // The attachment file is moved into the notification store: hand over a fresh copy.
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let (name, bytes) = source.png();
    let dir = workspace_state::state_dir().join("notify");
    let file = dir.join(format!("{name}-{stamp}-{seq}.png"));
    let have_image = std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&file, bytes).is_ok();
    objc::rc::autoreleasepool(|| unsafe {
        let center: *mut Object = msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
        if center.is_null() {
            return false;
        }
        let content: *mut Object = msg_send![class!(UNMutableNotificationContent), new];
        let _: () = msg_send![content, setTitle: ns(title)];
        if !subtitle.is_empty() {
            let _: () = msg_send![content, setSubtitle: ns(subtitle)];
        }
        let _: () = msg_send![content, setBody: ns(body)];
        let sound: *mut Object = msg_send![class!(UNNotificationSound), defaultSound];
        let _: () = msg_send![content, setSound: sound];
        if have_image {
            let url: *mut Object = msg_send![class!(NSURL), fileURLWithPath: ns(&file.to_string_lossy())];
            let mut err: *mut Object = std::ptr::null_mut();
            let nil: *mut Object = std::ptr::null_mut();
            let att: *mut Object = msg_send![class!(UNNotificationAttachment), attachmentWithIdentifier: ns(name) URL: url options: nil error: &mut err];
            if !att.is_null() {
                let arr: *mut Object = msg_send![class!(NSArray), arrayWithObject: att];
                let _: () = msg_send![content, setAttachments: arr];
            } else {
                let _ = std::fs::remove_file(&file);
            }
        }
        let nil: *mut Object = std::ptr::null_mut();
        let req: *mut Object = msg_send![class!(UNNotificationRequest), requestWithIdentifier: ns(&format!("jt-{stamp}-{seq}")) content: content trigger: nil];
        // Rejected (permission revoked meanwhile, …): show it through osascript instead.
        let (t, st, b) = (title.to_string(), subtitle.to_string(), body.to_string());
        let done = block::ConcreteBlock::new(move |err: *mut Object| {
            if !err.is_null() {
                AUTHORIZED.store(false, Ordering::Relaxed);
                osascript(&t, &st, &b);
            }
        })
        .copy();
        let _: () = msg_send![center, addNotificationRequest: req withCompletionHandler: &*done];
        let _: () = msg_send![content, release];
        true
    })
}
