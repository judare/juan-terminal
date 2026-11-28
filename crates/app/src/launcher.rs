//! Global launcher (⌘Space, system-wide): applications, URLs, juan-terminal projects and the
//! active project's files — a Raycast/Spotlight replacement. Shown in a non-activating panel so
//! opening it doesn't pull the main window forward unless a project/file is picked.

#![allow(unexpected_cfgs)] // objc 0.2 macros

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use gpui::{
    App, AppContext, Bounds, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Global, HighlightStyle, Render, ScrollStrategy, SharedString, StyledText, Subscription,
    UniformListScrollHandle, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, div, point, prelude::*, px, size, uniform_list,
};
use project_search::{IndexedFile, fuzzy_filter, fuzzy_match};

use crate::quick_open::{SelectNext, SelectPrevious};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;
use crate::workspace::Workspace;

const WIDTH: f32 = 720.;
const HEIGHT: f32 = 500.;
const ROW_H: f32 = 34.;

// ---- data -------------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppEntry {
    name: String,
    path: PathBuf,
    icon: PathBuf,
}

#[derive(Clone)]
pub struct ProjectEntry {
    pub id: String,
    pub name: String,
    pub active: bool,
    pub icon: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub repos: Vec<RepoEntry>,
    pub servers: Vec<ServerEntry>,
    /// (label, dir, command line): filled in the background.
    pub dev: Vec<(String, PathBuf, String)>,
}

/// What the workspace hands the launcher each time it opens.
pub struct Sources {
    pub projects: Vec<ProjectEntry>,
    pub project_name: Option<String>,
    pub files: Arc<Vec<IndexedFile>>,
    pub roots: Vec<project_search::Root>,
}

fn icon_dir() -> PathBuf {
    workspace_state::state_dir().join("app-icons")
}

fn icon_path_for(app: &Path) -> PathBuf {
    let safe: String = app.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    icon_dir().join(format!("{safe}.png"))
}

/// Installed applications (cheap directory listing; run on every open so new installs show).
fn scan_apps() -> Vec<AppEntry> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dirs = [
        PathBuf::from("/Applications"),
        PathBuf::from("/Applications/Utilities"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        home.join("Applications"),
        home.join("Applications/Chrome Apps.localized"),
    ];
    let mut out: Vec<AppEntry> = Vec::new();
    let push = |p: PathBuf, out: &mut Vec<AppEntry>| {
        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { return };
        if out.iter().any(|a| a.name == name) {
            return;
        }
        let icon = icon_path_for(&p);
        out.push(AppEntry { name, path: p, icon });
    };
    for dir in &dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "app") {
                push(p, &mut out);
            } else if p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                // Vendor folders, e.g. /Applications/Adobe Photoshop/Adobe Photoshop.app.
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for s in sub.flatten() {
                        let sp = s.path();
                        if sp.extension().is_some_and(|x| x == "app") {
                            push(sp, &mut out);
                        }
                    }
                }
            }
        }
    }
    push(PathBuf::from("/System/Library/CoreServices/Finder.app"), &mut out);
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Render missing app icons to PNG (NSWorkspace iconForFile) on a background thread.
fn cache_icons(apps: &[AppEntry]) {
    let missing: Vec<(PathBuf, PathBuf)> = apps.iter().filter(|a| !a.icon.exists()).map(|a| (a.path.clone(), a.icon.clone())).collect();
    if missing.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(icon_dir());
        for (app, out) in missing {
            if let Some(png) = unsafe { app_icon_png(&app, 64.) } {
                let _ = std::fs::write(out, png);
            }
        }
    });
}

#[repr(C)]
struct NSRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

unsafe fn app_icon_png(app: &Path, side: f64) -> Option<Vec<u8>> {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    type Id = *mut Object;
    unsafe {
        let pool: Id = msg_send![class!(NSAutoreleasePool), new];
        let result = (|| {
            let path = std::ffi::CString::new(app.to_str()?).ok()?;
            let ns_path: Id = msg_send![class!(NSString), stringWithUTF8String: path.as_ptr()];
            let ws: Id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let icon: Id = msg_send![ws, iconForFile: ns_path];
            if icon.is_null() {
                return None;
            }
            let mut rect = NSRect { x: 0., y: 0., w: side, h: side };
            let nil: Id = std::ptr::null_mut();
            let cg: *mut std::ffi::c_void = msg_send![icon, CGImageForProposedRect: &mut rect as *mut NSRect as *mut std::ffi::c_void context: nil hints: nil];
            if cg.is_null() {
                return None;
            }
            let rep: Id = msg_send![class!(NSBitmapImageRep), alloc];
            let rep: Id = msg_send![rep, initWithCGImage: cg];
            if rep.is_null() {
                return None;
            }
            let props: Id = msg_send![class!(NSDictionary), dictionary];
            // NSBitmapImageFileTypePNG = 4
            let data: Id = msg_send![rep, representationUsingType: 4u64 properties: props];
            let bytes = if data.is_null() {
                None
            } else {
                let len: usize = msg_send![data, length];
                let ptr: *const u8 = msg_send![data, bytes];
                Some(std::slice::from_raw_parts(ptr, len).to_vec())
            };
            let _: () = msg_send![rep, release];
            bytes
        })();
        let _: () = msg_send![pool, drain];
        result
    }
}

