//! Blocks sites inside Brave without touching DNS: a background thread watches Brave's tabs
//! (AppleScript) and sends any tab on a blocked domain to `about:blank`. It also injects a
//! small guard script into the active tab of each window that:
//! * blanks the page when its text has many adult keywords, whatever the URL (several hits of
//!   several different words, to avoid false positives) or an adult rating meta tag;
//! * on `brave_no_video_websites` (x.com) hides and stops every video (images stay).
//!
//! With the media plugin's `open_links` on, tabs on a YouTube video are closed and the link is
//! handed to Fragua's player instead (before any blocking). YouTube Music stays browsable even
//! when youtube.com is blocked: playing a song pauses it there, goes back and plays it in Fragua.
//!
//! Polls every second while Brave is frontmost, every 3 s otherwise, and does nothing (no
//! process spawned) while Brave isn't running. The guard needs Brave's
//! View › Developer › Allow JavaScript from Apple Events; URL blocking works without it.

#![allow(unexpected_cfgs)] // objc 0.2 macros

use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const APP: &str = "Brave Browser";
const BUNDLE_ID: &str = "com.brave.Browser";

#[derive(Clone, Default, PartialEq)]
pub struct Rules {
    /// Domains (subdomains included) whose tabs are blanked.
    pub blocked: Vec<String>,
    /// Domains whose videos are hidden.
    pub no_video: Vec<String>,
    /// Blank pages with many adult keywords, whatever the URL.
    pub adult_filter: bool,
}

static RULES: Mutex<Option<Rules>> = Mutex::new(None);
/// Where YouTube video links found in Brave go (the media player).
static HANDOFF: Mutex<Option<futures::channel::mpsc::UnboundedSender<String>>> = Mutex::new(None);
impl Rules {
    fn enforces(&self) -> bool {
        self.adult_filter || !self.blocked.is_empty() || !self.no_video.is_empty()
    }
}

static STARTED: AtomicBool = AtomicBool::new(false);
/// Brave refused to run the guard script (JavaScript from Apple Events is off).
static NO_JS: AtomicBool = AtomicBool::new(false);

/// Set the rules; the first call starts the watcher thread.
pub fn set_rules(rules: Rules) {
    *RULES.lock().unwrap() = Some(rules);
    start();
}

/// Send YouTube video links opened in Brave to `tx` (their tabs are closed).
pub fn set_youtube_handoff(tx: futures::channel::mpsc::UnboundedSender<String>) {
    *HANDOFF.lock().unwrap() = Some(tx);
    start();
}

fn start() {
    if !STARTED.swap(true, Ordering::SeqCst) {
        let _ = std::thread::Builder::new().name("brave-guard".into()).spawn(run);
    }
}

/// The player wants YouTube links right now.
fn handoff() -> Option<futures::channel::mpsc::UnboundedSender<String>> {
    let tx = HANDOFF.lock().unwrap().clone()?;
    crate::plugins::setting(crate::plugins::MEDIA, "open_links").then_some(tx)
}

/// True (once per change) when Brave has JavaScript from Apple Events turned off.
pub fn take_no_js_warning() -> bool {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let off = NO_JS.load(Ordering::Relaxed);
    if !off {
        WARNED.store(false, Ordering::Relaxed);
        return false;
    }
    !WARNED.swap(true, Ordering::Relaxed)
}

fn run() {
    loop {
        let (running, front) = brave_state();
        if running {
            let rules = RULES.lock().unwrap().clone().unwrap_or_default();
            let handoff = handoff();
            // Nothing to enforce (website blocker plugin off, no YouTube hand-off): don't touch
            // Brave at all.
            if rules.enforces() || handoff.is_some() {
                tick(&rules, handoff.as_ref());
            }
        }
        std::thread::sleep(Duration::from_millis(if front { 1000 } else { 3000 }));
    }
}

/// (Brave is running, Brave is the frontmost app), asked in-process (no subprocess).
fn brave_state() -> (bool, bool) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    type Id = *mut Object;
    objc::rc::autoreleasepool(|| unsafe {
        let c = std::ffi::CString::new(BUNDLE_ID).unwrap();
        let bid: Id = msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
        let apps: Id = msg_send![class!(NSRunningApplication), runningApplicationsWithBundleIdentifier: bid];
        let n: usize = if apps.is_null() { 0 } else { msg_send![apps, count] };
        if n == 0 {
            return (false, false);
        }
        let ws: Id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let front: Id = msg_send![ws, frontmostApplication];
        if front.is_null() {
            return (true, false);
        }
        let fid: Id = msg_send![front, bundleIdentifier];
        let is_front: bool = !fid.is_null() && msg_send![fid, isEqualToString: bid];
        (true, is_front)
    })
}

