//! Google data for the plugin: today's and tomorrow's events of the calendars shown in Google
//! Calendar, and unread mail in the inbox. Synced every 2 minutes while connected (one global
//! entity for the whole app); a 20 s clock posts "starts in N min" notifications.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use gpui::{App, AppContext, Context, Entity, Global, Task};
use serde_json::Value;

use super::auth;
use crate::plugins::{self, GOOGLE};

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: String,
    pub title: String,
    /// Unix seconds.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    /// Meet / Zoom / Teams link.
    pub meeting: Option<String>,
    pub link: String,
    pub location: String,
    pub calendar: String,
    /// Calendar color (#rrggbb as u32).
    pub color: u32,
}

impl Event {
    pub fn is_now(&self, now: i64) -> bool {
        !self.all_day && self.start <= now && now < self.end
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mail {
    pub id: String,
    pub thread: String,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub at: i64,
}

pub struct Google {
    pub events: Vec<Event>,
    pub mails: Vec<Mail>,
    pub unread: u64,
    pub syncing: bool,
    pub error: Option<String>,
    pub last_sync: Option<i64>,
    notified: HashSet<String>,
    seen_mail: Option<HashSet<String>>,
    sync_task: Option<Task<()>>,
    _clock: Option<Task<()>>,
}

struct GoogleGlobal(Entity<Google>);
impl Global for GoogleGlobal {}

pub fn global(cx: &App) -> Option<Entity<Google>> {
    cx.try_global::<GoogleGlobal>().map(|g| g.0.clone())
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Start the sync loop (once).
pub fn init(cx: &mut App) {
    if cx.has_global::<GoogleGlobal>() {
        return;
    }
    let g = cx.new(|cx| {
        let mut g = Google { events: Vec::new(), mails: Vec::new(), unread: 0, syncing: false, error: None, last_sync: None, notified: HashSet::new(), seen_mail: None, sync_task: None, _clock: None };
        g._clock = Some(cx.spawn(async move |this, cx| {
            let mut ticks: u64 = 0;
            loop {
                let Ok(()) = this.update(cx, |g: &mut Google, cx| {
                    // Every 2 min (6 × 20 s), plus right away at start.
                    if ticks % 6 == 0 {
                        g.sync(cx);
                    }
                    g.remind(cx);
                }) else {
                    break;
                };
                ticks += 1;
                cx.background_executor().timer(Duration::from_secs(20)).await;
            }
        }));
        g
    });
    cx.set_global(GoogleGlobal(g));
}

impl Google {
    /// Fetch events and mail now (no-op while one is running, offline or disconnected).
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        if self.syncing || !plugins::enabled(GOOGLE) || !auth::connected() || !crate::net::online() {
            return;
        }
        self.syncing = true;
        cx.notify();
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async { (fetch_events(), fetch_mail()) }).await;
            let _ = this.update(cx, |g, cx| {
                g.syncing = false;
                g.last_sync = Some(now());
                let (events, mail) = res;
                let mut errors = Vec::new();
                match events {
                    Ok(e) => g.events = e,
                    Err(e) => errors.push(format!("Calendario: {e:#}")),
                }
                match mail {
                    Ok((unread, mails)) => {
                        g.notify_new_mail(&mails);
                        g.unread = unread;
                        g.mails = mails;
                    }
                    Err(e) => errors.push(format!("Gmail: {e:#}")),
                }
                g.error = (!errors.is_empty()).then(|| errors.join(" · "));
                cx.notify();
            });
        }));
    }

    /// Signed out: forget the data.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.events.clear();
        self.mails.clear();
        self.unread = 0;
        self.error = None;
        self.last_sync = None;
        self.seen_mail = None;
        cx.notify();
    }

    /// "Starts in N min" for each meeting, once.
    fn remind(&mut self, cx: &mut Context<Self>) {
        let Some(minutes) = plugins::choice_number(GOOGLE, "notify_minutes").filter(|_| plugins::enabled(GOOGLE)) else { return };
        let t = now();
        for e in &self.events {
            if e.all_day || self.notified.contains(&e.id) {
                continue;
            }
            let left = e.start - t;
            if left <= minutes as i64 * 60 && left > -60 {
                self.notified.insert(e.id.clone());
                let when = if left <= 30 { "Empieza ahora".to_string() } else { format!("Empieza en {} min", (left + 59) / 60) };
                let body = match &e.meeting {
                    Some(_) => "Únete desde el launcher (⌘Space › meet) o la pestaña Agenda".to_string(),
                    None => e.location.clone(),
                };
                crate::notify::notify(crate::notify::Source::Plain, &e.title, &format!("{when} · {}", fmt_time(e.start)), &body);
            }
        }
        // Keep the set small.
        let ids: HashSet<&String> = self.events.iter().map(|e| &e.id).collect();
        self.notified.retain(|id| ids.contains(id));
        let _ = cx;
    }

    fn notify_new_mail(&mut self, mails: &[Mail]) {
        let ids: HashSet<String> = mails.iter().map(|m| m.id.clone()).collect();
        if let Some(seen) = &self.seen_mail {
            if plugins::setting(GOOGLE, "mail_notifications") {
                for m in mails.iter().filter(|m| !seen.contains(&m.id)).take(3) {
                    crate::notify::notify(crate::notify::Source::Plain, &m.from, &m.subject, &m.snippet);
                }
            }
        }
        self.seen_mail = Some(ids);
    }

    /// The meeting happening now, else the next one today (not all-day).
    pub fn next_meeting(&self) -> Option<&Event> {
        let t = now();
        let end_of_day = end_of_today();
        self.events.iter().filter(|e| !e.all_day && e.end > t && e.start < end_of_day).min_by_key(|e| e.start)
    }
}