/// `example.com`, `https://…`, `localhost:3000`, `192.168.0.1:8080/x` → URL to open.
fn as_url(q: &str) -> Option<String> {
    let q = q.trim();
    if q.is_empty() || q.contains(char::is_whitespace) {
        return None;
    }
    let lower = q.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file://") {
        return Some(q.to_string());
    }
    let host_port = lower.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_port.split(':').next().unwrap_or("");
    let port_ok = host_port.split_once(':').is_none_or(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if !port_ok {
        return None;
    }
    if host == "localhost" || host == "127.0.0.1" || (host.split('.').count() == 4 && host.split('.').all(|p| p.parse::<u8>().is_ok())) {
        return Some(format!("http://{q}"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    let tld = labels.last().copied().unwrap_or("");
    if labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        // "main.rs", "index.ts": file names, not sites.
        && !matches!(tld, "rs" | "ts" | "tsx" | "js" | "jsx" | "json" | "md" | "toml" | "yaml" | "yml" | "lock" | "txt" | "css" | "html" | "vue" | "py" | "go" | "sh" | "svg" | "png" | "env")
    {
        return Some(format!("https://{q}"));
    }
    None
}

// ---- project commands -------------------------------------------------------------------

/// Origin remote → browser URL (`git@github.com:o/r.git` → `https://github.com/o/r`).
pub(crate) fn web_url(repo: &Path) -> Option<String> {
    let cfg = std::fs::read_to_string(repo.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    let mut first: Option<String> = None;
    for line in cfg.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_origin = l == "[remote \"origin\"]";
            continue;
        }
        let Some(v) = l.strip_prefix("url").map(str::trim_start).and_then(|r| r.strip_prefix('=')) else { continue };
        let v = v.trim().to_string();
        if in_origin {
            return remote_to_web(&v);
        }
        first.get_or_insert(v);
    }
    first.and_then(|v| remote_to_web(&v))
}

fn remote_to_web(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches('/');
    let r = r.strip_suffix(".git").unwrap_or(r);
    if let Some(rest) = r.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map(|x| x.1).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        return Some(format!("https://{}/{path}", host.split(':').next()?));
    }
    if let Some((scheme, rest)) = r.split_once("://") {
        if scheme != "https" && scheme != "http" {
            return None;
        }
        let rest = rest.split_once('@').map(|x| x.1).unwrap_or(rest);
        return Some(format!("https://{rest}"));
    }
    // scp-like: git@host:owner/repo
    let rest = r.split_once('@').map(|x| x.1).unwrap_or(r);
    let (host, path) = rest.split_once(':')?;
    (!host.is_empty() && !path.is_empty()).then(|| format!("https://{host}/{path}"))
}

#[derive(Clone)]
pub struct RepoEntry {
    pub name: String,
    pub path: PathBuf,
    web: Option<String>,
}

impl RepoEntry {
    pub fn new(name: String, path: PathBuf) -> Self {
        let web = web_url(&path);
        Self { name, path, web }
    }
}

#[derive(Clone)]
pub struct ServerEntry {
    pub id: String,
    pub name: String,
    /// `user@host`
    pub target: String,
}

/// What to do inside juan-terminal once the project is active.
#[derive(Clone)]
pub enum ProjectAction {
    Open,
    Claude(Option<PathBuf>, Option<String>),
    Terminal(Option<PathBuf>, Option<String>),
    Ssh(String),
    Actions,
    Git,
    Servers,
    Dev(PathBuf),
}

#[derive(Clone)]
pub enum Pick {
    /// `/usr/bin/open` (URL or path).
    Open(String),
    App(PathBuf),
    File(PathBuf),
    Copy(String),
    Project(String, ProjectAction),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Claude,
    Terminal,
    Ssh(usize),
    /// Drill into a repository.
    Repo,
    Actions,
    Git,
    Servers,
    Dev(usize),
    Finder,
    GitHub,
    GitHubActions,
    PullRequests,
    CopyPath,
}

impl Kind {
    fn keywords(self) -> &'static [&'static str] {
        match self {
            Kind::Claude => &["claude", "cc", "ai"],
            Kind::Terminal => &["terminal", "term", "shell", "sh"],
            Kind::Ssh(_) => &["ssh", "server", "servers"],
            Kind::Repo => &["repo", "repos", "repositories"],
            Kind::Actions => &["actions", "ci", "gha", "workflows"],
            Kind::Git => &["git", "changes", "diff"],
            Kind::Servers => &["servers", "dev", "ports"],
            Kind::Dev(_) => &["dev", "run", "start"],
            Kind::Finder => &["finder", "reveal", "folder"],
            Kind::GitHub => &["github", "gh", "web"],
            Kind::GitHubActions => &["actions", "ci", "gha", "github", "gh"],
            Kind::PullRequests => &["pr", "prs", "pulls", "github", "gh"],
            Kind::CopyPath => &["copy", "path"],
        }
    }

    fn icon(self) -> (&'static str, u32) {
        match self {
            Kind::Claude => ("icons/claude.svg", 0xd97757),
            Kind::Terminal => ("icons/terminal.svg", 0x98c379),
            Kind::Ssh(_) => ("icons/server.svg", 0x56b6c2),
            Kind::Repo => ("icons/repo.svg", theme::ACCENT),
            Kind::Actions | Kind::GitHubActions => ("icons/action-workflow.svg", 0xc678dd),
            Kind::Git => ("icons/git.svg", 0xe5704b),
            Kind::Servers => ("icons/ports.svg", 0x61afef),
            Kind::Dev(_) => ("icons/action-running.svg", 0x98c379),
            Kind::Finder => ("icons/finder.svg", 0x61afef),
            Kind::GitHub | Kind::PullRequests => ("icons/external.svg", theme::TEXT_MUTED),
            Kind::CopyPath => ("icons/copy.svg", theme::TEXT_MUTED),
        }
    }

    fn badge(self) -> &'static str {
        match self {
            Kind::Claude => "Claude Code",
            Kind::Terminal => "Terminal",
            Kind::Ssh(_) => "SSH",
            Kind::Repo => "Repository",
            Kind::Actions => "Actions",
            Kind::Git => "Git",
            Kind::Servers => "Servers",
            Kind::Dev(_) => "Dev server",
            Kind::Finder => "Finder",
            Kind::GitHub | Kind::GitHubActions | Kind::PullRequests => "GitHub",
            Kind::CopyPath => "Clipboard",
        }
    }
}

struct Cmd {
    project: usize,
    repo: Option<usize>,
    /// Shown inside a repository's scope (vs. the project's).
    repo_level: bool,
    kind: Kind,
    title: String,
    subtitle: String,
    /// Text matched after a keyword ("ssh bemo" → "bemo" against this).
    target: String,
}

