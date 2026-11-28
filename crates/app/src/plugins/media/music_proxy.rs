//! Loopback HTTP proxy between `AVPlayer` and googlevideo.
//!
//! googlevideo only serves byte ranges of up to 1 MiB per request (open-ended or larger ranges
//! get 403), while `AVPlayer` asks for `bytes=N-`. The proxy answers AVPlayer's ranges by
//! fetching 1 MiB chunks on demand (each downloaded whole, then handed over at AVPlayer's pace), so playback still starts after
//! the first chunk and only what is being played (plus AVPlayer's small read-ahead) is pulled.
//! When AVPlayer seeks or stops it closes the connection and the proxy stops fetching.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const CHUNK: u64 = 1024 * 1024;

fn debug(msg: impl FnOnce() -> String) {
    if std::env::var_os("JT_PROXY_DEBUG").is_some() {
        eprintln!("[proxy] {}", msg());
    }
}
/// First request of a response: small, so the first bytes reach AVPlayer sooner.
const FIRST_CHUNK: u64 = 256 * 1024;

#[derive(Clone)]
struct Upstream {
    url: String,
    mime: String,
    /// Total size, learned from the first `Content-Range`.
    len: Option<u64>,
}

struct Proxy {
    port: u16,
    streams: Mutex<HashMap<String, Upstream>>,
    agent: ureq::Agent,
}

static PROXY: OnceLock<Option<Proxy>> = OnceLock::new();

fn proxy() -> Option<&'static Proxy> {
    PROXY
        .get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").ok()?;
            let port = listener.local_addr().ok()?.port();
            std::thread::Builder::new()
                .name("music-proxy".into())
                .spawn(move || {
                    for conn in listener.incoming().flatten() {
                        let _ = std::thread::Builder::new().name("music-proxy-conn".into()).spawn(move || serve(conn));
                    }
                })
                .ok()?;
            let agent: ureq::Agent = ureq::Agent::config_builder().timeout_connect(Some(Duration::from_secs(10))).timeout_recv_body(Some(Duration::from_secs(20))).build().into();
            Some(Proxy { port, streams: Mutex::new(HashMap::new()), agent })
        })
        .as_ref()
}

/// Local URL that serves `url` (a googlevideo stream). `key` identifies it (track id + kind).
pub fn local_url(key: &str, url: &str, video: bool) -> Option<String> {
    let p = proxy()?;
    let token: String = key.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let mime = if video { "video/mp4" } else { "audio/mp4" };
    let mut streams = p.streams.lock().ok()?;
    if streams.len() > 64 {
        streams.clear();
    }
    // googlevideo puts the size in the URL (`clen=`): no probe round trip before the first byte.
    let len = url.split_once('?').and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("clen="))).and_then(|v| v.parse().ok());
    streams.insert(token.clone(), Upstream { url: url.to_string(), mime: mime.into(), len });
    Some(format!("http://127.0.0.1:{}/{}.{}", p.port, token, if video { "mp4" } else { "m4a" }))
}

fn serve(conn: TcpStream) {
    let Some(p) = proxy() else { return };
    let _ = conn.set_read_timeout(Some(Duration::from_secs(120)));
    let Ok(write_half) = conn.try_clone() else { return };
    let mut out = write_half;
    let mut reader = BufReader::new(conn);
    // Keep-alive: AVPlayer reuses connections for several ranges.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut range: Option<(u64, Option<u64>)> = None;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).ok().filter(|n| *n > 0).is_none() {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("range") {
                    range = parse_range(v.trim());
                }
            }
        }
        let token = path.trim_start_matches('/').split('.').next().unwrap_or("").to_string();
        let up = p.streams.lock().ok().and_then(|s| s.get(&token).cloned());
        let Some(mut up) = up else {
            let _ = out.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            continue;
        };
        if up.len.is_none() {
            match probe_len(&p.agent, &up.url) {
                Some(len) => {
                    up.len = Some(len);
                    if let Ok(mut s) = p.streams.lock() {
                        if let Some(u) = s.get_mut(&token) {
                            u.len = Some(len);
                        }
                    }
                }
                None => {
                    let _ = out.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
            }
        }
        let len = up.len.unwrap_or(0);
        if len == 0 {
            let _ = out.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
            return;
        }
        let (start, end, partial) = match range {
            Some((s, e)) if s < len => (s, e.unwrap_or(len - 1).min(len - 1), true),
            Some(_) => {
                let _ = out.write_all(format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\n\r\n").as_bytes());
                continue;
            }
            None => (0, len - 1, false),
        };
        debug(|| format!("{method} {token} range={range:?} → {start}-{end}/{len}"));
        let mut head = String::new();
        head.push_str(if partial { "HTTP/1.1 206 Partial Content\r\n" } else { "HTTP/1.1 200 OK\r\n" });
        head.push_str(&format!("Content-Type: {}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: keep-alive\r\n", up.mime, end - start + 1));
        if partial {
            head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
        }
        head.push_str("\r\n");
        if out.write_all(head.as_bytes()).is_err() {
            return;
        }
        if method == "HEAD" {
            continue;
        }
        let ok = pipe(&p.agent, &up.url, start, end, &mut out);
        debug(|| format!("  {start}-{end} done ok={ok}"));
        if !ok {
            // Client went away (seek / stop) or upstream failed: drop the connection.
            return;
        }
    }
}

