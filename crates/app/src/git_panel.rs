//! Git panel (sidebar "Git" mode): per-repository status, staging, commit, branches,
//! fetch/pull/push and history. Every git call runs on a background thread.

use std::path::{Path, PathBuf};

use git_engine::{Branch, ChangeKind, Commit, FileChange, RemoteOp, RepoStatus};
use gpui::{
    ClickEvent, Context, Entity, EventEmitter, PromptLevel, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use crate::diff_view::toolbar_button;
use crate::text_area::{TextArea, TextAreaEvent};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

const MAX_LISTED: usize = 400;

pub enum GitPanelEvent {
    OpenDiff { repo: PathBuf, change: FileChange, staged: bool },
    OpenCommit { repo: PathBuf, commit: Commit },
    CompareBranch { repo: PathBuf, base: String, head: String },
    OpenFile(PathBuf),
    /// Right-click on a changed file: show the file context menu (with "Locate in Explorer").
    ContextMenu { position: gpui::Point<gpui::Pixels>, path: PathBuf },
    /// Status changed (refresh open diffs, branch labels…).
    Changed,
}

struct RepoState {
    name: String,
    path: PathBuf,
    status: Option<RepoStatus>,
    error: Option<String>,
    message: Option<(String, bool)>, // (text, is_error)
    busy: Option<&'static str>,
    commit_input: Entity<TextArea>,
    branch_input: Entity<TextInput>,
    branches: Option<Vec<Branch>>,
    history: Option<Vec<Commit>>,
    collapsed: bool,
    _subs: Vec<Subscription>,
}

/// Git decoration for a file-tree row: color, plus the status letter (files) or the number
/// of changed files below it (folders, any depth).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeMark {
    pub color: u32,
    pub letter: Option<&'static str>,
    pub count: usize,
}

pub struct GitPanel {
    repos: Vec<RepoState>,
    /// Absolute path -> decoration, rebuilt when any status changes.
    tree_marks: std::rc::Rc<std::collections::HashMap<PathBuf, TreeMark>>,
    /// Parent dir -> deleted entries in it (path, is_dir), so the tree can still list them.
    deleted: std::rc::Rc<std::collections::HashMap<PathBuf, Vec<(PathBuf, bool)>>>,
    /// Repos with a status query in flight (polling never stacks up).
    refreshing: std::collections::HashSet<PathBuf>,
    /// Commits per day over the last `ACTIVITY_DAYS` days (oldest first), across every repo.
    activity: Option<Vec<u32>>,
    /// Local day number (days since 1970-01-01) of the last entry of `activity`.
    activity_today: i64,
    activity_loading: bool,
}

const ACTIVITY_DAYS: usize = 90;

impl EventEmitter<GitPanelEvent> for GitPanel {}

impl GitPanel {
    pub fn new() -> Self {
        Self {
            repos: Vec::new(),
            tree_marks: Default::default(),
            deleted: Default::default(),
            refreshing: Default::default(),
            activity: None,
            activity_today: 0,
            activity_loading: false,
        }
    }

    /// Set repositories for the active project and refresh their status.
    pub fn set_repos(&mut self, repos: Vec<(String, PathBuf)>, window: &mut Window, cx: &mut Context<Self>) {
        let same = repos.len() == self.repos.len() && repos.iter().zip(&self.repos).all(|((_, p), r)| *p == r.path);
        if !same {
            self.repos = repos
                .into_iter()
                .enumerate()
                .map(|(ix, (name, path))| {
                    let commit_input = cx.new(|cx| TextArea::new("Message  ·  ⌘↵ to commit", 2, 10, cx));
                    let branch_input = cx.new(|cx| TextInput::new("New branch name (Enter)", cx));
                    let s1 = cx.subscribe_in(&commit_input, window, move |this, _, e, window, cx| {
                        if let TextAreaEvent::Submit = e {
                            this.commit(ix, window, cx);
                        }
                    });
                    let s2 = cx.subscribe_in(&branch_input, window, move |this, input, e, _, cx| {
                        if let TextInputEvent::Submit { .. } = e {
                            let name = input.read(cx).content.trim().to_string();
                            if !name.is_empty() {
                                input.update(cx, |i, cx| i.set_text("", cx));
                                this.run(ix, "Creating branch", cx, move |p| git_engine::create_branch(p, &name).map(|_| format!("Switched to new branch {name}")));
                                if let Some(r) = this.repos.get_mut(ix) {
                                    r.branches = None;
                                }
                            }
                        }
                    });
                    RepoState {
                        name,
                        path,
                        status: None,
                        error: None,
                        message: None,
                        busy: None,
                        commit_input,
                        branch_input,
                        branches: None,
                        history: None,
                        collapsed: false,
                        _subs: vec![s1, s2],
                    }
                })
                .collect();
            self.rebuild_tree_marks();
            self.activity = None;
            self.load_activity(cx);
        }
        self.refresh_all(cx);
    }