fn build_cmds(projects: &[ProjectEntry]) -> Vec<Cmd> {
    let mut out = Vec::new();
    for (pi, p) in projects.iter().enumerate() {
        let mut add = |repo: Option<usize>, repo_level: bool, kind: Kind, title: String, subtitle: String, target: String| {
            out.push(Cmd { project: pi, repo, repo_level, kind, title, subtitle, target });
        };
        let pname = p.name.clone();
        add(None, false, Kind::Claude, "Open Claude Code".into(), pname.clone(), pname.clone());
        add(None, false, Kind::Terminal, "New terminal".into(), pname.clone(), pname.clone());
        for (si, s) in p.servers.iter().enumerate() {
            add(None, false, Kind::Ssh(si), format!("SSH · {}", s.name), format!("{pname} — {}", s.target), format!("{pname} {}", s.name));
        }
        for (ri, r) in p.repos.iter().enumerate() {
            add(Some(ri), false, Kind::Repo, r.name.clone(), pname.clone(), format!("{pname} {}", r.name));
        }
        if !p.repos.is_empty() {
            add(None, false, Kind::Actions, "GitHub Actions".into(), pname.clone(), pname.clone());
            add(None, false, Kind::Git, "Git changes".into(), pname.clone(), pname.clone());
        }
        add(None, false, Kind::Servers, "Servers & dev servers".into(), pname.clone(), pname.clone());
        for (di, d) in p.dev.iter().enumerate() {
            add(None, false, Kind::Dev(di), format!("Run dev · {}", d.0), format!("{pname} — {}", d.2), format!("{pname} {}", d.0));
        }
        if p.root.is_some() {
            add(None, false, Kind::Finder, "Reveal in Finder".into(), pname.clone(), pname.clone());
        }
        for (ri, r) in p.repos.iter().enumerate() {
            let sub = format!("{pname} / {}", r.name);
            let target = format!("{pname} {}", r.name);
            add(Some(ri), true, Kind::Claude, "Open Claude Code".into(), sub.clone(), target.clone());
            add(Some(ri), true, Kind::Terminal, "New terminal".into(), sub.clone(), target.clone());
            if let Some(web) = &r.web {
                add(Some(ri), true, Kind::GitHub, "Open on the web".into(), web.trim_start_matches("https://").into(), target.clone());
                if web.contains("github.com") {
                    add(Some(ri), true, Kind::GitHubActions, "Actions on GitHub".into(), sub.clone(), target.clone());
                    add(Some(ri), true, Kind::PullRequests, "Pull requests".into(), sub.clone(), target.clone());
                }
            }
            add(Some(ri), true, Kind::Finder, "Reveal in Finder".into(), sub.clone(), target.clone());
            add(Some(ri), true, Kind::CopyPath, "Copy path".into(), r.path.display().to_string(), target.clone());
        }
    }
    out
}

// ---- view -------------------------------------------------------------------------------