pub fn fmt_time(t: i64) -> String {
    Local.timestamp_opt(t, 0).single().map(|d| d.format("%-I:%M %p").to_string().to_lowercase()).unwrap_or_default()
}

/// "en 12 min", "ahora", "hace 5 min".
pub fn fmt_until(start: i64, end: i64) -> String {
    let t = now();
    if start <= t && t < end {
        let left = (end - t + 59) / 60;
        return format!("ahora · quedan {left} min");
    }
    let d = start - t;
    match d {
        ..=0 => "terminó".into(),
        1..3600 => format!("en {} min", (d + 59) / 60),
        _ => format!("en {} h {} min", d / 3600, (d % 3600) / 60),
    }
}

fn start_of_today() -> i64 {
    let today = Local::now().date_naive();
    Local.from_local_datetime(&today.and_hms_opt(0, 0, 0).unwrap_or_default()).earliest().map(|d| d.timestamp()).unwrap_or_else(now)
}

pub fn end_of_today() -> i64 {
    start_of_today() + 86_400
}

fn parse_time(v: &Value) -> Option<(i64, bool)> {
    if let Some(dt) = v["dateTime"].as_str() {
        return DateTime::parse_from_rfc3339(dt).ok().map(|d| (d.timestamp(), false));
    }
    let date = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    let local = Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest()?;
    Some((local.timestamp(), true))
}

fn hex_color(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0x4285f4)
}

/// A video call link in the event: Meet, else a Zoom / Teams URL in the location or notes.
fn meeting_link(e: &Value) -> Option<String> {
    if let Some(l) = e["hangoutLink"].as_str() {
        return Some(l.to_string());
    }
    if let Some(points) = e["conferenceData"]["entryPoints"].as_array() {
        if let Some(u) = points.iter().find(|p| p["entryPointType"] == "video").and_then(|p| p["uri"].as_str()) {
            return Some(u.to_string());
        }
    }
    let text = format!("{} {}", e["location"].as_str().unwrap_or(""), e["description"].as_str().unwrap_or(""));
    text.split(|c: char| c.is_whitespace() || c == '"' || c == '<' || c == '>')
        .find(|w| w.starts_with("https://") && (w.contains("zoom.us/j/") || w.contains("teams.microsoft.com/l/meetup-join") || w.contains("meet.google.com/")))
        .map(|w| w.trim_end_matches(['.', ',', ')']).to_string())
}