    /// Recount the commits per day (all branches, current user) for the heat map.
    fn load_activity(&mut self, cx: &mut Context<Self>) {
        if self.activity_loading {
            return;
        }
        self.activity_loading = true;
        let paths: Vec<PathBuf> = self.repos.iter().map(|r| r.path.clone()).collect();
        cx.spawn(async move |this, cx| {
            let (today, counts) = cx
                .background_executor()
                .spawn(async move {
                    let today = local_day();
                    let mut seen = std::collections::HashSet::new();
                    let mut counts = vec![0u32; ACTIVITY_DAYS];
                    for p in &paths {
                        for (hash, date) in git_engine::activity(p, ACTIVITY_DAYS as u32).unwrap_or_default() {
                            let Some(day) = parse_day(&date) else { continue };
                            let back = today - day;
                            if (0..ACTIVITY_DAYS as i64).contains(&back) && seen.insert(hash) {
                                counts[ACTIVITY_DAYS - 1 - back as usize] += 1;
                            }
                        }
                    }
                    (today, counts)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.activity_loading = false;
                this.activity_today = today;
                this.activity = Some(counts);
                cx.notify();
            });
        })
        .detach();
    }

    /// GitHub-style contribution grid: one column per week, one row per weekday.
    fn render_activity(&self) -> Option<gpui::Div> {
        let counts = self.activity.as_ref()?;
        let total: u32 = counts.iter().sum();
        let max = counts.iter().copied().max().unwrap_or(0);
        let first = self.activity_today - (ACTIVITY_DAYS as i64 - 1);
        let lead = weekday(first) as usize; // empty cells before the first day (week starts Sunday)
        let weeks = (lead + ACTIVITY_DAYS).div_ceil(7);
        const CELL: f32 = 11.;
        const GAP: f32 = 3.;
        let label = |t: &'static str| div().h(px(CELL)).flex().items_center().text_size(px(9.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(t);
        let mut grid = div().flex().gap(px(GAP)).child(
            div().flex().flex_col().gap(px(GAP)).pr(px(2.)).children(["", "Mon", "", "Wed", "", "Fri", ""].map(label)),
        );
        for w in 0..weeks {
            let mut col = div().flex().flex_col().gap(px(GAP));
            for d in 0..7 {
                let slot = w * 7 + d;
                let cell = div().size(px(CELL)).rounded(px(2.));
                col = col.child(match slot.checked_sub(lead).filter(|&i| i < ACTIVITY_DAYS) {
                    None => cell.into_any_element(),
                    Some(i) => {
                        let n = counts[i];
                        let (_, m, day) = crate::usage_stats::civil_from_days(first + i as i64);
                        let tip: SharedString = format!("{} {} on {} {day}", n, if n == 1 { "commit" } else { "commits" }, MONTHS[m as usize - 1]).into();
                        cell.id(SharedString::from(format!("act-{i}")))
                            .bg(theme::rgb(heat_color(level(n, max))))
                            .tooltip(move |_, cx| cx.new(|_| DynTooltip(tip.clone())).into())
                            .into_any_element()
                    }
                });
            }
            grid = grid.child(col);
        }
        let legend = div()
            .flex()
            .items_center()
            .gap(px(GAP))
            .text_size(px(9.5))
            .text_color(theme::rgb(theme::TEXT_FAINT))
            .child("Less")
            .children((0..5).map(|l| div().size(px(CELL - 2.)).rounded(px(2.)).bg(theme::rgb(heat_color(l)))))
            .child("More");
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(12.))
                .pt(px(10.))
                .pb(px(10.))
                .border_b_1()
                .border_color(theme::rgb(theme::BORDER))
                .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{total} {} in the last {ACTIVITY_DAYS} days", if total == 1 { "commit" } else { "commits" })))
                .child(grid)
                .child(legend),
        )
    }

    /// Total changed files across all repositories (for the sidebar badge).
    pub fn total_changes(&self) -> usize {
        self.repos.iter().filter_map(|r| r.status.as_ref()).map(|s| s.files.len()).sum()
    }

    pub fn tree_marks(&self) -> std::rc::Rc<std::collections::HashMap<PathBuf, TreeMark>> {
        self.tree_marks.clone()
    }

    /// Repo and change for an absolute path in any repository's status.
    pub fn change_at(&self, abs: &std::path::Path) -> Option<(PathBuf, FileChange)> {
        self.repos.iter().find_map(|r| {
            let rel = abs.strip_prefix(&r.path).ok()?.to_string_lossy().into_owned();
            let f = r.status.as_ref()?.files.iter().find(|f| f.path == rel)?;
            Some((r.path.clone(), f.clone()))
        })
    }

    /// Deleted files (and folders gone with them), keyed by parent dir.
    pub fn deleted_entries(&self) -> std::rc::Rc<std::collections::HashMap<PathBuf, Vec<(PathBuf, bool)>>> {
        self.deleted.clone()
    }

    fn rebuild_tree_marks(&mut self) {
        // Folder color: the "strongest" change below it (added < modified < conflicted).
        fn rank(c: u32) -> u8 {
            match c {
                theme::ADDED => 0,
                0xff6b6b => 2,
                _ => 1,
            }
        }
        let mut map: std::collections::HashMap<PathBuf, TreeMark> = Default::default();
        let mut deleted: std::collections::HashMap<PathBuf, Vec<(PathBuf, bool)>> = Default::default();
        for repo in &self.repos {
            let Some(st) = repo.status.as_ref() else { continue };
            for f in &st.files {
                let kind = f.unstaged.or(f.staged).unwrap_or(ChangeKind::Modified);
                let color = kind_color(kind);
                let path = repo.path.join(&f.path);
                map.insert(path.clone(), TreeMark { color, letter: Some(kind.letter()), count: 0 });
                if kind == ChangeKind::Deleted && !path.exists() {
                    // The file, plus every missing folder above it (once each).
                    let mut child = path.clone();
                    let mut is_dir = false;
                    while let Some(parent) = child.parent() {
                        let list = deleted.entry(parent.to_path_buf()).or_default();
                        if list.iter().any(|(p, _)| *p == child) {
                            break;
                        }
                        list.push((child.clone(), is_dir));
                        if parent.exists() || !parent.starts_with(&repo.path) {
                            break;
                        }
                        child = parent.to_path_buf();
                        is_dir = true;
                    }
                }
                let dir_color = if kind == ChangeKind::Deleted { theme::MODIFIED } else { color };
                let mut dir = path.parent();
                while let Some(d) = dir {
                    if !d.starts_with(&repo.path) || d == repo.path {
                        break;
                    }
                    let m = map.entry(d.to_path_buf()).or_insert(TreeMark { color: dir_color, letter: None, count: 0 });
                    m.count += 1;
                    if rank(dir_color) > rank(m.color) {
                        m.color = dir_color;
                    }
                    dir = d.parent();
                }
            }
        }
        self.tree_marks = std::rc::Rc::new(map);
        self.deleted = std::rc::Rc::new(deleted);
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        for ix in 0..self.repos.len() {
            self.refresh(ix, cx);
        }
    }

    /// Refresh the repositories containing `path` (after a save).
    pub fn refresh_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        for ix in 0..self.repos.len() {
            if path.starts_with(&self.repos[ix].path) {
                self.refresh(ix, cx);
            }
        }
    }

    fn refresh(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.get(ix) else { return };
        let path = repo.path.clone();
        if !self.refreshing.insert(path.clone()) {
            return;
        }
        let want_history = repo.history.is_some();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let (status, history) = cx
                .background_executor()
                .spawn(async move {
                    let st = git_engine::status(&p);
                    let hist = want_history.then(|| git_engine::log(&p, 100).unwrap_or_default());
                    (st, hist)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refreshing.remove(&path);
                let mut changed = false;
                if let Some(r) = this.repos.iter_mut().find(|r| r.path == path) {
                    match status {
                        Ok(s) => {
                            changed |= r.status.as_ref() != Some(&s) || r.error.is_some();
                            r.status = Some(s);
                            r.error = None;
                        }
                        Err(e) => {
                            let e = format!("{e:#}");
                            changed |= r.error.as_ref() != Some(&e);
                            r.error = Some(e);
                        }
                    }
                    if let Some(h) = history {
                        changed |= r.history.as_ref() != Some(&h);
                        r.history = Some(h);
                    }
                }
                // Polling runs every few seconds: only repaint when something changed.
                if changed {
                    this.rebuild_tree_marks();
                    cx.emit(GitPanelEvent::Changed);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Run a git operation in the background, then refresh the repo.
    fn run(&mut self, ix: usize, label: &'static str, cx: &mut Context<Self>, op: impl FnOnce(&Path) -> anyhow::Result<String> + Send + 'static) {
        let Some(repo) = self.repos.get_mut(ix) else { return };
        if repo.busy.is_some() {
            return;
        }
        repo.busy = Some(label);
        repo.message = None;
        let path = repo.path.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx.background_executor().spawn(async move { op(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some((ix, r)) = this.repos.iter_mut().enumerate().find(|(_, r)| r.path == path) {
                    r.busy = None;
                    r.message = match result {
                        Ok(msg) if msg.is_empty() => None,
                        Ok(msg) => Some((msg, false)),
                        Err(e) => Some((format!("{e:#}"), true)),
                    };
                    this.refresh(ix, cx);
                }
                this.load_activity(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn apply_hunk(&mut self, repo: &Path, patch: String, reverse: bool, cx: &mut Context<Self>) {
        if let Some(ix) = self.repos.iter().position(|r| r.path == repo) {
            self.run(ix, if reverse { "Unstaging hunk" } else { "Staging hunk" }, cx, move |p| git_engine::apply_to_index(p, &patch, reverse).map(|_| String::new()));
        }
    }

    fn commit(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.get(ix) else { return };
        let message = repo.commit_input.read(cx).content.clone();
        let input = repo.commit_input.clone();
        if repo.status.as_ref().is_some_and(|s| s.staged().next().is_none()) {
            if let Some(r) = self.repos.get_mut(ix) {
                r.message = Some(("Nothing staged. Stage files first (+).".into(), true));
            }
            cx.notify();
            return;
        }
        // Clear the input only after a successful commit.
        let path = repo.path.clone();
        if let Some(r) = self.repos.get_mut(ix) {
            r.busy = Some("Committing");
            r.message = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx.background_executor().spawn(async move { git_engine::commit(&p, &message, false) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some((ix, r)) = this.repos.iter_mut().enumerate().find(|(_, r)| r.path == path) {
                    r.busy = None;
                    match result {
                        Ok(summary) => {
                            r.message = Some((summary, false));
                            input.update(cx, |i, cx| i.set_text("", cx));
                        }
                        Err(e) => r.message = Some((format!("{e:#}"), true)),
                    }
                    this.refresh(ix, cx);
                }
            });
        })
        .detach();
    }

    fn confirm_discard(&mut self, ix: usize, change: FileChange, window: &mut Window, cx: &mut Context<Self>) {
        let untracked = change.unstaged == Some(ChangeKind::Untracked);
        let answer = window.prompt(
            PromptLevel::Critical,
            &format!("Discard changes to “{}”?", change.path),
            Some(if untracked { "This untracked file will be deleted. This cannot be undone." } else { "Your uncommitted changes to this file will be lost. This cannot be undone." }),
            &["Discard", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.run(ix, "Discarding", cx, move |p| git_engine::discard(p, &change).map(|_| String::new()));
            });
        })
        .detach();
    }

    fn toggle_branches(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.get_mut(ix) else { return };
        if repo.branches.take().is_some() {
            cx.notify();
            return;
        }
        let path = repo.path.clone();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx.background_executor().spawn(async move { git_engine::branches(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(r) = this.repos.iter_mut().find(|r| r.path == path) {
                    match result {
                        Ok(b) => r.branches = Some(b),
                        Err(e) => r.message = Some((format!("{e:#}"), true)),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_history(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.get_mut(ix) else { return };
        if repo.history.take().is_none() {
            repo.history = Some(Vec::new());
            self.refresh(ix, cx);
        }
        cx.notify();
    }

    fn render_change(&self, ix: usize, change: &FileChange, staged: bool, row_id: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = if staged { change.staged } else { change.unstaged }.unwrap_or(ChangeKind::Modified);
        let color = kind_color(kind);
        let name = change.path.rsplit('/').next().unwrap_or(&change.path).to_string();
        let mut dir = change.path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        // Only show the folder when it fits next to the name (a 1-char ellipsis looks broken).
        if name.chars().count() + dir.chars().count() > 30 {
            dir = dir.rsplit('/').next().unwrap_or("").to_string();
            if name.chars().count() + dir.chars().count() > 30 {
                dir.clear();
            }
        }
        let (icon, icon_color) = crate::assets::file_icon(&name);
        let path = self.repos[ix].path.clone();
        let group: SharedString = format!("chg-{ix}-{row_id}").into();
        let (c_open, c_stage, c_discard) = (change.clone(), change.clone(), change.clone());
        let p_open = path.clone();
        let file_path = path.join(&change.path);
        let menu_path = file_path.clone();
        div()
            .id(SharedString::from(format!("chg-{ix}-{staged}-{row_id}")))
            .group(group.clone())
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(24.))
            .pl(px(22.))
            .pr(px(8.))
            .mx(px(6.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
            .on_click(cx.listener(move |_, e: &ClickEvent, _, cx| {
                if e.click_count() == 2 && file_path.is_file() {
                    cx.emit(GitPanelEvent::OpenFile(file_path.clone()));
                } else {
                    cx.emit(GitPanelEvent::OpenDiff { repo: p_open.clone(), change: c_open.clone(), staged });
                }
            }))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |_, e: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                cx.emit(GitPanelEvent::ContextMenu { position: e.position, path: menu_path.clone() });
            }))
            .child(gpui::svg().path(icon).size(px(13.)).flex_none().text_color(theme::rgb(icon_color)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(px(6.))
                    .overflow_hidden()
                    .child(div().flex_shrink().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).text_color(theme::rgb(if kind == ChangeKind::Deleted { theme::TEXT_MUTED } else { theme::TEXT })).when(kind == ChangeKind::Deleted, |d| d.line_through()).child(name))
                    .when(!dir.is_empty(), |d| d.child(div().flex_none().whitespace_nowrap().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(dir))),
            )
            .child(if !staged && kind != ChangeKind::Conflicted {
                mini_button(SharedString::from(format!("discard-{ix}-{row_id}")), "icons/undo.svg")
                    .invisible()
                    .group_hover(group.clone(), |s| s.visible())
                    .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.confirm_discard(ix, c_discard.clone(), w, cx)
                    }))
                    .into_any_element()
            } else {
                div().w(px(18.)).flex_none().into_any_element()
            })
            .child(
                mini_button(SharedString::from(format!("stage-{ix}-{staged}-{row_id}")), if staged { "icons/minus.svg" } else { "icons/plus.svg" })
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        let c = c_stage.clone();
                        if staged {
                            this.run(ix, "Unstaging", cx, move |p| {
                                let mut paths = vec![c.path.as_str()];
                                if let Some(o) = c.orig_path.as_deref() {
                                    paths.push(o);
                                }
                                git_engine::unstage(p, &paths).map(|_| String::new())
                            });
                        } else {
                            this.run(ix, "Staging", cx, move |p| git_engine::stage(p, &[c.path.as_str()]).map(|_| String::new()));
                        }
                    })),
            )
            .child(div().w(px(12.)).flex_none().flex().justify_end().text_size(px(11.)).font_family(theme::MONO_FONT).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(color)).child(kind.letter()))
    }
}

fn kind_color(kind: ChangeKind) -> u32 {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => theme::ADDED,
        ChangeKind::Deleted => theme::DELETED,
        ChangeKind::Conflicted => 0xff6b6b,
        _ => theme::MODIFIED,
    }
}

pub fn icon_action(id: impl Into<gpui::ElementId>, icon: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.))
        .px(px(5.))
        .h(px(22.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(theme::rgb(theme::TEXT_MUTED))
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT)))
        .child(gpui::svg().path(icon).size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)))
        .tooltip(move |_, cx| cx.new(|_| Tooltip(label)).into())
}

