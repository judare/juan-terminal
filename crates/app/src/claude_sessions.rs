//! Claude Code sessions stored on disk (`~/.claude/projects/<encoded cwd>/<id>.jsonl`), for the
//! history picker of a Claude terminal.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
    pub user_messages: usize,
}

/// Claude encodes the project directory by replacing every non-alphanumeric char with '-'.
pub fn encode_dir(cwd: &Path) -> String {
    cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

pub fn projects_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("projects"),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude/projects"),
    }
}

pub(crate) fn text_of(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => items.iter().find_map(|i| (i["type"] == "text").then(|| i["text"].as_str().map(str::to_string)).flatten()),
        _ => None,
    }
}

/// Title (custom title / summary / first real user message) and user message count.
pub fn read_session(path: &Path) -> Option<(String, usize)> {
    let f = std::fs::File::open(path).ok()?;
    let mut title: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut users = 0;
    for line in BufReader::new(f).lines().take(4000) {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("custom-title") => title = v["customTitle"].as_str().map(str::to_string).or(title),
            Some("summary") if title.is_none() => title = v["summary"].as_str().map(str::to_string),
            Some("user") if v["isMeta"] != true => {
                if let Some(t) = text_of(&v["message"]["content"]) {
                    let t = t.trim();
                    // Skip command echoes / system reminders / tool results.
                    if !t.is_empty() && !t.starts_with('<') && !t.starts_with("Caveat:") {
                        users += 1;
                        if first_user.is_none() {
                            first_user = Some(t.lines().next().unwrap_or(t).to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut t = title.or(first_user)?;
    if t.chars().count() > 90 {
        t = t.chars().take(88).collect::<String>() + "…";
    }
    Some((t, users))
}

/// Sessions for `cwd`, newest first (empty sessions skipped).
pub fn list_sessions(cwd: &Path, limit: usize) -> Vec<SessionInfo> {
    let dir = projects_dir().join(encode_dir(cwd));
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
        .into_iter()
        .filter_map(|(modified, p)| {
            let (title, user_messages) = read_session(&p)?;
            Some(SessionInfo { id: p.file_stem()?.to_string_lossy().into_owned(), title, modified, user_messages })
        })
        .take(limit)
        .collect()
}

pub fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_claude() {
        assert_eq!(encode_dir(Path::new("/Users/x/Desktop/repositories/blokay/blokay")), "-Users-x-Desktop-repositories-blokay-blokay");
        assert_eq!(encode_dir(Path::new("/a/my.app")), "-a-my-app");
    }

    #[test]
    fn reads_title_and_counts() {
        let p = std::env::temp_dir().join(format!("jt-sess-{}.jsonl", std::process::id()));
        std::fs::write(
            &p,
            [
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"}}"#,
                r#"{"type":"user","message":{"role":"user","content":"arregla el login\nmás detalles"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"gracias"}]}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        assert_eq!(read_session(&p), Some(("arregla el login".to_string(), 2)));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn lists_real_sessions_if_present() {
        // Smoke test on this machine's data: must not panic, newest first.
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let s = list_sessions(&home.join("Desktop/repositories/blokay/blokay"), 5);
        for w in s.windows(2) {
            assert!(w[0].modified >= w[1].modified);
        }
    }
}
