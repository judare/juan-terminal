//! Root view: project sidebar with file tree, editor tabs, terminal panel. Owns persisted state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use std::sync::Arc;

use gpui::{
    AnyElement, AnyWindowHandle, App, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable, MouseButton,
    MouseMoveEvent, MouseUpEvent, PathPromptOptions, PromptLevel, Render, SharedString,
    Subscription, Task, Window, WindowBounds, WindowHandle, actions, div, point, prelude::*, px, uniform_list,
};
use project_search::{IndexedFile, Root, list_files};
use terminal_engine::{SessionConfig, TermDims, TerminalSession};
use workspace_state::{
    Project, Settings, StateStore, TerminalState, WindowGeometry, WorkspaceState,
    discover_repositories, head_branch, is_git_repo,
};

use crate::editor_view::{EditorEvent, EditorView};
use crate::diff_view::{DiffSource, DiffView, DiffViewEvent};
use crate::git_panel::{GitPanel, GitPanelEvent};
use crate::quick_open::{QuickOpen, QuickOpenEvent};
use crate::search_panel::{SearchPanel, SearchPanelEvent};
use crate::tools_panel::{ToolsPanel, ToolsPanelEvent};
use crate::terminal_view::{TerminalView, TerminalViewEvent};
use crate::theme;

/// A file or folder dragged out of the sidebar tree. Dropped on a WhatsApp chat it becomes an
/// attachment; dropped on a terminal (e.g. Claude) it types its shell-quoted path.
#[derive(Clone)]
pub struct DraggedFile {
    pub path: PathBuf,
    pub is_dir: bool,
}

impl Render for DraggedFile {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let el = if self.is_dir {
            crate::assets::folder_icon_el(&name, false, 16., None, false)
        } else {
            let (icon_path, icon_color) = crate::assets::file_icon(&name);
            icon(icon_path, icon_color)
        };
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(6.))
            .bg(theme::rgb(theme::SELECTED))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .shadow_md()
            .text_size(px(13.))
            .text_color(theme::rgb(theme::TEXT))
            .child(el)
            .child(name)
    }
}

actions!(
    workspace,
    [
        NewTerminal,
        NewWindow,
        NewFile,
        NewWordDocument,
        NewSpreadsheet,
        NewPresentation,
        CloseTerminal,
        NextTerminal,
        PreviousTerminal,
        OpenProject,
        AddRepository,
        ToggleSidebar,
        ToggleTerminalPanel,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        CloseEditor,
        NextEditor,
        PreviousEditor,
        ToggleAutosave,
        ToggleTheme,
        SaveAll,
        Quit,
        ToggleQuickOpen,
        SearchInProject,
        ShowFiles,
        OpenClaude,
        ShowGit,
        CloseDiff,
        ShowTools,
        LintProject,
        FormatFile,
        ToggleCommandPalette,
        OpenSettings,
        ExtendProjectHours,
        ShowUsage,
        ToggleMusic,
    ]
);

/// Unix time and the Mac's current UTC offset in seconds (DST included).
fn clock_now() -> (i64, i64) {
    // SAFETY: localtime_r only writes into the struct we pass.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (t as i64, tm.tm_gmtoff as i64)
    }
}

const SCRATCH: &str = "";
const TOP_BAR_HEIGHT: f32 = 38.;
const ROW_HEIGHT: f32 = 28.;
const MIN_SIDEBAR: f32 = 160.;
const MAX_SIDEBAR: f32 = 520.;

pub(crate) struct Tabs<T> {
    pub(crate) tabs: Vec<Entity<T>>,
    active: usize,
}

impl<T> Default for Tabs<T> {
    fn default() -> Self {
        Self { tabs: Vec::new(), active: 0 }
    }
}

impl<T> Tabs<T> {
    fn active(&self) -> Option<&Entity<T>> {
        self.tabs.get(self.active)
    }
    fn remove(&mut self, view: &Entity<T>) -> bool {
        let Some(ix) = self.tabs.iter().position(|t| t == view) else { return false };
        self.tabs.remove(ix);
        if ix < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        true
    }
}

#[derive(Clone)]
struct DirEntry {
    name: SharedString,
    path: PathBuf,
    is_dir: bool,
    /// The virtual "others" folder of a repository.
    virtual_others: bool,
    /// Gone from disk but deleted in git: listed struck through.
    deleted: bool,
}

/// Key used in `expanded` for a repository's virtual "others" folder.
fn others_key(repo: &Path) -> PathBuf {
    repo.join(":others:")
}

/// Config/tooling files that go into the "others" folder.
fn is_config_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const EXACT: &[&str] = &[
        "package.json", "package-lock.json", "pnpm-lock.yaml", "pnpm-workspace.yaml", "yarn.lock", "bun.lockb", "bun.lock",
        "jsconfig.json", "components.json", "vercel.json", "netlify.toml", "biome.json", "turbo.json", "nx.json", "renovate.json",
        "dockerfile", "cargo.lock", "rust-toolchain", "rust-toolchain.toml", "makefile", "justfile", "procfile",
    ];
    EXACT.contains(&n.as_str())
        || n.starts_with('.')
        || n.starts_with("tsconfig")
        || n.starts_with("docker-compose")
        || n.contains(".config.")
        || n.ends_with(".config")
}

#[derive(Clone)]
enum Row {
    Hint,
    Project { id: String, name: SharedString, icon: Option<PathBuf>, active: bool, expanded: bool },
    Repo { pid: String, name: SharedString, path: PathBuf, branch: Option<String>, missing: bool, expanded: bool },
    Entry { entry: DirEntry, depth: usize, expanded: bool, dimmed: bool },
}

struct ContextMenu {
    position: gpui::Point<gpui::Pixels>,
    target: PathBuf,
    is_dir: bool,
    /// Repository roots / project roots can't be renamed or trashed from here.
    is_root: bool,
    /// Opened from the Git panel: offer "Locate in Explorer" (reveal in the file tree).
    locate: bool,
}

enum NameAction {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
}

struct NamePrompt {
    input: Entity<crate::text_input::TextInput>,
    action: NameAction,
    error: Option<SharedString>,
    _sub: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Files,
    Search,
    Git,
    Tools,
    Ports,
    Servers,
    WhatsApp,
    Claude,
}