fn tick(rules: &Rules, handoff: Option<&futures::channel::mpsc::UnboundedSender<String>>) {
    // Only the YouTube hand-off: no guard script in pages.
    let js = if rules.enforces() { guard_js(&rules.no_video, rules.adult_filter) } else { String::new() };
    // One line per tab: window id, tab id, URL, guard result (active tabs only).
    let script = format!(
        r#"on run argv
  set js to item 1 of argv
  set sep to character id 9
  set out to ""
  tell application "{APP}"
    repeat with w in windows
      set wid to id of w
      set act to active tab index of w
      set ti to 0
      repeat with t in tabs of w
        set ti to ti + 1
        set u to URL of t
        set r to ""
        if js is not "" and ti = act and u starts with "http" then
          try
            set r to (execute t javascript js) as text
          on error
            set r to "nojs"
          end try
        end if
        set out to out & wid & sep & (id of t) & sep & u & sep & r & linefeed
      end repeat
    end repeat
  end tell
  return out
end run"#
    );
    let Ok(out) = Command::new("/usr/bin/osascript").arg("-e").arg(&script).arg(&js).output() else { return };
    if !out.status.success() {
        return;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    // "b:<window>/<tab>" to blank, "c:<window>/<tab>" to close (handed to the player).
    let mut act: Vec<String> = Vec::new();
    let mut no_js = false;
    for line in raw.lines() {
        let f: Vec<&str> = line.splitn(4, '\t').collect();
        let [w, t, url, res] = f[..] else { continue };
        no_js |= res == "nojs";
        let host = url_host(url);
        let ytm = handoff.is_some() && host.as_deref() == Some(YTM_HOST);
        if let Some((tx, (_, music))) = handoff.zip(crate::music::youtube_link(url)) {
            if first_handoff(url) {
                let _ = tx.unbounded_send(url.to_string());
            }
            // A YouTube Music tab stays open (back to where the song was picked).
            act.push(format!("{}:{w}/{t}", if music && ytm { "m" } else { "c" }));
            continue;
        }
        if (res == "adult" && rules.adult_filter) || (!ytm && host.is_some_and(|h| is_blocked(&h, &rules.blocked))) {
            act.push(format!("b:{w}/{t}"));
        }
    }
    NO_JS.store(no_js && !js.is_empty(), Ordering::Relaxed);
    if !act.is_empty() {
        // Tab ids (~2e9) overflow AppleScript integers (`tab id N` fails): match them as text.
        // Backwards, so closing a tab doesn't shift the ones still to visit.
        let script = format!(
            r#"on run argv
  tell application "{APP}"
    repeat with w in windows
      set wid to (id of w) as text
      repeat with i from (count of tabs of w) to 1 by -1
        set t to tab i of w
        set k to wid & "/" & ((id of t) as text)
        try
          if argv contains ("c:" & k) then
            close t
          else if argv contains ("m:" & k) then
            try
              execute t javascript "document.querySelectorAll('video,audio').forEach(v => v.pause()); history.back(); 'ok'"
            on error
              set URL of t to "https://{YTM_HOST}/"
            end try
          else if argv contains ("b:" & k) then
            set URL of t to "about:blank"
          end if
        end try
      end repeat
    end repeat
  end tell
end run"#
        );
        let _ = Command::new("/usr/bin/osascript").arg("-e").arg(&script).args(&act).output();
    }
}

const YTM_HOST: &str = "music.youtube.com";

/// False for a link already handed off in the last 15 s (its tab didn't close yet).
fn first_handoff(url: &str) -> bool {
    use std::time::Instant;
    static RECENT: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
    let mut recent = RECENT.lock().unwrap();
    recent.retain(|(_, at)| at.elapsed() < Duration::from_secs(15));
    if recent.iter().any(|(u, _)| u == url) {
        return false;
    }
    recent.push((url.to_string(), Instant::now()));
    true
}

/// Lowercase host of an http(s) URL.
fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    let host = auth.split(':').next()?.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// `host` is one of `domains` or a subdomain of one.
fn is_blocked(host: &str, domains: &[String]) -> bool {
    domains.iter().any(|d| host == d || host.strip_suffix(d.as_str()).is_some_and(|p| p.ends_with('.')))
}

fn guard_js(no_video: &[String], adult_filter: bool) -> String {
    GUARD_JS.replace("__NO_VIDEO__", &serde_json::to_string(no_video).unwrap_or_else(|_| "[]".into())).replace("__ADULT__", if adult_filter { "true" } else { "false" })
}

/// Returns "ok", or "adult" once the page was flagged (it blanks itself too). Idempotent: a
/// second injection only reports the state.
const GUARD_JS: &str = r#"(() => {
  if (window.__jtGuard) return window.__jtGuard.state;
  const g = window.__jtGuard = { state: 'ok' };
  const host = location.hostname.toLowerCase();
  const on = (list) => list.some(d => host === d || host.endsWith('.' + d));
  const blank = () => { g.state = 'adult'; try { window.stop(); } catch (_) {} location.replace('about:blank'); };

  // Adult keywords: blank when there are >= 6 hits of >= 3 different words.
  const WORDS = ['porn\\p{L}*', 'xxx', 'xvideos', 'xnxx', 'xhamster', 'hentai', 'nsfw', 'milfs?', 'blowjobs?', 'cumshots?',
    'creampies?', 'gangbangs?', 'bukkake', 'deepthroat\\p{L}*', 'handjobs?', 'camgirls?', 'onlyfans', 'nudes?', 'naked',
    'desnud[oa]s?', 'tetas', 'boobs', 'tits', 'pussy', 'horny', 'follando', 'cogiendo', 'masturba\\p{L}*', 'orgasm\\p{L}*',
    'squirt\\p{L}*', 'bdsm', 'fetish\\p{L}*', 'step ?(?:sis|sister|mom)', 'escorts?', 'sex ?tapes?', 'anal sex',
    'sexo (?:anal|oral|expl[ií]cito|en vivo)', 'webcam sex', 'nopor', 'chicas webcam', 'videos? (?:porno|xxx)'];
  const RE = new RegExp('(?<![\\p{L}\\p{N}])(' + WORDS.join('|') + ')(?![\\p{L}\\p{N}])', 'giu');
  const check = () => {
    if (g.state !== 'ok' || !__ADULT__) return;
    const rating = [...document.querySelectorAll('meta[name="rating" i],meta[http-equiv="rating" i]')].map(m => (m.content || '').toLowerCase()).join(' ');
    if (/rta-5042-1996-1400-1577-rta|\badult\b/.test(rating)) return blank();
    const meta = [...document.querySelectorAll('meta[name="description" i],meta[name="keywords" i]')].map(m => m.content || '').join(' ');
    const text = (document.title + ' ' + meta + ' ' + (document.body ? document.body.innerText : '')).slice(0, 400000);
    const seen = new Set();
    let total = 0;
    for (const m of text.matchAll(RE)) { seen.add(m[1].toLowerCase()); total++; }
    if (total >= 6 && seen.size >= 3) blank();
  };

  // No videos on these sites (images stay).
  const noVideo = on(__NO_VIDEO__);
  const HIDE = 'video,[data-testid="videoPlayer"],[data-testid="videoComponent"]';
  const stopVideos = () => {
    document.querySelectorAll(HIDE).forEach(el => {
      el.style.setProperty('display', 'none', 'important');
      if (el.tagName === 'VIDEO') { try { el.muted = true; el.pause(); el.preload = 'none'; } catch (_) {} }
    });
  };
  if (noVideo) {
    const st = document.createElement('style');
    st.textContent = HIDE + '{display:none!important}';
    (document.head || document.documentElement).appendChild(st);
    document.addEventListener('play', e => { const v = e.target; if (v && v.tagName === 'VIDEO') { v.muted = true; v.pause(); } }, true);
    stopVideos();
  }

  let pending = null;
  new MutationObserver(() => {
    if (noVideo) stopVideos();
    if (!pending) pending = setTimeout(() => { pending = null; check(); }, 2000);
  }).observe(document.documentElement, { childList: true, subtree: true, characterData: true });
  check();
  return g.state;
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_domains() {
        assert_eq!(url_host("https://m.YouTube.com:443/watch?v=1").as_deref(), Some("m.youtube.com"));
        assert_eq!(url_host("http://u:p@work.example./x").as_deref(), Some("work.example"));
        assert_eq!(url_host("about:blank"), None);
        let d = vec!["youtube.com".to_string(), "work.example".to_string()];
        assert!(is_blocked("youtube.com", &d));
        assert!(is_blocked("music.youtube.com", &d));
        assert!(is_blocked("app.work.example", &d));
        assert!(!is_blocked("notyoutube.com", &d));
        assert!(!is_blocked("work.example.evil.io", &d));
        assert!(guard_js(&["x.com".into()], true).contains(r#"on(["x.com"])"#));
    }
}
