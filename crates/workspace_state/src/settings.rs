//! User-edited settings: `~/Library/Application Support/juan-terminal/settings.toml`.
//!
//! Unlike `state.toml` (rewritten by the app all the time), this file is only written once
//! (a commented template) and then edited by the user, in the app (⌘,) or any editor.
//!
//! * `blocked_websites`: domains blocked system-wide through a managed block in `/etc/hosts`.
//! * `[projects.<id>] schedule`: hours a project may be opened. Outside them the project is
//!   locked in the sidebar and closed if it is open.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_SETTINGS: &str = include_str!("../default-settings.toml");

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub blocked_websites: Vec<String>,
    /// Keyed by project id (`id` in state.toml; the folder name for most projects).
    pub projects: BTreeMap<String, ProjectSettings>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectSettings {
    /// Windows when the project can be opened. Empty = always.
    pub schedule: Vec<ScheduleWindow>,
    /// Fixed UTC offset the schedule is written in, e.g. "-05:00" (Colombia, no DST).
    /// Absent = the Mac's local time.
    pub utc_offset: Option<String>,
}

/// `start`/`end` as "HH:MM". `start > end` crosses midnight (`days` is the day the window
/// starts). `start == end` is the whole day. Empty `days` = every day.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScheduleWindow {
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

pub fn settings_path() -> PathBuf {
    crate::state_dir().join("settings.toml")
}

/// Write the commented template if there is no settings file yet. Returns its path.
pub fn ensure_settings_file() -> std::io::Result<PathBuf> {
    let path = settings_path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, DEFAULT_SETTINGS)?;
    }
    Ok(path)
}

/// Load settings. A missing file means the template's defaults; an invalid one is an error
/// (the caller keeps the previous settings and reports it).
pub fn load_settings() -> Result<Settings, String> {
    let text = match fs::read_to_string(settings_path()) {
        Ok(t) => t,
        Err(_) => DEFAULT_SETTINGS.to_string(),
    };
    let settings: Settings = toml::from_str(&text).map_err(|e| format!("settings.toml: {e}"))?;
    for (id, p) in &settings.projects {
        if let Some(o) = &p.utc_offset {
            parse_offset(o).ok_or_else(|| format!("settings.toml [projects.{id}]: invalid utc_offset {o:?} (use \"-05:00\")"))?;
        }
        for w in &p.schedule {
            w.validate().map_err(|e| format!("settings.toml [projects.{id}]: {e}"))?;
        }
    }
    Ok(settings)
}

const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// "-05:00" / "+5" / "-0530" → seconds east of UTC.
fn parse_offset(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches("UTC").trim_start_matches("GMT");
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None if rest.len() == 4 => rest.split_at(2),
        None => (rest, "0"),
    };
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// Weekday (0 = Sunday) and minute of day for a Unix time shifted by `offset` seconds.
pub fn weekday_minute(epoch: i64, offset: i64) -> (u32, u32) {
    let t = epoch + offset;
    let days = t.div_euclid(86_400);
    let minute = (t.rem_euclid(86_400) / 60) as u32;
    // 1970-01-01 was a Thursday (4).
    (((days + 4).rem_euclid(7)) as u32, minute)
}

fn day_index(d: &str) -> Option<u32> {
    let d = d.trim().to_ascii_lowercase();
    DAYS.iter().position(|x| d.starts_with(x)).map(|i| i as u32)
}

impl ScheduleWindow {
    fn validate(&self) -> Result<(), String> {
        parse_hm(&self.start).ok_or_else(|| format!("invalid start time {:?} (use HH:MM)", self.start))?;
        parse_hm(&self.end).ok_or_else(|| format!("invalid end time {:?} (use HH:MM)", self.end))?;
        for d in &self.days {
            day_index(d).ok_or_else(|| format!("invalid day {d:?} (use mon, tue, …)"))?;
        }
        Ok(())
    }

    fn has_day(&self, weekday: u32) -> bool {
        self.days.is_empty() || self.days.iter().any(|d| day_index(d) == Some(weekday))
    }

    /// `weekday`: 0 = Sunday. `minute`: minutes since local midnight.
    pub fn contains(&self, weekday: u32, minute: u32) -> bool {
        let (Some(s), Some(e)) = (parse_hm(&self.start), parse_hm(&self.end)) else { return false };
        let yesterday = (weekday + 6) % 7;
        if s == e {
            self.has_day(weekday)
        } else if s < e {
            self.has_day(weekday) && minute >= s && minute < e
        } else {
            (self.has_day(weekday) && minute >= s) || (self.has_day(yesterday) && minute < e)
        }
    }

    pub fn describe(&self) -> String {
        let days = if self.days.is_empty() { "every day".to_string() } else { self.days.join(", ") };
        if self.start.trim() == self.end.trim() { format!("{days}, all day") } else { format!("{days} {}–{}", self.start.trim(), self.end.trim()) }
    }
}

impl ProjectSettings {
    /// Weekday/minute to evaluate the schedule at: the project's `utc_offset` if set,
    /// otherwise the Mac's local offset.
    pub fn clock(&self, epoch: i64, local_offset: i64) -> (u32, u32) {
        let offset = self.utc_offset.as_deref().and_then(parse_offset).unwrap_or(local_offset);
        weekday_minute(epoch, offset)
    }