pub struct Workspace {
    /// Last title pushed to the native window (shown in Mission Control / Window menu).
    window_title: String,
    context_menu: Option<ContextMenu>,
    /// "+" on the terminal tab bar: Terminal / Claude Code / the project's SSH servers.
    new_tab_menu: Option<gpui::Point<gpui::Pixels>>,
    name_prompt: Option<NamePrompt>,
    sidebar_mode: SidebarMode,
    /// Repaints the Claude sessions board every second while it's visible.
    pub(crate) board_tick: Option<Task<()>>,
    quick_open: Option<(Entity<QuickOpen>, Subscription)>,
    palette: Option<(Entity<crate::command_palette::CommandPalette>, Subscription, Option<FocusHandle>)>,
    /// Agent usage dashboard (full-window overlay) and the focus to restore when it closes.
    usage: Option<(Entity<crate::usage_view::UsageView>, Subscription, Option<FocusHandle>)>,
    /// Music / podcast / video search overlay (⌘⇧M) and the focus to restore when it closes.
    music_search: Option<(Entity<crate::music_ui::MusicSearch>, Subscription, Option<FocusHandle>)>,
    /// "Now playing" card at the bottom of the sidebar, and the large video view.
    music_card: Option<Entity<crate::music_ui::MusicCard>>,
    music_theater: Option<Entity<crate::music_ui::MusicTheater>>,
    search_panel: Option<Entity<SearchPanel>>,
    git_panel: Option<Entity<GitPanel>>,
    /// Created on first visit; dropped (stops polling) when hidden is not needed: it only
    /// polls while it exists, and it's cheap.
    ports_panel: Option<Entity<crate::ports::PortsPanel>>,
    /// Claude Code IDE integration server (selection, diagnostics, diffs, hook events).
    pub(crate) ide: Option<claude_ide::IdeServer>,
    servers_panel: Option<Entity<crate::servers_panel::ServersPanel>>,
    whatsapp_panel: Option<Entity<crate::whatsapp_panel::WhatsAppPanel>>,
    /// GitHub Actions section under the file tree (hidden when no repo has workflows).
    actions_panel: Option<Entity<crate::actions_panel::ActionsPanel>>,
    /// Release tags section under GitHub Actions.
    tags_panel: Option<Entity<crate::tags_panel::TagsPanel>>,
    actions_height: f32,
    resizing_actions: bool,
    /// Local cache file → (server, remote path) for files opened over SFTP.
    remote_files: HashMap<PathBuf, (workspace_state::SshServer, String)>,
    pub(crate) claude_pending: Option<crate::claude_bridge::PendingDiff>,
    pub(crate) claude_sessions: usize,
    /// Chat picked in the WhatsApp panel, opened on the next render (needs the window).
    pending_chat: Option<(String, String)>,
    /// "Pedir a Claude…" on a chat, prepared and waiting for the window to open its terminal.
    pending_ask: Option<crate::wa_reply::AskPlan>,
    _ask_task: Option<Task<()>>,
    next_terminal_id: u64,
    /// Saved output for the next terminal spawned (restoring a project's tabs).
    pending_restore: Option<String>,
    /// Repos found by rediscovery, applied to the Git panel when a window is at hand.
    pending_git_repos: Option<Vec<(String, PathBuf)>>,
    /// Launcher action waiting for its project to become active.
    pending_launch: Option<(String, crate::launcher::ProjectAction)>,
    tools_panel: Option<Entity<ToolsPanel>>,
    pub(crate) diff: Option<Entity<DiffView>>,
    pub(crate) diff_active: bool,
    /// File index per project (built on demand with `git ls-files`, never watched).
    index: HashMap<String, Arc<Vec<IndexedFile>>>,
    indexing: bool,
    index_task: Option<Task<()>>,
    stay_awake: crate::power::StayAwake,
    /// oxfmt/oxlint per repository root, located once per session (no polling).
    file_tools: HashMap<PathBuf, crate::editor_view::FileTools>,
    store: StateStore,
    state: WorkspaceState,
    /// Saves window geometry and the launch project (the first window; passed on when it closes).
    primary: bool,
    /// Projects as last read from / written to disk: other windows' changes are merged
    /// against this so two windows never overwrite each other.
    synced: Vec<Project>,
    /// settings.toml (schedules, blocked websites); reloaded when saved in the editor.
    settings: Settings,
    /// Projects outside their schedule right now (re-evaluated every 20 s).
    locked: HashSet<String>,
    /// Project already warned that it closes soon.
    close_warned: Option<String>,
    /// Hosts last sent to /etc/hosts (so the 20 s tick only re-applies when a website
    /// schedule opens or closes, and a cancelled password prompt isn't repeated).
    applied_hosts: Option<Vec<String>>,
    pub(crate) terminals: HashMap<String, Tabs<TerminalView>>,
    pub(crate) editors: HashMap<String, Tabs<EditorView>>,
    branches: HashMap<PathBuf, Option<String>>,
    collapsed: HashSet<String>,
    expanded: HashSet<PathBuf>,
    dir_cache: HashMap<PathBuf, Rc<Vec<DirEntry>>>,
    /// Per repository root: (others, regular) split of its top-level entries.
    others_cache: HashMap<PathBuf, (Rc<Vec<DirEntry>>, Rc<Vec<DirEntry>>)>,
    /// Git-deleted entries by parent dir (from the git panel), refreshed each sidebar render.
    deleted_entries: Rc<HashMap<PathBuf, Vec<(PathBuf, bool)>>>,
    rows: Rc<Vec<Row>>,
    tree_scroll: gpui::UniformListScrollHandle,
    /// Path revealed with "Locate in Explorer": highlighted, scrolled to on the next render.
    located: Option<PathBuf>,
    locate_pending: bool,
    /// Bumped per diff request and per file open: a diff that finishes loading after a newer
    /// request (e.g. the double-click that opens the file) is dropped instead of covering it.
    diff_seq: u64,
    sidebar_visible: bool,
    sidebar_width: f32,
    resizing_sidebar: bool,
    terminal_visible: bool,
    terminal_width: f32,
    resizing_terminal: bool,
    font_size: f32,
    focus_handle: FocusHandle,
    pub(crate) error: Option<SharedString>,
    pub(crate) lsp: crate::language_servers::LanguageServers,
    pub(crate) _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(store: StateStore, state: WorkspaceState, primary: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dock_owner = cx.entity_id().as_u64();
        let mut subscriptions = vec![
            cx.on_app_quit(|this, cx| {
                this.save_scrollback(cx);
                this.persist(cx);
                async {}
            }),
            // A closed window no longer counts toward the Dock badge.
            cx.on_release(move |_, _| crate::claude_bridge::set_dock_count(dock_owner, 0)),
            cx.observe_window_bounds(window, |this, window, _| {
                if let WindowBounds::Windowed(b) = window.window_bounds() {
                    this.state.window = Some(WindowGeometry {
                        x: b.origin.x.into(),
                        y: b.origin.y.into(),
                        width: b.size.width.into(),
                        height: b.size.height.into(),
                    });
                }
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                this.stay_awake.set(active);
                // Repaint so a shared video player moves to the window just focused.
                cx.notify();
                if active {
                    crate::launcher::set_last_workspace(window.window_handle(), cx);
                    this.on_window_activated(cx);
                    if let (Some(repos), Some(gp)) = (this.pending_git_repos.take(), this.git_panel.clone()) {
                        gp.update(cx, |g, cx| g.set_repos(repos, window, cx));
                    }
                } else if this.state.autosave {
                    this.save_all(cx);
                }
            }),
        ];
        let (music_card, music_theater) = match crate::music::global(cx) {
            Some(music) => {
                use crate::music_ui::{MusicCard, MusicCardEvent, MusicTheater, TheaterEvent};
                let card = cx.new(|cx| MusicCard::new(music.clone(), cx));
                let theater = cx.new(|cx| MusicTheater::new(music, cx));
                subscriptions.push(cx.subscribe_in(&card, window, |this, _, event, window, cx| match event {
                    MusicCardEvent::OpenSearch => this.toggle_music(&ToggleMusic, window, cx),
                    MusicCardEvent::OpenTheater => this.open_theater(window, cx),
                }));
                subscriptions.push(cx.subscribe_in(&theater, window, |this, _, _: &TheaterEvent, window, cx| this.focus_main(window, cx)));
                (Some(card), Some(theater))
            }
            None => (None, None),
        };
        let sidebar_width = state.sidebar_width.unwrap_or(260.).clamp(MIN_SIDEBAR, MAX_SIDEBAR);
        let terminal_width = state.terminal_panel_width.unwrap_or(520.).max(240.);
        let font_size = state.font_size.unwrap_or(theme::DEFAULT_FONT_SIZE);
        let mut this = Self {
            window_title: String::new(),
            context_menu: None,
            new_tab_menu: None,
            name_prompt: None,
            sidebar_mode: SidebarMode::Files,
            board_tick: None,
            quick_open: None,
            palette: None,
            usage: None,
            music_search: None,
            music_card,
            music_theater,
            search_panel: None,
            git_panel: None,
            ports_panel: None,
            ide: None,
            servers_panel: None,
            whatsapp_panel: None,
            actions_panel: None,
            tags_panel: None,
            actions_height: 300.,
            resizing_actions: false,
            remote_files: HashMap::new(),
            claude_pending: None,
            claude_sessions: 0,
            pending_chat: None,
            pending_ask: None,
            _ask_task: None,
            next_terminal_id: 1,
            pending_restore: None,
            pending_git_repos: None,
            pending_launch: None,
            tools_panel: None,
            diff: None,
            diff_active: false,
            index: HashMap::new(),
            indexing: false,
            index_task: None,
            stay_awake: Default::default(),
            file_tools: HashMap::new(),
            store,
            synced: state.projects.clone(),
            state,
            primary,
            settings: Settings::default(),
            locked: HashSet::new(),
            close_warned: None,
            applied_hosts: None,
            terminals: HashMap::new(),
            editors: HashMap::new(),
            branches: HashMap::new(),
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            dir_cache: HashMap::new(),
            others_cache: HashMap::new(),
            deleted_entries: Default::default(),
            rows: Rc::new(Vec::new()),
            tree_scroll: gpui::UniformListScrollHandle::new(),
            located: None,
            locate_pending: false,
            diff_seq: 0,
            sidebar_visible: true,
            sidebar_width,
            resizing_sidebar: false,
            terminal_visible: true,
            terminal_width,
            resizing_terminal: false,
            font_size,
            focus_handle: cx.focus_handle(),
            error: None,
            lsp: Default::default(),
            _subscriptions: subscriptions,
        };
        if let Some(id) = this.state.active_project.clone() {
            if this.state.project(&id).is_none() {
                this.state.active_project = this.state.projects.first().map(|p| p.id.clone());
            }
        }
        for p in &this.state.projects {
            this.expanded.extend(p.expanded.iter().cloned());
        }
        let _ = workspace_state::ensure_settings_file();
        match workspace_state::load_settings() {
            Ok(s) => this.settings = s,
            Err(e) => this.error = Some(e.into()),
        }
        this.refresh_locks();
        // Never start inside a project that is outside its hours.
        if this.state.active_project.as_ref().is_some_and(|id| this.locked.contains(id)) {
            this.state.active_project = this.state.projects.iter().find(|p| !this.locked.contains(&p.id)).map(|p| p.id.clone());
        }
        this.apply_blocked_websites(cx);
        // Save this window's project when it closes (other windows keep running).
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.on_close(cx));
            true
        });
        this.ide = crate::claude_bridge::start(window, cx);
        this.ensure_project_loaded(window, cx);
        this.sync_ide(cx);
        // Project schedules: lock/close projects when their hours end.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_secs(20)).await;
                if this.update_in(cx, |this, window, cx| this.schedule_tick(window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Git status changes from terminals/Claude don't go through the editor: poll while the
        // window is in front (status is cheap; repaint only happens when it changed).
        cx.spawn_in(window, async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_secs(3)).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    if window.is_window_active() {
                        if let Some(gp) = &this.git_panel {
                            gp.update(cx, |g, cx| g.refresh_all(cx));
                        }
                        // Files edited by Claude/terminals/other apps while we're in front:
                        // clean buffers reload, dirty ones show the conflict bar (one stat each).
                        for tabs in this.editors.values() {
                            for e in &tabs.tabs {
                                e.update(cx, |e, cx| e.check_disk(cx));
                            }
                        }
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        // Linked WhatsApp: the panel exists from the start so the mode tab shows unread chats.
        if whatsapp::data_dir().join("session.db").exists() {
            this.ensure_whatsapp_panel(cx);
        }
        // Manual testing: open the music search at launch.
        if primary && std::env::var_os("JT_MUSIC_AT_START").is_some() {
            this.toggle_music(&ToggleMusic, window, cx);
        }
        this
    }

    pub(crate) fn active_key(&self) -> String {
        self.state.active_project.clone().unwrap_or_default()
    }

    pub(crate) fn active_project(&self) -> Option<&Project> {
        self.state.active_project.as_deref().and_then(|id| self.state.project(id))
    }

    fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        self.terminals.get(&self.active_key())?.active()
    }

    pub(crate) fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.editors.get(&self.active_key())?.active()
    }

    fn on_window_activated(&mut self, cx: &mut Context<Self>) {
        self.rediscover_repositories(cx);
        self.refresh_branches();
        // Re-read expanded folders (no watchers; refresh on focus).
        // Dropping the cache makes the next render re-read only the folders still expanded.
        self.dir_cache.clear();
        self.others_cache.clear();
        for tabs in self.editors.values() {
            for e in &tabs.tabs {
                e.update(cx, |e, cx| e.check_disk(cx));
            }
        }
        if let Some(gp) = &self.git_panel {
            gp.update(cx, |g, cx| g.refresh_all(cx));
        }
        cx.notify();
    }

    fn refresh_branches(&mut self) {
        let Some(project) = self.active_project() else { return };
        let repos: Vec<PathBuf> = project.repositories.iter().map(|r| r.path.clone()).collect();
        for path in repos {
            let branch = head_branch(&path);
            self.branches.insert(path, branch);
        }
    }

    fn children(&mut self, dir: &Path) -> Rc<Vec<DirEntry>> {
        if let Some(c) = self.dir_cache.get(dir) {
            return c.clone();
        }
        let mut entries: Vec<DirEntry> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name == ".git" || name == ".DS_Store" {
                            return None;
                        }
                        // Follow symlinks for the dir check.
                        let is_dir = e.path().is_dir();
                        Some(DirEntry { name: name.into(), path: e.path(), is_dir, virtual_others: false, deleted: false })
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        let rc = Rc::new(entries);
        self.dir_cache.insert(dir.to_path_buf(), rc.clone());
        rc
    }

    /// `entries` of `dir` plus the files deleted in git that used to be there.
    fn with_deleted(&self, dir: &Path, entries: Rc<Vec<DirEntry>>) -> Rc<Vec<DirEntry>> {
        let Some(gone) = self.deleted_entries.get(dir).filter(|g| !g.is_empty()) else { return entries };
        let mut all: Vec<DirEntry> = entries.as_ref().clone();
        for (path, is_dir) in gone {
            if all.iter().any(|e| &e.path == path) {
                continue;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            all.push(DirEntry { name: name.into(), path: path.clone(), is_dir: *is_dir, virtual_others: false, deleted: true });
        }
        all.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Rc::new(all)
    }

    /// Start terminals and reopen files for the active project, lazily, once.
    /// Pick up repositories created after the project was added (e.g. a later `git init`).
    fn rediscover_repositories(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.state.active_project.clone() else { return };
        let Some(root) = self.state.project(&id).and_then(|p| p.root.clone()).filter(|r| r.is_dir()) else { return };
        let found = discover_repositories(&root, 2);
        let Some(project) = self.state.project_mut(&id) else { return };
        let mut added = false;
        for repo in found {
            if !project.repositories.iter().any(|r| r.path == repo) {
                project.add_repository(&repo);
                added = true;
            }
        }
        if added {
            let repos: Vec<(String, PathBuf)> = project.repositories.iter().map(|r| (r.name.clone(), r.path.clone())).collect();
            self.refresh_branches();
            if let Some(tp) = self.tools_panel.clone() {
                let r = repos.clone();
                tp.update(cx, |t, cx| t.set_repos(r, cx));
            }
            if let Some(ap) = self.actions_panel.clone() {
                let r = repos.clone();
                ap.update(cx, |a, cx| a.set_repos(r, cx));
            }
            if let Some(tp) = self.tags_panel.clone() {
                let r = repos.clone();
                tp.update(cx, |t, cx| t.set_repos(r, cx));
            }
            self.pending_git_repos = Some(repos);
            self.persist(cx);
            cx.notify();
        }
    }

    fn ensure_project_loaded(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rediscover_repositories(cx);
        if let (Some(repos), Some(gp)) = (self.pending_git_repos.take(), self.git_panel.clone()) {
            gp.update(cx, |g, cx| g.set_repos(repos, window, cx));
        }
        let key = self.active_key();
        self.refresh_branches();
        if !self.terminals.contains_key(&key) {
            if let Some(p) = self.active_project() {
                let dirs = p.expanded.clone();
                self.expanded.extend(dirs);
            }
            self.terminals.insert(key.clone(), Tabs::default());
            let saved: Vec<TerminalState> = self.active_project().map(|p| p.terminals.clone()).unwrap_or_default();
            let active = self.active_project().and_then(|p| p.active_terminal).unwrap_or(0);
            // Default layout: a shell and a Claude tab. Restored Claude tabs relaunch `claude`.
            let default_cwd = self.active_project().and_then(|p| p.default_cwd());
            let has_shell = saved.iter().any(|t| t.title.as_deref() != Some("Claude"));
            let has_claude = saved.iter().any(|t| t.title.as_deref() == Some("Claude"));
            for t in &saved {
                let cwd = t.cwd.clone().filter(|p| p.is_dir()).or_else(|| default_cwd.clone());
                self.pending_restore = t.scrollback.as_deref().and_then(|id| std::fs::read_to_string(scrollback_file(id)).ok());
                if t.title.as_deref() == Some("Claude") {
                    self.open_claude(cwd, t.repository.clone(), window, cx);
                } else {
                    self.open_terminal(cwd, t.repository.clone(), window, cx);
                }
            }
            if !has_shell {
                self.open_terminal(default_cwd.clone(), None, window, cx);
            }
            if !has_claude {
                self.open_claude(default_cwd, None, window, cx);
            }
            if let Some(tabs) = self.terminals.get_mut(&key) {
                tabs.active = if saved.is_empty() { tabs.tabs.len().saturating_sub(1) } else { active.min(tabs.tabs.len().saturating_sub(1)) };
            }
        }
        self.sync_git_panel(window, cx);
        self.sync_actions_panel(window, cx);
        if !self.editors.contains_key(&key) {
            self.editors.insert(key.clone(), Tabs::default());
            let files: Vec<PathBuf> = self.active_project().map(|p| p.open_files.clone()).unwrap_or_default();
            let active = self.active_project().and_then(|p| p.active_file).unwrap_or(0);
            for f in files.into_iter().filter(|f| f.is_file()) {
                self.open_file(&f, false, window, cx);
            }
            if let Some(tabs) = self.editors.get_mut(&key) {
                tabs.active = active.min(tabs.tabs.len().saturating_sub(1));
            }
        }
        self.focus_main(window, cx);
    }

    fn focus_main(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.diff.as_ref().filter(|_| self.diff_active) {
            window.focus(&d.focus_handle(cx));
        } else if let Some(e) = self.active_editor() {
            window.focus(&e.focus_handle(cx));
        } else if let Some(t) = self.active_terminal() {
            window.focus(&t.focus_handle(cx));
        } else {
            window.focus(&self.focus_handle);
        }
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.active_terminal() {
            window.focus(&t.focus_handle(cx));
        }
    }

    // ---- terminals ----------------------------------------------------------------------

    fn open_terminal(&mut self, cwd: Option<PathBuf>, repository: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_terminal(cwd, repository, window, cx);
    }

    /// Open a terminal that starts the real `claude` CLI inside the user's login shell,
    /// so PATH, auth, MCP servers, hooks and settings behave exactly as in any terminal.
    fn open_claude(&mut self, cwd: Option<PathBuf>, repository: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.spawn_terminal(cwd, repository, window, cx) {
            view.update(cx, |t, cx| {
                t.label = Some("Claude".into());
                // Typed ahead: the shell reads it once its startup files are loaded.
                t.send_input("claude\r");
                cx.notify();
            });
            self.persist(cx);
        }
    }

    /// "Pedir a Claude…" on a WhatsApp chat: gather the chat (and its linked project) in the
    /// background, then open a Claude Code terminal with it and the instruction.
    pub(crate) fn ask_claude_about_chat(&mut self, jid: &str, instruction: &str, cx: &mut Context<Self>) {
        let (jid, instruction) = (jid.to_string(), instruction.to_string());
        let task = cx.background_executor().spawn(async move { crate::wa_reply::ask_plan(&jid, &instruction) });
        self._ask_task = Some(cx.spawn(async move |this, cx| {
            let plan = task.await;
            let _ = this.update(cx, |this, cx| {
                match plan {
                    Ok(plan) => this.pending_ask = Some(plan),
                    Err(e) => this.error = Some(format!("Pedir a Claude: {e}").into()),
                }
                cx.notify();
            });
        }));
    }

    fn open_claude_ask(&mut self, plan: crate::wa_reply::AskPlan, window: &mut Window, cx: &mut Context<Self>) {
        let q = |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let mut line = String::new();
        if let Some(cfg) = &plan.mcp_config {
            line.push_str(&format!("JT_MCP_CONFIG={} ", q(cfg)));
        }
        line.push_str("claude");
        for d in &plan.add_dirs {
            line.push_str(&format!(" --add-dir {}", q(d)));
        }
        line.push_str(&format!(" \"$(<{})\"\r", q(&plan.prompt_file)));
        if let Some(view) = self.spawn_terminal(Some(plan.cwd), None, window, cx) {
            view.update(cx, |t, cx| {
                t.label = Some(match &plan.project_name {
                    Some(p) => format!("Claude · WhatsApp · {p}"),
                    None => "Claude · WhatsApp".into(),
                });
                t.send_input(&line);
                cx.notify();
            });
            self.persist(cx);
        }
    }

    fn open_ssh(&mut self, server: &workspace_state::SshServer, window: &mut Window, cx: &mut Context<Self>) {
        let args = ssh_remote::interactive_args(server);
        let label = server.name.clone();
        if let Some(view) = self.spawn_terminal_with(None, None, Some(("ssh".into(), args)), window, cx) {
            view.update(cx, |t, cx| {
                t.label = Some(format!("ssh · {label}"));
                cx.notify();
            });
        }
    }

    fn spawn_terminal(&mut self, cwd: Option<PathBuf>, repository: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<TerminalView>> {
        self.spawn_terminal_with(cwd, repository, None, window, cx)
    }

    /// `program`: run this instead of the login shell (e.g. `ssh user@host`).
    fn spawn_terminal_with(&mut self, cwd: Option<PathBuf>, repository: Option<String>, program: Option<(String, Vec<String>)>, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<TerminalView>> {
        let key = self.active_key();
        let cwd = cwd.or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let mut env = crate::shell_integration::env_for_shell();
        let jt_id = format!("t{}-{}", std::process::id(), self.next_terminal_id);
        self.next_terminal_id += 1;
        env.extend(self.claude_terminal_env(&jt_id));
        if let Some(p) = self.active_project() {
            env.insert("JT_PROJECT_ID".into(), p.id.clone());
            if let Some(cfg) = crate::servers_mcp_config(p) {
                env.insert("JT_MCP_CONFIG".into(), cfg.to_string_lossy().into_owned());
            }
            env.extend(p.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let config = SessionConfig { cwd: cwd.clone(), env, program, restore: self.pending_restore.take(), ..SessionConfig::default() };
        let (session, rx) = match TerminalSession::spawn(config, TermDims::new(80, 24, 8, 16), theme::terminal_palette()) {
            Ok(s) => s,
            Err(err) => {
                self.error = Some(format!("Could not start shell: {err:#}").into());
                cx.notify();
                return None;
            }
        };
        let font_size = self.font_size;
        let view = cx.new(|cx| TerminalView::new(jt_id, session, rx, cwd, repository, font_size + theme::TERM_FONT_BONUS, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, view, event, window, cx| match event {
            TerminalViewEvent::TitleChanged | TerminalViewEvent::AttentionCleared => cx.notify(),
            TerminalViewEvent::OpenPath { path, line, column } => {
                let (path, line, column) = (path.clone(), *line, *column);
                if path.is_dir() {
                    cx.reveal_path(&path);
                } else {
                    this.open_file(&path, true, window, cx);
                    if let (Some(l), Some(e)) = (line, this.active_editor().cloned()) {
                        e.update(cx, |e, cx| e.go_to(l.saturating_sub(1), column.unwrap_or(1).saturating_sub(1), cx));
                    }
                    this.persist(cx);
                }
            }
            TerminalViewEvent::Exited(code) => {
                if view.read(cx).dev_dir.is_some() {
                    this.sync_dev(cx);
                }
                if *code == Some(0) {
                    this.close_terminal_view(view, window, cx);
                } else {
                    cx.notify();
                }
            }
        });
        self._subscriptions.push(sub);
        let tabs = self.terminals.entry(key).or_default();
        tabs.tabs.push(view.clone());
        tabs.active = tabs.tabs.len() - 1;
        self.terminal_visible = true;
        self.error = None;
        self.focus_terminal(window, cx);
        cx.notify();
        Some(view)
    }

    // ---- global launcher (⌘Space) -------------------------------------------------------

    pub(crate) fn launcher_sources(&self, _cx: &App) -> crate::launcher::Sources {
        use crate::launcher::{ProjectEntry, RepoEntry, ServerEntry};
        let active = self.state.active_project.clone();
        crate::launcher::Sources {
            projects: self
                .state
                .projects
                .iter()
                .map(|p| ProjectEntry {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    active: active.as_deref() == Some(p.id.as_str()),
                    icon: p.icon_path(),
                    root: p.root.clone().filter(|r| r.is_dir()),
                    repos: p.repositories.iter().filter(|r| r.path.is_dir()).map(|r| RepoEntry::new(r.name.clone(), r.path.clone())).collect(),
                    servers: p
                        .servers
                        .iter()
                        .map(|s| ServerEntry {
                            id: s.id.clone(),
                            name: s.name.clone(),
                            target: if s.user.is_empty() { s.host.clone() } else { format!("{}@{}", s.user, s.host) },
                        })
                        .collect(),
                    dev: Vec::new(),
                })
                .collect(),
            project_name: self.active_project().map(|p| p.name.clone()),
            files: self.current_index(),
            roots: self.index_roots(),
        }
    }

    /// Run a launcher action in project `id`, switching to it first if needed (the switch may
    /// ask for confirmation; the action then runs once the project is active).
    pub(crate) fn launcher_run(&mut self, id: String, action: crate::launcher::ProjectAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.active_project.as_deref() == Some(id.as_str()) {
            self.pending_launch = None;
            self.run_launch_action(action, window, cx);
        } else {
            self.pending_launch = Some((id.clone(), action));
            self.request_switch_project(id, window, cx);
        }
    }

    fn run_launch_action(&mut self, action: crate::launcher::ProjectAction, window: &mut Window, cx: &mut Context<Self>) {
        use crate::launcher::ProjectAction as A;
        match action {
            A::Open => self.focus_main(window, cx),
            A::Claude(cwd, repo) => self.open_claude(cwd, repo, window, cx),
            A::Terminal(cwd, repo) => {
                self.open_terminal(cwd, repo, window, cx);
                self.persist(cx);
            }
            A::Ssh(server_id) => {
                let server = self.active_project().and_then(|p| p.servers.iter().find(|s| s.id == server_id).cloned());
                if let Some(server) = server {
                    self.on_servers_event(&crate::servers_panel::ServersPanelEvent::OpenTerminal(server), window, cx);
                }
            }
            A::Actions => {
                self.sidebar_visible = true;
                self.sidebar_mode = SidebarMode::Files;
                self.sync_actions_panel(window, cx);
                if let Some(ap) = self.actions_panel.clone() {
                    ap.update(cx, |a, cx| {
                        a.expanded = true;
                        a.refresh(cx);
                    });
                }
                cx.notify();
            }
            A::Git => self.show_git(window, cx),
            A::Servers => self.show_servers(window, cx),
            A::Dev(dir) => {
                let cmd = crate::ports::dev_command_list(&self.port_roots()).into_iter().find(|c| c.dir == dir);
                if let Some(cmd) = cmd {
                    self.show_servers(window, cx);
                    self.on_servers_event(&crate::servers_panel::ServersPanelEvent::RunDev(cmd), window, cx);
                }
            }
        }
    }

    // ---- index, quick open, search ------------------------------------------------------

    fn index_roots(&self) -> Vec<Root> {
        let Some(p) = self.active_project() else { return Vec::new() };
        let mut roots: Vec<Root> = p
            .repositories
            .iter()
            .filter(|r| r.path.is_dir())
            .map(|r| Root { label: r.name.clone(), path: r.path.clone(), is_git: true, exclude: vec![] })
            .collect();
        if let Some(root) = p.root.as_ref().filter(|r| r.is_dir() && !p.repositories.iter().any(|x| &x.path == *r)) {
            roots.push(Root {
                label: p.name.clone(),
                path: root.clone(),
                is_git: is_git_repo(root),
                exclude: p.repositories.iter().map(|r| r.path.clone()).collect(),
            });
        }
        roots
    }

    /// Rebuild the active project's file index in the background.
    fn refresh_index(&mut self, cx: &mut Context<Self>) {
        let key = self.active_key();
        let roots = self.index_roots();
        self.indexing = true;
        self.index_task = Some(cx.spawn(async move |this, cx| {
            let files = cx.background_executor().spawn(async move { list_files(&roots) }).await;
            let files = Arc::new(files);
            let _ = this.update(cx, |this, cx| {
                this.indexing = false;
                this.index.insert(key.clone(), files.clone());
                if this.active_key() == key {
                    if let Some((qo, _)) = &this.quick_open {
                        qo.update(cx, |q, cx| q.set_files(files.clone(), cx));
                    }
                    if let Some(sp) = &this.search_panel {
                        sp.update(cx, |s, cx| s.set_files(files.clone(), cx));
                    }
                }
                cx.notify();
            });
        }));
    }

    fn current_index(&self) -> Arc<Vec<IndexedFile>> {
        self.index.get(&self.active_key()).cloned().unwrap_or_default()
    }

    fn toggle_quick_open(&mut self, _: &ToggleQuickOpen, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_open.take().is_some() {
            self.focus_main(window, cx);
            cx.notify();
            return;
        }
        let files = self.current_index();
        let indexing = files.is_empty();
        let qo = cx.new(|cx| QuickOpen::new(files, indexing, window, cx));
        let sub = cx.subscribe_in(&qo, window, |this, _, event, window, cx| {
            match event {
                QuickOpenEvent::Open(path) => {
                    let path = path.clone();
                    this.quick_open = None;
                    this.open_file(&path, true, window, cx);
                    this.persist(cx);
                }
                QuickOpenEvent::Dismiss => {
                    this.quick_open = None;
                    this.focus_main(window, cx);
                }
            }
            cx.notify();
        });
        self.quick_open = Some((qo, sub));
        self.refresh_index(cx);
        cx.notify();
    }

    fn toggle_usage(&mut self, _: &ShowUsage, window: &mut Window, cx: &mut Context<Self>) {
        use crate::usage_view::{UsageEvent, UsageView};
        if let Some((_, _, prev)) = self.usage.take() {
            match prev {
                Some(p) => window.focus(&p),
                None => self.focus_main(window, cx),
            }
            cx.notify();
            return;
        }
        let prev = window.focused(cx);
        let view = cx.new(|cx| UsageView::new(window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
            UsageEvent::Dismiss => this.toggle_usage(&ShowUsage, window, cx),
        });
        self.usage = Some((view, sub, prev));
        cx.notify();
    }

    fn toggle_music(&mut self, _: &ToggleMusic, window: &mut Window, cx: &mut Context<Self>) {
        use crate::music_ui::{MusicSearch, MusicSearchEvent};
        if let Some((_, _, prev)) = self.music_search.take() {
            match prev {
                Some(p) => window.focus(&p),
                None => self.focus_main(window, cx),
            }
            cx.notify();
            return;
        }
        let Some(music) = crate::music::global(cx) else { return };
        let prev = window.focused(cx);
        let view = cx.new(|cx| MusicSearch::new(music.clone(), window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            let prev = this.music_search.take().and_then(|(_, _, p)| p);
            let theater = matches!(event, MusicSearchEvent::Played) && this.music_theater.as_ref().is_some_and(|t| t.read(cx).is_open(cx));
            if let MusicSearchEvent::OpenFile(path) = event {
                this.open_file(path, true, window, cx);
            } else if theater {
                this.open_theater(window, cx);
            } else {
                match prev {
                    Some(p) => window.focus(&p),
                    None => this.focus_main(window, cx),
                }
            }
            cx.notify();
        });
        self.music_search = Some((view, sub, prev));
        cx.notify();
    }

    /// Global ⌘⇧M from another app: bring up the music search (never closes it).
    pub(crate) fn show_music(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.music_search {
            Some((view, _, _)) => window.focus(&view.focus_handle(cx)),
            None => self.toggle_music(&ToggleMusic, window, cx),
        }
    }

    pub(crate) fn toggle_music_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_music(&ToggleMusic, window, cx);
    }

    fn open_theater(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(theater), Some(music)) = (self.music_theater.clone(), crate::music::global(cx)) {
            music.update(cx, |m, cx| m.set_theater(true, cx));
            window.focus(&theater.focus_handle(cx));
            cx.notify();
        }
    }

    fn toggle_palette(&mut self, _: &ToggleCommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        use crate::command_palette::{CommandPalette, PaletteEvent};
        if let Some((_, _, prev)) = self.palette.take() {
            if let Some(p) = prev {
                window.focus(&p);
            }
            cx.notify();
            return;
        }
        let prev = window.focused(cx);
        let palette = cx.new(|cx| CommandPalette::new(window, cx));
        let sub = cx.subscribe_in(&palette, window, |this, _, event, window, cx| {
            let prev = this.palette.take().and_then(|(_, _, p)| p);
            match prev {
                Some(p) => window.focus(&p),
                None => this.focus_main(window, cx),
            }
            if let PaletteEvent::Run(action) = event {
                // Runs on the element that had focus before the palette opened.
                window.dispatch_action(action.boxed_clone(), cx);
            }
            cx.notify();
        });
        self.palette = Some((palette, sub, prev));
        cx.notify();
    }

    fn ensure_search_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<SearchPanel> {
        if let Some(sp) = &self.search_panel {
            return sp.clone();
        }
        let files = self.current_index();
        let sp = cx.new(|cx| SearchPanel::new(files, window, cx));
        let sub = cx.subscribe_in(&sp, window, |this, _, event, window, cx| match event {
            SearchPanelEvent::Replaced(paths) => {
                for tabs in this.editors.values() {
                    for e in &tabs.tabs {
                        if e.read(cx).path().is_some_and(|p| paths.contains(p)) {
                            e.update(cx, |e, cx| e.check_disk(cx));
                        }
                    }
                }
                if let Some(gp) = &this.git_panel {
                    gp.update(cx, |g, cx| g.refresh_all(cx));
                }
                cx.notify();
            }
            SearchPanelEvent::Open { path, line, column } => {
                let (path, line, column) = (path.clone(), *line, *column);
                this.open_file(&path, true, window, cx);
                if let Some(e) = this.active_editor().cloned() {
                    e.update(cx, |e, cx| e.go_to(line, column, cx));
                }
                this.persist(cx);
            }
        });
        self._subscriptions.push(sub);
        self.search_panel = Some(sp.clone());
        sp
    }

    fn project_repos(&self) -> Vec<(String, PathBuf)> {
        self.active_project().map(|p| p.repositories.iter().filter(|r| r.path.is_dir()).map(|r| (r.name.clone(), r.path.clone())).collect()).unwrap_or_default()
    }

    /// Keep the git panel pointed at the active project's repos (status is cheap and gives the
    /// change counters even when the Git tab is not open).
    fn sync_git_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repos = self.project_repos();
        let gp = match &self.git_panel {
            Some(gp) => gp.clone(),
            None => {
                let gp = cx.new(|_| GitPanel::new());
                let sub = cx.subscribe_in(&gp, window, |this, _, event, window, cx| this.on_git_event(event, window, cx));
                self._subscriptions.push(sub);
                self.git_panel = Some(gp.clone());
                gp
            }
        };
        gp.update(cx, |g, cx| g.set_repos(repos, window, cx));
    }

    /// Point the Actions section at the project's repos (it hides itself without workflows).
    fn sync_actions_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repos = self.project_repos();
        let ap = match &self.actions_panel {
            Some(ap) => ap.clone(),
            None => {
                let ap = cx.new(|_| crate::actions_panel::ActionsPanel::new());
                let sub = cx.subscribe_in(&ap, window, |this, _, event, window, cx| match event {
                    crate::actions_panel::ActionsPanelEvent::OpenLog(path) => {
                        let path = path.clone();
                        this.open_file(&path, true, window, cx);
                        if let Some(e) = this.active_editor().cloned() {
                            e.update(cx, |e, cx| e.check_disk(cx));
                        }
                        this.persist(cx);
                    }
                });
                self._subscriptions.push(sub);
                self.actions_panel = Some(ap.clone());
                ap
            }
        };
        ap.update(cx, |a, cx| a.set_repos(repos.clone(), cx));
        let tp = self.tags_panel.get_or_insert_with(|| cx.new(|_| crate::tags_panel::TagsPanel::new())).clone();
        tp.update(cx, |t, cx| t.set_repos(repos, cx));
    }

    fn ensure_tools_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ToolsPanel> {
        let tp = match &self.tools_panel {
            Some(tp) => tp.clone(),
            None => {
                let tp = cx.new(|_| ToolsPanel::new());
                let sub = cx.subscribe_in(&tp, window, |this, _, event, window, cx| match event {
                    ToolsPanelEvent::OpenAt { path, line, column } => {
                        let (path, line, column) = (path.clone(), *line, *column);
                        this.open_file(&path, true, window, cx);
                        if let Some(e) = this.active_editor().cloned() {
                            e.update(cx, |e, cx| e.go_to(line, column, cx));
                        }
                        this.persist(cx);
                    }
                    ToolsPanelEvent::FilesChanged => {
                        for tabs in this.editors.values() {
                            for e in &tabs.tabs {
                                e.update(cx, |e, cx| e.check_disk(cx));
                            }
                        }
                        if let Some(gp) = &this.git_panel {
                            gp.update(cx, |g, cx| g.refresh_all(cx));
                        }
                    }
                });
                self._subscriptions.push(sub);
                self.tools_panel = Some(tp.clone());
                tp
            }
        };
        let repos = self.project_repos();
        tp.update(cx, |t, cx| t.set_repos(repos, cx));
        tp
    }

    fn show_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.sidebar_mode = SidebarMode::Servers;
        let servers = self.active_project().map(|p| p.servers.clone()).unwrap_or_default();
        let sp = match self.servers_panel.clone() {
            Some(sp) => sp,
            None => {
                let sp = cx.new(|_| crate::servers_panel::ServersPanel::new());
                let sub = cx.subscribe_in(&sp, window, |this, _, event, window, cx| this.on_servers_event(event, window, cx));
                self._subscriptions.push(sub);
                self.servers_panel = Some(sp.clone());
                sp
            }
        };
        sp.update(cx, |p, cx| p.set_servers(servers, cx));
        self.sync_dev(cx);
        cx.notify();
    }

    /// Push the project's dev scripts and which run now to the Servers panel.
    fn sync_dev(&mut self, cx: &mut Context<Self>) {
        let Some(sp) = self.servers_panel.clone() else { return };
        let roots = self.port_roots();
        let dev = crate::ports::dev_command_list(&roots);
        let running: HashSet<PathBuf> = self
            .terminals
            .get(&self.active_key())
            .map(|t| t.tabs.iter().filter_map(|v| v.read(cx).dev_dir.clone().filter(|_| !v.read(cx).has_exited())).collect())
            .unwrap_or_default();
        sp.update(cx, |p, cx| p.set_dev(dev, running, cx));
    }

    fn show_whatsapp(&mut self, cx: &mut Context<Self>) {
        self.ensure_whatsapp_panel(cx);
        self.sidebar_mode = SidebarMode::WhatsApp;
        self.sidebar_visible = true;
        cx.notify();
    }

    fn ensure_whatsapp_panel(&mut self, cx: &mut Context<Self>) -> Entity<crate::whatsapp_panel::WhatsAppPanel> {
        if let Some(p) = &self.whatsapp_panel {
            return p.clone();
        }
        let panel = cx.new(crate::whatsapp_panel::WhatsAppPanel::new);
        let sub = cx.subscribe(&panel, |this, _, e: &crate::whatsapp_panel::WhatsAppPanelEvent, cx| match e {
            crate::whatsapp_panel::WhatsAppPanelEvent::OpenChat { jid, name } => {
                this.pending_chat = Some((jid.clone(), name.clone()));
                cx.notify();
            }
            crate::whatsapp_panel::WhatsAppPanelEvent::AskClaude { jid, instruction } => this.ask_claude_about_chat(jid, instruction, cx),
        });
        // The unread badge on the mode tab follows the panel.
        let obs = cx.observe(&panel, |_, _, cx| cx.notify());
        self._subscriptions.push(sub);
        self._subscriptions.push(obs);
        self.whatsapp_panel = Some(panel.clone());
        panel
    }

    /// Open (or focus) a WhatsApp conversation as an editor tab.
    pub(crate) fn open_chat(&mut self, jid: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.diff_active = false;
        let key = self.active_key();
        let tabs = self.editors.entry(key.clone()).or_default();
        let view = if let Some(ix) = tabs.tabs.iter().position(|e| e.read(cx).chat_jid(cx) == Some(jid)) {
            tabs.active = ix;
            tabs.tabs[ix].clone()
        } else {
            // A single chat tab: a new conversation replaces the open one (or, failing that, the
            // untouched preview tab).
            let old = tabs.tabs.iter().position(|e| e.read(cx).chat_jid(cx).is_some()).or_else(|| {
                tabs.tabs.iter().position(|e| {
                    let e = e.read(cx);
                    e.preview && !e.is_dirty()
                })
            });
            let font_size = self.font_size;
            let (j, n) = (jid.to_string(), name.to_string());
            let view = cx.new(|cx| {
                let mut e = EditorView::new_chat(j, n, font_size, cx);
                e.preview = true;
                e
            });
            self.subscribe_editor(&view, window, cx);
            let tabs = self.editors.entry(key).or_default();
            match old {
                Some(i) => {
                    tabs.tabs[i] = view.clone();
                    tabs.active = i;
                }
                None => {
                    tabs.tabs.push(view.clone());
                    tabs.active = tabs.tabs.len() - 1;
                }
            }
            view
        };
        if let Some(chat) = view.read(cx).chat.clone() {
            chat.update(cx, |c, cx| c.take_draft(cx));
            let focus = chat.read(cx).composer_focus(cx);
            window.focus(&focus);
        }
        self.show_whatsapp(cx);
    }

    fn on_servers_event(&mut self, event: &crate::servers_panel::ServersPanelEvent, window: &mut Window, cx: &mut Context<Self>) {
        use crate::servers_panel::ServersPanelEvent as E;
        match event {
            E::Changed(list) => {
                let Some(id) = self.state.active_project.clone() else { return };
                if let Some(p) = self.state.project_mut(&id) {
                    p.servers = list.clone();
                }
                self.persist(cx);
            }
            E::OpenTerminal(server) => self.open_ssh(server, window, cx),
            E::RunDev(cmd) => {
                let already = self.terminals.get(&self.active_key()).is_some_and(|t| t.tabs.iter().any(|v| v.read(cx).dev_dir.as_ref() == Some(&cmd.dir) && !v.read(cx).has_exited()));
                if !already {
                    // A normal interactive terminal with the command typed in: same environment
                    // as running it by hand (`.zshrc` → nvm's node, aliases…). `zsh -l -c` skips
                    // .zshrc and picked another node.
                    if let Some(view) = self.spawn_terminal_with(Some(cmd.dir.clone()), None, None, window, cx) {
                        let (label, dir, line) = (cmd.label.clone(), cmd.dir.clone(), cmd.command_line());
                        view.update(cx, |t, cx| {
                            t.label = Some(format!("dev · {label}"));
                            t.dev_dir = Some(dir);
                            // Typed ahead: the shell reads it once its startup files are loaded.
                            t.send_input(&format!("{line}\r"));
                            cx.notify();
                        });
                    }
                }
                self.sync_dev(cx);
            }
            E::StopDev(dir) => {
                let view = self.terminals.get(&self.active_key()).and_then(|t| t.tabs.iter().find(|v| v.read(cx).dev_dir.as_ref() == Some(dir)).cloned());
                if let Some(view) = view {
                    // Dropping the tab hangs up and then kills the whole process group.
                    self.close_terminal_view(&view, window, cx);
                }
                self.sync_dev(cx);
            }
            E::OpenRemoteFile { server, remote, local } => {
                self.remote_files.insert(local.clone(), (server.clone(), remote.clone()));
                self.open_file(local, true, window, cx);
            }
        }
    }

    /// An editor saved a file that came from a server: upload it back.
    fn upload_if_remote(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some((server, remote)) = self.remote_files.get(path).cloned() else { return };
        let local = path.to_path_buf();
        cx.spawn(async move |this, cx| {
            let (s2, r2) = (server.clone(), remote.clone());
            let res = cx.background_executor().spawn(async move { ssh_remote::upload(&s2, &local, &r2) }).await;
            let _ = this.update(cx, |this, cx| {
                this.error = match res {
                    Ok(()) => None,
                    Err(e) => Some(format!("Upload to {}:{remote} failed: {e:#}", server.name).into()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn show_ports(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.sidebar_mode = SidebarMode::Ports;
        let roots = self.port_roots();
        let pp = self.ports_panel.get_or_insert_with(|| cx.new(crate::ports::PortsPanel::new)).clone();
        pp.update(cx, |p, cx| {
            p.set_roots(roots, cx);
            p.refresh(cx);
        });
        cx.notify();
    }

    /// Project folders (repos + root) used to tag which listeners belong to this project.
    fn port_roots(&self) -> Vec<(String, PathBuf)> {
        self.active_project()
            .map(|p| {
                let mut v: Vec<(String, PathBuf)> = p.repositories.iter().map(|r| (r.name.clone(), r.path.clone())).collect();
                if let Some(root) = &p.root {
                    v.push((p.name.clone(), root.clone()));
                }
                v
            })
            .unwrap_or_default()
    }

    pub(crate) fn tools_panel_entity(&self) -> Option<Entity<ToolsPanel>> {
        self.tools_panel.clone()
    }

    fn show_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.sidebar_mode = SidebarMode::Tools;
        self.ensure_tools_panel(window, cx);
        self.sync_server_status(cx);
        cx.notify();
    }

    /// Format the active file with the oxfmt of the repository that contains it.
    fn format_file(&mut self, _: &FormatFile, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        let Some(path) = editor.read(cx).path().cloned() else { return };
        let repo = self
            .active_project()
            .and_then(|p| p.repositories.iter().map(|r| r.path.clone()).chain(p.root.clone()).find(|r| path.starts_with(r)))
            .unwrap_or_else(|| path.parent().map(|p| p.to_path_buf()).unwrap_or_default());
        if editor.read(cx).is_dirty() && !editor.update(cx, |e, cx| e.save(cx)) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let tool = code_quality::locate(code_quality::Tool::Oxfmt, &repo).ok_or_else(|| anyhow::anyhow!("oxfmt not found for this repository"))?;
                    code_quality::format(&tool, &[p])
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => editor.update(cx, |e, cx| e.check_disk(cx)),
                    Err(e) => this.error = Some(format!("Format failed: {e:#}").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn show_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.sidebar_mode = SidebarMode::Git;
        let repos = self.project_repos();
        let gp = match &self.git_panel {
            Some(gp) => gp.clone(),
            None => {
                let gp = cx.new(|_| GitPanel::new());
                let sub = cx.subscribe_in(&gp, window, |this, _, event, window, cx| this.on_git_event(event, window, cx));
                self._subscriptions.push(sub);
                self.git_panel = Some(gp.clone());
                gp
            }
        };
        gp.update(cx, |g, cx| g.set_repos(repos, window, cx));
        cx.notify();
    }

    /// Diff of one working-tree/index change in a tab.
    fn open_change_diff(&mut self, repo: PathBuf, change: git_engine::FileChange, staged: bool, window: &mut Window, cx: &mut Context<Self>) {
        let title = format!("{}{}", change.path.rsplit('/').next().unwrap_or(&change.path), if staged { " (staged)" } else { "" });
        let source = DiffSource::Changes { repo: repo.clone(), path: change.path.clone(), staged };
        self.load_diff(title, source, Some(repo.clone()), window, cx, move || git_engine::file_diff(&repo, &change, staged));
    }

    fn on_git_event(&mut self, event: &GitPanelEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            GitPanelEvent::OpenDiff { repo, change, staged } => {
                // Images/media/binaries: a text diff says only "Binary file"; show the file.
                let full = repo.join(&change.path);
                if crate::asset_view::asset_kind(&full).is_some() && full.is_file() {
                    self.open_file_preview(&full, window, cx);
                    self.persist(cx);
                    return;
                }
                self.open_change_diff(repo.clone(), change.clone(), *staged, window, cx);
            }
            GitPanelEvent::OpenCommit { repo, commit } => {
                let (r, hash) = (repo.clone(), commit.hash.clone());
                let title = format!("{} {}", commit.short, commit.subject);
                self.load_diff(title, DiffSource::ReadOnly, Some(repo.clone()), window, cx, move || git_engine::show_commit(&r, &hash));
            }
            GitPanelEvent::CompareBranch { repo, base, head } => {
                let (r, b, h) = (repo.clone(), base.clone(), head.clone());
                let title = format!("{base}…{head}");
                self.load_diff(title, DiffSource::ReadOnly, Some(repo.clone()), window, cx, move || git_engine::diff_refs(&r, &b, &h, None));
            }
            GitPanelEvent::OpenFile { path, repo, change, staged } => {
                let path = path.clone();
                self.open_file(&path, true, window, cx);
                self.persist(cx);
                // Jump to the first change once the diff is computed (off the UI thread).
                let (repo, change, staged) = (repo.clone(), change.clone(), *staged);
                cx.spawn(async move |this, cx| {
                    let line = cx
                        .background_executor()
                        .spawn(async move { git_engine::file_diff(&repo, &change, staged).ok().and_then(|t| git_engine::parse_diff(&t).first()?.first_change_line()) })
                        .await;
                    let Some(line) = line else { return };
                    let _ = this.update(cx, |this, cx| {
                        let editor = this.editors.values().flat_map(|t| t.tabs.iter()).find(|e| e.read(cx).path().map(|p| p.as_path()) == Some(path.as_path())).cloned();
                        if let Some(e) = editor {
                            e.update(cx, |e, cx| e.go_to(line, 0, cx));
                        }
                    });
                })
                .detach();
            }
            GitPanelEvent::ContextMenu { position, path } => {
                let exists = path.exists();
                self.context_menu = Some(ContextMenu { position: *position, target: path.clone(), is_dir: path.is_dir(), is_root: !exists, locate: true });
                cx.notify();
            }
            GitPanelEvent::Changed => {
                self.refresh_branches();
                // Keep an open working-tree diff in sync after staging/commits.
                if let Some(d) = self.diff.clone() {
                    if let DiffSource::Changes { repo, path, staged } = d.read(cx).source.clone() {
                        cx.spawn(async move |_, cx| {
                            let r = repo.clone();
                            let text = cx
                                .background_executor()
                                .spawn(async move {
                                    let st = git_engine::status(&r).ok()?;
                                    let change = st.files.into_iter().find(|f| f.path == path)?;
                                    git_engine::file_diff(&r, &change, staged).ok()
                                })
                                .await
                                .unwrap_or_default();
                            let _ = d.update(cx, |d, cx| d.set_files(git_engine::parse_diff(&text), cx));
                        })
                        .detach();
                    }
                }
                cx.notify();
            }
        }
    }

    fn load_diff(
        &mut self,
        title: String,
        source: DiffSource,
        root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
        compute: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
    ) {
        self.diff_seq += 1;
        let seq = self.diff_seq;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { compute() }).await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                _ if this.diff_seq != seq => {}
                Err(e) => {
                    this.error = Some(format!("{e:#}").into());
                    cx.notify();
                }
                Ok(text) => {
                    let files = git_engine::parse_diff(&text);
                    let view = cx.new(|cx| DiffView::new(title, source, root, files, cx));
                    let sub = cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
                        DiffViewEvent::ApplyHunk { repo, patch, reverse } => {
                            if let Some(gp) = &this.git_panel {
                                let (repo, patch, reverse) = (repo.clone(), patch.clone(), *reverse);
                                gp.update(cx, |g, cx| g.apply_hunk(&repo, patch, reverse, cx));
                            }
                        }
                        DiffViewEvent::OpenFile { path, line } => {
                            let (path, line) = (path.clone(), *line);
                            this.open_file(&path, true, window, cx);
                            if let Some(e) = this.active_editor().cloned() {
                                e.update(cx, |e, cx| e.go_to(line, 0, cx));
                            }
                        }
                        DiffViewEvent::Resolve(_) => {}
                    });
                    this._subscriptions.push(sub);
                    window.focus(&view.focus_handle(cx));
                    this.diff = Some(view);
                    this.diff_active = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn search_in_project(&mut self, _: &SearchInProject, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.sidebar_mode = SidebarMode::Search;
        let sp = self.ensure_search_panel(window, cx);
        // Prefill with the editor selection, like most editors.
        if let Some(sel) = self.active_editor().map(|e| e.read(cx).doc.selected_text()).filter(|s| !s.is_empty() && !s.contains('\n')) {
            sp.update(cx, |s, cx| s.set_query(&sel, cx));
        }
        sp.update(cx, |s, cx| s.focus_input(window, cx));
        self.refresh_index(cx);
        cx.notify();
    }

    fn close_terminal_view(&mut self, view: &Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        for tabs in self.terminals.values_mut() {
            tabs.remove(view);
        }
        if self.active_terminal().is_some() {
            self.focus_terminal(window, cx);
        } else {
            self.focus_main(window, cx);
        }
        self.persist(cx);
        cx.notify();
    }

    /// Most urgent Claude badge among a project's terminals (optionally only one repo's):
    /// "needs you" beats "finished".
    fn attention_in(&self, pid: &str, repo: Option<&str>, cx: &App) -> Option<crate::terminal_view::Attention> {
        use crate::terminal_view::Attention;
        let tabs = self.terminals.get(pid)?;
        let mut best = None;
        for t in &tabs.tabs {
            let t = t.read(cx);
            if repo.is_some() && t.repository.as_deref() != repo {
                continue;
            }
            match t.attention {
                Some(Attention::NeedsInput) => return Some(Attention::NeedsInput),
                Some(a) => best = Some(a),
                None => {}
            }
        }
        best
    }

    /// Switch to the project and focus its terminal that has a Claude badge (repo first).
    fn jump_to_attention(&mut self, pid: &str, repo: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        use crate::terminal_view::Attention;
        if self.state.active_project.as_deref() != Some(pid) {
            self.switch_project(pid, window, cx);
        }
        let Some(tabs) = self.terminals.get(pid) else { return };
        let rank = |a: Option<Attention>| match a {
            Some(Attention::NeedsInput) => 2,
            Some(Attention::Done) => 1,
            None => 0,
        };
        let ix = tabs
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| repo.is_none() || t.read(cx).repository.as_deref() == repo)
            .max_by_key(|(_, t)| rank(t.read(cx).attention))
            .filter(|(_, t)| t.read(cx).attention.is_some())
            .map(|(i, _)| i);
        if let Some(ix) = ix {
            self.select_terminal(ix, window, cx);
        }
    }

    /// Show a terminal of this window by its `JT_TERMINAL_ID` (sessions board).
    pub(crate) fn focus_terminal_id(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        for tabs in self.terminals.values_mut() {
            if let Some(ix) = tabs.tabs.iter().position(|t| t.read(cx).jt_id == id) {
                tabs.active = ix;
                let t = tabs.tabs[ix].clone();
                self.terminal_visible = true;
                window.focus(&t.focus_handle(cx));
                cx.notify();
                return;
            }
        }
    }

    fn select_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tabs) = self.terminals.get_mut(&self.active_key()) {
            if ix < tabs.tabs.len() {
                tabs.active = ix;
            }
        }
        self.terminal_visible = true;
        self.focus_terminal(window, cx);
        cx.notify();
    }

    fn open_repo_terminal(&mut self, pid: &str, repo_name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_project(pid, window, cx);
        if let Some(tabs) = self.terminals.get_mut(pid) {
            if let Some(ix) = tabs.tabs.iter().position(|t| t.read(cx).repository.as_deref() == Some(repo_name) && !t.read(cx).has_exited()) {
                tabs.active = ix;
                self.terminal_visible = true;
                self.focus_terminal(window, cx);
                cx.notify();
                return;
            }
        }
        let path = self.state.project(pid).and_then(|p| p.repositories.iter().find(|r| r.name == repo_name)).map(|r| r.path.clone());
        self.open_terminal(path, Some(repo_name.to_string()), window, cx);
        self.persist(cx);
    }

    // ---- editors ------------------------------------------------------------------------

    /// New blank Word/Excel/PowerPoint file: ask where, write the template, open it.
    fn new_office(&mut self, ext: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .active_project()
            .and_then(|p| p.root.clone().or_else(|| p.repositories.first().map(|r| r.path.clone())))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Documents")))
            .unwrap_or_default();
        let name = match ext {
            "docx" => "Document.docx",
            "xlsx" => "Workbook.xlsx",
            _ => "Presentation.pptx",
        };
        let rx = cx.prompt_for_new_path(&dir, Some(name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let path = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)) { path } else { path.with_extension(ext) };
            let Some(blank) = crate::office::template(&path) else { return };
            let res = std::fs::write(&path, blank);
            let _ = this.update_in(cx, |this, window, cx| match res {
                Ok(()) => this.open_file(&path, true, window, cx),
                Err(e) => {
                    this.error = Some(format!("{}: {e}", path.display()).into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn open_file(&mut self, path: &Path, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.diff_active = false;
        self.diff_seq += 1;
        let key = self.active_key();
        let tabs = self.editors.entry(key.clone()).or_default();
        if let Some(ix) = tabs.tabs.iter().position(|e| e.read(cx).path().map(|p| p.as_path()) == Some(path)) {
            tabs.active = ix;
        } else {
            let font_size = self.font_size;
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if size > crate::asset_view::MAX_OPEN_BYTES {
                self.error = Some(format!("{} is {} MB; files over 100 MB can't be opened.", path.display(), size / (1024 * 1024)).into());
                cx.notify();
                return;
            }
            let csv_doc = crate::asset_view::is_text_table(path).then(|| editor::Document::open(path).ok()).flatten();
            let docx = if crate::office::docx::is_docx(path) {
                match crate::office::docx::DocxModel::load(path) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        self.error = Some(format!("{}: {e}", path.display()).into());
                        cx.notify();
                        return;
                    }
                }
            } else {
                None
            };
            let view = if let Some(m) = docx {
                cx.new(|cx| EditorView::new_docx(m, font_size, cx))
            } else if let Some(doc) = csv_doc {
                cx.new(|cx| EditorView::new_csv(doc, font_size, cx))
            } else if let Some(kind) = crate::asset_view::asset_kind(path) {
                let p = path.to_path_buf();
                cx.new(|cx| EditorView::new_asset(p, kind, font_size, cx))
            } else {
                match editor::Document::open(path) {
                    Ok(doc) => cx.new(|cx| EditorView::new(doc, font_size, cx)),
                    // Unknown binary formats: show the file card instead of failing.
                    Err(e) if format!("{e}").contains("binary") || format!("{e}").contains("UTF-8") => {
                        let p = path.to_path_buf();
                        cx.new(|cx| EditorView::new_asset(p, crate::asset_view::AssetKind::Binary, font_size, cx))
                    }
                    Err(e) => {
                        self.error = Some(format!("{e:#}").into());
                        cx.notify();
                        return;
                    }
                }
            };
            self.attach_file_tools(&view, cx);
            self.subscribe_editor(&view, window, cx);
            let tabs = self.editors.entry(key).or_default();
            tabs.tabs.push(view);
            tabs.active = tabs.tabs.len() - 1;
        }
        if focus {
            if let Some(e) = self.active_editor() {
                window.focus(&e.focus_handle(cx));
            }
        }
        cx.notify();
    }

    fn subscribe_editor(&mut self, view: &Entity<EditorView>, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.subscribe_in(view, window, |this, view, event, window, cx| match event {
            EditorEvent::SaveAs => this.save_as(view.clone(), false, window, cx),
            EditorEvent::OpenLocation { path, line, character } => {
                let (path, line, character) = (path.clone(), *line, *character);
                this.open_file(&path, true, window, cx);
                if let Some(e) = this.active_editor().cloned() {
                    e.update(cx, |e, cx| e.go_to(line, character, cx));
                }
                this.persist(cx);
            }
            EditorEvent::Changed => {
                this.sync_ide(cx);
                cx.notify();
            }
            EditorEvent::SelectionChanged => this.sync_ide(cx),
            EditorEvent::Saved(path) => {
                if *path == workspace_state::settings_path() {
                    this.reload_settings(window, cx);
                }
                this.upload_if_remote(path, cx);
                if let Some(gp) = &this.git_panel {
                    gp.update(cx, |g, cx| g.refresh_path(path, cx));
                }
            }
            EditorEvent::Error(e) => {
                this.error = Some(e.clone().into());
                cx.notify();
            }
            EditorEvent::OpenFile(path) => {
                let path = path.clone();
                this.open_file(&path, true, window, cx);
            }
            EditorEvent::AskClaude { jid, instruction } => this.ask_claude_about_chat(jid, instruction, cx),
            EditorEvent::OpenChat { jid, name } => {
                this.pending_chat = Some((jid.clone(), name.clone()));
                cx.notify();
            }
        });
        self._subscriptions.push(sub);
    }

    /// ⌘N: empty untitled buffer; its first save asks where to put it.
    fn new_untitled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff_active = false;
        let font_size = self.font_size;
        let view = cx.new(|cx| EditorView::new(editor::Document::from_str("", editor::Language::PlainText), font_size, cx));
        self.subscribe_editor(&view, window, cx);
        let tabs = self.editors.entry(self.active_key()).or_default();
        tabs.tabs.push(view.clone());
        tabs.active = tabs.tabs.len() - 1;
        window.focus(&view.focus_handle(cx));
        cx.notify();
    }

    /// Native save panel for an untitled buffer (starting in the active project); the chosen
    /// extension sets the language. `close` removes the tab once written.
    fn save_as(&mut self, view: Entity<EditorView>, close: bool, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .active_project()
            .and_then(|p| p.root.clone().or_else(|| p.repositories.first().map(|r| r.path.clone())))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        let rx = cx.prompt_for_new_path(&dir, Some("untitled.txt"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                view.update(cx, |v, cx| v.set_path(path.clone(), cx));
                if !view.update(cx, |v, cx| v.save(cx)) {
                    return;
                }
                this.attach_file_tools(&view, cx);
                this.persist(cx);
                if close {
                    this.remove_editor(&view, window, cx);
                }
            });
        })
        .detach();
    }

    /// Open files dropped on the window or handed over by Finder/Dock ("Open With").
    /// Folders and files that can't be opened are reported instead of silently ignored.
    pub fn open_dropped(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let (files, skipped): (Vec<&PathBuf>, Vec<&PathBuf>) = paths.iter().partition(|p| p.is_file());
        let mut problems: Vec<String> = Vec::new();
        let dirs = skipped.iter().filter(|p| p.is_dir()).count();
        if dirs > 0 {
            problems.push(if dirs == 1 { "1 folder skipped (use Add Repository… for folders)".into() } else { format!("{dirs} folders skipped (use Add Repository… for folders)") });
        }
        let missing = skipped.len() - dirs;
        if missing > 0 {
            problems.push(format!("{missing} item(s) not found"));
        }
        let last = files.len().saturating_sub(1);
        for (ix, path) in files.iter().enumerate() {
            let before = self.error.clone();
            self.error = None;
            self.open_file(path, ix == last, window, cx);
            if let Some(err) = self.error.take() {
                problems.push(err.to_string());
            } else {
                self.error = before;
            }
        }
        if !problems.is_empty() {
            self.error = Some(format!("Couldn't open everything dropped: {}", problems.join(" · ")).into());
        }
        if !files.is_empty() {
            self.persist(cx);
        }
        cx.notify();
    }

    /// Find oxfmt/oxlint for the file's repository (cached) and hand them to the editor.
    fn attach_file_tools(&mut self, view: &Entity<EditorView>, cx: &mut Context<Self>) {
        let Some(path) = view.read(cx).path().cloned() else { return };
        if view.read(cx).asset.is_some() || view.read(cx).docx.is_some() {
            return;
        }
        let Some(repo) = self
            .active_project()
            .and_then(|p| p.repositories.iter().map(|r| r.path.clone()).chain(p.root.clone()).filter(|r| path.starts_with(r)).max_by_key(|r| r.as_os_str().len()))
        else {
            return;
        };
        if let Some(tools) = self.file_tools.get(&repo).cloned() {
            view.update(cx, |v, cx| v.set_tools(tools.clone(), cx));
            self.attach_language_servers(view, &repo, &tools, cx);
            return;
        }
        let view = view.downgrade();
        cx.spawn(async move |this, cx| {
            let r = repo.clone();
            let tools = cx
                .background_executor()
                .spawn(async move {
                    crate::editor_view::FileTools {
                        oxfmt: code_quality::locate(code_quality::Tool::Oxfmt, &r),
                        oxlint: code_quality::locate(code_quality::Tool::Oxlint, &r),
                    }
                })
                .await;
            let _ = this.update(cx, |this, _| {
                this.file_tools.insert(repo.clone(), tools.clone());
            });
            let _ = view.update(cx, |v, cx| v.set_tools(tools.clone(), cx));
            if let Some(view) = view.upgrade() {
                let _ = this.update(cx, |this, cx| this.attach_language_servers(&view, &repo, &tools, cx));
            }
        })
        .detach();
    }

    /// Single-click open: reuse the current preview tab (if untouched) instead of adding one.
    pub fn open_file_preview(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.active_key();
        let already = self.editors.get(&key).is_some_and(|t| t.tabs.iter().any(|e| e.read(cx).path().map(|p| p.as_path()) == Some(path)));
        if already {
            self.open_file(path, true, window, cx);
            return;
        }
        let old = self.editors.get(&key).and_then(|t| {
            t.tabs.iter().position(|e| {
                let e = e.read(cx);
                e.preview && !e.is_dirty()
            })
        });
        let before = self.editors.get(&key).map(|t| t.tabs.len()).unwrap_or(0);
        self.open_file(path, true, window, cx);
        let Some(tabs) = self.editors.get_mut(&key) else { return };
        if tabs.tabs.len() != before + 1 {
            return; // open failed
        }
        let new = tabs.tabs.pop().expect("just opened");
        new.update(cx, |e, _| e.preview = true);
        match old {
            Some(i) => {
                // Dropping the old entity closes it (LSP didClose in EditorView::drop).
                tabs.tabs[i] = new;
                tabs.active = i;
            }
            None => {
                tabs.tabs.push(new);
                tabs.active = tabs.tabs.len() - 1;
            }
        }
        cx.notify();
    }

    /// Keep the active editor open (double-click in the tree or on its tab).
    fn pin_active_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.active_editor().cloned() {
            e.update(cx, |e, _| e.preview = false);
            cx.notify();
        }
    }

    fn close_editor_view(&mut self, view: Entity<EditorView>, window: &mut Window, cx: &mut Context<Self>) {
        if view.read(cx).is_dirty() {
            let name = view.read(cx).title();
            let answer = window.prompt(
                PromptLevel::Warning,
                &format!("Do you want to save the changes to “{name}”?"),
                Some("Your changes will be lost if you don't save them."),
                &["Save", "Don't Save", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let answer = answer.await.unwrap_or(2);
                let _ = this.update_in(cx, |this, window, cx| match answer {
                    0 if view.read(cx).is_untitled() => this.save_as(view.clone(), true, window, cx),
                    0 => {
                        if view.update(cx, |v, cx| v.save(cx)) {
                            this.remove_editor(&view, window, cx);
                        }
                    }
                    1 => this.remove_editor(&view, window, cx),
                    _ => {}
                });
            })
            .detach();
        } else {
            self.remove_editor(&view, window, cx);
        }
    }

    fn remove_editor(&mut self, view: &Entity<EditorView>, window: &mut Window, cx: &mut Context<Self>) {
        for tabs in self.editors.values_mut() {
            tabs.remove(view);
        }
        self.focus_main(window, cx);
        self.persist(cx);
        cx.notify();
    }

    fn select_editor(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.diff_active = false;
        if let Some(tabs) = self.editors.get_mut(&self.active_key()) {
            if ix < tabs.tabs.len() {
                tabs.active = ix;
            }
        }
        self.focus_main(window, cx);
        // A conversation tab brings up the WhatsApp sidebar.
        let is_chat = self.editors.get(&self.active_key()).and_then(|t| t.tabs.get(t.active)).is_some_and(|e| e.read(cx).chat_jid(cx).is_some());
        if is_chat {
            self.show_whatsapp(cx);
        }
        cx.notify();
    }

    fn close_diff(&mut self, _: &CloseDiff, window: &mut Window, cx: &mut Context<Self>) {
        self.diff = None;
        self.diff_active = false;
        self.focus_main(window, cx);
        cx.notify();
    }

    fn save_all(&mut self, cx: &mut Context<Self>) {
        for tabs in self.editors.values() {
            for e in &tabs.tabs {
                if e.read(cx).is_dirty() && !e.read(cx).is_untitled() {
                    e.update(cx, |e, cx| e.save(cx));
                }
            }
        }
    }

    fn dirty_count(&self, cx: &App) -> usize {
        self.editors.values().flat_map(|t| t.tabs.iter()).filter(|e| e.read(cx).is_dirty()).count()
    }

    // ---- projects -----------------------------------------------------------------------

    /// Switch to another project. The project being left is unloaded: its terminals are
    /// closed (processes get SIGHUP) and its editors released; both are restored on return.
    fn switch_project(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.active_project.as_deref() == Some(id) {
            return;
        }
        if self.focus_project_window(id, window, cx) {
            return;
        }
        if self.show_locked(id, window, cx) {
            return;
        }
        self.activate_project(Some(id.to_string()), window, cx);
    }

    /// Unload the current project and load `new` (None = no project).
    fn activate_project(&mut self, new: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let id = new.as_deref().unwrap_or(SCRATCH);
        let old = self.active_key();
        self.persist(cx); // records terminal cwds and open files of the old project
        self.terminals.remove(&old);
        self.editors.remove(&old);
        // Stops tsgo / oxlint servers of the project being left.
        self.lsp.clear();
        self.sync_server_status(cx);
        self.search_panel = None;
        self.quick_open = None;
        self.diff = None;
        self.diff_active = false;
        if let Some(tp) = self.tools_panel.clone() {
            let repos = self.state.project(id).map(|p| p.repositories.iter().map(|r| (r.name.clone(), r.path.clone())).collect()).unwrap_or_default();
            tp.update(cx, |t, cx| t.set_repos(repos, cx));
        }
        if let Some(gp) = self.git_panel.clone() {
            let repos = self.state.project(id).map(|p| p.repositories.iter().map(|r| (r.name.clone(), r.path.clone())).collect()).unwrap_or_default();
            gp.update(cx, |g, cx| g.set_repos(repos, window, cx));
        }
        self.state.active_project = new.clone();
        self.collapsed.remove(id);
        self.ensure_project_loaded(window, cx);
        if let Some(sp) = self.servers_panel.clone() {
            let servers = self.active_project().map(|p| p.servers.clone()).unwrap_or_default();
            sp.update(cx, |p, cx| p.set_servers(servers, cx));
            self.sync_dev(cx);
        }
        self.persist(cx);
        if let Some((pid, action)) = self.pending_launch.take() {
            if new.as_deref() == Some(pid.as_str()) {
                self.run_launch_action(action, window, cx);
            }
        }
        cx.notify();
    }

    /// If `id` is already open in another window, bring that window forward (a project lives
    /// in one window only). The pending launcher action moves along with it.
    fn focus_project_window(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(other) = window_with_project(id, Some(window.window_handle()), cx) else { return false };
        let pending = self.pending_launch.take().filter(|(pid, _)| pid == id);
        cx.defer(move |cx| {
            cx.activate(true);
            let _ = other.update(cx, |w, window, cx| {
                window.activate_window();
                if let Some((pid, action)) = pending {
                    w.launcher_run(pid, action, window, cx);
                }
            });
        });
        true
    }

    /// Open `id` (None = empty) in a new window, cascaded from this one.
    fn open_project_window(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.persist(cx);
        let pending = self.pending_launch.take().filter(|(pid, _)| Some(pid) == id.as_ref());
        let mut state = self.store.load();
        state.active_project = id;
        let b = window.bounds();
        let bounds = Bounds::new(point(b.origin.x + px(28.), b.origin.y + px(28.)), b.size);
        let store = StateStore::new(self.store.path().parent().map(Path::to_path_buf).unwrap_or_default());
        cx.defer(move |cx| match crate::open_workspace_window(store, state, bounds, false, cx) {
            Ok(handle) => {
                if let Some((pid, action)) = pending {
                    let _ = handle.update(cx, |w, window, cx| w.launcher_run(pid, action, window, cx));
                }
            }
            Err(e) => crate::startup_log(&format!("new window: {e:#}")),
        });
    }

    /// The window is closing: save its project and hand "primary" to another window.
    fn on_close(&mut self, cx: &mut Context<Self>) {
        if self.state.autosave {
            self.save_all(cx);
        }
        self.save_scrollback(cx);
        self.persist(cx);
        if self.primary {
            self.primary = false;
            cx.defer(|cx| {
                if let Some(next) = workspace_windows(cx).into_iter().next() {
                    let _ = next.update(cx, |w, _, cx| {
                        w.primary = true;
                        w.persist(cx);
                    });
                }
            });
        }
    }

    /// Ask before leaving a project that has live terminals or unsaved files.
    fn request_switch_project(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.active_project.as_deref() == Some(id.as_str()) {
            return;
        }
        if self.focus_project_window(&id, window, cx) {
            return;
        }
        if self.show_locked(&id, window, cx) {
            return;
        }
        let old = self.active_key();
        let terminals = self.terminals.get(&old).map(|t| t.tabs.iter().filter(|t| !t.read(cx).has_exited()).count()).unwrap_or(0);
        let dirty: Vec<Entity<EditorView>> = self
            .editors
            .get(&old)
            .map(|t| t.tabs.iter().filter(|e| e.read(cx).is_dirty()).cloned().collect())
            .unwrap_or_default();
        if terminals == 0 && dirty.is_empty() {
            self.switch_project(&id, window, cx);
            return;
        }
        let old_name = self.active_project().map(|p| p.name.clone()).unwrap_or_else(|| "Scratch".into());
        let new_name = self.state.project(&id).map(|p| p.name.clone()).unwrap_or_default();
        let mut detail = String::new();
        if terminals > 0 {
            detail.push_str(&format!(
                "{terminals} terminal(s) in “{old_name}” will be closed and their running processes stopped. They reopen in the same folders when you come back."
            ));
        }
        if !dirty.is_empty() {
            if !detail.is_empty() {
                detail.push_str("\n\n");
            }
            detail.push_str(&format!("{} unsaved file(s) will be saved.", dirty.len()));
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Switch to “{new_name}”?"),
            Some(&detail),
            &["Switch", "New Window", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            if answer == Ok(1) {
                let _ = this.update_in(cx, |this, window, cx| this.open_project_window(Some(id.clone()), window, cx));
                return;
            }
            if answer != Ok(0) {
                let _ = this.update(cx, |this, _| this.pending_launch = None);
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                for e in &dirty {
                    e.update(cx, |e, cx| e.save(cx));
                }
                this.switch_project(&id, window, cx);
            });
        })
        .detach();
    }

    // ---- schedules & blocked websites -------------------------------------------------

    /// Recompute which projects are outside their hours. True if anything changed.
    fn refresh_locks(&mut self) -> bool {
        let (now, offset) = clock_now();
        let locked: HashSet<String> = self
            .state
            .projects
            .iter()
            .filter(|p| p.unlocked_until.is_none_or(|t| t <= now) && self.settings.locked_reason(&p.id, now, offset).is_some()).map(|p| p.id.clone()).collect();
        let changed = locked != self.locked;
        self.locked = locked;
        changed
    }

    /// If `id` is locked, tell the user when it opens and return true.
    fn show_locked(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.refresh_locks();
        if !self.locked.contains(id) {
            return false;
        }
        let (now, offset) = clock_now();
        let hours = self.settings.locked_reason(id, now, offset).unwrap_or_default();
        let name = self.state.project(id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string());
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("“{name}” is locked right now"),
            Some(&format!("Its hours: {hours}.\n\nEmergency? Open it anyway for a while. Schedules are in settings.toml (⌘,).")),
            &["Open for 1 hour", "Open for 4 hours", "Cancel"],
            cx,
        );
        let id = id.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let hours = match answer.await {
                Ok(0) => 1,
                Ok(1) => 4,
                _ => {
                    let _ = this.update(cx, |this, _| this.pending_launch = None);
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.unlock_for(&id, hours * 3600, cx);
                this.request_switch_project(id.clone(), window, cx);
            });
        })
        .detach();
        true
    }

    /// Emergency override: `id` stays open `secs` more (from now, or from the current
    /// override if it's later).
    fn unlock_for(&mut self, id: &str, secs: i64, cx: &mut Context<Self>) {
        let (now, _) = clock_now();
        if let Some(p) = self.state.project_mut(id) {
            p.unlocked_until = Some(p.unlocked_until.unwrap_or(now).max(now) + secs);
        }
        self.close_warned = None;
        self.refresh_locks();
        self.push_brave_rules();
        self.persist(cx);
        cx.notify();
    }

    fn schedule_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Only the primary window touches /etc/hosts and the Brave guard on the tick.
        if self.primary && self.applied_hosts.as_ref() != Some(&self.settings.hosts_file().0) {
            self.apply_blocked_websites(cx);
        }
        let changed = self.refresh_locks();
        if self.primary {
            self.push_brave_rules();
            if crate::brave_guard::take_no_js_warning() {
                self.error = Some("Brave: turn on View › Developer › Allow JavaScript from Apple Events so juan-terminal can block adult pages and x.com videos.".into());
                cx.notify();
            }
        }
        let Some(active) = self.state.active_project.clone() else {
            if changed {
                cx.notify();
            }
            return;
        };
        if self.locked.contains(&active) {
            // Hours are over: save files and close the project (terminals get SIGHUP).
            for e in self.editors.get(&active).map(|t| t.tabs.clone()).unwrap_or_default() {
                if e.read(cx).is_dirty() && !e.read(cx).is_untitled() {
                    e.update(cx, |e, cx| e.save(cx));
                }
            }
            let name = self.active_project().map(|p| p.name.clone()).unwrap_or_default();
            let elsewhere = open_elsewhere(window, cx);
            let next = self.state.projects.iter().find(|p| !self.locked.contains(&p.id) && !elsewhere.contains(&p.id)).map(|p| p.id.clone());
            self.activate_project(next, window, cx);
            self.close_warned = None;
            self.error = Some(format!("“{name}” closed: outside its hours (files were saved).").into());
            cx.notify();
            return;
        }
        // Warn 5 minutes before the active project closes.
        let (now, offset) = clock_now();
        let soon = now + 5 * 60;
        let override_ends = self.active_project().and_then(|p| p.unlocked_until).filter(|t| *t > now);
        if override_ends.is_none_or(|t| t <= soon) && self.settings.locked_reason(&active, soon, offset).is_some() {
            if self.close_warned.as_deref() != Some(active.as_str()) {
                self.close_warned = Some(active.clone());
                let name = self.active_project().map(|p| p.name.clone()).unwrap_or_default();
                self.error = Some(format!("“{name}” closes in 5 minutes (end of its hours). Open files will be saved. Need more time: ⌘⇧P → “Keep Project Open 1 More Hour”.").into());
                cx.notify();
            }
        } else {
            self.close_warned = None;
        }
        if changed {
            cx.notify();
        }
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        match workspace_state::ensure_settings_file() {
            Ok(path) => {
                self.open_file(&path, true, window, cx);
                self.pin_active_editor(cx);
            }
            Err(e) => self.error = Some(format!("settings.toml: {e}").into()),
        }
        cx.notify();
    }

    /// settings.toml was saved: reload, re-check schedules, update /etc/hosts.
    fn reload_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match workspace_state::load_settings() {
            Ok(s) => {
                self.settings = s;
                self.error = Some("Settings reloaded".into());
                self.schedule_tick(window, cx);
                self.apply_blocked_websites(cx);
            }
            Err(e) => self.error = Some(format!("{e} (previous settings kept)").into()),
        }
        cx.notify();
    }

    /// Sites Brave blanks right now (schedules and locked projects change with the clock).
    fn push_brave_rules(&self) {
        let (now, offset) = clock_now();
        let locked: Vec<String> = self.locked.iter().cloned().collect();
        crate::brave_guard::set_rules(crate::brave_guard::Rules {
            blocked: self.settings.brave_blocked_at(now, offset, &locked),
            no_video: self.settings.brave_no_video(),
        });
    }

    fn apply_blocked_websites(&mut self, cx: &mut Context<Self>) {
        self.push_brave_rules();
        let (hosts, release) = self.settings.hosts_file();
        self.applied_hosts = Some(hosts.clone());
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { crate::website_block::apply(&hosts, &release) }).await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(a) if a.pending > 0 => {
                        let hours = a.pending_until.map(|t| ((t - clock_now().0).max(0) + 3599) / 3600).unwrap_or(24);
                        this.error = Some(format!("{} removed website(s) stay blocked for ~{hours} h more (removals take 24 h).", a.pending).into());
                    }
                    Ok(a) if a.changed => this.error = Some("Blocked websites updated in /etc/hosts".into()),
                    Ok(_) => return,
                    Err(e) => this.error = Some(format!("Blocked websites not applied: {e}").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn click_project(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.active_project.as_deref() == Some(id) {
            if !self.collapsed.remove(id) {
                self.collapsed.insert(id.to_string());
            }
            cx.notify();
        } else {
            self.request_switch_project(id.to_string(), window, cx);
        }
    }

    // ---- file operations ----------------------------------------------------------------

    fn open_context_menu(&mut self, position: gpui::Point<gpui::Pixels>, target: PathBuf, is_dir: bool, is_root: bool, cx: &mut Context<Self>) {
        self.context_menu = Some(ContextMenu { position, target, is_dir, is_root, locate: false });
        cx.notify();
    }

    fn prompt_name(&mut self, action: NameAction, window: &mut Window, cx: &mut Context<Self>) {
        use crate::text_input::{TextInput, TextInputEvent};
        self.context_menu = None;
        let initial = match &action {
            NameAction::Rename(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            _ => String::new(),
        };
        let placeholder = match &action {
            NameAction::NewFile(_) => "File name (folders with /, e.g. src/new.ts)",
            NameAction::NewFolder(_) => "Folder name",
            NameAction::Rename(_) => "New name",
        };
        let input = cx.new(|cx| {
            let mut i = TextInput::new(placeholder, cx);
            if !initial.is_empty() {
                i.set_text(&initial, cx);
            }
            i
        });
        let sub = cx.subscribe_in(&input, window, |this, input, event, window, cx| match event {
            TextInputEvent::Submit { .. } => {
                let name = input.read(cx).content.trim().to_string();
                this.submit_name(name, window, cx);
            }
            TextInputEvent::Cancel => {
                this.name_prompt = None;
                this.focus_main(window, cx);
                cx.notify();
            }
            TextInputEvent::Changed => {
                if let Some(p) = this.name_prompt.as_mut() {
                    p.error = None;
                }
                cx.notify();
            }
        });
        window.focus(&input.focus_handle(cx));
        self.name_prompt = Some(NamePrompt { input, action, error: None, _sub: sub });
        cx.notify();
    }

    fn submit_name(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.name_prompt.as_ref() else { return };
        let invalid = name.is_empty() || name == "." || name == ".." || name.split('/').any(|p| p == "..");
        let result: Result<(PathBuf, bool), String> = if invalid {
            Err("Invalid name".into())
        } else {
            match &prompt.action {
                NameAction::NewFile(dir) => {
                    let path = dir.join(&name);
                    if path.exists() {
                        Err(format!("“{name}” already exists"))
                    } else {
                        // Office files start from a blank document (an empty file isn't valid).
                        let blank = crate::office::template(&path).unwrap_or(&[]);
                        std::fs::create_dir_all(path.parent().unwrap_or(dir))
                            .and_then(|_| std::fs::OpenOptions::new().write(true).create_new(true).open(&path))
                            .and_then(|mut f| std::io::Write::write_all(&mut f, blank))
                            .map(|_| (path, true))
                            .map_err(|e| e.to_string())
                    }
                }
                NameAction::NewFolder(dir) => {
                    let path = dir.join(&name);
                    if path.exists() {
                        Err(format!("“{name}” already exists"))
                    } else {
                        std::fs::create_dir_all(&path).map(|_| (path, false)).map_err(|e| e.to_string())
                    }
                }
                NameAction::Rename(old) => {
                    if name.contains('/') {
                        Err("Name can't contain /".into())
                    } else {
                        let new = old.with_file_name(&name);
                        if new == *old {
                            Ok((new, false))
                        } else if new.exists() {
                            Err(format!("“{name}” already exists"))
                        } else {
                            std::fs::rename(old, &new).map_err(|e| e.to_string()).map(|_| {
                                // Point open editors at the new location.
                                for tabs in self.editors.values() {
                                    for e in &tabs.tabs {
                                        e.update(cx, |e, cx| {
                                            if let Some(p) = e.doc.path.clone() {
                                                if let Ok(rest) = p.strip_prefix(old) {
                                                    e.doc.path = Some(if rest.as_os_str().is_empty() { new.clone() } else { new.join(rest) });
                                                    cx.notify();
                                                }
                                            }
                                        });
                                    }
                                }
                                (new, false)
                            })
                        }
                    }
                }
            }
        };
        match result {
            Err(e) => {
                if let Some(p) = self.name_prompt.as_mut() {
                    p.error = Some(e.into());
                }
            }
            Ok((path, open)) => {
                self.name_prompt = None;
                if let Some(parent) = path.parent() {
                    // Reveal the result: expand every ancestor inside the tree.
                    let mut dir = Some(parent.to_path_buf());
                    while let Some(d) = dir {
                        self.dir_cache.remove(&d);
                        if self.active_project().is_some_and(|p| p.repositories.iter().any(|r| d.starts_with(&r.path)) || p.root.as_ref().is_some_and(|r| d.starts_with(r))) {
                            self.expanded.insert(d.clone());
                        } else {
                            break;
                        }
                        dir = d.parent().map(|p| p.to_path_buf());
                    }
                }
                self.dir_cache.clear();
                self.others_cache.clear();
                if open {
                    self.open_file(&path, true, window, cx);
                } else {
                    self.focus_main(window, cx);
                }
                if let Some(gp) = &self.git_panel {
                    gp.update(cx, |g, cx| g.refresh_all(cx));
                }
                self.persist(cx);
            }
        }
        cx.notify();
    }

    fn confirm_trash(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let detail = if path.is_dir() { "The folder and everything in it will be moved to the Trash." } else { "The file will be moved to the Trash." };
        let answer = window.prompt(PromptLevel::Warning, &format!("Move “{name}” to the Trash?"), Some(detail), &["Move to Trash", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let p = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { std::process::Command::new("/usr/bin/trash").arg(&p).output() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(out) if out.status.success() => {
                        // Close editors for trashed files (unsaved changes there are discarded).
                        let doomed: Vec<Entity<EditorView>> = this
                            .editors
                            .values()
                            .flat_map(|t| t.tabs.iter())
                            .filter(|e| e.read(cx).path().is_some_and(|p| p.starts_with(&path)))
                            .cloned()
                            .collect();
                        for e in doomed {
                            this.remove_editor(&e, window, cx);
                        }
                        this.dir_cache.clear();
                        this.others_cache.clear();
                        if let Some(gp) = &this.git_panel {
                            gp.update(cx, |g, cx| g.refresh_all(cx));
                        }
                    }
                    Ok(out) => this.error = Some(format!("Could not move to Trash: {}", String::from_utf8_lossy(&out.stderr).trim()).into()),
                    Err(e) => this.error = Some(format!("Could not move to Trash: {e}").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_context_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.context_menu.as_ref()?;
        let target = menu.target.clone();
        let dir = if menu.is_dir { target.clone() } else { target.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| target.clone()) };
        let is_root = menu.is_root;
        let relative = self
            .active_project()
            .and_then(|p| p.repositories.iter().map(|r| r.path.clone()).chain(p.root.clone()).find(|r| target.starts_with(r)))
            .and_then(|root| target.strip_prefix(&root).ok().map(|r| r.to_string_lossy().into_owned()));

        let item = |id: &'static str, icon_path: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(26.))
                .px(px(10.))
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(theme::rgb(theme::TEXT))
                .hover(|s| s.bg(theme::rgb(theme::BRAND_DEEP)))
                .child(if icon_path == "icons/claude.svg" {
                    crate::assets::claude_logo(14.).into_any_element()
                } else {
                    gpui::svg().path(icon_path).size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)).into_any_element()
                })
                .child(label)
        };
        let sep = || div().h(px(1.)).my(px(4.)).bg(theme::rgb(theme::BORDER));
        let (d1, d2, d3, d4, d5) = (dir.clone(), dir.clone(), dir.clone(), target.clone(), target.clone());
        let (t1, t2, t3, t4) = (target.clone(), target.clone(), target.clone(), target.clone());

        let locate = menu.locate;
        let t_locate = target.clone();
        let mut list = div()
            .w(px(230.))
            .p(px(4.))
            .rounded(px(8.))
            .bg(theme::rgb(0x202227))
            .border_1()
            .border_color(theme::rgb(0x33363d))
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.context_menu = None;
                cx.notify();
            }))
            .when(locate, |d| {
                d.child(item("cm-locate", "icons/folder-open.svg", "Locate in Explorer").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.locate_in_tree(&t_locate, cx))))
                    .child(sep())
            })
            .child(item("cm-new-file", "icons/new-file.svg", "New File…").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.prompt_name(NameAction::NewFile(d1.clone()), w, cx))))
            .child(item("cm-new-folder", "icons/new-folder.svg", "New Folder…").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.prompt_name(NameAction::NewFolder(d2.clone()), w, cx))));
        if !is_root {
            list = list
                .child(item("cm-rename", "icons/edit.svg", "Rename…").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.prompt_name(NameAction::Rename(t1.clone()), w, cx))))
                .child(item("cm-trash", "icons/trash.svg", "Move to Trash").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.confirm_trash(t2.clone(), w, cx))));
        }
        list = list
            .child(sep())
            .child(item("cm-finder", "icons/finder.svg", "Reveal in Finder").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.reveal_path(&t3);
                this.context_menu = None;
                cx.notify();
            })))
            .child(item("cm-open-default", "icons/file.svg", "Open with Default App").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.open_with_system(&t4);
                this.context_menu = None;
                cx.notify();
            })))
            .child(item("cm-copy-path", "icons/copy.svg", "Copy Path").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(d4.display().to_string()));
                this.context_menu = None;
                cx.notify();
            })));
        if let Some(rel) = relative.filter(|r| !r.is_empty()) {
            list = list.child(item("cm-copy-rel", "icons/copy.svg", "Copy Relative Path").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel.clone()));
                this.context_menu = None;
                cx.notify();
            })));
        }
        list = list
            .child(sep())
            .child(item("cm-terminal", "icons/terminal.svg", "Open Terminal Here").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                this.context_menu = None;
                this.open_terminal(Some(d3.clone()), None, w, cx);
                this.persist(cx);
            })))
            .child(item("cm-claude", "icons/claude.svg", "Open Claude Here").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                this.context_menu = None;
                let cwd = if d5.is_dir() { d5.clone() } else { d5.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| d5.clone()) };
                this.open_claude(Some(cwd), None, w, cx);
            })));
        Some(gpui::deferred(gpui::anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element())
    }

    fn render_new_tab_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let position = self.new_tab_menu?;
        let servers = self.active_project().map(|p| p.servers.clone()).unwrap_or_default();
        let item = |id: gpui::ElementId, icon_el: AnyElement, label: SharedString, detail: Option<SharedString>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(26.))
                .px(px(10.))
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(theme::rgb(theme::TEXT))
                .hover(|s| s.bg(theme::rgb(theme::BRAND_DEEP)))
                .child(icon_el)
                .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().child(label))
                .when_some(detail, |d, t| d.child(div().flex_none().max_w(px(120.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(t)))
        };
        let svg_icon = |path: &'static str, color: u32| gpui::svg().path(path).size(px(13.)).text_color(theme::rgb(color)).into_any_element();
        let mut list = div()
            .w(px(250.))
            .p(px(4.))
            .rounded(px(8.))
            .bg(theme::rgb(0x202227))
            .border_1()
            .border_color(theme::rgb(0x33363d))
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.new_tab_menu = None;
                cx.notify();
            }))
            .child(item("nt-terminal".into(), svg_icon("icons/terminal.svg", 0x98c379), "Terminal".into(), Some("⌘T".into())).on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                this.new_tab_menu = None;
                this.new_terminal(&NewTerminal, w, cx);
            })))
            .child(item("nt-claude".into(), crate::assets::claude_logo(14.).into_any_element(), "Claude Code".into(), None).on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                this.new_tab_menu = None;
                let cwd = this.active_terminal().and_then(|t| t.read(cx).current_cwd()).or_else(|| this.active_project().and_then(|p| p.default_cwd()));
                this.open_claude(cwd, None, w, cx);
            })));
        list = list.child(div().h(px(1.)).my(px(4.)).bg(theme::rgb(theme::BORDER))).child(
            div().px(px(10.)).py(px(3.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("SSH"),
        );
        if servers.is_empty() {
            list = list.child(item("nt-ssh-none".into(), svg_icon("icons/server.svg", theme::TEXT_FAINT), "No servers — add one in Servers".into(), None).on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                this.new_tab_menu = None;
                this.show_servers(w, cx);
            })));
        }
        for (ix, server) in servers.into_iter().enumerate() {
            let detail: SharedString = if server.user.is_empty() { server.host.clone().into() } else { format!("{}@{}", server.user, server.host).into() };
            let label: SharedString = if server.name.is_empty() { detail.clone() } else { server.name.clone().into() };
            list = list.child(item(("nt-ssh", ix).into(), svg_icon("icons/server.svg", 0x56b6c2), label, Some(detail)).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                this.new_tab_menu = None;
                this.open_ssh(&server, w, cx);
                cx.notify();
            })));
        }
        Some(gpui::deferred(gpui::anchored().position(position).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element())
    }

    fn render_name_prompt(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.name_prompt.as_ref()?;
        let (title, target) = match &p.action {
            NameAction::NewFile(d) => ("New file in", d),
            NameAction::NewFolder(d) => ("New folder in", d),
            NameAction::Rename(f) => ("Rename", f),
        };
        let shown = target
            .to_string_lossy()
            .replace(&std::env::var("HOME").unwrap_or_default(), "~");
        Some(
            div()
                .id("name-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .pt(px(90.))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, w, cx| {
                        this.name_prompt = None;
                        this.focus_main(w, cx);
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .w(px(460.))
                        .p(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .rounded(px(10.))
                        .bg(theme::rgb(theme::SIDEBAR_BG))
                        .border_1()
                        .border_color(theme::rgb(theme::BORDER))
                        .shadow_lg()
                        .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{title} {shown}")))
                        .child(
                            div()
                                .flex()
                                .px(px(8.))
                                .py(px(5.))
                                .rounded(px(6.))
                                .bg(theme::rgb(theme::EDITOR_BG))
                                .border_1()
                                .border_color(theme::rgb(theme::ACCENT))
                                .child(p.input.clone()),
                        )
                        .when_some(p.error.clone(), |d, e| d.child(div().text_size(px(12.)).text_color(theme::rgb(0xf0b3b8)).child(e)))
                        .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Enter to confirm · Esc to cancel")),
                )
                .into_any_element(),
        )
    }

    /// Split a repository's top-level entries into the virtual "others" folder (git-ignored,
    /// dotfiles, config/tooling files) and the regular ones. One `git check-ignore` call.
    fn repo_split(&mut self, repo: &Path) -> (Rc<Vec<DirEntry>>, Rc<Vec<DirEntry>>) {
        if let Some(v) = self.others_cache.get(repo) {
            return v.clone();
        }
        let entries = self.children(repo);
        let ignored: HashSet<String> = {
            use std::io::Write;
            let names: Vec<String> = entries.iter().map(|e| if e.is_dir { format!("{}/", e.name) } else { e.name.to_string() }).collect();
            let child = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["check-ignore", "--stdin", "-z"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn();
            match child {
                Ok(mut c) => {
                    if let Some(mut stdin) = c.stdin.take() {
                        let _ = stdin.write_all(names.join("\0").as_bytes());
                    }
                    c.wait_with_output()
                        .map(|o| o.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).trim_end_matches('/').to_string()).collect())
                        .unwrap_or_default()
                }
                Err(_) => HashSet::new(),
            }
        };
        let (others, regular): (Vec<DirEntry>, Vec<DirEntry>) =
            entries.iter().cloned().partition(|e| ignored.contains(e.name.as_ref()) || (!e.is_dir && is_config_file(&e.name)) || (e.is_dir && e.name.starts_with('.')));
        let v = (Rc::new(others), Rc::new(regular));
        self.others_cache.insert(repo.to_path_buf(), v.clone());
        v
    }

    /// Show `path` in the file tree: Files mode, every ancestor expanded, row highlighted and
    /// scrolled into view. A missing path (deleted file) reveals its folder.
    fn locate_in_tree(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.context_menu = None;
        let mut target = path.to_path_buf();
        while !target.exists() {
            match target.parent() {
                Some(p) => target = p.to_path_buf(),
                None => return,
            }
        }
        let Some(project) = self.active_project().cloned() else { return };
        let Some(repo) = project.repositories.iter().map(|r| r.path.clone()).find(|r| target.starts_with(r)) else { return };
        self.collapsed.remove(&project.id);
        self.sidebar_visible = true;
        self.sidebar_mode = SidebarMode::Files;
        // Top-level entry may live in the repo's virtual "others" folder.
        if let Some(top) = target.strip_prefix(&repo).ok().and_then(|r| r.components().next()).map(|c| repo.join(c)) {
            let (others, _) = self.repo_split(&repo);
            if others.iter().any(|e| e.path == top) {
                self.expanded.insert(others_key(&repo));
            }
        }
        let mut dir = if target.is_dir() { Some(target.clone()) } else { target.parent().map(|p| p.to_path_buf()) };
        while let Some(d) = dir {
            if !d.starts_with(&repo) {
                break;
            }
            self.expanded.insert(d.clone());
            dir = d.parent().map(|p| p.to_path_buf());
        }
        self.located = Some(target);
        self.locate_pending = true;
        cx.notify();
    }

    fn toggle_dir(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
            self.dir_cache.remove(path); // fresh listing on expand
        }
        cx.notify();
    }

    /// Write each live terminal's output to `scrollback/<jt_id>.ansi` (quit / window close),
    /// so a relaunch shows it again. Files untouched for two weeks are dropped.
    fn save_scrollback(&mut self, cx: &mut Context<Self>) {
        let dir = workspace_state::state_dir().join("scrollback");
        let _ = std::fs::create_dir_all(&dir);
        for (key, tabs) in &self.terminals {
            if key == SCRATCH {
                continue;
            }
            for t in &tabs.tabs {
                let t = t.read(cx);
                if t.has_exited() {
                    continue;
                }
                let file = scrollback_file(&t.jt_id);
                match t.dump_scrollback() {
                    Some(text) => {
                        let _ = std::fs::write(&file, text);
                    }
                    None => {
                        let _ = std::fs::remove_file(&file);
                    }
                }
            }
        }
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(14 * 24 * 3600);
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Persist state, including live terminals' working directories and open files.
    pub(crate) fn persist(&mut self, cx: &mut Context<Self>) {
        for (key, tabs) in &self.terminals {
            if key == SCRATCH {
                continue;
            }
            let Some(project) = self.state.project_mut(key) else { continue };
            project.terminals = tabs
                .tabs
                .iter()
                .filter(|t| !t.read(cx).has_exited())
                .map(|t| {
                    let t = t.read(cx);
                    TerminalState {
                        title: t.label.clone().filter(|l| l == "Claude"),
                        cwd: t.current_cwd(),
                        repository: t.repository.clone(),
                        scrollback: Some(t.jt_id.clone()),
                    }
                })
                .collect();
            project.active_terminal = Some(tabs.active);
        }
        for (key, tabs) in &self.editors {
            let Some(project) = self.state.project_mut(key) else { continue };
            project.open_files = tabs.tabs.iter().filter_map(|e| e.read(cx).path().cloned()).collect();
            project.active_file = Some(tabs.active);
        }
        // Only projects this window has open: the others' folders belong to other windows.
        for project in self.state.projects.iter_mut().filter(|p| self.terminals.contains_key(&p.id) || self.editors.contains_key(&p.id)) {
            let roots: Vec<&PathBuf> = project.repositories.iter().map(|r| &r.path).chain(project.root.iter()).collect();
            project.expanded = self.expanded.iter().filter(|p| roots.iter().any(|r| p.starts_with(r))).cloned().collect();
            project.expanded.sort();
        }
        self.state.sidebar_width = Some(self.sidebar_width);
        self.state.terminal_panel_width = Some(self.terminal_width);
        self.state.font_size = Some(self.font_size);
        self.state.light_theme = theme::is_light();
        // Merge with what other windows saved since our last sync.
        let disk = self.store.load();
        let merged = merge_projects(&self.synced, &self.state.projects, disk.projects);
        self.state.projects = merged.clone();
        self.synced = merged;
        let mut out = self.state.clone();
        if !self.primary {
            out.active_project = disk.active_project;
            out.window = disk.window;
        }
        if let Err(err) = self.store.save(&out) {
            self.error = Some(format!("Could not save workspace: {err:#}").into());
        }
    }

    // ---- actions ------------------------------------------------------------------------

    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self
            .active_terminal()
            .and_then(|t| t.read(cx).current_cwd())
            .or_else(|| self.active_project().and_then(|p| p.default_cwd()));
        let repo = self.active_terminal().and_then(|t| t.read(cx).repository.clone());
        self.open_terminal(cwd, repo, window, cx);
        self.persist(cx);
    }

    fn close_terminal(&mut self, _: &CloseTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_terminal().cloned() {
            self.close_terminal_view(&view, window, cx);
        }
    }

    fn cycle_terminal(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tabs) = self.terminals.get(&self.active_key()) {
            let n = tabs.tabs.len();
            if n > 0 {
                let ix = if forward { (tabs.active + 1) % n } else { (tabs.active + n - 1) % n };
                self.select_terminal(ix, window, cx);
            }
        }
    }

    fn cycle_editor(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tabs) = self.editors.get(&self.active_key()) {
            let n = tabs.tabs.len();
            if n > 0 {
                let ix = if forward { (tabs.active + 1) % n } else { (tabs.active + n - 1) % n };
                self.select_editor(ix, window, cx);
            }
        }
    }

    fn close_editor(&mut self, _: &CloseEditor, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_editor().cloned() {
            self.close_editor_view(view, window, cx);
        }
    }

    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.dirty_count(cx);
        if dirty == 0 {
            self.persist(cx);
            cx.quit();
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("You have {dirty} unsaved file(s). Save before quitting?"),
            None,
            &["Save All", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await.unwrap_or(2);
            let _ = this.update(cx, |this, cx| {
                if answer == 2 {
                    return;
                }
                if answer == 0 {
                    this.save_all(cx);
                }
                this.persist(cx);
                cx.quit();
            });
        })
        .detach();
    }

    fn open_project(&mut self, _: &OpenProject, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open Project".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| this.add_project(&path, window, cx));
        })
        .detach();
    }

    fn add_project(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(existing) = self.state.projects.iter().find(|p| p.root.as_deref() == Some(path)) {
            let id = existing.id.clone();
            self.request_switch_project(id, window, cx);
            return;
        }
        // Register the project, then switch to it (with the usual confirmation).
        let previous = self.state.active_project.clone();
        let id = self.state.add_project_from_folder(path);
        self.state.active_project = previous;
        if let Some(first) = self.state.project(&id).and_then(|p| p.repositories.first()).map(|r| r.path.clone()) {
            self.expanded.insert(first);
        }
        self.persist(cx);
        self.request_switch_project(id, window, cx);
        cx.notify();
    }

    fn add_repository(&mut self, _: &AddRepository, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.state.active_project.clone() else {
            self.open_project(&OpenProject, window, cx);
            return;
        };
        self.prompt_add_repository(project_id, window, cx);
    }

    fn prompt_add_repository(&mut self, project_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: true, prompt: Some("Add Repository".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                let Some(project) = this.state.project_mut(&project_id) else { return };
                let mut not_repos = Vec::new();
                for path in paths {
                    let repos = if is_git_repo(&path) { vec![path.clone()] } else { discover_repositories(&path, 2) };
                    if repos.is_empty() {
                        not_repos.push(path.display().to_string());
                    }
                    for repo in repos {
                        project.add_repository(&repo);
                    }
                }
                this.error = (!not_repos.is_empty()).then(|| format!("Not a Git repository: {}", not_repos.join(", ")).into());
                this.refresh_branches();
                this.persist(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_remove_project(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.state.project(&id).map(|p| p.name.clone()) else { return };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remove project “{name}”?"),
            Some("Its terminals and editors will be closed. Files and repositories on disk are not touched."),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.terminals.remove(&id);
                this.editors.remove(&id);
                this.state.remove_project(&id);
                this.ensure_project_loaded(window, cx);
                this.persist(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_remove_repository(&mut self, project_id: String, repo: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remove repository “{repo}” from this project?"),
            Some("The repository on disk is not touched."),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(p) = this.state.project_mut(&project_id) {
                    p.remove_repository(&repo);
                }
                this.persist(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.font_size = size.clamp(8., 32.);
        let size = self.font_size;
        for tabs in self.terminals.values() {
            for t in &tabs.tabs {
                t.update(cx, |t, cx| t.set_font_size(size + theme::TERM_FONT_BONUS, cx));
            }
        }
        for tabs in self.editors.values() {
            for e in &tabs.tabs {
                e.update(cx, |e, cx| e.set_font_size(size, cx));
            }
        }
    }

    // ---- rendering ----------------------------------------------------------------------

    fn build_rows(&mut self) -> Rc<Vec<Row>> {
        let mut rows = Vec::new();
        if self.state.projects.is_empty() {
            rows.push(Row::Hint);
        }
        let active_file: Option<PathBuf> = None;
        let projects = self.state.projects.clone();
        for project in &projects {
            let active = self.state.active_project.as_deref() == Some(project.id.as_str());
            let expanded = active && !self.collapsed.contains(&project.id);
            rows.push(Row::Project { id: project.id.clone(), name: project.name.clone().into(), icon: project.icon_path(), active, expanded });
            if !expanded {
                continue;
            }
            for repo in &project.repositories {
                let repo_expanded = self.expanded.contains(&repo.path);
                let missing = !repo.path.is_dir();
                rows.push(Row::Repo {
                    pid: project.id.clone(),
                    name: repo.name.clone().into(),
                    path: repo.path.clone(),
                    branch: self.branches.get(&repo.path).cloned().flatten(),
                    missing,
                    expanded: repo_expanded,
                });
                if repo_expanded && !missing {
                    let (others, regular) = self.repo_split(&repo.path);
                    let regular = self.with_deleted(&repo.path, regular);
                    if !others.is_empty() {
                        let key = others_key(&repo.path);
                        let open = self.expanded.contains(&key);
                        rows.push(Row::Entry {
                            entry: DirEntry { name: "others".into(), path: key, is_dir: true, virtual_others: true, deleted: false },
                            depth: 1,
                            expanded: open,
                            dimmed: false,
                        });
                        if open {
                            for entry in others.iter() {
                                let expanded = entry.is_dir && self.expanded.contains(&entry.path);
                                rows.push(Row::Entry { entry: entry.clone(), depth: 2, expanded, dimmed: true });
                                if expanded {
                                    self.push_tree(&entry.path, 3, &active_file, &mut rows);
                                }
                            }
                        }
                    }
                    for entry in regular.iter() {
                        let expanded = entry.is_dir && self.expanded.contains(&entry.path);
                        rows.push(Row::Entry { entry: entry.clone(), depth: 1, expanded, dimmed: is_dimmed(&entry.name) });
                        if expanded {
                            self.push_tree(&entry.path, 2, &active_file, &mut rows);
                        }
                    }
                }
            }
            // Everything in the project root that isn't a repository goes into a virtual
            // "others" folder next to the repositories.
            if let Some(root) = project.root.as_ref().filter(|r| r.is_dir() && !project.repositories.iter().any(|repo| &repo.path == *r)) {
                let repo_paths: Vec<PathBuf> = project.repositories.iter().map(|r| r.path.clone()).collect();
                let entries: Vec<DirEntry> = self.children(root).iter().filter(|e| !repo_paths.contains(&e.path)).cloned().collect();
                if !entries.is_empty() {
                    let key = others_key(root);
                    let open = self.expanded.contains(&key);
                    rows.push(Row::Entry {
                        entry: DirEntry { name: "others".into(), path: key, is_dir: true, virtual_others: true, deleted: false },
                        depth: 0,
                        expanded: open,
                        dimmed: false,
                    });
                    if open {
                        for entry in entries.iter() {
                            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
                            rows.push(Row::Entry { entry: entry.clone(), depth: 1, expanded, dimmed: is_dimmed(&entry.name) });
                            if expanded {
                                self.push_tree(&entry.path, 2, &active_file, &mut rows);
                            }
                        }
                    }
                }
            }
        }
        Rc::new(rows)
    }

    fn push_tree(&mut self, dir: &Path, depth: usize, selected: &Option<PathBuf>, rows: &mut Vec<Row>) {
        let entries = self.children(dir);
        let entries = self.with_deleted(dir, entries);
        for entry in entries.iter() {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            rows.push(Row::Entry { entry: entry.clone(), depth, expanded, dimmed: is_dimmed(&entry.name) });
            if expanded {
                self.push_tree(&entry.path, depth + 1, selected, rows);
            }
        }
    }

    fn render_row(&mut self, ix: usize, active_path: Option<&PathBuf>, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else { return div().into_any_element() };
        let inner = div().flex().items_center().gap(px(6.)).w_full().h(px(ROW_HEIGHT)).rounded(px(5.)).cursor_pointer().text_size(px(14.5));
        let el: AnyElement = match row {
            Row::Hint => div()
                .id(ix)
                .px(px(8.))
                .py(px(6.))
                .text_size(px(12.))
                .text_color(theme::rgb(theme::TEXT_MUTED))
                .cursor_pointer()
                .child("No projects yet. Open folder…  ⌘O")
                .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.open_project(&OpenProject, w, cx)))
                .into_any_element(),
            Row::Project { id, name, icon: project_icon, active, expanded } => {
                let locked = self.locked.contains(&id);
                let group: SharedString = format!("p-{id}").into();
                let (id_click, id_rm) = (id.clone(), id.clone());
                inner
                    .id(ix)
                    .group(group.clone())
                    .px(px(6.))
                    .when(active, |d| d.bg(theme::rgb(theme::SELECTED)))
                    .when(!active, |d| d.hover(|s| s.bg(theme::rgb(theme::HOVER))))
                    .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.click_project(&id_click, w, cx)))
                    .when_some(self.state.project(&id).and_then(|p| p.root.clone()).filter(|_| active), |d, root| {
                        d.on_mouse_down(MouseButton::Right, cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| this.open_context_menu(e.position, root.clone(), true, true, cx)))
                    })
                    .child(chevron(expanded))
                    .child(match project_icon {
                        Some(path) => gpui::img(path).size(px(16.)).flex_none().rounded(px(3.)).object_fit(gpui::ObjectFit::Contain).into_any_element(),
                        None => icon("icons/project-empty.svg", theme::TEXT_FAINT),
                    })
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::rgb(if active { theme::TEXT } else if locked { theme::TEXT_FAINT } else { theme::TEXT_MUTED }))
                            .child(name),
                    )
                    // Outside its hours (settings.toml): can't be opened.
                    .when(locked, |d| d.child(icon("icons/lock.svg", theme::TEXT_FAINT)))
                    .when_some(self.attention_in(&id, None, cx), |d, a| {
                        let pid = id.clone();
                        d.child(attention_dot(SharedString::from(format!("att-{id}")), a).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                            cx.stop_propagation();
                            this.jump_to_attention(&pid, None, w, cx)
                        })))
                    })
                    .child(
                        icon_button(SharedString::from(format!("rm-{id}")), "icons/close.svg")
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                cx.stop_propagation();
                                this.confirm_remove_project(id_rm.clone(), w, cx)
                            })),
                    )
                    .into_any_element()
            }
            Row::Repo { pid, name, path, branch, missing, expanded } => {
                let group: SharedString = format!("r-{}", path.display()).into();
                let (pid_t, name_t, pid_rm, name_rm) = (pid.clone(), name.to_string(), pid.clone(), name.to_string());
                let path_click = path.clone();
                let repo_attention = self.attention_in(&pid, Some(&name), cx);
                inner
                    .id(ix)
                    .group(group.clone())
                    .relative()
                    .pl(px(14.))
                    .pr(px(10.))
                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_dir(&path_click, cx)))
                    .on_mouse_down(MouseButton::Right, {
                        let p = path.clone();
                        cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| this.open_context_menu(e.position, p.clone(), true, true, cx))
                    })
                    .child(chevron(expanded))
                    .child(icon("icons/repo.svg", if missing { theme::TEXT_FAINT } else { 0xe0714f }))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme::rgb(if missing { theme::TEXT_FAINT } else { theme::TEXT }))
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(100.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(12.))
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::rgb(theme::TEXT_FAINT))
                            .child(if missing { "missing".to_string() } else { branch.unwrap_or_default() }),
                    )
                    .when_some(repo_attention, |d, a| {
                        let (pid_a, name_a) = (pid.clone(), name_t.clone());
                        d.child(attention_dot(SharedString::from(format!("att-{}", path.display())), a).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                            cx.stop_propagation();
                            this.jump_to_attention(&pid_a, Some(&name_a), w, cx)
                        })))
                    })
                    // Actions float over the right edge (over the branch) on hover, so the
                    // branch label can sit flush right without reserving space for them.
                    .child({
                        div()
                            .absolute()
                            .right(px(4.))
                            .top_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .gap(px(1.))
                            .pl(px(6.))
                            .rounded(px(5.))
                            .bg(theme::rgb(theme::HOVER))
                            .invisible()
                            .group_hover(group.clone(), |s| s.visible())
                            .child(icon_button(SharedString::from(format!("term-{}", path.display())), "icons/terminal.svg").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                cx.stop_propagation();
                                this.open_repo_terminal(&pid_t, &name_t, w, cx)
                            })))
                            .child(icon_button(SharedString::from(format!("rmr-{}", path.display())), "icons/close.svg").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                cx.stop_propagation();
                                this.confirm_remove_repository(pid_rm.clone(), name_rm.clone(), w, cx)
                            })))
                    })
                    .into_any_element()
            }
            Row::Entry { entry, depth, expanded, dimmed, .. } => {
                let selected = active_path == Some(&entry.path) || self.located.as_ref() == Some(&entry.path);
                let path = entry.path.clone();
                let is_dir = entry.is_dir;
                let mark = self.git_panel.as_ref().and_then(|g| g.read(cx).tree_marks().get(&entry.path).copied());
                let gone = entry.deleted;
                let text_color = if gone {
                    theme::DELETED
                } else if let Some(m) = mark {
                    m.color
                } else if selected {
                    theme::TEXT
                } else if dimmed {
                    theme::TEXT_FAINT
                } else {
                    0xc3c7ce
                };
                let virtual_others = entry.virtual_others;
                let row_icon = if is_dir || virtual_others {
                    crate::assets::folder_icon_el(&entry.name, expanded, 16., dimmed.then_some(theme::TEXT_FAINT), virtual_others)
                } else {
                    let (icon_path, icon_color) = crate::assets::file_icon(&entry.name);
                    icon(icon_path, if dimmed { theme::TEXT_FAINT } else { icon_color })
                };
                inner
                    .id(ix)
                    .pl(px(14. + depth as f32 * 14.))
                    .pr(px(8.))
                    .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::rgb(theme::HOVER))))
                    .when(!virtual_others, |d| {
                        d.on_mouse_down(MouseButton::Right, {
                            let p = path.clone();
                            cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| this.open_context_menu(e.position, p.clone(), is_dir, false, cx))
                        })
                    })
                    .when(!virtual_others && !gone, |d| {
                        d.on_drag(DraggedFile { path: path.clone(), is_dir }, |f, _, _, cx| cx.new(|_| f.clone()))
                    })
                    .on_click(cx.listener(move |this, e: &ClickEvent, w, cx| {
                        this.located = None;
                        if is_dir {
                            if e.click_count() <= 1 {
                                this.toggle_dir(&path, cx);
                            }
                        } else if gone {
                            // Not on disk: show what was deleted.
                            if let Some((repo, change)) = this.git_panel.as_ref().and_then(|g| g.read(cx).change_at(&path)) {
                                let staged = change.unstaged.is_none();
                                this.open_change_diff(repo, change, staged, w, cx);
                            }
                        } else if e.click_count() >= 2 {
                            this.open_file(&path, true, w, cx);
                            this.pin_active_editor(cx);
                            this.persist(cx);
                        } else {
                            this.open_file_preview(&path, w, cx);
                            this.persist(cx);
                        }
                    }))
                    .child(if is_dir { chevron(expanded) } else { div().w(px(12.)).flex_none() })
                    .child(row_icon)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(theme::rgb(text_color))
                            .when(gone, |d| d.line_through())
                            .child(entry.name.clone()),
                    )
                    .when_some(mark, |d, m| {
                        // Folder counts are quieter than a file's status letter.
                        let (label, size, weight) = match m.letter {
                            Some(l) => (l.to_string(), 11., gpui::FontWeight::SEMIBOLD),
                            None => (m.count.to_string(), 10., gpui::FontWeight::NORMAL),
                        };
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(size))
                                .font_family(theme::MONO_FONT)
                                .font_weight(weight)
                                .text_color(theme::rgb(m.color))
                                .child(label),
                        )
                    })
                    .into_any_element()
            }
        };
        // Full-width row with a small inset so hover/selection backgrounds span the sidebar.
        div().w_full().px(px(6.)).child(el).into_any_element()
    }

    pub(crate) fn claude_board_visible(&self) -> bool {
        self.sidebar_visible && self.sidebar_mode == SidebarMode::Claude
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.deleted_entries = self.git_panel.as_ref().map(|g| g.read(cx).deleted_entries()).unwrap_or_default();
        self.rows = if self.sidebar_mode == SidebarMode::Files { self.build_rows() } else { Rc::new(Vec::new()) };
        if self.locate_pending && self.sidebar_mode == SidebarMode::Files {
            self.locate_pending = false;
            if let Some(target) = self.located.as_ref() {
                if let Some(ix) = self.rows.iter().position(|r| matches!(r, Row::Entry { entry, .. } if &entry.path == target)) {
                    self.tree_scroll.scroll_to_item(ix, gpui::ScrollStrategy::Center);
                }
            }
        }
        let search_panel = (self.sidebar_mode == SidebarMode::Search).then(|| self.search_panel.clone()).flatten();
        let git_panel = (self.sidebar_mode == SidebarMode::Git).then(|| self.git_panel.clone()).flatten();
        let git_total = self.git_panel.as_ref().map(|g| g.read(cx).total_changes()).unwrap_or(0);
        let problems = self.tools_panel.as_ref().map(|t| t.read(cx).total_problems()).unwrap_or(0);
        let tools_panel = (self.sidebar_mode == SidebarMode::Tools).then(|| self.tools_panel.clone()).flatten();
        let ports_panel = (self.sidebar_mode == SidebarMode::Ports).then(|| self.ports_panel.clone()).flatten();
        let servers_panel = (self.sidebar_mode == SidebarMode::Servers).then(|| self.servers_panel.clone()).flatten();
        let whatsapp_panel = (self.sidebar_mode == SidebarMode::WhatsApp).then(|| self.whatsapp_panel.clone()).flatten();
        let wa_unread = self.whatsapp_panel.as_ref().map(|p| p.read(cx).total_unread()).unwrap_or(0);
        let claude_needs = self.claude_needs_you(window, cx);
        let claude_board = (self.sidebar_mode == SidebarMode::Claude).then(|| self.render_claude_board(window, cx));
        // The ports list polls only while it's on screen.
        if let Some(pp) = self.ports_panel.clone() {
            let on = self.sidebar_mode == SidebarMode::Ports && self.sidebar_visible;
            if pp.read(cx).active != on {
                pp.update(cx, |p, _| p.active = on);
            }
        }
        let actions_panel = self.actions_panel.clone().filter(|a| a.read(cx).has_any());
        if let Some(ap) = self.actions_panel.clone() {
            let on = self.sidebar_mode == SidebarMode::Files && self.sidebar_visible;
            ap.update(cx, |a, cx| a.set_visible(on, cx));
        }
        let actions_panel = actions_panel.filter(|_| self.sidebar_mode == SidebarMode::Files);
        let tags_panel = self.tags_panel.clone().filter(|t| self.sidebar_mode == SidebarMode::Files && t.read(cx).has_any());
        let actions_open = actions_panel.as_ref().is_some_and(|a| a.read(cx).expanded) || tags_panel.as_ref().is_some_and(|t| t.read(cx).expanded);
        let bottom_panels = actions_panel.is_some() || tags_panel.is_some();
        let actions_flex = actions_panel.as_ref().is_some_and(|a| a.read(cx).expanded);
        let tags_flex = tags_panel.as_ref().is_some_and(|t| t.read(cx).expanded);
        let actions_height = self.actions_height;
        let count = self.rows.len();
        let music_card = self.music_card.clone();
        if let Some(card) = &music_card {
            let w = self.sidebar_width;
            if card.read(cx).width != w {
                card.update(cx, |c, _| c.width = w);
            }
        }
        // Active project's icon + name next to the traffic lights.
        let title_project = self.active_project().map(|p| (p.icon_path(), SharedString::from(p.name.clone())));
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(self.sidebar_width))
            .flex_none()
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .child(
                div()
                    .id("sidebar-titlebar")
                    .h(px(TOP_BAR_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .pl(px(80.))
                    .pr(px(12.))
                    .overflow_hidden()
                    .on_click(|e: &ClickEvent, window, _| {
                        if e.click_count() == 2 {
                            window.titlebar_double_click();
                        }
                    })
                    .when_some(title_project, |d, (icon_path, name)| {
                        d.child(match icon_path {
                            Some(path) => gpui::img(path).size(px(18.)).flex_none().rounded(px(4.)).object_fit(gpui::ObjectFit::Contain).into_any_element(),
                            None => icon("icons/project-empty.svg", theme::TEXT_MUTED),
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(14.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme::rgb(theme::TEXT))
                                .child(name),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(16.))
                    .h(px(26.))
                    .flex_none()
                    .child(div().text_size(px(13.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child("PROJECTS"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(
                                icon_button("music", "icons/music.svg")
                                    .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Música, podcasts y videos (⌘⇧M)")).into())
                                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.toggle_music(&ToggleMusic, w, cx))),
                            )
                            .child(
                                icon_button("usage-stats", "icons/chart.svg")
                                    .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Agent usage (⌘⇧U)")).into())
                                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.toggle_usage(&ShowUsage, w, cx))),
                            )
                            .child(
                                icon_button("add-project", "icons/plus.svg")
                                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.open_project(&OpenProject, w, cx))),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .px(px(10.))
                    .pb(px(6.))
                    .flex_none()
                    .child(mode_tab("mode-files", "icons/files.svg", "Files", self.sidebar_mode == SidebarMode::Files).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.sidebar_mode = SidebarMode::Files;
                        // Viewing a git diff: reveal that file in the tree.
                        if let Some(d) = this.diff.as_ref().filter(|_| this.diff_active) {
                            if let DiffSource::Changes { repo, path, .. } = &d.read(cx).source {
                                let target = repo.join(path);
                                this.locate_in_tree(&target, cx);
                            }
                        }
                        cx.notify();
                    })))
                    .child(mode_tab("mode-search", "icons/search.svg", "Search", self.sidebar_mode == SidebarMode::Search).on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                        this.search_in_project(&SearchInProject, w, cx);
                    })))
                    .child(mode_tab("mode-claude", "icons/claude-spark.svg", "Claude sessions", self.sidebar_mode == SidebarMode::Claude).when(claude_needs > 0, |d| d.child(
                        mode_badge(claude_needs, 0xb8862b, 0xffffff),
                    )).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.sidebar_mode = SidebarMode::Claude;
                        cx.notify();
                    })))
                    .child(mode_tab("mode-tools", "icons/gear.svg", "Tools", self.sidebar_mode == SidebarMode::Tools).when(problems > 0, |d| d.child(
                        mode_badge(problems, 0x8c2f39, 0xffffff),
                    )).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.show_tools(w, cx))))
                    .child(mode_tab("mode-git", "icons/git.svg", "Git", self.sidebar_mode == SidebarMode::Git).when(git_total > 0, |d| d.child(
                        mode_badge(git_total, 0x2f5fb3, 0xffffff),
                    )).on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                        this.show_git(w, cx);
                    })))
                    .child(mode_tab("mode-ports", "icons/ports.svg", "Ports", self.sidebar_mode == SidebarMode::Ports).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.show_ports(cx))))
                    .child(mode_tab("mode-servers", "icons/server.svg", "Servers", self.sidebar_mode == SidebarMode::Servers).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.show_servers(w, cx))))
                    .child(mode_tab("mode-whatsapp", "icons/whatsapp.svg", "WhatsApp", self.sidebar_mode == SidebarMode::WhatsApp).when(wa_unread > 0, |d| d.child(
                        mode_badge(wa_unread, 0x1f9d55, 0xffffff),
                    )).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.show_whatsapp(cx)))),
            )
            .when_some(search_panel, |d, sp| d.child(div().flex_1().min_h_0().child(sp)))
            .when_some(git_panel, |d, gp| d.child(div().flex_1().min_h_0().child(gp)))
            .when_some(tools_panel, |d, tp| d.child(div().flex_1().min_h_0().child(tp)))
            .when_some(ports_panel, |d, pp| d.child(div().flex_1().min_h_0().child(pp)))
            .when_some(servers_panel, |d, sp| d.child(div().flex_1().min_h_0().child(sp)))
            .when_some(whatsapp_panel, |d, wp| d.child(div().flex_1().min_h_0().child(wp)))
            .when_some(claude_board, |d, b| d.child(div().flex_1().min_h_0().child(b)))
            .when(self.sidebar_mode == SidebarMode::Files, |d| d.child(
                uniform_list(
                    "sidebar-rows",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let active_path = this.active_editor().and_then(|e| e.read(cx).path().cloned());
                        range.map(|ix| this.render_row(ix, active_path.as_ref(), cx)).collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.tree_scroll.clone())
                .flex_1()
                .w_full()
                .pb(px(8.)),
            ))
            .children(music_card)
            .when(bottom_panels, |d| {
                d.child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .w_full()
                        .when(actions_open, |d| d.h(px(actions_height)))
                        .when_some(actions_panel, |d, ap| d.child(div().w_full().when(actions_flex, |d| d.flex_1().min_h_0()).when(!actions_flex, |d| d.h(px(28.)).flex_none()).child(ap)))
                        .when_some(tags_panel, |d, tp| d.child(div().w_full().when(tags_flex, |d| d.flex_1().min_h_0()).when(!tags_flex, |d| d.h(px(28.)).flex_none()).child(tp)))
                        .when(actions_open, |d| {
                            d.child(
                                div()
                                    .id("actions-resize")
                                    .absolute()
                                    .top(px(-3.))
                                    .left_0()
                                    .right_0()
                                    .h(px(6.))
                                    .cursor_row_resize()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.resizing_actions = true;
                                            cx.stop_propagation();
                                            cx.notify();
                                        }),
                                    ),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    fn render_terminal_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (tabs, active) = self.terminals.get(&self.active_key()).map(|t| (t.tabs.clone(), t.active)).unwrap_or_default();
        let mut bar = div()
            .id("terminal-tabs")
            .flex()
            .items_end()
            .h(px(TOP_BAR_HEIGHT))
            .flex_none()
            .bg(theme::rgb(theme::TABBAR_BG))
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .overflow_x_scroll()
            .on_click(|e: &ClickEvent, window, _| {
                if e.click_count() == 2 {
                    window.titlebar_double_click();
                }
            });
        for (ix, tab) in tabs.iter().enumerate() {
            let is_active = ix == active;
            let t = tab.read(cx);
            let (title, exited) = (t.title(), t.has_exited());
            let is_claude = t.label.as_deref() == Some("Claude");
            let attention = t.attention;
            let is_ssh = t.label.as_deref().is_some_and(|l| l.starts_with("ssh · "));
            let (tab_icon, tab_color) = if is_claude { ("icons/claude.svg", 0xd97757) } else if is_ssh { ("icons/server.svg", 0x56b6c2) } else { ("icons/terminal.svg", 0x98c379) };
            let tab_ref = tab.clone();
            bar = bar.child(
                tab_chip(("term-tab", ix), tab_icon, tab_color, title, is_active, exited, false)
                    .when_some(attention, |d, a| {
                        let (color, tip) = match a {
                            crate::terminal_view::Attention::Done => (0x98c379, "Claude terminó"),
                            crate::terminal_view::Attention::NeedsInput => (0xe5c07b, "Claude te necesita"),
                        };
                        d.child(div().flex_none().size(px(7.)).rounded_full().bg(theme::rgb(color)))
                            .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.select_terminal(ix, w, cx)
                    }))
                    .child(close_button(("close-term", ix), is_active).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.close_terminal_view(&tab_ref, w, cx)
                    }))),
            );
        }
        bar = bar.child(
            div().h_full().flex().items_center().px(px(4.)).child(
                icon_button("new-terminal", "icons/plus.svg").on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.new_tab_menu = if this.new_tab_menu.is_some() { None } else { Some(e.position()) };
                    cx.notify();
                })),
            ),
        );
        let content: AnyElement = match self.active_terminal() {
            Some(t) => t.clone().into_any_element(),
            None => empty_state("No terminal open", "⌘T new terminal").into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(self.terminal_width))
            .flex_none()
            .child(bar)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    fn render_editor_area(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (tabs, active) = self.editors.get(&self.active_key()).map(|t| (t.tabs.clone(), t.active)).unwrap_or_default();
        let mut bar = div()
            .id("editor-tabs")
            .flex()
            .items_end()
            .h(px(TOP_BAR_HEIGHT))
            .flex_none()
            .bg(theme::rgb(theme::TABBAR_BG))
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .overflow_x_scroll()
            .when(!self.sidebar_visible, |d| d.pl(px(78.)))
            .on_click(|e: &ClickEvent, window, _| {
                if e.click_count() == 2 {
                    window.titlebar_double_click();
                }
            });
        for (ix, tab) in tabs.iter().enumerate() {
            let is_active = ix == active && !self.diff_active;
            let e = tab.read(cx);
            let (title, dirty, preview) = (e.title(), e.is_dirty(), e.preview);
            let (errors, warnings) = e.problem_counts();
            let (icon_path, icon_color) = crate::assets::file_icon(&title);
            let full_path: Option<SharedString> = e.path().map(|p| p.display().to_string().into());
            let chat = e.chat_badge(cx);
            let tab_ref = tab.clone();
            let lead = match &chat {
                Some((avatar, _)) => crate::wa_ui::avatar(avatar.as_ref(), &title, 18.),
                None => icon(icon_path, icon_color),
            };
            let unread = chat.map(|(_, n)| n).unwrap_or(0);
            bar = bar.child(
                tab_chip_with(("editor-tab", ix), lead, title, is_active, false, dirty)
                    .when(unread > 0, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .min_w(px(16.))
                                .h(px(16.))
                                .px(px(4.))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(theme::rgb(crate::wa_ui::GREEN))
                                .text_size(px(10.))
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(theme::rgb(0x0b141a))
                                .child(if unread > 99 { "99+".to_string() } else { unread.to_string() }),
                        )
                    })
                    .when(preview, |d| d.italic())
                    .when_some(full_path, |d, p| d.tooltip(move |_, cx| cx.new(|_| crate::git_panel::DynTooltip(p.clone())).into()))
                    .when(errors + warnings > 0, |d| {
                        d.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme::rgb(if errors > 0 { theme::DELETED } else { theme::MODIFIED }))
                                .child((errors + warnings).to_string()),
                        )
                    })
                    .on_click(cx.listener(move |this, e: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.select_editor(ix, w, cx);
                        if e.click_count() >= 2 {
                            this.pin_active_editor(cx);
                        }
                    }))
                    .child(close_button(("close-editor", ix), is_active).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.close_editor_view(tab_ref.clone(), w, cx)
                    }))),
            );
        }
        if let Some(diff) = self.diff.clone() {
            let title = diff.read(cx).title.clone();
            let active = self.diff_active;
            bar = bar.child(
                tab_chip("diff-tab", "icons/git.svg", 0xe0714f, title, active, false, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.diff_active = true;
                        this.focus_main(w, cx);
                        cx.notify();
                    }))
                    .child(close_button("close-diff", active).on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.close_diff(&CloseDiff, w, cx);
                    }))),
            );
        }
        let content: AnyElement = match (self.diff.as_ref().filter(|_| self.diff_active), self.active_editor()) {
            (Some(d), _) => d.clone().into_any_element(),
            (None, Some(e)) => e.clone().into_any_element(),
            (None, None) => empty_state("No file open", "Pick a file in the sidebar  ·  ⌘P to search files").into_any_element(),
        };
        div()
            .id("editor-area")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .group("editor-drop")
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                this.open_dropped(paths.paths(), window, cx);
            }))
            .child(bar)
            .child(div().flex_1().min_h_0().child(content))
            // Shown only while files are dragged over the editor area.
            .child(
                div()
                    .group("editor-drop-overlay") // gives it a hitbox so the drag style applies
                    .invisible()
                    .group_drag_over::<gpui::ExternalPaths>("editor-drop", |s| s.visible())
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .p(px(12.))
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(4.))
                            .rounded(px(8.))
                            .border_2()
                            .border_dashed()
                            .border_color(theme::rgb(theme::ACCENT))
                            .bg(theme::rgb(theme::ACCENT).opacity(0.1))
                            .child(div().text_size(px(15.)).text_color(theme::rgb(theme::TEXT)).child("Drop to open"))
                            .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child("Files open as tabs, from any folder")),
                    ),
            )
            .when_some(self.error.clone(), |d, err| {
                d.child(
                    div()
                        .id("error-bar")
                        .flex_none()
                        .px(px(12.))
                        .py(px(6.))
                        .bg(theme::rgb(0x3a1f22))
                        .text_size(px(12.))
                        .text_color(theme::rgb(0xf0b3b8))
                        .cursor_pointer()
                        .child(err)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }
}

fn is_dimmed(name: &str) -> bool {
    matches!(name, "node_modules" | "target" | "dist" | "build" | ".next" | ".nuxt" | "coverage" | ".turbo" | ".cache") || name.starts_with('.')
}

fn icon(path: &'static str, color: u32) -> gpui::AnyElement {
    if path == "icons/claude.svg" {
        return crate::assets::claude_logo(16.).into_any_element();
    }
    gpui::svg().path(path).size(px(16.)).flex_none().text_color(theme::rgb(color)).into_any_element()
}

fn empty_state(title: &'static str, hint: &'static str) -> gpui::Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .gap(px(4.))
        .items_center()
        .justify_center()
        .bg(theme::rgb(theme::EDITOR_BG))
        .child(div().mb(px(14.)).opacity(0.9).child(crate::assets::fragua_mark(44., theme::MARK_A, theme::MARK_B)))
        .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(title))
        .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(hint))
}

