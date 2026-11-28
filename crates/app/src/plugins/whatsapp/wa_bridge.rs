//! App-wide WhatsApp glue (one per process, not per window): reconnects at launch, macOS
//! notifications, the unread count on the Dock badge, and drafts of messages Claude wants to
//! send through the MCP tool (they go to the chat's composer, never straight out).
#![allow(unexpected_cfgs)] // objc 0.2 macros

use futures::StreamExt;
use gpui::App;
use whatsapp::{Update, WhatsApp};

/// Dock-badge owner id for WhatsApp's unread count (windows use their entity ids).
const DOCK_OWNER: u64 = u64::MAX - 7;

fn app_is_active() -> bool {
    use objc::runtime::{NO, Object};
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let active: objc::runtime::BOOL = msg_send![app, isActive];
        active != NO
    }
}

static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Unread count for the Dock badge (0 while the plugin or its Dock setting is off).
fn dock_unread() -> usize {
    if !crate::plugins::setting(crate::plugins::WHATSAPP, "dock_badge") {
        return 0;
    }
    WhatsApp::try_global().map(|wa| wa.store().total_unread() as usize).unwrap_or(0)
}

/// The WhatsApp plugin or its settings changed: repaint the Dock badge.
pub fn refresh_dock() {
    crate::claude_bridge::set_dock_count(DOCK_OWNER, dock_unread());
}

/// Idempotent: also called when the WhatsApp plugin is switched on.
pub fn init(cx: &mut App) {
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return refresh_dock();
    }
    // Only start the service when a device was linked before (no cost otherwise).
    let wa_dir = whatsapp::data_dir();
    if !wa_dir.join("session.db").exists() {
        return start_lazy(cx);
    }
    listen(cx);
}

/// Not linked yet: the service starts when the WhatsApp tab is opened; we hook in then.
fn start_lazy(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            gpui::Timer::after(std::time::Duration::from_secs(2)).await;
            if WhatsApp::try_global().is_some() {
                let _ = cx.update(listen);
                break;
            }
        }
    })
    .detach();
}

fn listen(cx: &mut App) {
    let wa = WhatsApp::global();
    let mut rx = wa.subscribe();
    refresh_dock();
    cx.spawn(async move |cx| {
        while let Some(u) = rx.next().await {
            match u {
                Update::Chats => refresh_dock(),
                Update::Notify { chat, title, body } => {
                    if crate::plugins::setting(crate::plugins::WHATSAPP, "notifications") && !(app_is_active() && crate::chat_view::is_chat_open(&chat)) {
                        crate::notify::notify(crate::notify::Source::WhatsApp, "WhatsApp", &title, &body);
                    }
                }
                Update::ConfirmSend(req) => {
                    // Never sent straight away: the message lands in the chat's composer, where
                    // the user reviews, edits and sends it.
                    let (id, jid, name) = (req.id, req.chat.clone(), req.chat_name.clone());
                    crate::chat_view::queue_draft(req);
                    WhatsApp::global().answer_send(id, whatsapp::SendAnswer::Drafted);
                    let _ = cx.update(|cx| {
                        let ws = cx
                            .active_window()
                            .and_then(|w| w.downcast::<crate::workspace::Workspace>())
                            .or_else(|| crate::workspace::workspace_windows(cx).into_iter().next());
                        if let Some(ws) = ws {
                            let _ = ws.update(cx, |w, window, cx| w.open_chat(&jid, &name, window, cx));
                        }
                    });
                }
                _ => {}
            }
        }
    })
    .detach();
}