fn parse_range(v: &str) -> Option<(u64, Option<u64>)> {
    let spec = v.strip_prefix("bytes=")?;
    let (a, b) = spec.split(',').next()?.split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end = b.trim();
    Some((start, if end.is_empty() { None } else { end.parse().ok() }))
}

fn probe_len(agent: &ureq::Agent, url: &str) -> Option<u64> {
    for _ in 0..2 {
        if let Ok(resp) = agent.get(url).header("Range", "bytes=0-0").call() {
            let total = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit('/').next()).and_then(|t| t.trim().parse().ok());
            if total.is_some() {
                return total;
            }
        }
    }
    None
}

/// Stream `start..=end` to `out` in ≤1 MiB upstream requests. Each chunk is downloaded whole
/// first (well under a second) and then written at whatever pace AVPlayer reads, so a slow
/// reader never stalls an upstream connection into a timeout. False if the client hung up or
/// upstream kept failing.
fn pipe(agent: &ureq::Agent, url: &str, start: u64, end: u64, out: &mut TcpStream) -> bool {
    let mut pos = start;
    let mut first = true;
    while pos <= end {
        let size = if std::mem::take(&mut first) { FIRST_CHUNK } else { CHUNK };
        let chunk_end = (pos + size - 1).min(end);
        let Some(data) = fetch_chunk(agent, url, pos, chunk_end) else {
            debug(|| format!("  fetch {pos}-{chunk_end} FAILED"));
            return false;
        };
        if out.write_all(&data).is_err() {
            return false;
        }
        pos = chunk_end + 1;
    }
    let _ = out.flush();
    true
}

fn fetch_chunk(agent: &ureq::Agent, url: &str, from: u64, to: u64) -> Option<Vec<u8>> {
    let want = (to - from + 1) as usize;
    let mut data: Vec<u8> = Vec::with_capacity(want);
    for attempt in 0..4 {
        let at = from + data.len() as u64;
        let r = agent.get(url).header("Range", &format!("bytes={at}-{to}")).call();
        if let Err(e) = &r {
            debug(|| format!("  upstream {at}-{to}: {e}"));
        }
        if let Ok(resp) = r {
            let mut body = resp.into_body().into_reader();
            let _ = body.read_to_end(&mut data);
            if data.len() >= want {
                data.truncate(want);
                return Some(data);
            }
        }
        std::thread::sleep(Duration::from_millis(200 * (attempt + 1)));
    }
    None
}

/// Download a whole stream to `path` (via `path.part`, renamed when complete), in the same
/// ≤1 MiB chunks. Used to keep songs / podcasts / videos for offline playback.
pub fn download_to(url: &str, path: &std::path::Path) -> bool {
    download_with_progress(url, path, |_, _| true)
}

/// Paths being downloaded right now: the same file is never written twice at once (playing a
/// podcast caches it while a "download" click may fetch it too).
static IN_FLIGHT: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// [`download_to`] reporting `(done, total)` bytes after each chunk; `on_progress` returning
/// false cancels it. If another download of `path` is running, waits for it instead.
pub fn download_with_progress(url: &str, path: &std::path::Path, mut on_progress: impl FnMut(u64, u64) -> bool) -> bool {
    loop {
        if path.exists() {
            return true;
        }
        {
            let mut busy = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            if !busy.iter().any(|p| p == path) {
                busy.push(path.to_path_buf());
                break;
            }
        }
        if !on_progress(0, 0) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let ok = download_inner(url, path, &mut on_progress);
    IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| p != path);
    ok
}

fn download_inner(url: &str, path: &std::path::Path, on_progress: &mut impl FnMut(u64, u64) -> bool) -> bool {
    let Some(p) = proxy() else { return false };
    let len = url.split_once('?').and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("clen="))).and_then(|v| v.parse::<u64>().ok()).or_else(|| probe_len(&p.agent, url));
    let Some(len) = len.filter(|l| *l > 0) else { return false };
    let part = path.with_extension("part");
    let Ok(mut file) = std::fs::File::create(&part) else { return false };
    let mut pos = 0;
    while pos < len {
        if !on_progress(pos, len) {
            let _ = std::fs::remove_file(&part);
            return false;
        }
        let end = (pos + CHUNK - 1).min(len - 1);
        let Some(data) = fetch_chunk(&p.agent, url, pos, end) else {
            let _ = std::fs::remove_file(&part);
            return false;
        };
        if file.write_all(&data).is_err() {
            let _ = std::fs::remove_file(&part);
            return false;
        }
        pos = end + 1;
    }
    on_progress(len, len);
    drop(file);
    std::fs::rename(&part, path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-1"), Some((0, Some(1))));
        assert_eq!(parse_range("bytes=500-"), Some((500, None)));
        assert_eq!(parse_range("items=0-1"), None);
    }

    /// Manual: `JT_PROXY_URL=<googlevideo url> cargo test -p juan-terminal proxy_live -- --ignored --nocapture`,
    /// then play the printed URL (AVPlayer / ffplay) within 60 s.
    #[test]
    #[ignore]
    fn proxy_live() {
        let url = std::env::var("JT_PROXY_URL").expect("JT_PROXY_URL");
        let local = super::local_url("test-a", &url, false).expect("proxy");
        println!("LOCAL {local}");
        std::fs::write(std::env::var("JT_PROXY_OUT").unwrap_or("/tmp/jt-proxy-url".into()), &local).ok();
        std::thread::sleep(std::time::Duration::from_secs(std::env::var("JT_PROXY_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60)));
    }
}