/// Count badge pinned to the top-right corner of a sidebar mode tab: never takes space from
/// the icon (a long count used to squeeze it).
fn mode_badge(n: usize, bg: u32, fg: u32) -> gpui::Div {
    let text = if n > 999 { "999+".to_string() } else { n.to_string() };
    div()
        .absolute()
        .top(px(1.))
        .right(px(-3.))
        .h(px(14.))
        .min_w(px(14.))
        .px(px(3.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme::rgb(bg))
        .text_size(px(9.5))
        .line_height(px(14.))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(theme::rgb(fg))
        .child(text)
}

fn mode_tab(id: &'static str, icon_path: &'static str, label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(5.))
        .px(px(8.))
        .min_w(px(36.))
        .h(px(34.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(14.))
        .when(active, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
        .when(!active, |d| d.text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.bg(theme::rgb(theme::HOVER))))
        .child(gpui::svg().path(icon_path).size(px(20.)).flex_none().text_color(theme::rgb(if active { theme::ACCENT } else { theme::TEXT_MUTED })))
        .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(label)).into())
}

/// Claude badge in the sidebar: yellow = waiting for you, green = finished. Click to jump.
fn attention_dot(id: SharedString, a: crate::terminal_view::Attention) -> gpui::Stateful<gpui::Div> {
    let (color, tip) = match a {
        crate::terminal_view::Attention::Done => (0x98c379, "Claude terminó · click para ir"),
        crate::terminal_view::Attention::NeedsInput => (0xe5c07b, "Claude te necesita · click para ir"),
    };
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(16.))
        .rounded(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
        .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
        .child(div().size(px(8.)).rounded_full().bg(theme::rgb(color)))
}

