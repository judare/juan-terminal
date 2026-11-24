//! Projects, repositories and persisted workspace state.
//!
//! State is a single TOML file at `~/Library/Application Support/juan-terminal/state.toml`
//! (override the directory with `JUAN_TERMINAL_STATE_DIR`). Writes are atomic (temp + rename).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod settings;
pub use settings::*;

pub const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorkspaceState {
    pub version: u32,
    pub active_project: Option<String>,
    pub sidebar_width: Option<f32>,
    pub terminal_panel_height: Option<f32>,
    pub terminal_panel_width: Option<f32>,
    /// Save dirty files when the window loses focus.
    pub autosave: bool,
    /// Light color theme (default: dark).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub light_theme: bool,
    pub font_size: Option<f32>,
    pub window: Option<WindowGeometry>,
    pub projects: Vec<Project>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// Optional folder the project was created from; repositories may live anywhere.
    pub root: Option<PathBuf>,
    pub repositories: Vec<Repository>,
    /// Extra environment variables for every terminal in this project.
    pub env: BTreeMap<String, String>,
    pub commands: Vec<ProjectCommand>,
    /// Terminal sessions to restore (working directories, titles).
    pub terminals: Vec<TerminalState>,
    pub active_terminal: Option<usize>,
    /// Files open in the editor, restored on activation.
    pub open_files: Vec<PathBuf>,
    pub active_file: Option<usize>,
    /// Expanded folders in the file tree.
    pub expanded: Vec<PathBuf>,
    /// SSH servers for this project (Servers tab, SFTP, Claude's ssh_exec tool).
    /// Omitted when empty so the TOML stays hand-editable (`[[projects.servers]]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<SshServer>,
    /// Emergency override of the project's schedule (settings.toml): open until this Unix time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unlocked_until: Option<i64>,
    /// Sidebar icon (PNG/SVG/WebP…). Relative paths resolve against `<state dir>/icons/`.
    /// When unset, `<state dir>/icons/<id>.{png,svg,webp,jpg}` is used if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
}

/// An SSH destination. Only the key's *path* is stored, never key material or passwords.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SshServer {
    pub id: String,
    pub name: String,
    pub host: String,
    /// 0 = default (22).
    pub port: u16,
    pub user: String,
    pub identity_file: Option<PathBuf>,
    /// Directory the SFTP browser starts in (empty = remote home).
    pub start_dir: String,
}