/// Events from now-ish (start of today) to the end of tomorrow, all visible calendars.
pub fn fetch_events() -> Result<Vec<Event>> {
    let list = auth::get_json("https://www.googleapis.com/calendar/v3/users/me/calendarList?minAccessRole=reader")?;
    let from = start_of_today();
    let to = from + 2 * 86_400;
    let iso = |t: i64| chrono::Utc.timestamp_opt(t, 0).single().map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).unwrap_or_default();
    let mut out = Vec::new();
    for cal in list["items"].as_array().into_iter().flatten() {
        if cal["selected"].as_bool() != Some(true) && cal["primary"].as_bool() != Some(true) {
            continue;
        }
        let Some(id) = cal["id"].as_str() else { continue };
        let name = cal["summaryOverride"].as_str().or(cal["summary"].as_str()).unwrap_or("").to_string();
        let color = hex_color(cal["backgroundColor"].as_str().unwrap_or("#4285f4"));
        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/{}/events?singleEvents=true&orderBy=startTime&maxResults=100&timeMin={}&timeMax={}",
            auth::urlencode(id),
            auth::urlencode(&iso(from)),
            auth::urlencode(&iso(to))
        );
        let Ok(events) = auth::get_json(&url) else { continue };
        for e in events["items"].as_array().into_iter().flatten() {
            if e["status"] == "cancelled" {
                continue;
            }
            // Declined by me: not on my agenda.
            let declined = e["attendees"].as_array().is_some_and(|a| a.iter().any(|x| x["self"].as_bool() == Some(true) && x["responseStatus"] == "declined"));
            if declined {
                continue;
            }
            let (Some((start, all_day)), Some((end, _))) = (parse_time(&e["start"]), parse_time(&e["end"])) else { continue };
            out.push(Event {
                id: format!("{id}/{}", e["id"].as_str().unwrap_or("")),
                title: e["summary"].as_str().unwrap_or("(sin título)").to_string(),
                start,
                end,
                all_day,
                meeting: meeting_link(e),
                link: e["htmlLink"].as_str().unwrap_or("").to_string(),
                location: e["location"].as_str().unwrap_or("").to_string(),
                calendar: name.clone(),
                color,
            });
        }
    }
    out.sort_by_key(|e| (e.start, !e.all_day));
    out.dedup_by(|a, b| a.title == b.title && a.start == b.start);
    Ok(out)
}

/// (unread threads in the inbox, the latest unread messages).
pub fn fetch_mail() -> Result<(u64, Vec<Mail>)> {
    let label = auth::get_json("https://gmail.googleapis.com/gmail/v1/users/me/labels/INBOX")?;
    let unread = label["threadsUnread"].as_u64().unwrap_or(0);
    let list = auth::get_json("https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults=15&q=is%3Aunread%20in%3Ainbox")?;
    let mut out = Vec::new();
    for m in list["messages"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str() else { continue };
        let url = format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=Subject");
        let Ok(msg) = auth::get_json(&url) else { continue };
        let header = |name: &str| -> String {
            msg["payload"]["headers"].as_array().and_then(|h| h.iter().find(|x| x["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))).and_then(|x| x["value"].as_str()).unwrap_or("").to_string()
        };
        let from = header("From");
        // "Nombre <correo>" → "Nombre".
        let from = from.split('<').next().map(|s| s.trim().trim_matches('"').to_string()).filter(|s| !s.is_empty()).unwrap_or(from);
        out.push(Mail {
            id: id.to_string(),
            thread: msg["threadId"].as_str().unwrap_or(id).to_string(),
            from,
            subject: header("Subject"),
            snippet: html_unescape(msg["snippet"].as_str().unwrap_or("")),
            at: msg["internalDate"].as_str().and_then(|s| s.parse::<i64>().ok()).map(|ms| ms / 1000).unwrap_or(0),
        });
    }
    Ok((unread, out))
}

fn html_unescape(s: &str) -> String {
    s.replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Gmail web link for a thread (in the connected account).
pub fn mail_url(thread: &str) -> String {
    match auth::email() {
        Some(e) => format!("https://mail.google.com/mail/u/?authuser={}#inbox/{thread}", auth::urlencode(&e)),
        None => format!("https://mail.google.com/mail/u/0/#inbox/{thread}"),
    }
}

/// Open a URL in the default browser.
pub fn open(url: &str) {
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_meeting_links() {
        let e = serde_json::json!({"hangoutLink": "https://meet.google.com/abc-defg-hij"});
        assert_eq!(meeting_link(&e).as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        let e = serde_json::json!({"description": "Únete: https://us02web.zoom.us/j/123?pwd=x."});
        assert_eq!(meeting_link(&e).as_deref(), Some("https://us02web.zoom.us/j/123?pwd=x"));
        assert_eq!(meeting_link(&serde_json::json!({})), None);
    }
}