fn chevron(expanded: bool) -> gpui::Div {
    div()
        .w(px(13.))
        .flex_none()
        .child(gpui::svg().path(if expanded { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)))
}

fn icon_button(id: impl Into<gpui::ElementId>, icon_path: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .p(px(3.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_color(theme::rgb(theme::TEXT_FAINT))
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
        .child(gpui::svg().path(icon_path).size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)))
}

fn tab_chip(
    id: impl Into<gpui::ElementId>,
    icon_path: &'static str,
    icon_color: u32,
    title: SharedString,
    active: bool,
    exited: bool,
    dirty: bool,
) -> gpui::Stateful<gpui::Div> {
    tab_chip_with(id, icon(icon_path, if exited { theme::TEXT_FAINT } else { icon_color }), title, active, exited, dirty)
}

/// Tab with any leading element (file icon, chat picture…).
fn tab_chip_with(id: impl Into<gpui::ElementId>, lead: gpui::AnyElement, title: SharedString, active: bool, exited: bool, dirty: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(TOP_BAR_HEIGHT - 8.))
        .max_w(px(220.))
        .pl(px(10.))
        .pr(px(6.))
        .rounded_t(px(6.))
        .relative()
        .cursor_pointer()
        .when(active, |d| d.bg(theme::rgb(theme::EDITOR_BG)).child(div().absolute().top_0().left(px(8.)).right(px(8.)).h(px(2.)).rounded_b(px(2.)).bg(theme::rgb(theme::ACCENT))))
        .when(!active, |d| d.hover(|s| s.bg(theme::rgb(theme::HOVER))))
        .child(lead)
        .child(
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(12.))
                .text_color(theme::rgb(if exited {
                    theme::TEXT_FAINT
                } else if active {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                }))
                .child(title),
        )
        .when(dirty, |d| d.child(div().size(px(6.)).rounded_full().bg(theme::rgb(theme::MODIFIED))))
}