impl SshServer {
    pub fn port_or_default(&self) -> u16 {
        if self.port == 0 { 22 } else { self.port }
    }
    pub fn destination(&self) -> String {
        if self.user.is_empty() { self.host.clone() } else { format!("{}@{}", self.user, self.host) }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Repository {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectCommand {
    pub name: String,
    pub command: String,
    /// Repository name to run in; project root / first repository when absent.
    pub repository: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TerminalState {
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Repository this terminal belongs to, if any.
    pub repository: Option<String>,
    /// Saved output (`scrollback/<id>.ansi` in the state dir), shown again on restore.
    pub scrollback: Option<String>,
}

impl WorkspaceState {
    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn project_mut(&mut self, id: &str) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    /// Create a project from a folder, auto-registering the Git repositories found inside.
    /// Returns the new project's id. Never moves or modifies the folder.
    pub fn add_project_from_folder(&mut self, folder: &Path) -> String {
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.display().to_string());
        let id = self.unique_id(&slug(&name));
        let mut project = Project { id: id.clone(), name, root: Some(folder.to_path_buf()), ..Default::default() };
        for repo in discover_repositories(folder, 2) {
            project.add_repository(&repo);
        }
        self.projects.push(project);
        self.active_project = Some(id.clone());
        id
    }

    pub fn remove_project(&mut self, id: &str) {
        self.projects.retain(|p| p.id != id);
        if self.active_project.as_deref() == Some(id) {
            self.active_project = self.projects.first().map(|p| p.id.clone());
        }
    }

    fn unique_id(&self, base: &str) -> String {
        let base = if base.is_empty() { "project" } else { base };
        let mut id = base.to_string();
        let mut n = 2;
        while self.project(&id).is_some() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}

impl Project {
    /// Register a repository. Returns false if the path is already registered.
    pub fn add_repository(&mut self, path: &Path) -> bool {
        if self.repositories.iter().any(|r| r.path == path) {
            return false;
        }
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let mut name = base.clone();
        let mut n = 2;
        while self.repositories.iter().any(|r| r.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        self.repositories.push(Repository { name, path: path.to_path_buf() });
        true
    }

    pub fn remove_repository(&mut self, name: &str) {
        self.repositories.retain(|r| r.name != name);
    }

    /// Default working directory for new terminals.
    pub fn default_cwd(&self) -> Option<PathBuf> {
        self.root.clone().or_else(|| self.repositories.first().map(|r| r.path.clone()))
    }

    /// Resolved icon file, if one is configured or found by convention and exists.
    pub fn icon_path(&self) -> Option<PathBuf> {
        let dir = state_dir().join("icons");
        if let Some(icon) = &self.icon {
            let path = if icon.is_absolute() { icon.clone() } else { dir.join(icon) };
            return path.is_file().then_some(path);
        }
        ["png", "svg", "webp", "jpg", "jpeg"].iter().map(|ext| dir.join(format!("{}.{ext}", self.id))).find(|p| p.is_file())
    }
}

/// True when `path` is the top level of a Git work tree (`.git` dir, or `.git` file for
/// worktrees/submodules). Does not run `git`.
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Current branch name (or short detached SHA) by reading `HEAD`. Cheap enough for the
/// sidebar label; every real Git operation goes through the `git` executable.
pub fn head_branch(repo: &Path) -> Option<String> {
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_file() {
        // Worktrees and submodules: `.git` is a file containing `gitdir: <path>`.
        let text = fs::read_to_string(&dot_git).ok()?;
        let dir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if dir.is_absolute() { dir } else { repo.join(dir) }
    } else {
        dot_git
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => Some(head.get(..7)?.to_string()),
    }
}

/// Find Git repositories at `root` and below, up to `max_depth` levels. Does not descend into
/// repositories, hidden directories, or dependency/build folders. Bounded, one-shot scan.
pub fn discover_repositories(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    const SKIP: &[&str] = &["node_modules", "target", "dist", "build", "vendor", "Library"];
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if is_git_repo(&dir) {
            found.push(dir);
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    found.sort();
    found
}

fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Location of the state file.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("JUAN_TERMINAL_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/juan-terminal")
}

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { path: dir.join("state.toml") }
    }

    pub fn default_location() -> Self {
        Self::new(state_dir())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load state. A missing file yields defaults; a corrupt file is backed up and replaced
    /// with defaults so the app always starts.
    pub fn load(&self) -> WorkspaceState {
        match fs::read_to_string(&self.path) {
            Ok(text) => match toml::from_str::<WorkspaceState>(&text) {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("juan-terminal: state file is invalid ({err}); backing it up");
                    let _ = fs::rename(&self.path, self.path.with_extension("toml.bak"));
                    WorkspaceState { version: STATE_VERSION, ..Default::default() }
                }
            },
            Err(_) => WorkspaceState { version: STATE_VERSION, ..Default::default() },
        }
    }

    pub fn save(&self, state: &WorkspaceState) -> Result<()> {
        let dir = self.path.parent().context("state path has no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut state = state.clone();
        state.version = STATE_VERSION;
        let text = toml::to_string_pretty(&state).context("serializing state")?;
        let tmp = self.path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).context("replacing state file")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jt-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_nested_repositories() {
        let root = tempdir("discover");
        for repo in ["frontend", "backend", "services/payments"] {
            fs::create_dir_all(root.join(repo).join(".git")).unwrap();
        }
        fs::create_dir_all(root.join("node_modules/pkg/.git")).unwrap();
        fs::create_dir_all(root.join(".workspace")).unwrap();
        fs::create_dir_all(root.join("frontend/inner/.git")).unwrap(); // inside a repo: ignored
        let found: Vec<_> = discover_repositories(&root, 2)
            .into_iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        assert_eq!(found, vec![PathBuf::from("backend"), PathBuf::from("frontend"), PathBuf::from("services/payments")]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_from_folder_and_unique_names() {
        let root = tempdir("project");
        fs::create_dir_all(root.join("a/.git")).unwrap();
        let mut state = WorkspaceState::default();
        let id = state.add_project_from_folder(&root);
        let id2 = state.add_project_from_folder(&root);
        assert_ne!(id, id2);
        let p = state.project_mut(&id).unwrap();
        assert_eq!(p.repositories.len(), 1);
        assert!(!p.add_repository(&root.join("a")));
        assert!(p.add_repository(Path::new("/elsewhere/a")));
        assert_eq!(p.repositories[1].name, "a-2");
        state.remove_project(&id2);
        assert_eq!(state.active_project.as_deref(), Some(id.as_str()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn roundtrip_and_corrupt_recovery() {
        let dir = tempdir("store");
        let store = StateStore::new(dir.clone());
        assert!(store.load().projects.is_empty());

        let mut state = WorkspaceState::default();
        let id = state.add_project_from_folder(&dir);
        let p = state.project_mut(&id).unwrap();
        p.env.insert("NODE_ENV".into(), "development".into());
        p.commands.push(ProjectCommand { name: "dev".into(), command: "pnpm dev".into(), repository: None });
        p.terminals.push(TerminalState { title: None, cwd: Some(dir.clone()), repository: None, scrollback: None });
        state.window = Some(WindowGeometry { x: 1.0, y: 2.0, width: 800.0, height: 600.0 });
        store.save(&state).unwrap();
        let mut loaded = store.load();
        loaded.version = 0;
        state.version = 0;
        assert_eq!(loaded, state);

        fs::write(store.path(), "this is [not valid").unwrap();
        assert!(store.load().projects.is_empty());
        assert!(dir.join("state.toml.bak").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_head_branch() {
        let root = tempdir("head");
        fs::create_dir_all(root.join("r/.git")).unwrap();
        fs::write(root.join("r/.git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(head_branch(&root.join("r")).as_deref(), Some("feature/x"));
        fs::write(root.join("r/.git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(head_branch(&root.join("r")).as_deref(), Some("0123456"));
        fs::create_dir_all(root.join("wt")).unwrap();
        fs::write(root.join("wt/.git"), "gitdir: ../r/.git\n").unwrap();
        assert_eq!(head_branch(&root.join("wt")).as_deref(), Some("0123456"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Work App"), "work-app");
        assert_eq!(slug("!!"), "");
    }
}
