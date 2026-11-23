//! SSH / SFTP through the system OpenSSH client (no extra crypto stack to download or audit).
//!
//! All connections to a server share one authenticated master connection
//! (`ControlMaster=auto`, socket under the app state dir), so after the first login every
//! listing, transfer or `exec` is a cheap new channel. Background operations run with
//! `BatchMode=yes`: they never hang waiting for a password; the interactive terminal is where
//! passwords / passphrases / new host keys get answered.

pub mod mcp;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use workspace_state::SshServer;

/// Where ControlMaster sockets live. Kept short: macOS limits socket paths to 104 bytes.
pub fn control_dir() -> PathBuf {
    let d = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".ssh/jt-cm");
    let _ = std::fs::create_dir_all(&d);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    d
}

/// Common `-o` options. `batch`: never prompt (background work).
pub fn base_options(server: &SshServer, batch: bool) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}/%C", control_dir().display()),
        "-o".into(),
        "ControlPersist=300".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        // First connection to a new host trusts its key (TOFU); a *changed* key still fails.
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    if batch {
        v.extend(["-o".into(), "BatchMode=yes".into()]);
    }
    if let Some(key) = &server.identity_file {
        v.extend(["-i".into(), key.display().to_string(), "-o".into(), "IdentitiesOnly=yes".into()]);
    }
    v
}

/// `ssh` arguments for an interactive login shell in a terminal tab.
pub fn interactive_args(server: &SshServer) -> Vec<String> {
    let mut v = base_options(server, false);
    v.extend(["-p".into(), server.port_or_default().to_string(), server.destination()]);
    v
}

/// POSIX single-quote a string for a remote shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quote a path for an sftp batch command.
fn sftp_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    /// None when killed by the timeout.
    pub code: Option<i32>,
    pub truncated: bool,
}

const MAX_OUTPUT: usize = 200_000;

/// Run `cmd` on the server (`sh -c` on the remote side), capturing output, with a timeout.
pub fn exec(server: &SshServer, cmd: &str, timeout: Duration) -> Result<ExecOutput> {
    let mut args = base_options(server, true);
    args.extend(["-p".into(), server.port_or_default().to_string(), "-T".into(), server.destination(), "--".into(), cmd.to_string()]);
    run_with_timeout(Command::new("ssh").args(&args), None, timeout)
}

fn run_with_timeout(cmd: &mut Command, stdin: Option<&[u8]>, timeout: Duration) -> Result<ExecOutput> {
    use std::os::unix::process::CommandExt;
    let mut child = cmd
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("failed to run ssh (is OpenSSH installed?)")?;
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let read = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut r).take((MAX_OUTPUT * 4) as u64).read_to_end(&mut buf);
            // Drain the rest so the process isn't blocked on a full pipe.
            let _ = std::io::copy(&mut r, &mut std::io::sink());
            buf
        })
    };
    let out_t = read(Box::new(child.stdout.take().expect("piped")));
    let err_t = read(Box::new(child.stderr.take().expect("piped")));
    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait()? {
            break st.code();
        }
        if start.elapsed() > timeout {
            // SAFETY: kill(2) on the process group we created.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut out, mut err) = (out_t.join().unwrap_or_default(), err_t.join().unwrap_or_default());
    let truncated = out.len() > MAX_OUTPUT || err.len() > MAX_OUTPUT;
    out.truncate(MAX_OUTPUT);
    err.truncate(MAX_OUTPUT);
    Ok(ExecOutput { stdout: String::from_utf8_lossy(&out).into_owned(), stderr: String::from_utf8_lossy(&err).into_owned(), code, truncated })
}

