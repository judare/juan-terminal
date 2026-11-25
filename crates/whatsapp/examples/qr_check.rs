//! Connects with a throwaway session dir and waits for the first pairing QR (no linking).
use futures::StreamExt;

fn main() {
    let wa = whatsapp::WhatsApp::global();
    wa.start();
    let mut rx = wa.subscribe();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
    futures::executor::block_on(async {
        while std::time::Instant::now() < deadline {
            match rx.next().await {
                Some(whatsapp::Update::Status(s)) => {
                    match &s {
                        whatsapp::Status::Qr(code) => {
                            println!("QR OK ({} chars, {} fields)", code.len(), code.split(',').count());
                            return;
                        }
                        other => println!("status: {other:?}"),
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        println!("no QR");
    });
}