pub struct Tooltip(pub &'static str);

/// Tooltip with owned text.
pub struct DynTooltip(pub SharedString);

impl Render for DynTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(3.))
            .rounded(px(4.))
            .bg(theme::rgb(0x2a2c31))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .text_size(px(11.5))
            .text_color(theme::rgb(theme::TEXT))
            .child(self.0.clone())
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(3.))
            .rounded(px(4.))
            .bg(theme::rgb(0x2a2c31))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .text_size(px(11.5))
            .text_color(theme::rgb(theme::TEXT))
            .child(self.0)
    }
}

fn mini_button(id: impl Into<gpui::ElementId>, icon: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(18.))
        .h(px(18.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
        .child(gpui::svg().path(icon).size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)))
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Heat level 0–4 relative to the busiest day (like GitHub's quartiles).
fn level(n: u32, max: u32) -> u32 {
    if n == 0 || max == 0 { 0 } else { (n * 4).div_ceil(max).clamp(1, 4) }
}

fn heat_color(level: u32) -> u32 {
    match level {
        0 => theme::SELECTED,
        1 => 0x0e4429,
        2 => 0x006d32,
        3 => 0x26a641,
        _ => 0x39d353,
    }
}

/// 0 = Sunday. 1970-01-01 was a Thursday.
fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

fn parse_day(s: &str) -> Option<i64> {
    let mut it = s.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    Some(crate::usage_stats::days_from_civil(it.next()??, it.next()??, it.next()??))
}

/// Today's local day number (days since 1970-01-01).
fn local_day() -> i64 {
    // SAFETY: localtime_r only writes into the struct we pass.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    crate::usage_stats::days_from_civil(tm.tm_year as i64 + 1900, tm.tm_mon as i64 + 1, tm.tm_mday as i64)
}

fn count_pill(n: usize) -> gpui::Div {
    div()
        .flex_none()
        .px(px(6.))
        .h(px(16.))
        .flex()
        .items_center()
        .rounded_full()
        .bg(theme::rgb(0x3a3220))
        .text_size(px(10.5))
        .text_color(theme::rgb(theme::MODIFIED))
        .child(n.to_string())
}

fn section_header(label: String) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(24.))
        .pl(px(28.))
        .pr(px(8.))
        .mt(px(4.))
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::rgb(theme::TEXT_FAINT))
        .child(label)
}

