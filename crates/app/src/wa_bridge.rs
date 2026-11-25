//! App-wide WhatsApp glue (one per process, not per window): reconnects at launch, macOS
//! notifications, the unread count on the Dock badge, and the approval dialog for messages
//! Claude wants to send through the MCP tool.
#![allow(unexpected_cfgs)] // objc 0.2 macros

use futures::StreamExt;
use gpui::{App, PromptLevel};
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

pub fn init(cx: &mut App) {
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
    crate::claude_bridge::set_dock_count(DOCK_OWNER, wa.store().total_unread() as usize);
    cx.spawn(async move |cx| {
        while let Some(u) = rx.next().await {
            match u {
                Update::Chats => {
                    let n = WhatsApp::global().store().total_unread() as usize;
                    crate::claude_bridge::set_dock_count(DOCK_OWNER, n);
                }
                Update::Notify { chat, title, body } => {
                    if !(app_is_active() && crate::chat_view::is_chat_open(&chat)) {
                        crate::notify::notify(crate::notify::Source::WhatsApp, "WhatsApp", &title, &body);
                    }
                }
                Update::ConfirmSend(req) => {
                    let id = req.id;
                    let mut detail = format!("Para: {}\n", req.chat_name);
                    if !req.text.is_empty() {
                        detail.push_str(&format!("\n{}\n", req.text));
                    }
                    if !req.files.is_empty() {
                        detail.push_str("\nAdjuntos:\n");
                        for f in &req.files {
                            detail.push_str(&format!("• {}\n", f.display()));
                        }
                    }
                    let answer = cx
                        .update(|cx| {
                            let window = cx.active_window().or_else(|| cx.windows().into_iter().next())?;
                            window
                                .update(cx, |_, window, cx| {
                                    window.prompt(PromptLevel::Warning, "Claude quiere enviar un mensaje de WhatsApp", Some(&detail), &["Enviar", "No enviar"], cx)
                                })
                                .ok()
                        })
                        .ok()
                        .flatten();
                    // Wait off the update loop so other updates keep flowing meanwhile.
                    cx.background_executor()
                        .spawn(async move {
                            let approved = match answer {
                                Some(rx) => rx.await == Ok(0),
                                None => false,
                            };
                            WhatsApp::global().answer_send(id, approved);
                        })
                        .detach();
                }
                _ => {}
            }
        }
    })
    .detach();
}
