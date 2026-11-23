//! zsh integration: after every command the shell prints its duration and exit status, and
//! "command not found" gets a readable message with a suggestion. Loaded through ZDOTDIR,
//! which then hands control back to the user's own startup files (never modified).

use std::collections::HashMap;
use std::path::PathBuf;

const ZSHENV: &str = include_str!("../shell/zsh/.zshenv");
const INTEGRATION: &str = include_str!("../shell/zsh/jt-integration.zsh");

fn dir() -> PathBuf {
    workspace_state::state_dir().join("shell/zsh")
}

/// Write the scripts (only when changed) and return the env vars that activate them.
pub fn env_for_shell() -> HashMap<String, String> {
    let mut env = HashMap::new();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    if !shell.ends_with("/zsh") {
        return env;
    }
    let d = dir();
    let write = |name: &str, body: &str| -> std::io::Result<()> {
        let p = d.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(body) {
            std::fs::create_dir_all(&d)?;
            std::fs::write(p, body)?;
        }
        Ok(())
    };
    if write(".zshenv", ZSHENV).is_err() || write("jt-integration.zsh", INTEGRATION).is_err() {
        return env; // fall back to a plain shell
    }
    // Claude hook settings (see claude_ide::hook_settings_json), used by the `claude` wrapper.
    let settings = workspace_state::state_dir().join("claude-hooks.json");
    let body = claude_ide::hook_settings_json();
    if std::fs::read_to_string(&settings).ok().as_deref() != Some(body.as_str()) {
        let _ = std::fs::write(&settings, &body);
    }
    env.insert("JT_CLAUDE_SETTINGS".into(), settings.to_string_lossy().into_owned());
    if let Ok(user) = std::env::var("ZDOTDIR") {
        env.insert("JT_USER_ZDOTDIR".into(), user);
    }
    env.insert("ZDOTDIR".into(), d.to_string_lossy().into_owned());
    env
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};
    use terminal_engine::{Mark, Palette, SessionConfig, Snapshot, TermDims, TerminalSession, parse_mark_uri};

    fn screen(s: &TerminalSession) -> String {
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        (0..30).map(|r| snap.row_text(r)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn zsh_emits_block_markers() {
        let home = std::env::temp_dir().join(format!("jt-zsh-m-{}", std::process::id()));
        std::fs::create_dir_all(home.join("proj")).unwrap();
        let home = home.canonicalize().unwrap();
        std::fs::write(home.join("proj/package.json"), "{}").unwrap();
        // A user hook that clobbers $? must not break the exit status we report.
        std::fs::write(home.join(".zshrc"), "other() { true }\nprecmd_functions+=(other)\n").unwrap();
        let mut cfg = SessionConfig::default();
        cfg.program = Some(("/bin/zsh".into(), vec!["-i".into()]));
        cfg.cwd = Some(home.join("proj"));
        cfg.env.insert("HOME".into(), home.to_string_lossy().into());
        cfg.env.insert("ZDOTDIR".into(), concat!(env!("CARGO_MANIFEST_DIR"), "/shell/zsh").into());
        cfg.env.insert("JT_USER_ZDOTDIR".into(), home.to_string_lossy().into());
        let (s, _rx) = TerminalSession::spawn(cfg, TermDims::new(100, 30, 8, 16), Palette::dark()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        s.write(b"false\r\recho hi\rgti\r".as_slice());
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let mut snap = Snapshot::default();
            s.snapshot(&mut snap);
            let ends: Vec<String> = snap.marks.iter().filter(|m| m.1 == Mark::End).map(|m| m.2.clone()).collect();
            let prompts = snap.marks.iter().filter(|m| m.1 == Mark::Prompt).count();
            let codes: Vec<String> = ends.iter().map(|u| parse_mark_uri(u).unwrap().1["code"].clone()).collect();
            if codes == ["1", "0", "127"] && prompts == 4 {
                let text = screen(&s);
                assert!(text.contains("did you mean git?"), "{text}");
                // The typed command carries the cmd mark.
                let cmd_row = (0..snap.rows).find(|r| snap.row_text(*r).starts_with("echo hi")).expect("command row");
                assert_eq!(snap.row(cmd_row)[0].mark, Mark::Cmd);
                let p = parse_mark_uri(&snap.marks[0].2).unwrap().1;
                assert_eq!(p["cwd"], "~/proj");
                break;
            }
            assert!(Instant::now() < deadline, "markers: {:?}\n{}", snap.marks, screen(&s));
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(s);
        let _ = std::fs::remove_dir_all(home);
    }
}

#[cfg(test)]
mod claude_mouse_probe {
    use std::time::Duration;
    use terminal_engine::{Palette, SessionConfig, Snapshot, TermDims, TerminalSession};

    /// Manual check against the real `claude` (no prompt is sent): does its TUI enable mouse
    /// tracking? Run with `cargo test -- --ignored claude_enables_mouse`.
    #[test]
    #[ignore]
    fn claude_enables_mouse() {
        let mut cfg = SessionConfig::default();
        cfg.program = Some(("claude".into(), vec![]));
        cfg.cwd = Some(std::path::PathBuf::from(std::env::var("HOME").unwrap()).join("Desktop/repositories/employees"));
        for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("CLAUDE")) {
            cfg.env.insert(k, String::new());
        }
        let (s, _rx) = TerminalSession::spawn(cfg, TermDims::new(120, 40, 8, 16), Palette::dark()).unwrap();
        std::thread::sleep(Duration::from_secs(8));
        let mut snap = Snapshot::default();
        s.snapshot(&mut snap);
        let m = snap.mode;
        println!("alt={} click={} drag={} motion={} sgr={}", m.alt_screen, m.mouse_click, m.mouse_drag, m.mouse_motion, m.sgr_mouse);
    }
}