/// Run an sftp batch script. Fails with sftp's message if any command fails.
fn sftp_batch(server: &SshServer, script: &str, timeout: Duration) -> Result<String> {
    let mut args = base_options(server, true);
    args.extend(["-P".into(), server.port_or_default().to_string(), "-b".into(), "-".into(), server.destination()]);
    let out = run_with_timeout(Command::new("sftp").args(&args), Some(script.as_bytes()), timeout)?;
    match out.code {
        Some(0) => Ok(out.stdout),
        None => bail!("sftp timed out"),
        Some(_) => {
            let msg = out.stderr.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("sftp failed").to_string();
            bail!("{msg}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    pub perms: String,
    /// As printed by sftp ("Jan  2 12:00" / "Jan  2  2024").
    pub modified: String,
}

/// Parse sftp `ls -la` long listing lines (sftp-server "longname" format).
pub fn parse_ls(out: &str) -> Vec<RemoteEntry> {
    let mut v = Vec::new();
    for line in out.lines() {
        let line = line.trim_end();
        if line.starts_with("sftp>") || line.len() < 10 {
            continue;
        }
        let first = line.as_bytes()[0];
        if !b"-dlcbps".contains(&first) {
            continue;
        }
        // perms links owner group size month day time-or-year name…
        let mut rest = line;
        let mut fields = Vec::new();
        for _ in 0..8 {
            let t = rest.trim_start();
            let end = t.find(char::is_whitespace).unwrap_or(t.len());
            fields.push(&t[..end]);
            rest = &t[end..];
        }
        // The name is everything after the 8th field (one separating space; names may have spaces).
        let mut name = rest.strip_prefix(' ').unwrap_or(rest).trim_start_matches(' ').to_string();
        if fields.len() < 8 || name.is_empty() {
            continue;
        }
        let is_link = first == b'l';
        if is_link {
            if let Some(i) = name.find(" -> ") {
                name.truncate(i);
            }
        }
        // sftp prints the path as given; keep the last component.
        if let Some(i) = name.trim_end_matches('/').rfind('/') {
            name = name[i + 1..].to_string();
        }
        if name == "." || name == ".." {
            continue;
        }
        v.push(RemoteEntry {
            name,
            is_dir: first == b'd',
            is_link,
            size: fields[4].parse().unwrap_or(0),
            perms: fields[0].to_string(),
            modified: format!("{} {} {}", fields[5], fields[6], fields[7]),
        });
    }
    v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    v
}

pub fn list_dir(server: &SshServer, dir: &str) -> Result<Vec<RemoteEntry>> {
    let out = sftp_batch(server, &format!("ls -la {}\n", sftp_quote(dir)), Duration::from_secs(30))?;
    Ok(parse_ls(&out))
}

/// Remote home / start directory.
pub fn home_dir(server: &SshServer) -> Result<String> {
    let out = sftp_batch(server, "pwd\n", Duration::from_secs(30))?;
    out.lines()
        .find_map(|l| l.strip_prefix("Remote working directory: "))
        .map(|s| s.trim().to_string())
        .context("could not read the remote working directory")
}

pub fn download(server: &SshServer, remote: &str, local: &Path) -> Result<()> {
    if let Some(p) = local.parent() {
        std::fs::create_dir_all(p)?;
    }
    sftp_batch(server, &format!("get -p {} {}\n", sftp_quote(remote), sftp_quote(&local.display().to_string())), Duration::from_secs(600)).map(|_| ())
}

pub fn upload(server: &SshServer, local: &Path, remote: &str) -> Result<()> {
    let flag = if local.is_dir() { "-rp" } else { "-p" };
    sftp_batch(server, &format!("put {flag} {} {}\n", sftp_quote(&local.display().to_string()), sftp_quote(remote)), Duration::from_secs(600)).map(|_| ())
}

pub fn mkdir(server: &SshServer, remote: &str) -> Result<()> {
    sftp_batch(server, &format!("mkdir {}\n", sftp_quote(remote)), Duration::from_secs(30)).map(|_| ())
}

pub fn rename(server: &SshServer, from: &str, to: &str) -> Result<()> {
    sftp_batch(server, &format!("rename {} {}\n", sftp_quote(from), sftp_quote(to)), Duration::from_secs(30)).map(|_| ())
}

/// Delete a file, or a directory recursively (sftp can't remove non-empty dirs, so use exec).
pub fn remove(server: &SshServer, remote: &str, is_dir: bool) -> Result<()> {
    if remote.trim_end_matches('/').is_empty() || remote == "~" {
        bail!("refusing to delete {remote:?}");
    }
    if is_dir {
        let out = exec(server, &format!("rm -rf -- {}", shell_quote(remote)), Duration::from_secs(120))?;
        if out.code != Some(0) {
            bail!("{}", out.stderr.trim());
        }
        Ok(())
    } else {
        sftp_batch(server, &format!("rm {}\n", sftp_quote(remote)), Duration::from_secs(30)).map(|_| ())
    }
}

/// Check that a background (BatchMode) connection works; returns a readable reason if not.
pub fn check(server: &SshServer) -> Result<()> {
    let out = exec(server, "true", Duration::from_secs(15))?;
    match out.code {
        Some(0) => Ok(()),
        None => bail!("timed out connecting to {}", server.destination()),
        Some(_) => {
            let e = out.stderr.trim();
            if e.contains("Permission denied") || e.contains("passphrase") {
                bail!("{e}\nConnect once in a terminal (password/passphrase), or use a key without a passphrase / ssh-agent.")
            }
            bail!("{}", if e.is_empty() { "connection failed" } else { e })
        }
    }
}

/// Close the shared master connection (e.g. on project switch).
pub fn disconnect(server: &SshServer) {
    let mut args = base_options(server, true);
    args.extend(["-p".into(), server.port_or_default().to_string(), "-O".into(), "exit".into(), server.destination()]);
    let _ = Command::new("ssh").args(&args).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

pub fn parent_remote(path: &str) -> String {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => t[..i].to_string(),
        None => ".".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> SshServer {
        SshServer { id: "s1".into(), name: "prod".into(), host: "10.0.0.5".into(), port: 2222, user: "deploy".into(), identity_file: Some("/k/id_ed25519".into()), start_dir: String::new() }
    }

    #[test]
    fn args_and_quoting() {
        let a = interactive_args(&server());
        assert!(a.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(a.windows(2).any(|w| w == ["-i", "/k/id_ed25519"]));
        assert_eq!(a.last().unwrap(), "deploy@10.0.0.5");
        assert!(!a.iter().any(|x| x == "BatchMode=yes"), "interactive may prompt");
        assert!(base_options(&server(), true).iter().any(|x| x == "BatchMode=yes"));
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(sftp_quote("a \"b\""), "\"a \\\"b\\\"\"");
        assert_eq!(SshServer { port: 0, ..server() }.port_or_default(), 22);
        assert_eq!(join_remote("/var/www", "a.txt"), "/var/www/a.txt");
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(parent_remote("/var/www/"), "/var");
        assert_eq!(parent_remote("/var"), "/");
    }

    #[test]
    fn parses_sftp_long_listing() {
        let out = "sftp> ls -la \"/srv/app\"\n\
drwxr-xr-x    5 deploy   deploy       4096 Mar  3 10:12 /srv/app/.\n\
drwxr-xr-x    3 root     root         4096 Jan  9  2024 /srv/app/..\n\
-rw-r--r--    1 deploy   deploy        220 Mar  3 10:12 /srv/app/.env\n\
drwxr-xr-x    2 deploy   deploy       4096 Mar  3 10:12 /srv/app/logs\n\
-rw-r--r--    1 deploy   deploy    1048576 Feb 28 09:01 /srv/app/my file.txt\n\
lrwxrwxrwx    1 deploy   deploy         11 Mar  1 08:00 /srv/app/current -> releases/42\n";
        let e = parse_ls(out);
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["logs", ".env", "current", "my file.txt"]);
        assert!(e[0].is_dir);
        assert_eq!(e[3].size, 1_048_576);
        assert!(e[2].is_link);
        assert_eq!(e[1].modified, "Mar 3 10:12");
    }

    #[test]
    fn timeout_kills_and_output_is_captured() {
        let out = run_with_timeout(Command::new("/bin/sh").args(["-c", "echo hi; echo err >&2; exit 3"]), None, Duration::from_secs(5)).unwrap();
        assert_eq!((out.stdout.as_str(), out.stderr.as_str(), out.code), ("hi\n", "err\n", Some(3)));
        let t = Instant::now();
        let out = run_with_timeout(Command::new("/bin/sh").args(["-c", "sleep 30"]), None, Duration::from_millis(300)).unwrap();
        assert_eq!(out.code, None);
        assert!(t.elapsed() < Duration::from_secs(3));
        let out = run_with_timeout(Command::new("/bin/cat").arg("-"), Some(b"stdin works"), Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout, "stdin works");
    }

    #[test]
    fn refuses_to_delete_root() {
        assert!(remove(&server(), "/", true).is_err());
        assert!(remove(&server(), "~", true).is_err());
    }
}