    pub fn is_open(&self, weekday: u32, minute: u32) -> bool {
        self.schedule.is_empty() || self.schedule.iter().any(|w| w.contains(weekday, minute))
    }

    pub fn describe(&self) -> String {
        let s = self.schedule.iter().map(ScheduleWindow::describe).collect::<Vec<_>>().join("; ");
        match self.utc_offset.as_deref() {
            Some(o) => format!("{s} (UTC{})", o.trim()),
            None => s,
        }
    }
}

impl Settings {
    /// `None` when the project can be opened at Unix time `epoch`; otherwise its allowed
    /// hours. `local_offset`: the Mac's UTC offset in seconds.
    pub fn locked_reason(&self, project_id: &str, epoch: i64, local_offset: i64) -> Option<String> {
        let p = self.projects.get(project_id)?;
        let (day, minute) = p.clock(epoch, local_offset);
        (!p.is_open(day, minute)).then(|| p.describe())
    }

    /// Normalized hosts entries: lowercase, no scheme/path/port, each with its `www.` twin.
    pub fn blocked_hosts(&self) -> Vec<String> {
        let mut out = std::collections::BTreeSet::new();
        for raw in &self.blocked_websites {
            let mut d = raw.trim().to_ascii_lowercase();
            if let Some((_, rest)) = d.split_once("://") {
                d = rest.to_string();
            }
            let d = d.split(['/', ':', '?', '#']).next().unwrap_or("").trim_matches('.').to_string();
            if d.is_empty() || !d.contains('.') || !d.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
                continue;
            }
            if !d.starts_with("www.") {
                out.insert(format!("www.{d}"));
            }
            out.insert(d);
        }
        out.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(days: &[&str], start: &str, end: &str) -> ScheduleWindow {
        ScheduleWindow { days: days.iter().map(|d| d.to_string()).collect(), start: start.into(), end: end.into() }
    }

    #[test]
    fn schedule_windows() {
        let m = |h: u32, min: u32| h * 60 + min;
        // Mon–Sat 18:00 → midnight (bemovil).
        let bemovil = w(&["mon", "tue", "wed", "thu", "fri", "sat"], "18:00", "00:00");
        assert!(bemovil.contains(1, m(18, 0)));
        assert!(bemovil.contains(6, m(23, 59)));
        assert!(!bemovil.contains(1, m(17, 59)));
        assert!(!bemovil.contains(0, m(20, 0))); // Sunday
        assert!(!bemovil.contains(0, m(0, 0))); // Sat window ends at midnight
        // Crossing midnight: Fri 22:00 → 02:00 is open Sat 01:00, not Sun 01:00.
        let late = w(&["fri"], "22:00", "02:00");
        assert!(late.contains(6, m(1, 0)));
        assert!(!late.contains(0, m(1, 0)));
        // Same-day window, whole day, every day.
        assert!(w(&[], "09:00", "17:00").contains(3, m(9, 0)));
        assert!(!w(&[], "09:00", "17:00").contains(3, m(17, 0)));
        assert!(w(&["sun"], "00:00", "00:00").contains(0, m(12, 0)));
        assert!(!w(&["sun"], "00:00", "00:00").contains(1, m(12, 0)));
    }

    #[test]
    fn default_template_parses_and_normalizes_hosts() {
        let s: Settings = toml::from_str(DEFAULT_SETTINGS).unwrap();
        assert!(s.blocked_websites.len() > 50);
        let hosts = s.blocked_hosts();
        assert!(hosts.contains(&"pornhub.com".to_string()) && hosts.contains(&"www.pornhub.com".to_string()));
        assert!(hosts.contains(&"bet365.com".to_string()));
        let s = Settings { blocked_websites: vec!["https://Example.com/x".into(), "www.a.io".into(), "bad host".into(), "nodot".into()], ..Default::default() };
        assert_eq!(s.blocked_hosts(), vec!["example.com", "www.a.io", "www.example.com"]);
    }

    #[test]
    fn locked_reason_offsets_and_validation() {
        let s: Settings = toml::from_str(
            "[projects.bemovil]\nutc_offset = \"-05:00\"\nschedule = [{ days = [\"mon\"], start = \"08:00\", end = \"16:00\" }]\n",
        )
        .unwrap();
        // 2026-11-23 (Monday) 13:00 UTC = 08:00 in Colombia.
        let mon_8_bogota = 1_795_438_800;
        assert_eq!(weekday_minute(mon_8_bogota, -5 * 3600), (1, 8 * 60));
        // The Mac's local offset (here EDT, -4) doesn't matter: utc_offset wins.
        assert_eq!(s.locked_reason("bemovil", mon_8_bogota, -4 * 3600), None);
        assert_eq!(s.locked_reason("bemovil", mon_8_bogota - 60, -4 * 3600).as_deref(), Some("mon 08:00–16:00 (UTC-05:00)"));
        assert!(s.locked_reason("bemovil", mon_8_bogota + 8 * 3600, 0).is_some());
        assert_eq!(s.locked_reason("other", 0, 0), None);
        assert_eq!(parse_offset("-05:00"), Some(-18_000));
        assert_eq!(parse_offset("+0530"), Some(19_800));
        assert_eq!(parse_offset("UTC-5"), Some(-18_000));
        assert_eq!(parse_offset("5"), None);
        assert!(w(&["mon"], "25:00", "00:00").validate().is_err());
        assert!(w(&["moon"], "10:00", "11:00").validate().is_err());
    }
}