#[derive(Clone)]
enum Item {
    Header(SharedString),
    /// Calculator / currency answer: (copied value, shown result, interpretation).
    Calc(String, String, String),
    Url(String),
    App(usize, Vec<usize>),
    Project(usize, Vec<usize>),
    File(usize, Vec<usize>),
    Cmd(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Project(usize),
    Repo(usize, usize),
}

pub enum LauncherEvent {
    Pick(Pick),
    Dismiss,
}

pub struct Launcher {
    input: Entity<TextInput>,
    apps: Vec<AppEntry>,
    projects: Vec<ProjectEntry>,
    cmds: Vec<Cmd>,
    scope: Option<Scope>,
    project_name: Option<String>,
    files: Arc<Vec<IndexedFile>>,
    items: Vec<Item>,
    selected: usize,
    scroll: UniformListScrollHandle,
    was_active: bool,
    _subs: Vec<Subscription>,
    _index_task: Option<gpui::Task<()>>,
    _dev_task: Option<gpui::Task<()>>,
}

impl EventEmitter<LauncherEvent> for Launcher {}

const ALL_KEYWORDS: &[Kind] = &[
    Kind::Claude,
    Kind::Terminal,
    Kind::Ssh(0),
    Kind::Repo,
    Kind::Actions,
    Kind::Git,
    Kind::Servers,
    Kind::Dev(0),
    Kind::Finder,
    Kind::GitHub,
    Kind::GitHubActions,
    Kind::PullRequests,
    Kind::CopyPath,
];

impl Launcher {
    fn new(sources: Sources, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search apps, projects, files or type a URL…  (ssh, claude, repo, actions, dev…)", cx));
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
                TextInputEvent::Changed => this.update_items(cx),
                TextInputEvent::Submit { .. } => this.confirm(cx),
                TextInputEvent::Cancel => {
                    if !this.pop_scope(cx) {
                        cx.emit(LauncherEvent::Dismiss);
                    }
                }
            }),
            // Clicking elsewhere (another app / window) closes it, like Spotlight.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.was_active = true;
                } else if this.was_active {
                    cx.emit(LauncherEvent::Dismiss);
                }
            }),
        ];
        window.focus(&input.focus_handle(cx));
        let apps = scan_apps();
        cache_icons(&apps);
        // No index yet for this project (⌘P never used): build one for the launcher.
        let index_task = (sources.files.is_empty() && !sources.roots.is_empty()).then(|| {
            let roots = sources.roots.clone();
            cx.spawn(async move |this, cx| {
                let files = cx.background_executor().spawn(async move { project_search::list_files(&roots) }).await;
                let _ = this.update(cx, |this, cx| {
                    this.files = Arc::new(files);
                    this.update_items(cx);
                });
            })
        });
        // Dev scripts of every project (package.json scan) off the main thread.
        let dev_roots: Vec<Vec<(String, PathBuf)>> = sources
            .projects
            .iter()
            .map(|p| {
                let mut v: Vec<(String, PathBuf)> = p.repos.iter().map(|r| (r.name.clone(), r.path.clone())).collect();
                if let Some(root) = &p.root {
                    v.push((p.name.clone(), root.clone()));
                }
                v
            })
            .collect();
        let dev_task = cx.spawn(async move |this, cx| {
            let dev = cx
                .background_executor()
                .spawn(async move {
                    dev_roots
                        .iter()
                        .map(|roots| crate::ports::dev_command_list(roots).into_iter().map(|d| { let line = d.command_line(); (d.label, d.dir, line) }).collect::<Vec<_>>())
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (p, d) in this.projects.iter_mut().zip(dev) {
                    p.dev = d;
                }
                this.cmds = build_cmds(&this.projects);
                this.update_items(cx);
            });
        });
        // Exchange rates for "1 usd to cop" (disk cache, refreshed when stale).
        cx.spawn(async move |this, cx| {
            let changed = cx.background_executor().spawn(async { crate::calc::refresh_rates() }).await;
            if changed {
                let _ = this.update(cx, |this, cx| this.update_items(cx));
            }
        })
        .detach();
        let cmds = build_cmds(&sources.projects);
        let mut this = Self {
            input,
            apps,
            projects: sources.projects,
            cmds,
            scope: None,
            project_name: sources.project_name,
            files: sources.files,
            items: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            was_active: false,
            _subs: subs,
            _index_task: index_task,
            _dev_task: Some(dev_task),
        };
        this.update_items(cx);
        this
    }

    fn update_items(&mut self, cx: &mut Context<Self>) {
        let prev_key = self.items.get(self.selected).map(|i| self.item_key(i));
        let query = self.input.read(cx).content.trim().to_string();
        let mut items = Vec::new();
        let section = |items: &mut Vec<Item>, title: String, v: Vec<Item>| {
            if !v.is_empty() {
                items.push(Item::Header(title.into()));
                items.extend(v);
            }
        };

        if let Some(scope) = self.scope {
            let mut v: Vec<(i64, usize)> = self
                .cmds
                .iter()
                .enumerate()
                .filter(|(_, c)| match scope {
                    Scope::Project(p) => c.project == p && !c.repo_level,
                    Scope::Repo(p, r) => c.project == p && c.repo == Some(r) && c.repo_level,
                })
                .filter_map(|(i, c)| {
                    let hay = format!("{} {} {}", c.title, c.kind.badge(), c.kind.keywords().join(" "));
                    fuzzy_match(&query, &hay).map(|(s, _)| (s, i))
                })
                .collect();
            if !query.is_empty() {
                v.sort_by(|a, b| b.0.cmp(&a.0));
            }
            let title = match scope {
                Scope::Project(p) => format!("{} — actions", self.projects[p].name),
                Scope::Repo(p, r) => format!("{} / {}", self.projects[p].name, self.projects[p].repos[r].name),
            };
            section(&mut items, title, v.into_iter().map(|(_, i)| Item::Cmd(i)).collect());
        } else {
            if let Some(a) = crate::calc::answer(&query) {
                items.push(Item::Calc(a.value, a.display, a.detail));
            }
            if let Some(url) = as_url(&query) {
                items.push(Item::Url(url));
            }
            let rank = |names: Vec<&str>, q: &str, limit: usize| -> Vec<(usize, Vec<usize>)> {
                let mut v: Vec<(i64, usize, Vec<usize>)> = names.iter().enumerate().filter_map(|(i, n)| fuzzy_match(q, n).map(|(s, p)| (s, i, p))).collect();
                if !q.is_empty() {
                    v.sort_by(|a, b| b.0.cmp(&a.0).then(names[a.1].len().cmp(&names[b.1].len())));
                }
                v.into_iter().take(limit).map(|(_, i, p)| (i, p)).collect()
            };
            let empty = query.is_empty();
            // "ssh bemo", "claude api", "repos", "dev front"…
            let (first, rest) = query.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((query.as_str(), ""));
            let first_l = first.to_lowercase();
            let keyword = (!empty && ALL_KEYWORDS.iter().any(|k| k.keywords().contains(&first_l.as_str()))).then_some(first_l);

            let cmds: Vec<usize> = if empty {
                Vec::new()
            } else if let Some(kw) = &keyword {
                let mut v: Vec<(i64, usize)> = self
                    .cmds
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.kind.keywords().contains(&kw.as_str()))
                    .filter_map(|(i, c)| fuzzy_match(rest, &c.target).map(|(s, _)| (s + if c.repo_level { 0 } else { 6 }, i)))
                    .collect();
                if !rest.is_empty() {
                    v.sort_by(|a, b| b.0.cmp(&a.0));
                }
                v.into_iter().take(60).map(|(_, i)| i).collect()
            } else {
                let mut v: Vec<(i64, usize)> = self
                    .cmds
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| {
                        let hay = format!("{} {}", c.title, c.target);
                        let s = fuzzy_match(&query, &c.target).map(|x| x.0).into_iter().chain(fuzzy_match(&query, &hay).map(|x| x.0 - 20)).max()?;
                        Some((s + if c.repo_level { 0 } else { 6 }, i))
                    })
                    .collect();
                v.sort_by(|a, b| b.0.cmp(&a.0));
                v.into_iter().take(12).map(|(_, i)| i).collect()
            };
            let cmd_items: Vec<Item> = cmds.into_iter().map(Item::Cmd).collect();

            let apps = rank(self.apps.iter().map(|a| a.name.as_str()).collect(), &query, if empty { usize::MAX } else { 8 });
            let app_items: Vec<Item> = apps.into_iter().map(|(i, p)| Item::App(i, p)).collect();

            if empty {
                let projects = rank(self.projects.iter().map(|p| p.name.as_str()).collect(), "", usize::MAX);
                section(&mut items, "Projects".into(), projects.into_iter().map(|(i, p)| Item::Project(i, p)).collect());
                section(&mut items, "Applications".into(), app_items);
            } else if keyword.is_some() {
                let projects = rank(self.projects.iter().map(|p| p.name.as_str()).collect(), &query, 5);
                section(&mut items, "Commands".into(), cmd_items);
                section(&mut items, "Projects".into(), projects.into_iter().map(|(i, p)| Item::Project(i, p)).collect());
                section(&mut items, "Applications".into(), app_items);
            } else {
                let projects = rank(self.projects.iter().map(|p| p.name.as_str()).collect(), &query, 5);
                let files = fuzzy_filter(&query, &self.files, 40);
                let project_items: Vec<Item> = projects.into_iter().map(|(i, p)| Item::Project(i, p)).collect();
                if project_items.is_empty() {
                    section(&mut items, "Applications".into(), app_items);
                    section(&mut items, "Commands".into(), cmd_items);
                } else {
                    section(&mut items, "Projects".into(), project_items);
                    section(&mut items, "Commands".into(), cmd_items);
                    section(&mut items, "Applications".into(), app_items);
                }
                let title = match &self.project_name {
                    Some(n) => format!("Files in {n}"),
                    None => "Files".into(),
                };
                section(&mut items, title, files.into_iter().map(|(i, p)| Item::File(i, p)).collect());
            }
        }
        self.items = items;
        // Background refreshes (dev scripts, file index) keep the selection when possible.
        let keep = prev_key.and_then(|k| self.items.iter().position(|i| self.item_key(i) == k));
        self.selected = keep.or_else(|| self.items.iter().position(|i| !matches!(i, Item::Header(_)))).unwrap_or(0);
        if keep.is_none() {
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// Identity of a row across rebuilds (only while the query is unchanged).
    fn item_key(&self, item: &Item) -> String {
        match item {
            Item::Header(t) => format!("h{t}"),
            Item::Url(u) => format!("u{u}"),
            Item::Calc(..) => "calc".into(),
            Item::App(i, _) => format!("a{i}"),
            Item::Project(i, _) => format!("p{i}"),
            Item::File(i, _) => format!("f{i}"),
            Item::Cmd(i) => {
                let c = &self.cmds[*i];
                format!("c{}-{:?}-{}-{}", c.project, c.repo, c.repo_level, c.title)
            }
        }
    }

    fn project_cwd(p: &ProjectEntry) -> Option<PathBuf> {
        p.root.clone().or_else(|| p.repos.first().map(|r| r.path.clone()))
    }

    fn cmd_pick(&self, c: &Cmd) -> Option<Pick> {
        let p = &self.projects[c.project];
        let repo = c.repo.map(|r| &p.repos[r]);
        let id = p.id.clone();
        let (cwd, repo_name) = match repo {
            Some(r) if c.repo_level => (Some(r.path.clone()), Some(r.name.clone())),
            _ => (Self::project_cwd(p), None),
        };
        let web = repo.and_then(|r| r.web.clone());
        Some(match c.kind {
            Kind::Claude => Pick::Project(id, ProjectAction::Claude(cwd, repo_name)),
            Kind::Terminal => Pick::Project(id, ProjectAction::Terminal(cwd, repo_name)),
            Kind::Ssh(si) => Pick::Project(id, ProjectAction::Ssh(p.servers.get(si)?.id.clone())),
            Kind::Repo => return None,
            Kind::Actions => Pick::Project(id, ProjectAction::Actions),
            Kind::Git => Pick::Project(id, ProjectAction::Git),
            Kind::Servers => Pick::Project(id, ProjectAction::Servers),
            Kind::Dev(di) => Pick::Project(id, ProjectAction::Dev(p.dev.get(di)?.1.clone())),
            Kind::Finder => Pick::Open(cwd?.display().to_string()),
            Kind::GitHub => Pick::Open(web?),
            Kind::GitHubActions => Pick::Open(format!("{}/actions", web?)),
            Kind::PullRequests => Pick::Open(format!("{}/pulls", web?)),
            Kind::CopyPath => Pick::Copy(cwd?.display().to_string()),
        })
    }

    fn pick(&self, ix: usize) -> Option<Pick> {
        Some(match self.items.get(ix)? {
            Item::Header(_) => return None,
            Item::Url(u) => Pick::Open(u.clone()),
            Item::Calc(v, ..) => Pick::Copy(v.clone()),
            Item::App(i, _) => Pick::App(self.apps[*i].path.clone()),
            Item::Project(i, _) => Pick::Project(self.projects[*i].id.clone(), ProjectAction::Open),
            Item::File(i, _) => Pick::File(self.files[*i].path.clone()),
            Item::Cmd(i) => return self.cmd_pick(&self.cmds[*i]),
        })
    }

    /// Scope a row drills into (Tab; Enter on a repository).
    fn drill_target(&self, ix: usize) -> Option<Scope> {
        match self.items.get(ix)? {
            Item::Project(i, _) => Some(Scope::Project(*i)),
            Item::Cmd(i) => {
                let c = &self.cmds[*i];
                match (c.kind, c.repo) {
                    (Kind::Repo, Some(r)) => Some(Scope::Repo(c.project, r)),
                    _ if self.scope.is_none() => Some(Scope::Project(c.project)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn enter_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = Some(scope);
        self.selected = usize::MAX;
        self.input.update(cx, |i, cx| i.set_text("", cx));
        self.update_items(cx);
    }

    /// Back out one level. False when already at the top.
    fn pop_scope(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.scope else { return false };
        self.scope = match scope {
            Scope::Repo(p, _) => Some(Scope::Project(p)),
            Scope::Project(_) => None,
        };
        self.selected = usize::MAX;
        self.input.update(cx, |i, cx| i.set_text("", cx));
        self.update_items(cx);
        true
    }

    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(Item::Cmd(i)) = self.items.get(ix) {
            if self.cmds[*i].kind == Kind::Repo {
                if let Some(s) = self.drill_target(ix) {
                    self.enter_scope(s, cx);
                }
                return;
            }
        }
        if let Some(p) = self.pick(ix) {
            cx.emit(LauncherEvent::Pick(p));
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        self.activate(self.selected, cx);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.items.len() as isize;
        if n == 0 {
            return;
        }
        let mut ix = self.selected.min(self.items.len() - 1) as isize;
        for _ in 0..n {
            ix = (ix + delta).rem_euclid(n);
            if !matches!(self.items[ix as usize], Item::Header(_)) {
                break;
            }
        }
        self.selected = ix as usize;
        // Keep the section header visible when moving to the first item.
        let target = if self.selected > 0 && matches!(self.items[self.selected - 1], Item::Header(_)) && delta < 0 { self.selected - 1 } else { self.selected };
        self.scroll.scroll_to_item(target, ScrollStrategy::Center);
        cx.notify();
    }

    fn on_key(&mut self, e: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        if k.modifiers.platform || k.modifiers.control || k.modifiers.alt {
            return;
        }
        match k.key.as_str() {
            "tab" if !k.modifiers.shift => {
                if let Some(s) = self.drill_target(self.selected) {
                    self.enter_scope(s, cx);
                }
                cx.stop_propagation();
            }
            "tab" => {
                self.pop_scope(cx);
                cx.stop_propagation();
            }
            "backspace" if self.scope.is_some() && self.input.read(cx).content.is_empty() => {
                self.pop_scope(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn project_icon(&self, i: usize, side: f32) -> gpui::AnyElement {
        match &self.projects[i].icon {
            Some(path) => gpui::img(path.clone()).size(px(side)).flex_none().rounded(px(4.)).object_fit(gpui::ObjectFit::Contain).into_any_element(),
            None => gpui::svg().path("icons/repo.svg").size(px(side - 2.)).flex_none().text_color(theme::rgb(theme::ACCENT)).into_any_element(),
        }
    }

    fn render_item(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let item = &self.items[ix];
        if let Item::Header(title) = item {
            return div()
                .id(ix)
                .w_full()
                .h(px(ROW_H))
                .px(px(18.))
                .pt(px(12.))
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::rgb(theme::TEXT_FAINT))
                .child(title.clone())
                .into_any_element();
        }
        let hl = |name: &str, positions: &[usize], offset: usize| -> StyledText {
            let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = positions
                .iter()
                .filter(|p| **p >= offset)
                .filter_map(|p| {
                    let start = name.char_indices().nth(p - offset)?.0;
                    let len = name[start..].chars().next()?.len_utf8();
                    Some((start..start + len, HighlightStyle { color: Some(theme::rgb(theme::ACCENT)), ..Default::default() }))
                })
                .collect();
            StyledText::new(SharedString::from(name.to_string())).with_highlights(highlights)
        };
        let plain = |s: &str| StyledText::new(SharedString::from(s.to_string()));
        let svg_icon = |path: &'static str, color: u32| gpui::svg().path(path).size(px(18.)).flex_none().text_color(theme::rgb(color)).into_any_element();
        let selected = ix == self.selected;
        let (icon, title, subtitle, kind, drill): (gpui::AnyElement, StyledText, String, &str, bool) = match item {
            Item::Header(_) => unreachable!(),
            Item::Url(u) => (svg_icon("icons/external.svg", theme::ACCENT), plain(&format!("Open {u}")), String::new(), "Website", false),
            Item::Calc(_, shown, detail) => (
                div().text_size(px(16.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::ACCENT)).child("=").into_any_element(),
                StyledText::new(SharedString::from(shown.clone())).with_highlights([(0..shown.len(), HighlightStyle { font_weight: Some(gpui::FontWeight::SEMIBOLD), ..Default::default() })]),
                detail.clone(),
                "↵ copy",
                false,
            ),
            Item::App(i, p) => {
                let a = &self.apps[*i];
                let icon = if a.icon.exists() {
                    gpui::img(a.icon.clone()).size(px(22.)).flex_none().into_any_element()
                } else {
                    svg_icon("icons/package.svg", theme::TEXT_MUTED)
                };
                (icon, hl(&a.name, p, 0), String::new(), "Application", false)
            }
            Item::Project(i, p) => {
                let pr = &self.projects[*i];
                let mut sub = Vec::new();
                if pr.active {
                    sub.push("current".to_string());
                }
                if !pr.repos.is_empty() {
                    sub.push(format!("{} repo{}", pr.repos.len(), if pr.repos.len() == 1 { "" } else { "s" }));
                }
                if !pr.servers.is_empty() {
                    sub.push(format!("{} server{}", pr.servers.len(), if pr.servers.len() == 1 { "" } else { "s" }));
                }
                (self.project_icon(*i, 20.), hl(&pr.name, p, 0), sub.join(" · "), "Project", true)
            }
            Item::File(i, p) => {
                let f = &self.files[*i];
                let split = f.display.rfind('/').map(|i| i + 1).unwrap_or(0);
                let name = &f.display[split..];
                let (icon, color) = crate::assets::file_icon(name);
                let offset = f.display[..split].chars().count();
                (svg_icon(icon, color), hl(name, p, offset), f.display[..split].trim_end_matches('/').to_string(), "File", false)
            }
            Item::Cmd(i) => {
                let c = &self.cmds[*i];
                let (path, color) = c.kind.icon();
                let icon = if c.kind == Kind::Repo || self.scope.is_some() { svg_icon(path, color) } else {
                    // Outside a scope, show which project the command belongs to.
                    div()
                        .relative()
                        .size(px(22.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(svg_icon(path, color))
                        .child(div().absolute().right(px(-4.)).bottom(px(-3.)).child(self.project_icon(c.project, 11.)))
                        .into_any_element()
                };
                let subtitle = if self.scope.is_some() && !matches!(c.kind, Kind::Ssh(_) | Kind::Dev(_) | Kind::GitHub | Kind::CopyPath) { String::new() } else { c.subtitle.clone() };
                (icon, plain(&c.title), subtitle, c.kind.badge(), c.kind == Kind::Repo)
            }
        };
        div()
            .id(ix)
            .w_full()
            .px(px(6.))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(ROW_H))
                    .px(px(12.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::rgb(theme::HOVER))))
                    .child(div().w(px(22.)).flex_none().flex().justify_center().child(icon))
                    .child(div().flex_none().text_size(px(14.)).text_color(theme::rgb(theme::TEXT)).child(title))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(12.))
                            .text_color(theme::rgb(theme::TEXT_FAINT))
                            .child(subtitle),
                    )
                    .when(selected && drill, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px(px(6.))
                                .py(px(1.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(theme::rgb(theme::BORDER))
                                .text_size(px(10.))
                                .text_color(theme::rgb(theme::TEXT_MUTED))
                                .child("Tab ⇥ actions"),
                        )
                    })
                    .child(div().flex_none().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(kind)),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.activate(ix, cx)))
            .into_any_element()
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Launcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.items.len();
        let crumb = self.scope.map(|s| {
            let (p, label) = match s {
                Scope::Project(p) => (p, self.projects[p].name.clone()),
                Scope::Repo(p, r) => (p, format!("{} / {}", self.projects[p].name, self.projects[p].repos[r].name)),
            };
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(3.))
                .rounded(px(6.))
                .bg(theme::rgb(theme::SELECTED))
                .text_size(px(13.))
                .text_color(theme::rgb(theme::TEXT))
                .child(self.project_icon(p, 14.))
                .child(label)
        });
        let hint = |k: &str, label: &str| {
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .child(div().px(px(5.)).rounded(px(3.)).bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT_MUTED)).child(k.to_string()))
                .child(label.to_string())
        };
        div()
            .key_context("QuickOpen")
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, cx)))
            .capture_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| this.on_key(e, cx)))
            .size_full()
            .flex()
            .flex_col()
            .font_family(theme::UI_FONT)
            .text_color(theme::rgb(theme::TEXT))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .rounded(px(12.))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(18.))
                    .h(px(54.))
                    .flex_none()
                    .border_b_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .text_size(px(17.))
                    .child(gpui::svg().path("icons/search.svg").size(px(16.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)))
                    .children(crumb)
                    .child(div().flex_1().min_w_0().child(self.input.clone())),
            )
            .when(count == 0, |d| {
                d.child(div().px(px(18.)).py(px(14.)).text_size(px(13.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("No results"))
            })
            .child(
                uniform_list(
                    "launcher-results",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| range.map(|ix| this.render_item(ix, cx)).collect::<Vec<_>>()),
                )
                .track_scroll(self.scroll.clone())
                .w_full()
                .flex_1()
                .pb(px(6.)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(14.))
                    .h(px(30.))
                    .border_t_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .text_size(px(11.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child(hint("↵", "open"))
                    .child(hint("⇥", "actions"))
                    .when(self.scope.is_some(), |d| d.child(hint("esc", "back")))
                    .child(div().flex_1())
                    .child("ssh · claude · repo · actions · dev · gh · pr"),
            )
    }
}

// ---- window management ------------------------------------------------------------------

struct LauncherGlobal {
    workspace: WindowHandle<Workspace>,
    window: Option<WindowHandle<Launcher>>,
}

impl Global for LauncherGlobal {}

/// Register ⌘Space and route presses to [`toggle`].
pub fn init(workspace: WindowHandle<Workspace>, cx: &mut App) {
    cx.set_global(LauncherGlobal { workspace, window: None });
    match crate::hotkey::register_cmd_space() {
        Ok(mut rx) => {
            cx.spawn(async move |cx| {
                while rx.next().await.is_some() {
                    let _ = cx.update(toggle);
                }
            })
            .detach();
        }
        Err(e) => crate::startup_log(&format!("launcher hotkey: {e}")),
    }
    // ⌘⇧M system-wide: music / podcasts / videos search, also from other apps.
    match crate::hotkey::register(crate::hotkey::KVK_M, crate::hotkey::CMD_KEY | crate::hotkey::SHIFT_KEY, "⌘⇧M") {
        Ok(mut rx) => {
            cx.spawn(async move |cx| {
                while rx.next().await.is_some() {
                    let _ = cx.update(toggle_music);
                }
            })
            .detach();
        }
        Err(e) => crate::startup_log(&format!("music search hotkey: {e}")),
    }
    // Manual testing without the hotkey.
    if std::env::var_os("JT_LAUNCHER_AT_START").is_some() {
        toggle(cx);
    }
}

/// Remember the workspace window used last (⌘Space acts on it).
pub fn set_last_workspace(handle: gpui::AnyWindowHandle, cx: &mut App) {
    if let (Some(ws), true) = (handle.downcast::<Workspace>(), cx.has_global::<LauncherGlobal>()) {
        cx.global_mut::<LauncherGlobal>().workspace = ws;
    }
}

/// Last used workspace window, or any open one if that was closed.
fn last_workspace(cx: &App) -> Option<WindowHandle<Workspace>> {
    let ws = cx.global::<LauncherGlobal>().workspace;
    if ws.read(cx).is_ok() {
        return Some(ws);
    }
    crate::workspace::workspace_windows(cx).into_iter().next()
}

fn close(cx: &mut App) {
    if let Some(w) = cx.global_mut::<LauncherGlobal>().window.take() {
        let _ = w.update(cx, |_, window, _| window.remove_window());
    }
}

/// Display under the mouse pointer (Spotlight opens where you're working).
fn target_bounds(cx: &App) -> Option<(gpui::DisplayId, Bounds<gpui::Pixels>)> {
    use objc::{class, msg_send, sel, sel_impl};
    #[repr(C)]
    struct NSPoint {
        x: f64,
        y: f64,
    }
    unsafe impl objc::Encode for NSPoint {
        fn encode() -> objc::Encoding {
            unsafe { objc::Encoding::from_str("{CGPoint=dd}") }
        }
    }
    let mouse: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
    let displays = cx.displays();
    let primary_h = cx.primary_display().map(|d| f32::from(d.bounds().size.height)).unwrap_or(0.);
    let (mx, my) = (mouse.x as f32, primary_h - mouse.y as f32);
    let d = displays
        .iter()
        .find(|d| {
            let b = d.bounds();
            mx >= f32::from(b.origin.x) && mx < f32::from(b.origin.x + b.size.width) && my >= f32::from(b.origin.y) && my < f32::from(b.origin.y + b.size.height)
        })
        .cloned()
        .or_else(|| cx.primary_display())?;
    let b = d.bounds();
    let x = b.origin.x + (b.size.width - px(WIDTH)) / 2.;
    let y = b.origin.y + b.size.height * 0.22;
    Some((d.id(), Bounds::new(point(x, y), size(px(WIDTH), px(HEIGHT)))))
}

pub fn toggle(cx: &mut App) {
    if cx.global::<LauncherGlobal>().window.is_some() {
        close(cx);
        return;
    }
    let Some(ws) = last_workspace(cx) else { return };
    let Ok(mut sources) = ws.read_with(cx, |w, cx| w.launcher_sources(cx)) else { return };
    // Projects open in any window count as open.
    let open: Vec<String> = crate::workspace::workspace_windows(cx).iter().filter_map(|w| w.read(cx).ok().and_then(|w| w.active_project_id().map(str::to_string))).collect();
    for p in &mut sources.projects {
        p.active |= open.contains(&p.id);
    }
    let (display_id, bounds) = match target_bounds(cx) {
        Some((d, b)) => (Some(d), b),
        None => (None, Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx)),
    };
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            display_id,
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some("dev.juan.terminal".into()),
            ..Default::default()
        },
        |window, cx| {
            let launcher = cx.new(|cx| Launcher::new(sources, window, cx));
            cx.subscribe(&launcher, |_, event: &LauncherEvent, cx| match event {
                LauncherEvent::Dismiss => close(cx),
                LauncherEvent::Pick(pick) => {
                    let pick = pick.clone();
                    close(cx);
                    run(pick, cx);
                }
            })
            .detach();
            launcher
        },
    );
    match opened {
        Ok(handle) => {
            hide_window_buttons();
            cx.global_mut::<LauncherGlobal>().window = Some(handle);
        }
        Err(e) => crate::startup_log(&format!("launcher window: {e:#}")),
    }
}

