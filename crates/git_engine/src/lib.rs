//! Git operations through the system `git` executable. Blocking functions meant to run on a
//! background thread. Hooks, signing and credentials are whatever the user's git config says.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

pub mod diff;
pub use diff::{DiffLine, DiffLineKind, FileDiff, Hunk, inline_changes, parse_diff, side_by_side, side_by_side_indices};

fn git(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    // Never block on an interactive prompt (credentials, editors).
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_EDITOR", "true");
    // Background status polling must never take index.lock (would break the user's own git).
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.stdin(Stdio::null());
    cmd
}

fn run(mut cmd: Command) -> Result<Output> {
    let out = cmd.output().context("failed to run git")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        bail!("{}", if stderr.is_empty() { stdout } else { stderr });
    }
    Ok(out)
}

fn run_text(cmd: Command) -> Result<String> {
    Ok(String::from_utf8_lossy(&run(cmd)?.stdout).into_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    pub fn letter(self) -> &'static str {
        match self {
            ChangeKind::Modified => "M",
            ChangeKind::Added => "A",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
            ChangeKind::Copied => "C",
            ChangeKind::TypeChanged => "T",
            ChangeKind::Untracked => "U",
            ChangeKind::Conflicted => "!",
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            b'M' => ChangeKind::Modified,
            b'A' => ChangeKind::Added,
            b'D' => ChangeKind::Deleted,
            b'R' => ChangeKind::Renamed,
            b'C' => ChangeKind::Copied,
            b'T' => ChangeKind::TypeChanged,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the repository root.
    pub path: String,
    pub orig_path: Option<String>,
    /// Change in the index (staged), if any.
    pub staged: Option<ChangeKind>,
    /// Change in the working tree (unstaged), if any.
    pub unstaged: Option<ChangeKind>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: Option<String>,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileChange>,
    /// A merge/rebase/cherry-pick is in progress.
    pub operation: Option<&'static str>,
}

impl RepoStatus {
    pub fn staged(&self) -> impl Iterator<Item = &FileChange> {
        self.files.iter().filter(|f| f.staged.is_some() && f.unstaged != Some(ChangeKind::Conflicted))
    }
    pub fn unstaged(&self) -> impl Iterator<Item = &FileChange> {
        self.files.iter().filter(|f| f.unstaged.is_some())
    }
    pub fn conflicted(&self) -> impl Iterator<Item = &FileChange> {
        self.files.iter().filter(|f| f.unstaged == Some(ChangeKind::Conflicted))
    }
}

/// `git status --porcelain=v2 --branch -z`
pub fn status(repo: &Path) -> Result<RepoStatus> {
    let mut cmd = git(repo);
    cmd.args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"]);
    let out = run(cmd)?.stdout;
    let mut st = parse_status(&out);
    let git_dir = git_dir(repo);
    if let Some(dir) = git_dir {
        st.operation = if dir.join("MERGE_HEAD").exists() {
            Some("merge")
        } else if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
            Some("rebase")
        } else if dir.join("CHERRY_PICK_HEAD").exists() {
            Some("cherry-pick")
        } else {
            None
        };
    }
    Ok(st)
}

fn git_dir(repo: &Path) -> Option<PathBuf> {
    let mut cmd = git(repo);
    cmd.args(["rev-parse", "--absolute-git-dir"]);
    run_text(cmd).ok().map(|s| PathBuf::from(s.trim()))
}

pub fn parse_status(out: &[u8]) -> RepoStatus {
    let mut st = RepoStatus::default();
    let mut fields = out.split(|b| *b == 0).peekable();
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(rec);
        if let Some(h) = line.strip_prefix("# ") {
            if let Some(b) = h.strip_prefix("branch.head ") {
                if b == "(detached)" {
                    st.detached = true;
                } else {
                    st.branch = Some(b.to_string());
                }
            } else if let Some(u) = h.strip_prefix("branch.upstream ") {
                st.upstream = Some(u.to_string());
            } else if let Some(ab) = h.strip_prefix("branch.ab ") {
                for part in ab.split(' ') {
                    if let Some(a) = part.strip_prefix('+') {
                        st.ahead = a.parse().unwrap_or(0);
                    } else if let Some(b) = part.strip_prefix('-') {
                        st.behind = b.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let kind = rec[0];
        match kind {
            b'1' | b'2' => {
                // 1 XY sub mH mI mW hH hI path   |  2 XY sub mH mI mW hH hI Xscore path<NUL>origPath
                let n = if kind == b'1' { 8 } else { 9 };
                let parts: Vec<&str> = line.splitn(n + 1, ' ').collect();
                if parts.len() < n + 1 {
                    continue;
                }
                let xy = parts[1].as_bytes();
                let path = parts[n].to_string();
                let orig_path = if kind == b'2' { fields.next().map(|p| String::from_utf8_lossy(p).into_owned()) } else { None };
                st.files.push(FileChange {
                    path,
                    orig_path,
                    staged: ChangeKind::from_code(xy[0]),
                    unstaged: ChangeKind::from_code(xy[1]),
                });
            }
            b'u' => {
                let parts: Vec<&str> = line.splitn(11, ' ').collect();
                if let Some(path) = parts.get(10) {
                    st.files.push(FileChange { path: path.to_string(), orig_path: None, staged: None, unstaged: Some(ChangeKind::Conflicted) });
                }
            }
            b'?' => {
                st.files.push(FileChange { path: line[2..].to_string(), orig_path: None, staged: None, unstaged: Some(ChangeKind::Untracked) });
            }
            _ => {}
        }
    }
    st
}

/// Context large enough that single-file diffs show the whole file (as VS Code does).
const FULL_CONTEXT: &str = "--unified=1000000";

/// Unified diff text for one file (whole file as context). `staged`: index vs HEAD; otherwise worktree vs index.
/// Untracked files are diffed against /dev/null.
pub fn file_diff(repo: &Path, change: &FileChange, staged: bool) -> Result<String> {
    let mut cmd = git(repo);
    if !staged && change.unstaged == Some(ChangeKind::Untracked) {
        cmd.args(["diff", "--no-color", "--no-ext-diff", FULL_CONTEXT, "--no-index", "--", "/dev/null", &change.path]);
        // --no-index exits 1 when files differ.
        let out = cmd.output().context("failed to run git")?;
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    cmd.args(["diff", "--no-color", "--no-ext-diff", FULL_CONTEXT, "-M"]);
    if staged {
        cmd.arg("--cached");
    }
    cmd.arg("--");
    if let Some(orig) = &change.orig_path {
        cmd.arg(orig);
    }
    cmd.arg(&change.path);
    run_text(cmd)
}

/// Unified diff between two in-memory versions of a file (for Claude's proposed edits).
/// `name` is shown as the file path in the diff headers.
pub fn diff_contents(name: &str, old: &str, new: &str) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("jt-diff-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
    let base = std::path::Path::new(name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    std::fs::create_dir_all(dir.join("old"))?;
    std::fs::create_dir_all(dir.join("new"))?;
    std::fs::write(dir.join("old").join(&base), old)?;
    std::fs::write(dir.join("new").join(&base), new)?;
    let out = Command::new("git")
        .current_dir(&dir)
        .args(["diff", "--no-color", "--no-ext-diff", FULL_CONTEXT, "--no-index", "--"])
        .arg(format!("old/{base}"))
        .arg(format!("new/{base}"))
        .output()
        .context("failed to run git");
    let _ = std::fs::remove_dir_all(&dir);
    // --no-index exits 1 when the files differ.
    let text = String::from_utf8_lossy(&out?.stdout).into_owned();
    Ok(text.replace(&format!("a/old/{base}"), &format!("a/{name}")).replace(&format!("b/new/{base}"), &format!("b/{name}")))
}

/// Diff between two refs (branch comparison), whole repo or one path.
pub fn diff_refs(repo: &Path, base: &str, head: &str, path: Option<&str>) -> Result<String> {
    let mut cmd = git(repo);
    cmd.args(["diff", "--no-color", "--no-ext-diff", "-M", &format!("{base}...{head}")]);
    if let Some(p) = path {
        cmd.args([FULL_CONTEXT, "--", p]);
    }
    run_text(cmd)
}

/// Files changed between two refs: (status letter, path).
pub fn changed_files_between(repo: &Path, base: &str, head: &str) -> Result<Vec<(String, String)>> {
    let mut cmd = git(repo);
    cmd.args(["diff", "--name-status", "-M", &format!("{base}...{head}")]);
    Ok(run_text(cmd)?
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let s = it.next()?.chars().next()?.to_string();
            let p = it.last()?.to_string();
            Some((s, p))
        })
        .collect())
}

pub fn stage(repo: &Path, paths: &[&str]) -> Result<()> {
    let mut cmd = git(repo);
    cmd.args(["add", "-A", "--"]).args(paths);
    run(cmd).map(|_| ())
}

pub fn unstage(repo: &Path, paths: &[&str]) -> Result<()> {
    let mut cmd = git(repo);
    if has_head(repo) {
        cmd.args(["restore", "--staged", "--"]).args(paths);
    } else {
        cmd.args(["rm", "--cached", "-r", "-q", "--"]).args(paths);
    }
    run(cmd).map(|_| ())
}

/// Discard working-tree changes. Untracked files are deleted. Destructive: callers must confirm.
pub fn discard(repo: &Path, change: &FileChange) -> Result<()> {
    if change.unstaged == Some(ChangeKind::Untracked) {
        let mut cmd = git(repo);
        cmd.args(["clean", "-f", "-q", "--", &change.path]);
        return run(cmd).map(|_| ());
    }
    let mut cmd = git(repo);
    cmd.args(["restore", "--worktree", "--", &change.path]);
    run(cmd).map(|_| ())
}

fn has_head(repo: &Path) -> bool {
    let mut cmd = git(repo);
    cmd.args(["rev-parse", "--verify", "-q", "HEAD"]);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Apply a patch to the index (stage/unstage a single hunk).
pub fn apply_to_index(repo: &Path, patch: &str, reverse: bool) -> Result<()> {
    use std::io::Write;
    let mut cmd = git(repo);
    cmd.args(["apply", "--cached", "--unidiff-zero", "--whitespace=nowarn"]);
    if reverse {
        cmd.arg("--reverse");
    }
    cmd.arg("-");
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("failed to run git apply")?;
    child.stdin.take().context("no stdin")?.write_all(patch.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Commit staged changes. Runs hooks and honors `commit.gpgsign`. Returns git's summary line.
pub fn commit(repo: &Path, message: &str, amend: bool) -> Result<String> {
    if message.trim().is_empty() && !amend {
        bail!("Commit message is empty");
    }
    let mut cmd = git(repo);
    cmd.args(["commit", "--cleanup=strip", "-F", "-"]);
    if amend {
        cmd.arg("--amend");
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    use std::io::Write;
    let mut child = cmd.spawn().context("failed to run git commit")?;
    child.stdin.take().context("no stdin")?.write_all(message.as_bytes())?;
    let out = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        bail!("{}", if stderr.is_empty() { stdout } else { format!("{stderr}\n{stdout}").trim().to_string() });
    }
    Ok(stdout.lines().next().unwrap_or("Committed").to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub is_remote: bool,
    pub is_current: bool,
    pub upstream: Option<String>,
    pub last_commit: String,
}

pub fn branches(repo: &Path) -> Result<Vec<Branch>> {
    let mut cmd = git(repo);
    cmd.args([
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(HEAD)%00%(refname)%00%(refname:short)%00%(upstream:short)%00%(committerdate:relative)",
        "refs/heads",
        "refs/remotes",
    ]);
    Ok(run_text(cmd)?
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split('\0').collect();
            if p.len() < 5 || p[1].ends_with("/HEAD") {
                return None;
            }
            Some(Branch {
                is_current: p[0] == "*",
                is_remote: p[1].starts_with("refs/remotes/"),
                name: p[2].to_string(),
                upstream: (!p[3].is_empty()).then(|| p[3].to_string()),
                last_commit: p[4].to_string(),
            })
        })
        .collect())
}

/// Switch branch. A remote branch (`origin/x`) creates a tracking local branch.
pub fn checkout(repo: &Path, branch: &Branch) -> Result<()> {
    let mut cmd = git(repo);
    if branch.is_remote {
        let local = branch.name.split_once('/').map(|(_, b)| b).unwrap_or(&branch.name);
        cmd.args(["switch", "--track", "-c", local, &branch.name]);
    } else {
        cmd.args(["switch", &branch.name]);
    }
    run(cmd).map(|_| ())
}

pub fn create_branch(repo: &Path, name: &str) -> Result<()> {
    let mut cmd = git(repo);
    cmd.args(["switch", "-c", name]);
    run(cmd).map(|_| ())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteOp {
    Fetch,
    Pull,
    Push,
}

/// fetch / pull (ff-only, never creates surprise merges) / push (sets upstream if missing).
pub fn remote(repo: &Path, op: RemoteOp) -> Result<String> {
    let mut cmd = git(repo);
    match op {
        RemoteOp::Fetch => {
            cmd.args(["fetch", "--prune"]);
        }
        RemoteOp::Pull => {
            cmd.args(["pull", "--ff-only"]);
        }
        RemoteOp::Push => {
            let st = status(repo)?;
            if st.upstream.is_none() {
                let branch = st.branch.context("not on a branch")?;
                cmd.args(["push", "-u", "origin", &branch]);
            } else {
                cmd.arg("push");
            }
        }
    }
    let out = run(cmd)?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok(text.trim().lines().last().unwrap_or("Done").to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub when: String,
    pub subject: String,
}

pub fn log(repo: &Path, limit: usize) -> Result<Vec<Commit>> {
    let mut cmd = git(repo);
    cmd.args(["log", &format!("-n{limit}"), "--format=%H%x00%h%x00%an%x00%ar%x00%s"]);
    let text = match run_text(cmd) {
        Ok(t) => t,
        Err(_) if !has_head(repo) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split('\0').collect();
            (p.len() == 5).then(|| Commit {
                hash: p[0].into(),
                short: p[1].into(),
                author: p[2].into(),
                when: p[3].into(),
                subject: p[4].into(),
            })
        })
        .collect())
}

/// Full diff of a commit (for the history view).
pub fn show_commit(repo: &Path, hash: &str) -> Result<String> {
    let mut cmd = git(repo);
    cmd.args(["show", "--no-color", "--no-ext-diff", "--format=%H%n%an <%ae>%n%ad%n%n%B", hash]);
    run_text(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_contents_produces_parseable_hunks() {
        let d = diff_contents("src/a.ts", "a\nb\nc\n", "a\nB\nc\nd\n").unwrap();
        let files = crate::parse_diff(&d);
        assert_eq!(files.len(), 1, "{d}");
        assert_eq!(files[0].stats(), (2, 1));
        assert!(d.contains("b/src/a.ts"));
        assert!(diff_contents("x", "same\n", "same\n").unwrap().is_empty());
    }
    use std::fs;

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jt-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success(), "{args:?}");
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "Test"]);
        g(&["config", "commit.gpgsign", "false"]);
        dir
    }

    #[test]
    fn status_stage_commit_flow() {
        let r = repo("flow");
        fs::write(r.join("a.txt"), "one\n").unwrap();
        let st = status(&r).unwrap();
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.files[0].unstaged, Some(ChangeKind::Untracked));

        stage(&r, &["a.txt"]).unwrap();
        let st = status(&r).unwrap();
        assert_eq!(st.files[0].staged, Some(ChangeKind::Added));
        unstage(&r, &["a.txt"]).unwrap(); // no HEAD yet
        assert_eq!(status(&r).unwrap().files[0].unstaged, Some(ChangeKind::Untracked));

        stage(&r, &["a.txt"]).unwrap();
        assert!(commit(&r, "  ", false).is_err());
        let summary = commit(&r, "first commit", false).unwrap();
        assert!(summary.contains("first commit"), "{summary}");
        assert!(status(&r).unwrap().files.is_empty());

        fs::write(r.join("a.txt"), "one\ntwo\n").unwrap();
        let st = status(&r).unwrap();
        assert_eq!(st.files[0].unstaged, Some(ChangeKind::Modified));
        let d = file_diff(&r, &st.files[0], false).unwrap();
        assert!(d.contains("+two"));

        discard(&r, &st.files[0]).unwrap();
        assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "one\n");

        let log = log(&r, 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].subject, "first commit");
        fs::remove_dir_all(r).unwrap();
    }

    #[test]
    fn branches_and_checkout() {
        let r = repo("branches");
        fs::write(r.join("a"), "x").unwrap();
        stage(&r, &["a"]).unwrap();
        commit(&r, "init", false).unwrap();
        create_branch(&r, "feature/x").unwrap();
        let bs = branches(&r).unwrap();
        assert!(bs.iter().any(|b| b.name == "feature/x" && b.is_current));
        let main = bs.iter().find(|b| b.name == "main").unwrap().clone();
        checkout(&r, &main).unwrap();
        assert_eq!(status(&r).unwrap().branch.as_deref(), Some("main"));
        fs::remove_dir_all(r).unwrap();
    }

    #[test]
    fn stage_single_hunk() {
        let r = repo("hunk");
        let base: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        fs::write(r.join("f.txt"), &base).unwrap();
        stage(&r, &["f.txt"]).unwrap();
        commit(&r, "base", false).unwrap();
        let changed = base.replace("line 2\n", "line 2 changed\n").replace("line 28\n", "line 28 changed\n");
        fs::write(r.join("f.txt"), changed).unwrap();
        let st = status(&r).unwrap();
        let d = parse_diff(&file_diff(&r, &st.files[0], false).unwrap());
        // Full context: one hunk with two separate change blocks.
        assert_eq!(d[0].hunks.len(), 1);
        let blocks = d[0].hunks[0].change_blocks();
        assert_eq!(blocks.len(), 2);
        apply_to_index(&r, &d[0].block_patch(0, blocks[0].clone()), false).unwrap();
        let staged = parse_diff(&file_diff(&r, &st.files[0], true).unwrap());
        let sb = staged[0].hunks[0].change_blocks();
        assert_eq!(sb.len(), 1);
        assert!(staged[0].hunks[0].lines[sb[0].clone()].iter().any(|l| l.text == "line 2 changed"));
        // Unstage it again.
        apply_to_index(&r, &staged[0].block_patch(0, sb[0].clone()), true).unwrap();
        assert!(file_diff(&r, &st.files[0], true).unwrap().is_empty());
        fs::remove_dir_all(r).unwrap();
    }

    #[test]
    fn parses_renames_and_conflicts() {
        let raw = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\0old.txt\0\
u UU N... 100644 100644 100644 100644 a b c conflict.txt\0? untracked.txt\0";
        let st = parse_status(raw);
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!(st.files[0].path, "new.txt");
        assert_eq!(st.files[0].orig_path.as_deref(), Some("old.txt"));
        assert_eq!(st.files[0].staged, Some(ChangeKind::Renamed));
        assert_eq!(st.conflicted().count(), 1);
        assert_eq!(st.files[2].unstaged, Some(ChangeKind::Untracked));
    }
}