impl Render for GitPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().id("git-panel").flex().flex_col().size_full().overflow_y_scroll().pb(px(16.));
        if let Some(heat) = self.render_activity() {
            list = list.child(heat);
        }
        if self.repos.is_empty() {
            list = list.child(div().px(px(16.)).py(px(8.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("This project has no Git repositories."));
        }
        for ix in 0..self.repos.len() {
            let repo = &self.repos[ix];
            let st = repo.status.clone();
            let branch = st.as_ref().and_then(|s| s.branch.clone()).unwrap_or_else(|| if st.as_ref().is_some_and(|s| s.detached) { "detached".into() } else { "…".into() });
            let (ahead, behind) = st.as_ref().map(|s| (s.ahead, s.behind)).unwrap_or((0, 0));
            let changes = st.as_ref().map(|s| s.files.len()).unwrap_or(0);
            let collapsed = repo.collapsed;

            // Header: name, branch, sync buttons.
            let clean = st.as_ref().is_some_and(|s| s.files.is_empty() && s.operation.is_none());
            list = list.child(
                div()
                    .id(SharedString::from(format!("repo-h-{ix}")))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(30.))
                    .pl(px(10.))
                    .pr(px(8.))
                    .when(ix > 0, |d| d.mt(px(6.)).border_t_1().border_color(theme::rgb(theme::BORDER)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.repos[ix].collapsed = !this.repos[ix].collapsed;
                        cx.notify();
                    }))
                    .child(gpui::svg().path(if collapsed { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" }).size(px(10.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)))
                    .child(gpui::svg().path("icons/repo.svg").size(px(13.)).flex_none().text_color(theme::rgb(0xe0714f)))
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(repo.name.clone()))
                    .when(changes > 0, |d| d.child(count_pill(changes)))
                    .child(div().flex_1())
                    .when(collapsed || clean, |d| d.child(div().flex_none().max_w(px(110.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_FAINT)).child(branch.clone()))),
            );
            if collapsed {
                continue;
            }
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .pl(px(28.))
                    .pr(px(8.))
                    .h(px(26.))
                    .child(
                        div()
                            .id(SharedString::from(format!("branch-{ix}")))
                            .flex()
                            .flex_shrink()
                            .min_w_0()
                            .items_center()
                            .gap(px(4.))
                            .px(px(6.))
                            .h(px(20.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .bg(theme::rgb(theme::HOVER))
                            .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                            .text_size(px(11.5))
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::rgb(theme::TEXT))
                            .child(gpui::svg().path("icons/git.svg").size(px(11.)).flex_none().text_color(theme::rgb(theme::TEXT_MUTED)))
                            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(branch))
                            .child(gpui::svg().path("icons/chevron-down.svg").size(px(8.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_branches(ix, cx))),
                    )
                    .when(ahead > 0, |d| d.child(div().flex_none().text_size(px(11.)).text_color(theme::rgb(theme::ADDED)).child(format!("↑{ahead}"))))
                    .when(behind > 0, |d| d.child(div().flex_none().text_size(px(11.)).text_color(theme::rgb(theme::MODIFIED)).child(format!("↓{behind}"))))
                    .when(clean, |d| d.child(gpui::svg().path("icons/check.svg").size(px(12.)).flex_none().text_color(theme::rgb(theme::ADDED))))
                    .child(div().flex_1())
                    .child(icon_action(SharedString::from(format!("fetch-{ix}")), "icons/fetch.svg", "Fetch").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.run(ix, "Fetching", cx, |p| git_engine::remote(p, RemoteOp::Fetch)))))
                    .child(icon_action(SharedString::from(format!("pull-{ix}")), "icons/pull.svg", "Pull").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.run(ix, "Pulling", cx, |p| git_engine::remote(p, RemoteOp::Pull)))))
                    .child(icon_action(SharedString::from(format!("push-{ix}")), "icons/push.svg", "Push").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.run(ix, "Pushing", cx, |p| git_engine::remote(p, RemoteOp::Push))))),
            );

            // Branch picker.
            if let Some(branches) = repo.branches.clone() {
                let current = repo.status.as_ref().and_then(|s| s.branch.clone()).unwrap_or_default();
                let mut picker = div().flex().flex_col().mx(px(12.)).my(px(4.)).p(px(4.)).rounded(px(6.)).bg(theme::rgb(theme::EDITOR_BG)).border_1().border_color(theme::rgb(theme::BORDER));
                picker = picker.child(div().flex().px(px(6.)).py(px(3.)).child(repo.branch_input.clone()));
                for (bi, b) in branches.iter().take(80).enumerate() {
                    let b2 = b.clone();
                    let b3 = b.clone();
                    let current2 = current.clone();
                    let group: SharedString = format!("br-{ix}-{bi}").into();
                    picker = picker.child(
                        div()
                            .id(SharedString::from(format!("br-{ix}-{bi}")))
                            .group(group.clone())
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .h(px(24.))
                            .px(px(6.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let b = b2.clone();
                                this.repos[ix].branches = None;
                                this.run(ix, "Switching branch", cx, move |p| git_engine::checkout(p, &b).map(|_| format!("Switched to {}", b.name)));
                            }))
                            .child(div().w(px(10.)).text_size(px(11.)).text_color(theme::rgb(theme::ACCENT)).child(if b.is_current { "●" } else { "" }))
                            .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).font_family(theme::MONO_FONT).text_color(theme::rgb(if b.is_remote { theme::TEXT_MUTED } else { theme::TEXT })).child(b.name.clone()))
                            .when(!b.is_current, |d| {
                                d.child(
                                    toolbar_button(SharedString::from(format!("cmp-{ix}-{bi}")), "Compare")
                                        .invisible()
                                        .group_hover(group.clone(), |s| s.visible())
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            let repo = this.repos[ix].path.clone();
                                            cx.emit(GitPanelEvent::CompareBranch { repo, base: b3.name.clone(), head: if current2.is_empty() { "HEAD".into() } else { current2.clone() } });
                                        })),
                                )
                            })
                            .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(b.last_commit.clone())),
                    );
                }
                list = list.child(picker);
            }

            if let Some(op) = st.as_ref().and_then(|s| s.operation) {
                list = list.child(div().mx(px(12.)).my(px(4.)).px(px(8.)).py(px(4.)).rounded(px(4.)).bg(theme::rgb(0x3a3220)).text_size(px(12.)).text_color(theme::rgb(theme::MODIFIED)).child(format!("A {op} is in progress. Resolve conflicts, stage, then commit (or abort in the terminal).")));
            }

            // Commit box (only when there is something to commit).
            let staged_count = st.as_ref().map(|s| s.staged().count()).unwrap_or(0);
            list = list.when(!clean && st.is_some(), |list| list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .ml(px(28.))
                    .mr(px(10.))
                    .my(px(4.))
                    .child(
                        div()
                            .w_full()
                            .px(px(8.))
                            .py(px(6.))
                            .rounded(px(5.))
                            .bg(theme::rgb(theme::EDITOR_BG))
                            .border_1()
                            .border_color(theme::rgb(theme::BORDER))
                            .child(repo.commit_input.clone()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("commit-{ix}")))
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(6.))
                            .h(px(26.))
                            .rounded(px(5.))
                            .text_size(px(12.))
                            .when(staged_count > 0, |d| {
                                d.cursor_pointer()
                                    .bg(theme::rgb(0x2b4a7a))
                                    .text_color(theme::rgb(theme::TEXT))
                                    .hover(|s| s.bg(theme::rgb(0x355a92)))
                                    .child(gpui::svg().path("icons/check.svg").size(px(12.)).text_color(theme::rgb(theme::TEXT)))
                            })
                            .when(staged_count == 0, |d| d.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT_FAINT)))
                            .child(if staged_count > 0 { format!("Commit {staged_count} staged") } else { "Stage files to commit".to_string() })
                            .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.commit(ix, w, cx))),
                    ),
            ));
            if let Some(b) = repo.busy {
                list = list.child(div().px(px(16.)).text_size(px(11.5)).text_color(theme::rgb(theme::ACCENT)).child(format!("{b}…")));
            }
            if let Some((msg, is_err)) = repo.message.clone() {
                list = list.child(
                    div()
                        .id(SharedString::from(format!("msg-{ix}")))
                        .mx(px(12.))
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .bg(theme::rgb(if is_err { 0x3a1f22 } else { 0x1d2b22 }))
                        .text_size(px(11.5))
                        .text_color(theme::rgb(if is_err { 0xf0b3b8 } else { 0xa8d8a0 }))
                        .child(msg)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.repos[ix].message = None;
                            cx.notify();
                        })),
                );
            }
            if let Some(e) = repo.error.clone() {
                list = list.child(div().px(px(16.)).text_size(px(11.5)).text_color(theme::rgb(0xf0b3b8)).child(e));
            }

            if let Some(st) = st {
                let conflicted: Vec<FileChange> = st.conflicted().cloned().collect();
                let staged: Vec<FileChange> = st.staged().cloned().collect();
                let unstaged: Vec<FileChange> = st.unstaged().filter(|f| f.unstaged != Some(ChangeKind::Conflicted)).cloned().collect();
                if !conflicted.is_empty() {
                    list = list.child(section_header(format!("CONFLICTS  {}", conflicted.len())).text_color(theme::rgb(0xff6b6b)));
                    for (i, c) in conflicted.iter().take(MAX_LISTED).enumerate() {
                        list = list.child(self.render_change(ix, c, false, 100_000 + i, cx));
                    }
                }
                if !staged.is_empty() {
                    list = list.child(
                        section_header(format!("STAGED  {}", staged.len())).child(div().flex_1()).child(
                            icon_action(SharedString::from(format!("unstage-all-{ix}")), "icons/minus.svg", "Unstage all").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.run(ix, "Unstaging", cx, |p| git_engine::unstage(p, &["."]).map(|_| String::new()))
                            })),
                        ),
                    );
                    for (i, c) in staged.iter().take(MAX_LISTED).enumerate() {
                        list = list.child(self.render_change(ix, c, true, i, cx));
                    }
                }
                if !unstaged.is_empty() {
                    list = list.child(
                        section_header(format!("CHANGES  {}", unstaged.len())).child(div().flex_1()).child(
                            icon_action(SharedString::from(format!("stage-all-{ix}")), "icons/plus.svg", "Stage all").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.run(ix, "Staging", cx, |p| git_engine::stage(p, &["."]).map(|_| String::new()))
                            })),
                        ),
                    );
                    for (i, c) in unstaged.iter().take(MAX_LISTED).enumerate() {
                        list = list.child(self.render_change(ix, c, false, 50_000 + i, cx));
                    }
                    if unstaged.len() > MAX_LISTED {
                        list = list.child(div().px(px(24.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("…and {} more", unstaged.len() - MAX_LISTED)));
                    }
                }
            }

            // History.
            let history_open = self.repos[ix].history.is_some();
            list = list.child(
                div()
                    .id(SharedString::from(format!("hist-{ix}")))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_history(ix, cx)))
                    .child(
                        section_header("HISTORY".into())
                            .hover(|s| s.text_color(theme::rgb(theme::TEXT_MUTED)))
                            .child(gpui::svg().path(if history_open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(8.)).text_color(theme::rgb(theme::TEXT_FAINT))),
                    ),
            );
            if let Some(history) = self.repos[ix].history.clone() {
                for (ci, c) in history.iter().enumerate() {
                    let commit = c.clone();
                    let path = self.repos[ix].path.clone();
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("c-{ix}-{ci}")))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(24.))
                            .pl(px(24.))
                            .pr(px(8.))
                            .mx(px(6.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(GitPanelEvent::OpenCommit { repo: path.clone(), commit: commit.clone() })))
                            .child(div().flex_none().text_size(px(11.)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::ACCENT)).child(c.short.clone()))
                            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).text_color(theme::rgb(theme::TEXT)).child(c.subject.clone()))
                            .child(div().flex_none().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(c.when.clone())),
                    );
                }
            }
        }
        list
    }
}

impl Default for GitPanel {
    fn default() -> Self {
        Self::new()
    }
}