/// ⌘⇧M: inside the app it toggles the search on the focused window, like the local binding;
/// from another app it brings juan-terminal to the front with the search open.
fn toggle_music(cx: &mut App) {
    if let Some(ws) = cx.active_window().and_then(|w| w.downcast::<Workspace>()) {
        let _ = ws.update(cx, |w, window, cx| w.toggle_music_action(window, cx));
        return;
    }
    open_music(cx);
}

/// Bring juan-terminal to the front with the music search open (menu bar item, ⌘⇧M from
/// another app).
pub fn open_music(cx: &mut App) {
    if !cx.has_global::<LauncherGlobal>() {
        return;
    }
    close(cx);
    let Some(ws) = last_workspace(cx) else { return };
    cx.activate(true);
    let _ = ws.update(cx, |w, window, cx| {
        window.activate_window();
        w.show_music(window, cx);
    });
}

fn run(pick: Pick, cx: &mut App) {
    let open = |arg: &std::ffi::OsStr| {
        let _ = std::process::Command::new("/usr/bin/open").arg(arg).spawn();
    };
    match pick {
        Pick::Open(u) => open(u.as_ref()),
        Pick::App(p) => open(p.as_os_str()),
        Pick::Copy(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
        Pick::Project(id, action) => {
            // The window that has the project open, else the last used one.
            let Some(ws) = crate::workspace::window_with_project(&id, None, cx).or_else(|| last_workspace(cx)) else { return };
            cx.activate(true);
            let _ = ws.update(cx, |w, window, cx| {
                window.activate_window();
                w.launcher_run(id, action, window, cx);
            });
        }
        Pick::File(path) => {
            let windows = crate::workspace::workspace_windows(cx);
            let owner = windows.iter().copied().find(|w| w.read(cx).is_ok_and(|w| w.owns_path(&path)));
            let Some(ws) = owner.or_else(|| last_workspace(cx)) else { return };
            cx.activate(true);
            let _ = ws.update(cx, |w, window, cx| {
                window.activate_window();
                w.open_file(&path, true, window, cx);
                w.persist(cx);
            });
        }
    }
}

/// The panel is titled (GPUI needs that for a full-size content view): hide its traffic lights.
fn hide_window_buttons() {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let windows: *mut Object = msg_send![app, windows];
        let n: usize = msg_send![windows, count];
        let panel = class!(NSPanel);
        for i in 0..n {
            let w: *mut Object = msg_send![windows, objectAtIndex: i];
            let is_panel: objc::runtime::BOOL = msg_send![w, isKindOfClass: panel];
            if is_panel == objc::runtime::NO {
                continue;
            }
            for b in 0..3u64 {
                let btn: *mut Object = msg_send![w, standardWindowButton: b];
                if !btn.is_null() {
                    let _: () = msg_send![btn, setHidden: objc::runtime::YES];
                }
            }
            let _: () = msg_send![w, setHasShadow: objc::runtime::YES];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{as_url, remote_to_web};

    #[test]
    fn remote_urls() {
        assert_eq!(remote_to_web("git@github.com:judare/x.git").as_deref(), Some("https://github.com/judare/x"));
        assert_eq!(remote_to_web("https://github.com/a/b.git").as_deref(), Some("https://github.com/a/b"));
        assert_eq!(remote_to_web("https://tok@github.com/a/b").as_deref(), Some("https://github.com/a/b"));
        assert_eq!(remote_to_web("ssh://git@gitlab.com:22/a/b.git").as_deref(), Some("https://gitlab.com/a/b"));
    }

    #[test]
    fn url_detection() {
        assert_eq!(as_url("google.com").as_deref(), Some("https://google.com"));
        assert_eq!(as_url("github.com/foo/bar").as_deref(), Some("https://github.com/foo/bar"));
        assert_eq!(as_url("https://x.dev").as_deref(), Some("https://x.dev"));
        assert_eq!(as_url("localhost:3000").as_deref(), Some("http://localhost:3000"));
        assert_eq!(as_url("192.168.1.10:8080/a").as_deref(), Some("http://192.168.1.10:8080/a"));
        assert_eq!(as_url("main.rs"), None);
        assert_eq!(as_url("safari"), None);
        assert_eq!(as_url("hello world.com"), None);
    }
}
