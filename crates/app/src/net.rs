//! Internet reachability, checked in the background so nothing in the app ever waits on it.
//! The UI shows a non-blocking "Sin conexión" notice while offline; WhatsApp reconnects when
//! the connection comes back.

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static ONLINE: AtomicBool = AtomicBool::new(true);

pub fn online() -> bool {
    ONLINE.load(Ordering::Relaxed)
}

/// Any of a few well-known anycast endpoints answering a TCP handshake = online.
fn probe() -> bool {
    let targets: [SocketAddr; 3] = [([1, 1, 1, 1], 443).into(), ([8, 8, 8, 8], 443).into(), ([9, 9, 9, 9], 443).into()];
    targets.iter().any(|a| TcpStream::connect_timeout(a, Duration::from_secs(2)).is_ok())
}

pub fn init(cx: &mut gpui::App) {
    std::thread::Builder::new()
        .name("net-probe".into())
        .spawn(|| {
            loop {
                let up = probe();
                // Two misses in a row before going offline (one dropped SYN isn't an outage).
                let up = up || { std::thread::sleep(Duration::from_secs(1)); probe() };
                ONLINE.store(up, Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(if up { 10 } else { 3 }));
            }
        })
        .ok();
    cx.spawn(async move |cx| {
        let mut was = true;
        loop {
            gpui::Timer::after(Duration::from_secs(1)).await;
            let now = online();
            if now == was {
                continue;
            }
            was = now;
            let ok = cx.update(|cx| {
                if now && let Some(wa) = whatsapp::WhatsApp::try_global() {
                    wa.reconnect();
                }
                cx.refresh_windows();
            });
            if ok.is_err() {
                break;
            }
        }
    })
    .detach();
}
