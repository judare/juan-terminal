//! Apps started from Finder/Raycast/Dock get launchd's minimal environment (PATH without
//! Homebrew, ~/.local/bin, nvm…), so git/oxlint/claude/npm wouldn't be found. Like other
//! editors, import the user's login-shell environment once at startup.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Parse `env -0` output.
pub fn parse_env0(bytes: &[u8]) -> Vec<(String, String)> {
    bytes
        .split(|b| *b == 0)
        .filter_map(|kv| {
            let s = std::str::from_utf8(kv).ok()?;
            let (k, v) = s.split_once('=')?;
            (!k.is_empty() && !k.contains(char::is_whitespace)).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Only needed when not started from a terminal (a terminal already gave us the real env).
pub fn needs_import() -> bool {
    std::env::var_os("TERM").is_none()
}

/// Run `$SHELL -l -i -c 'env -0'` and adopt its variables. Hard 3 s budget: a slow or
/// interactive `.zshrc`, or a background job that keeps stdout open, must never stall startup.
pub fn import_login_env() -> &'static str {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let Ok(mut child) = Command::new(&shell)
        .args(["-l", "-i", "-c", "/usr/bin/env -0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Own process group so a timeout can kill everything it started.
        .process_group(0)
        .spawn()
    else {
        return "spawn failed";
    };
    let pgid = child.id() as i32;
    let mut stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel();
    // Reader thread: env -0 output ends when the pipe closes (or we time out and move on).
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let deadline = Duration::from_secs(3);
    let out = match rx.recv_timeout(deadline) {
        Ok(out) => out,
        Err(_) => {
            // SAFETY: kill(2) on the group we just created.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            let _ = child.wait();
            return "timed out";
        }
    };
    let _ = child.try_wait();
    // Background jobs may outlive the shell holding nothing we need; don't leave them around.
    unsafe { libc::kill(-pgid, libc::SIGHUP) };
    // Variables that describe the throwaway shell itself, not the user's environment.
    const SKIP: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", "TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TERM_SESSION_ID"];
    let vars = parse_env0(&out);
    if vars.iter().all(|(k, _)| k != "PATH") {
        return "no PATH in output";
    }
    for (k, v) in vars {
        if !SKIP.contains(&k.as_str()) {
            // SAFETY: called first thing in main(), before the app starts other threads
            // (the reader thread above has finished: we received its result).
            unsafe { std::env::set_var(k, v) };
        }
    }
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_survives_background_job_holding_stdout() {
        // A background job that keeps stdout open must not block past the budget.
        let t = std::time::Instant::now();
        // Direct check of the reader logic with the same shape as import_login_env.
        let mut child = Command::new("/bin/zsh").args(["-c", "sleep 30 & /usr/bin/env -0"]).stdout(Stdio::piped()).process_group(0).spawn().unwrap();
        let pgid = child.id() as i32;
        let mut so = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = so.read_to_end(&mut v);
            let _ = tx.send(v);
        });
        let r = rx.recv_timeout(Duration::from_millis(1500));
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = child.wait();
        assert!(r.is_err(), "the old read_to_end approach would block here");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn parses_env0() {
        let v = parse_env0(b"PATH=/a:/b\0EMPTY=\0MULTI=x\ny\0bad\0");
        assert_eq!(v, vec![("PATH".into(), "/a:/b".into()), ("EMPTY".into(), "".into()), ("MULTI".into(), "x\ny".into())]);
    }
}