fn close_button(id: impl Into<gpui::ElementId>, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .p(px(2.))
        .rounded(px(3.))
        .when(!active, |d| d.opacity(0.6))
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
        .child(gpui::svg().path("icons/close.svg").size(px(10.)).text_color(theme::rgb(theme::TEXT_MUTED)))
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let waiting = self.terminals.values().flat_map(|t| t.tabs.iter()).filter(|t| t.read(cx).attention.is_some()).count();
        crate::claude_bridge::set_dock_count(cx.entity_id().as_u64(), waiting);
        if let Some((jid, name)) = self.pending_chat.take() {
            self.open_chat(&jid, &name, window, cx);
        }
        if let Some(plan) = self.pending_ask.take() {
            self.open_claude_ask(plan, window, cx);
        }
        if let Some(panel) = self.whatsapp_panel.clone() {
            let fg = if self.diff_active { None } else { self.active_editor().and_then(|e| e.read(cx).chat_jid(cx).map(str::to_string)) };
            let project = self.state.active_project.clone();
            panel.update(cx, |p, cx| {
                p.set_active_chat(fg, cx);
                p.set_project(project, cx);
            });
        }
        let title = self.active_project().map(|p| p.name.clone()).unwrap_or_else(|| "juan-terminal".into());
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        let sidebar = self.sidebar_visible.then(|| self.render_sidebar(window, cx));
        let context_menu = self.render_context_menu(cx);
        let new_tab_menu = self.render_new_tab_menu(cx);
        let name_prompt = self.render_name_prompt(cx);
        let center = self.render_editor_area(cx);
        let right = self.terminal_visible.then(|| self.render_terminal_panel(cx));
        let resizing = self.resizing_sidebar || self.resizing_terminal || self.resizing_actions;
        // Native video players sit above GPUI content: hide them while an overlay is open.
        let overlay = self.palette.is_some() || self.quick_open.is_some() || self.usage.is_some() || self.music_search.is_some() || context_menu.is_some() || new_tab_menu.is_some() || name_prompt.is_some();
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .flex()
            .bg(theme::rgb(theme::APP_BG))
            .font_family(theme::UI_FONT)
            .text_color(theme::rgb(theme::TEXT))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(|this, _: &NewWindow, window, cx| this.open_project_window(None, window, cx)))
            .on_action(cx.listener(|this, _: &NewFile, window, cx| this.new_untitled(window, cx)))
            .on_action(cx.listener(|this, _: &NewWordDocument, window, cx| this.new_office("docx", window, cx)))
            .on_action(cx.listener(|this, _: &NewSpreadsheet, window, cx| this.new_office("xlsx", window, cx)))
            .on_action(cx.listener(|this, _: &NewPresentation, window, cx| this.new_office("pptx", window, cx)))
            .on_action(cx.listener(Self::close_terminal))
            .on_action(cx.listener(|this, _: &NextTerminal, w, cx| this.cycle_terminal(true, w, cx)))
            .on_action(cx.listener(|this, _: &PreviousTerminal, w, cx| this.cycle_terminal(false, w, cx)))
            .on_action(cx.listener(|this, _: &NextEditor, w, cx| this.cycle_editor(true, w, cx)))
            .on_action(cx.listener(|this, _: &PreviousEditor, w, cx| this.cycle_editor(false, w, cx)))
            .on_action(cx.listener(Self::close_editor))
            .on_action(cx.listener(Self::open_project))
            .on_action(cx.listener(Self::add_repository))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::toggle_quick_open))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::toggle_usage))
            .on_action(cx.listener(Self::toggle_music))
            .on_action(cx.listener(Self::close_diff))
            .on_action(cx.listener(|this, _: &ShowGit, w, cx| this.show_git(w, cx)))
            .on_action(cx.listener(|this, _: &ShowTools, w, cx| this.show_tools(w, cx)))
            .on_action(cx.listener(|this, _: &LintProject, w, cx| {
                this.show_tools(w, cx);
                if let Some(tp) = &this.tools_panel {
                    tp.update(cx, |t, cx| t.lint_all(cx));
                }
            }))
            .on_action(cx.listener(Self::format_file))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(|this, _: &ExtendProjectHours, _, cx| {
                if let Some(id) = this.state.active_project.clone() {
                    this.unlock_for(&id, 3600, cx);
                    let until = this.active_project().and_then(|p| p.unlocked_until).unwrap_or(0);
                    let mins = (until - clock_now().0).max(0) / 60;
                    this.error = Some(format!("Project stays open for {} h {:02} min more (emergency override).", mins / 60, mins % 60).into());
                }
            }))
            .on_action(cx.listener(Self::search_in_project))
            .on_action(cx.listener(|this, _: &ShowFiles, _, cx| {
                this.sidebar_visible = true;
                this.sidebar_mode = SidebarMode::Files;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenClaude, w, cx| {
                let cwd = this.active_terminal().and_then(|t| t.read(cx).current_cwd()).or_else(|| this.active_project().and_then(|p| p.default_cwd()));
                this.open_claude(cwd, None, w, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveAll, _, cx| this.save_all(cx)))
            .on_action(cx.listener(|this, _: &ToggleAutosave, _, cx| {
                this.state.autosave = !this.state.autosave;
                this.error = Some(if this.state.autosave { "Autosave on (saves when the window loses focus)" } else { "Autosave off" }.into());
                this.persist(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|_, _: &ToggleTheme, _, cx| toggle_theme(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminalPanel, w, cx| {
                let terminal_focused = this.active_terminal().is_some_and(|t| t.focus_handle(cx).is_focused(w));
                if this.terminal_visible && !terminal_focused && this.active_editor().is_some() {
                    this.focus_terminal(w, cx);
                } else {
                    this.terminal_visible = !this.terminal_visible;
                    if this.terminal_visible {
                        if this.active_terminal().is_none() {
                            this.new_terminal(&NewTerminal, w, cx);
                        }
                        this.focus_terminal(w, cx);
                    } else {
                        this.focus_main(w, cx);
                    }
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _, cx| this.set_font_size(this.font_size + 1., cx)))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, _, cx| this.set_font_size(this.font_size - 1., cx)))
            .on_action(cx.listener(|this, _: &ResetFontSize, _, cx| this.set_font_size(theme::DEFAULT_FONT_SIZE, cx)))
            .when(resizing, |d| {
                let d = if self.resizing_actions { d.cursor_row_resize() } else { d.cursor_col_resize() };
                d
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, cx| {
                        if this.resizing_sidebar {
                            this.sidebar_width = f32::from(e.position.x).clamp(MIN_SIDEBAR, MAX_SIDEBAR);
                        }
                        if this.resizing_actions {
                            let h = f32::from(window.bounds().size.height);
                            this.actions_height = (h - f32::from(e.position.y)).clamp(90., (h - 200.).max(90.));
                        }
                        if this.resizing_terminal {
                            let w = f32::from(window.bounds().size.width);
                            let side = if this.sidebar_visible { this.sidebar_width } else { 0. };
                            this.terminal_width = (w - f32::from(e.position.x)).clamp(240., (w - side - 240.).max(240.));
                        }
                        cx.notify();
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.resizing_sidebar = false;
                            this.resizing_terminal = false;
                            this.resizing_actions = false;
                            this.persist(cx);
                            cx.notify();
                        }),
                    )
            })
            .children(sidebar)
            .when(self.sidebar_visible, |d| {
                d.child(
                    div()
                        .id("sidebar-resize")
                        .w(px(1.))
                        .h_full()
                        .flex_none()
                        .bg(theme::rgb(theme::BORDER))
                        .relative()
                        .child(
                            div()
                                .id("sidebar-resize-hit")
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(px(-3.))
                                .w(px(7.))
                                .cursor_col_resize()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.resizing_sidebar = true;
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
            })
            .child(center)
            .when(self.terminal_visible, |d| {
                d.child(
                    div()
                        .id("terminal-resize")
                        .w(px(1.))
                        .h_full()
                        .flex_none()
                        .bg(theme::rgb(theme::BORDER))
                        .relative()
                        .child(
                            div()
                                .id("terminal-resize-hit")
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(px(-3.))
                                .w(px(7.))
                                .cursor_col_resize()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.resizing_terminal = true;
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
            })
            .children(right)
            // Offline notice: informative only (no handlers, so it never takes clicks).
            .when(!crate::net::online(), |d| {
                d.child(
                    div().absolute().bottom(px(12.)).left_0().right_0().flex().justify_center().child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(12.))
                            .py(px(6.))
                            .rounded(px(16.))
                            .bg(theme::rgb(theme::SIDEBAR_BG))
                            .border_1()
                            .border_color(theme::rgb(theme::DELETED))
                            .text_size(px(12.))
                            .text_color(theme::rgb(theme::TEXT))
                            .shadow_md()
                            .child(gpui::svg().path("icons/offline.svg").size(px(14.)).text_color(theme::rgb(theme::DELETED)))
                            .child("Sin conexión a internet")
                            .child(div().text_color(theme::rgb(theme::TEXT_MUTED)).child("· puedes seguir trabajando")),
                    ),
                )
            })
            .when_some(self.palette.as_ref().map(|(p, _, _)| p.clone()), |d, p| {
                d.child(
                    div()
                        .id("palette-backdrop")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .justify_center()
                        .pt(px(70.))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, w, cx| this.toggle_palette(&ToggleCommandPalette, w, cx)),
                        )
                        .child(div().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(p)),
                )
            })
            .children(context_menu)
            .children(new_tab_menu)
            .children(name_prompt)
            .when_some(self.quick_open.as_ref().map(|(q, _)| q.clone()), |d, qo| {
                d.child(
                    div()
                        .id("quick-open-backdrop")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .justify_center()
                        .pt(px(70.))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, w, cx| {
                                this.quick_open = None;
                                this.focus_main(w, cx);
                                cx.notify();
                            }),
                        )
                        .child(div().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(qo)),
                )
            })
            .when_some(self.usage.as_ref().map(|(u, _, _)| u.clone()), |d, u| {
                d.child(div().id("usage-overlay").occlude().absolute().top_0().left_0().size_full().child(u))
            })
            .children(self.music_theater.clone())
            .when_some(self.music_search.as_ref().map(|(m, _, _)| m.clone()), |d, ms| {
                d.child(
                    div()
                        .id("music-search-backdrop")
                        .occlude()
                        .bg(gpui::hsla(0., 0., 0., 0.35))
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .justify_center()
                        .pt(px(70.))
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, w, cx| this.toggle_music(&ToggleMusic, w, cx)))
                        .child(div().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(ms)),
                )
            })
            // Last child: paints after every player slot in this window.
            .child(crate::video_player::frame_end(overlay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_files_go_to_others() {
        for n in ["package.json", "tsconfig.app.json", "vite.config.ts", ".oxfmtrc.json", ".env.local", "pnpm-lock.yaml", "eslint.config.js", "Dockerfile"] {
            assert!(is_config_file(n), "{n}");
        }
        for n in ["main.ts", "README.md", "index.html", "App.vue"] {
            assert!(!is_config_file(n), "{n}");
        }
        assert_eq!(others_key(Path::new("/r")), PathBuf::from("/r/:others:"));
    }
}

/// Workspace windows that are still open.
pub(crate) fn workspace_windows(cx: &App) -> Vec<WindowHandle<Workspace>> {
    cx.windows().into_iter().filter_map(|w| w.downcast::<Workspace>()).filter(|w| w.read(cx).is_ok()).collect()
}

/// The window (other than `except`) whose active project is `id`.
pub(crate) fn window_with_project(id: &str, except: Option<AnyWindowHandle>, cx: &App) -> Option<WindowHandle<Workspace>> {
    workspace_windows(cx)
        .into_iter()
        .filter(|w| Some(AnyWindowHandle::from(*w)) != except)
        .find(|w| w.read(cx).is_ok_and(|ws| ws.state.active_project.as_deref() == Some(id)))
}

/// Active projects of the other workspace windows.
fn open_elsewhere(window: &Window, cx: &App) -> HashSet<String> {
    let me = window.window_handle();
    workspace_windows(cx)
        .into_iter()
        .filter(|w| AnyWindowHandle::from(*w) != me)
        .filter_map(|w| w.read(cx).ok().and_then(|ws| ws.state.active_project.clone()))
        .collect()
}

impl Workspace {
    pub(crate) fn active_project_id(&self) -> Option<&str> {
        self.state.active_project.as_deref()
    }

    /// Whether `path` is inside this window's active project.
    pub(crate) fn owns_path(&self, path: &Path) -> bool {
        self.active_project().is_some_and(|p| p.repositories.iter().map(|r| &r.path).chain(p.root.iter()).any(|r| path.starts_with(r)))
    }
}

/// Three-way merge of the project list: `base` is what this window last synced, `ours` its
/// current list, `disk` what is saved now (possibly by another window). Projects this window
/// changed, added or removed win; everything else comes from disk.
fn merge_projects(base: &[Project], ours: &[Project], disk: Vec<Project>) -> Vec<Project> {
    let find = |list: &[Project], id: &str| list.iter().position(|p| p.id == id);
    let mut out: Vec<Project> = Vec::with_capacity(disk.len());
    for d in disk {
        let b = find(base, &d.id).map(|i| &base[i]);
        let o = find(ours, &d.id).map(|i| &ours[i]);
        match (b, o) {
            (Some(_), None) => {} // removed here
            (b, Some(o)) if b != Some(o) => out.push(o.clone()),
            _ => out.push(d),
        }
    }
    for o in ours {
        if find(&out, &o.id).is_none() && find(base, &o.id).is_none() {
            out.push(o.clone()); // added here
        }
    }
    out
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn p(id: &str, name: &str) -> Project {
        Project { id: id.into(), name: name.into(), ..Default::default() }
    }

    #[test]
    fn keeps_changes_from_both_windows() {
        let base = vec![p("a", "a"), p("b", "b"), p("c", "c")];
        // This window renamed a, removed c, added d.
        let ours = vec![p("a", "A"), p("b", "b"), p("d", "d")];
        // Another window renamed b and added e.
        let disk = vec![p("a", "a"), p("b", "B"), p("c", "c"), p("e", "e")];
        let names: Vec<String> = merge_projects(&base, &ours, disk).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["A", "B", "e", "d"]);
    }
}

/// Flip light/dark for the whole app: every window repaints and saves the choice.
pub fn toggle_theme(cx: &mut App) {
    theme::set_light(!theme::is_light());
    for w in workspace_windows(cx) {
        let _ = w.update(cx, |ws, _, cx| {
            for t in ws.terminals.values().flat_map(|tabs| tabs.tabs.clone()) {
                t.update(cx, |t, cx| t.apply_theme(cx));
            }
            ws.persist(cx);
            cx.notify();
        });
    }
    cx.refresh_windows();
}

/// Saved output of terminal `id` (see `Workspace::save_scrollback`).
fn scrollback_file(id: &str) -> PathBuf {
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    workspace_state::state_dir().join("scrollback").join(format!("{safe}.ansi"))
}
