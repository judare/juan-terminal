//! System-wide website blocking via a managed block in `/etc/hosts`.
//!
//! Only the block between our markers is ever changed; the rest of the file is kept byte for
//! byte. Writing needs root, so the new file is staged in a temp file and copied over
//! `/etc/hosts` with `osascript … with administrator privileges` (macOS password dialog).
//! Applied at launch, when settings.toml is saved and when a website schedule opens or
//! closes (checked every 20 s), and only when the block actually differs.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

const HOSTS: &str = "/etc/hosts";
const BEGIN: &str = "# >>> juan-terminal blocked websites (managed by juan-terminal, edit settings.toml) >>>";
const END: &str = "# <<< juan-terminal blocked websites <<<";
/// Block left by the old focus-time daemon; removed if still present.
const LEGACY: (&str, &str) = ("# >>> focus-time", "# <<< focus-time");

/// Remove the managed block(s) (ours and the legacy one) from a hosts file.
fn strip_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let l = line.trim_end();
        if !skipping && (l == BEGIN || l.starts_with(LEGACY.0)) {
            skipping = true;
            continue;
        }
        if skipping {
            if l == END || l.starts_with(LEGACY.1) {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Removing a site from the list only unblocks it after this long (adding is immediate), so
/// unblocking can't be done on impulse.
pub const UNBLOCK_DELAY_SECS: i64 = 24 * 3600;

/// Hosts currently in our managed block.
fn managed_hosts(text: &str) -> Vec<String> {
    let mut inside = false;
    let mut out = Vec::new();
    for l in text.lines() {
        let l = l.trim();
        if l == BEGIN {
            inside = true;
        } else if l == END {
            inside = false;
        } else if inside {
            if let Some(h) = l.strip_prefix("0.0.0.0 ") {
                out.push(h.trim().to_string());
            }
        }
    }
    out
}

/// What to block: everything wanted, plus hosts removed from the list less than
/// `UNBLOCK_DELAY_SECS` ago (`pending`: host → when its removal was first seen; updated).
/// `release`: hosts unblocked right away (scheduled sites inside their allowed hours).
/// Returns (hosts, earliest Unix time a pending host gets unblocked).
pub fn effective_hosts(wanted: &[String], release: &[String], current: &[String], pending: &mut BTreeMap<String, i64>, now: i64) -> (Vec<String>, Option<i64>) {
    let mut hosts: std::collections::BTreeSet<String> = wanted.iter().cloned().collect();
    pending.retain(|h, _| !hosts.contains(h) && !release.contains(h) && current.contains(h));
    let mut next = None;
    for h in current {
        if hosts.contains(h) || release.contains(h) {
            continue;
        }
        let since = *pending.entry(h.clone()).or_insert(now);
        if now - since < UNBLOCK_DELAY_SECS {
            hosts.insert(h.clone());
            let at = since + UNBLOCK_DELAY_SECS;
            next = Some(next.map_or(at, |n: i64| n.min(at)));
        }
    }
    pending.retain(|h, since| now - *since < UNBLOCK_DELAY_SECS || !current.contains(h));
    (hosts.into_iter().collect(), next)
}

fn pending_path() -> std::path::PathBuf {
    workspace_state::state_dir().join("unblock-pending.json")
}

/// The full hosts file with `hosts` blocked (empty list = block removed).
pub fn render(current: &str, hosts: &[String]) -> String {
    let mut out = strip_blocks(current);
    if hosts.is_empty() {
        return out;
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(BEGIN);
    out.push('\n');
    for h in hosts {
        out.push_str(&format!("0.0.0.0 {h}\n:: {h}\n"));
    }
    out.push_str(END);
    out.push('\n');
    out
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub struct Applied {
    /// /etc/hosts was rewritten (password asked).
    pub changed: bool,
    /// Removed sites still blocked by the delay: count and when the first one is released.
    pub pending: usize,
    pub pending_until: Option<i64>,
}

/// Make `/etc/hosts` block `wanted` (plus recently removed sites, see `effective_hosts`).
/// Blocking (waits for the password dialog): call from a background thread.
pub fn apply(wanted: &[String], release: &[String]) -> Result<Applied, String> {
    let current = std::fs::read_to_string(HOSTS).map_err(|e| format!("reading {HOSTS}: {e}"))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let mut pending: BTreeMap<String, i64> = std::fs::read_to_string(pending_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let (hosts, pending_until) = effective_hosts(wanted, release, &managed_hosts(&current), &mut pending, now);
    let _ = std::fs::write(pending_path(), serde_json::to_string_pretty(&pending).unwrap_or_default());
    let held = hosts.len().saturating_sub(wanted.len());
    let wanted_text = render(&current, &hosts);
    if wanted_text == current {
        return Ok(Applied { changed: false, pending: held, pending_until });
    }
    let wanted = wanted_text;
    let tmp = std::env::temp_dir().join(format!("juan-terminal-hosts-{}", std::process::id()));
    std::fs::write(&tmp, &wanted).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    let result = copy_as_root(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result.map(|_| Applied { changed: true, pending: held, pending_until })
}

fn copy_as_root(src: &Path) -> Result<(), String> {
    // `cat >` keeps the owner/mode of /etc/hosts. Then flush the DNS cache.
    let script = format!(
        "/bin/cat {} > {HOSTS} && /usr/bin/dscacheutil -flushcache; /usr/bin/killall -HUP mDNSResponder; true",
        sh_quote(&src.display().to_string())
    );
    let apple = format!(
        "do shell script \"{}\" with prompt \"juan-terminal needs your password to update the blocked websites in /etc/hosts.\" with administrator privileges",
        script.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let out = Command::new("/usr/bin/osascript").arg("-e").arg(apple).output().map_err(|e| format!("osascript: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") { Err("cancelled".into()) } else { Err(err.trim().to_string()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removals_are_delayed_additions_are_not() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut pending = BTreeMap::new();
        // Adding: immediate.
        let (h, next) = effective_hosts(&s(&["a.com", "b.com"]), &[], &s(&["a.com"]), &mut pending, 1000);
        assert_eq!((h, next), (s(&["a.com", "b.com"]), None));
        // Removing b.com: still blocked for 24 h.
        let (h, next) = effective_hosts(&s(&["a.com"]), &[], &s(&["a.com", "b.com"]), &mut pending, 1000);
        assert_eq!((h, next), (s(&["a.com", "b.com"]), Some(1000 + UNBLOCK_DELAY_SECS)));
        let (h, _) = effective_hosts(&s(&["a.com"]), &[], &s(&["a.com", "b.com"]), &mut pending, 1000 + UNBLOCK_DELAY_SECS - 1);
        assert_eq!(h, s(&["a.com", "b.com"]));
        // After the delay it's released.
        let (h, next) = effective_hosts(&s(&["a.com"]), &[], &s(&["a.com", "b.com"]), &mut pending, 1000 + UNBLOCK_DELAY_SECS);
        assert_eq!((h, next), (s(&["a.com"]), None));
        // Re-adding clears the pending removal.
        let mut pending = BTreeMap::from([("b.com".to_string(), 5)]);
        effective_hosts(&s(&["b.com"]), &[], &s(&["b.com"]), &mut pending, 10);
        assert!(pending.is_empty());
        // Scheduled sites in their allowed hours are released at once.
        let mut pending = BTreeMap::new();
        let (h, next) = effective_hosts(&s(&["a.com"]), &s(&["b.com"]), &s(&["a.com", "b.com"]), &mut pending, 1000);
        assert_eq!((h, next), (s(&["a.com"]), None));
        assert_eq!(managed_hosts(&render("x\n", &s(&["a.com"]))), s(&["a.com"]));
    }

    #[test]
    fn replaces_only_managed_blocks() {
        let base = "127.0.0.1 localhost\n::1 localhost\n";
        let legacy = format!("{base}# >>> focus-time (x) >>>\n0.0.0.0 a.com\n# <<< focus-time <<<\n10.0.0.1 my.lan\n");
        let out = render(&legacy, &["b.com".into()]);
        assert_eq!(out, format!("{base}10.0.0.1 my.lan\n{BEGIN}\n0.0.0.0 b.com\n:: b.com\n{END}\n"));
        // Idempotent, and an empty list removes the block.
        assert_eq!(render(&out, &["b.com".into()]), out);
        assert_eq!(render(&out, &[]), format!("{base}10.0.0.1 my.lan\n"));
        // Missing trailing newline is handled.
        assert_eq!(render("x", &["c.com".into()]), format!("x\n{BEGIN}\n0.0.0.0 c.com\n:: c.com\n{END}\n"));
    }
}
