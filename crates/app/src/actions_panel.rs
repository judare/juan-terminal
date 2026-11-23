//! GitHub Actions section under the file tree. Only shown when a repo of the project has
//! `.github/workflows/*.yml`. Lists the latest runs per repo and workflow on the checked-out
//! branch (via `gh`), with
//! job progress for running ones, refresh / cancel / re-run, and logs opened as files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use gpui::{ClickEvent, Context, EventEmitter, Render, SharedString, Task, Window, div, prelude::*, px};
use serde_json::Value;

use crate::git_panel::{DynTooltip, icon_action};
use crate::theme;

const RUNS_PER_REPO: usize = 40;
const ACTIVE_POLL: Duration = Duration::from_secs(6);
const IDLE_POLL: Duration = Duration::from_secs(60);

pub enum ActionsPanelEvent {
    /// A log/detail file was written; open it in the editor.
    OpenLog(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub number: u64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub started: String,
    pub completed: String,
    pub url: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub id: u64,
    pub number: u64,
    pub title: String,
    pub workflow: String,
    pub status: String,
    pub conclusion: String,
    pub branch: String,
    pub event: String,
    pub created: String,
    pub url: String,
}

impl Run {
    fn active(&self) -> bool {
        is_active(&self.status)
    }
}

fn is_active(status: &str) -> bool {
    matches!(status, "queued" | "in_progress" | "waiting" | "requested" | "pending")
}

#[derive(Default)]
struct RepoActions {
    name: String,
    path: PathBuf,
    runs: Vec<Run>,
    /// Local branch the runs are filtered to (empty: detached HEAD, all branches).
    branch: String,
    loaded: bool,
    loading: bool,
    error: Option<String>,
}

pub struct ActionsPanel {
    repos: Vec<RepoActions>,
    jobs: HashMap<u64, Vec<Job>>,
    /// Runs whose job list is shown.
    expanded_runs: HashSet<u64>,
    /// Repos shown open (collapsed by default).
    expanded_repos: HashSet<PathBuf>,
    /// Section open (else only its header shows).
    pub expanded: bool,
    /// On screen (Files mode, sidebar visible): polling runs only then.
    visible: bool,
    busy: HashSet<u64>,
    message: Option<String>,
    poll: Option<Task<()>>,
}

impl EventEmitter<ActionsPanelEvent> for ActionsPanel {}

/// Repo folders that contain at least one workflow file.
fn has_workflows(repo: &Path) -> bool {
    std::fs::read_dir(repo.join(".github/workflows"))
        .map(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "yml" || x == "yaml")))
        .unwrap_or(false)
}

fn gh(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh").args(args).current_dir(dir).env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1").output().map_err(|e| format!("gh: {e} (¿está instalado GitHub CLI?)"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("gh {} falló", args.first().unwrap_or(&"")) } else { err })
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Checked-out branch of `dir`, or None on detached HEAD / not a git repo.
fn current_branch(dir: &Path) -> Option<String> {
    let out = Command::new("git").args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(dir).output().ok()?;
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !b.is_empty() && b != "HEAD").then_some(b)
}

/// Runs of the checked-out branch (like GitHub's checks for that branch), newest first.
fn fetch_runs(dir: &Path) -> Result<(String, Vec<Run>), String> {
    let limit = RUNS_PER_REPO.to_string();
    let branch = current_branch(dir).unwrap_or_default();
    let mut args = vec!["run", "list", "-L", &limit, "--json", "databaseId,number,displayTitle,workflowName,status,conclusion,headBranch,event,createdAt,url"];
    if !branch.is_empty() {
        args.extend(["--branch", branch.as_str()]);
    }
    let out = gh(dir, &args)?;
    let v: Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
    let runs = v
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| Run {
                    id: n(r, "databaseId"),
                    number: n(r, "number"),
                    title: s(r, "displayTitle"),
                    workflow: s(r, "workflowName"),
                    status: s(r, "status"),
                    conclusion: s(r, "conclusion"),
                    branch: s(r, "headBranch"),
                    event: s(r, "event"),
                    created: s(r, "createdAt"),
                    url: s(r, "url"),
                })
                .collect::<Vec<Run>>()
        })
        .unwrap_or_default()
        .into_iter()
        // Nightly `schedule` runs aren't commit checks; they'd mask the push/PR result.
        .filter(|r: &Run| r.event != "schedule")
        .collect();
    Ok((branch, runs))
}

fn fetch_jobs(dir: &Path, run: u64) -> Result<Vec<Job>, String> {
    let out = gh(dir, &["run", "view", &run.to_string(), "--json", "jobs"])?;
    let v: Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
    Ok(v.get("jobs")
        .and_then(|j| j.as_array())
        .map(|a| {
            a.iter()
                .map(|j| Job {
                    id: n(j, "databaseId"),
                    name: s(j, "name"),
                    status: s(j, "status"),
                    conclusion: s(j, "conclusion"),
                    started: s(j, "startedAt"),
                    completed: s(j, "completedAt"),
                    url: s(j, "url"),
                    steps: j
                        .get("steps")
                        .and_then(|x| x.as_array())
                        .map(|st| st.iter().map(|x| Step { number: n(x, "number"), name: s(x, "name"), status: s(x, "status"), conclusion: s(x, "conclusion") }).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Seconds since the Unix epoch for `YYYY-MM-DDTHH:MM:SSZ`.
fn parse_time(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn ago(t: &str) -> String {
    let Some(ts) = parse_time(t) else { return String::new() };
    let d = (now() - ts).max(0);
    match d {
        0..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

fn duration(start: &str, end: &str) -> String {
    let Some(a) = parse_time(start) else { return String::new() };
    let b = parse_time(end).unwrap_or_else(now);
    let d = (b - a).max(0);
    if d < 60 { format!("{d}s") } else if d < 3600 { format!("{}m {}s", d / 60, d % 60) } else { format!("{}h {}m", d / 3600, (d % 3600) / 60) }
}

/// (icon, color) for a status/conclusion pair.
fn status_look(status: &str, conclusion: &str) -> (&'static str, u32) {
    if is_active(status) {
        return if status == "in_progress" { ("icons/action-running.svg", theme::MODIFIED) } else { ("icons/action-queued.svg", 0xb3a06a) };
    }
    match conclusion {
        "success" => ("icons/check.svg", theme::ADDED),
        "failure" | "timed_out" | "startup_failure" => ("icons/close.svg", theme::DELETED),
        "cancelled" => ("icons/action-stop.svg", theme::TEXT_FAINT),
        "skipped" | "neutral" => ("icons/minus.svg", theme::TEXT_FAINT),
        "action_required" => ("icons/action-queued.svg", theme::MODIFIED),
        _ => ("icons/minus.svg", theme::TEXT_FAINT),
    }
}

fn status_text(status: &str, conclusion: &str) -> String {
    if is_active(status) || conclusion.is_empty() { status.replace('_', " ") } else { conclusion.replace('_', " ") }
}

/// Most recent run of each workflow (runs come newest first).
fn latest_per_workflow(runs: &[Run]) -> Vec<Run> {
    let mut seen: Vec<&str> = Vec::new();
    runs.iter()
        .filter(|r| {
            let new = !seen.contains(&r.workflow.as_str());
            if new {
                seen.push(&r.workflow);
            }
            new
        })
        .cloned()
        .collect()
}

/// (succeeded, total, failed) over finished runs: skipped/cancelled count only in the total.
fn run_summary(runs: &[Run]) -> (usize, usize, usize) {
    let ok = runs.iter().filter(|r| r.conclusion == "success").count();
    let failed = runs.iter().filter(|r| matches!(r.conclusion.as_str(), "failure" | "timed_out" | "startup_failure")).count();
    (ok, runs.len(), failed)
}

/// (done, pending, success, failed) over jobs.
fn job_counts(jobs: &[Job]) -> (usize, usize, usize, usize) {
    let done = jobs.iter().filter(|j| !is_active(&j.status)).count();
    let ok = jobs.iter().filter(|j| j.conclusion == "success").count();
    let failed = jobs.iter().filter(|j| matches!(j.conclusion.as_str(), "failure" | "timed_out" | "startup_failure")).count();
    (done, jobs.len() - done, ok, failed)
}

fn step_mark(st: &Step) -> &'static str {
    if is_active(&st.status) {
        if st.status == "in_progress" { "●" } else { "○" }
    } else {
        match st.conclusion.as_str() {
            "success" => "✓",
            "failure" => "✗",
            "skipped" => "-",
            "cancelled" => "⊘",
            _ => "·",
        }
    }
}

/// Make a raw Actions log readable: short timestamps, no ANSI, group markers as headings.
fn clean_log(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.trim_start_matches('\u{feff}').lines() {
        let line = line.trim_start_matches('\u{feff}');
        let stamped = line.as_bytes().get(10) == Some(&b'T') && line.get(..28).is_some_and(|p| p.ends_with('Z'));
        let (time, rest) = match (stamped, line.get(11..19), line.get(29..)) {
            (true, Some(t), Some(r)) => (t, r.trim_end_matches('\r')),
            _ => ("", line),
        };
        let rest = strip_ansi(rest);
        if rest.starts_with("##[endgroup]") {
            continue;
        }
        let rest = if let Some(g) = rest.strip_prefix("##[group]") {
            format!("▸ {g}")
        } else if let Some(e) = rest.strip_prefix("##[error]") {
            format!("✗ ERROR: {e}")
        } else if let Some(w) = rest.strip_prefix("##[warning]") {
            format!("⚠ WARNING: {w}")
        } else if let Some(nt) = rest.strip_prefix("##[notice]") {
            format!("ℹ {nt}")
        } else {
            rest
        };
        if time.is_empty() {
            out.push_str(&rest);
        } else {
            out.push_str(time);
            out.push(' ');
            out.push_str(&rest);
        }
        out.push('\n');
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn job_section(dir: &Path, job: &Job) -> String {
    let mut out = String::new();
    out.push_str(&format!("═══ JOB: {}  [{}]  {}\n", job.name, status_text(&job.status, &job.conclusion), duration(&job.started, &job.completed)));
    if !job.url.is_empty() {
        out.push_str(&format!("{}\n", job.url));
    }
    out.push_str("\nSteps:\n");
    for st in &job.steps {
        out.push_str(&format!("  {} {:>2}. {}  ({})\n", step_mark(st), st.number, st.name, status_text(&st.status, &st.conclusion)));
    }
    out.push('\n');
    if is_active(&job.status) {
        out.push_str("(El log completo estará disponible cuando el job termine; refresca para actualizar.)\n");
    } else {
        match gh(dir, &["api", &format!("repos/{{owner}}/{{repo}}/actions/jobs/{}/logs", job.id)]) {
            Ok(raw) => out.push_str(&clean_log(&raw)),
            Err(e) => out.push_str(&format!("(No se pudo descargar el log: {e})\n")),
        }
    }
    out.push('\n');
    out
}

fn run_header(run: &Run, jobs: &[Job]) -> String {
    let (done, pending, ok, failed) = job_counts(jobs);
    format!(
        "{} #{} — {}\nEstado: {}\nRama: {} · evento: {} · hace {}\nJobs: {}/{} terminados · {} pendientes · {} exitosos · {} fallidos\n{}\n\n",
        run.workflow,
        run.number,
        run.title,
        status_text(&run.status, &run.conclusion),
        run.branch,
        run.event,
        ago(&run.created),
        done,
        jobs.len(),
        pending,
        ok,
        failed,
        run.url
    )
}

fn log_path(repo: &str, name: &str) -> PathBuf {
    let safe: String = repo.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    workspace_state::state_dir().join("actions").join(safe).join(name)
}

impl ActionsPanel {
    pub fn new() -> Self {
        Self {
            repos: Vec::new(),
            jobs: HashMap::new(),
            expanded_runs: HashSet::new(),
            expanded_repos: HashSet::new(),
            expanded: true,
            visible: false,
            busy: HashSet::new(),
            message: None,
            poll: None,
        }
    }

    /// Any repo of the project with workflows (else the section is hidden).
    pub fn has_any(&self) -> bool {
        !self.repos.is_empty()
    }

    pub fn set_repos(&mut self, repos: Vec<(String, PathBuf)>, cx: &mut Context<Self>) {
        let with: Vec<(String, PathBuf)> = repos.into_iter().filter(|(_, p)| has_workflows(p)).collect();
        let same = with.len() == self.repos.len() && with.iter().zip(&self.repos).all(|((_, p), r)| *p == r.path);
        if same {
            return;
        }
        self.repos = with.into_iter().map(|(name, path)| RepoActions { name, path, ..Default::default() }).collect();
        self.jobs.clear();
        self.expanded_runs.clear();
        self.message = None;
        if self.visible && self.expanded {
            self.refresh(cx);
        }
        self.ensure_poll(cx);
        cx.notify();
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible && self.expanded && self.repos.iter().any(|r| !r.loaded && !r.loading) {
            self.refresh(cx);
        }
        self.ensure_poll(cx);
    }

    fn any_active(&self) -> bool {
        self.repos.iter().any(|r| r.runs.iter().any(|x| x.active()))
    }

    fn ensure_poll(&mut self, cx: &mut Context<Self>) {
        if !self.visible || !self.expanded || self.repos.is_empty() {
            self.poll = None;
            return;
        }
        if self.poll.is_some() {
            return;
        }
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let wait = match this.update(cx, |this, _| if this.any_active() { ACTIVE_POLL } else { IDLE_POLL }) {
                    Ok(w) => w,
                    Err(_) => break,
                };
                gpui::Timer::after(wait).await;
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Reload run lists (and the jobs of active or opened runs).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        for ix in 0..self.repos.len() {
            self.refresh_repo(ix, cx);
        }
    }

    fn refresh_repo(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.get_mut(ix) else { return };
        if repo.loading {
            return;
        }
        repo.loading = true;
        let dir = repo.path.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let res = cx.background_executor().spawn(async move { fetch_runs(&d) }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(repo) = this.repos.iter_mut().find(|r| r.path == dir) else { return };
                repo.loading = false;
                repo.loaded = true;
                match res {
                    Ok((branch, runs)) => {
                        repo.error = None;
                        repo.branch = branch;
                        repo.runs = runs;
                    }
                    Err(e) => repo.error = Some(e),
                }
                let want: Vec<u64> = repo.runs.iter().filter(|r| r.active() || this.expanded_runs.contains(&r.id)).map(|r| r.id).collect();
                for id in want {
                    this.load_jobs(dir.clone(), id, cx);
                }
                this.ensure_poll(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn load_jobs(&mut self, dir: PathBuf, run: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { fetch_jobs(&dir, run) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(jobs) = res {
                    this.jobs.insert(run, jobs);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn find_run(&self, id: u64) -> Option<(&RepoActions, &Run)> {
        self.repos.iter().find_map(|r| r.runs.iter().find(|x| x.id == id).map(|x| (r, x)))
    }

    fn toggle_run(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.expanded_runs.remove(&id) {
            self.expanded_runs.insert(id);
            if let Some((repo, _)) = self.find_run(id) {
                let dir = repo.path.clone();
                self.load_jobs(dir, id, cx);
            }
        }
        cx.notify();
    }

    /// `gh run cancel|rerun`, then refresh that repo a moment later.
    fn run_command(&mut self, id: u64, verb: &'static str, cx: &mut Context<Self>) {
        let Some((repo, _)) = self.find_run(id) else { return };
        let dir = repo.path.clone();
        self.busy.insert(id);
        self.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let res = cx.background_executor().spawn(async move { gh(&d, &["run", verb, &id.to_string()]) }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy.remove(&id);
                if let Err(e) = res {
                    this.message = Some(e);
                }
                cx.notify();
            });
            gpui::Timer::after(Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(ix) = this.repos.iter().position(|r| r.path == dir) {
                    this.refresh_repo(ix, cx);
                }
            });
        })
        .detach();
    }

    /// Write the run's (or one job's) detail + log to a file and ask to open it.
    fn open_log(&mut self, run_id: u64, job_id: Option<u64>, cx: &mut Context<Self>) {
        let Some((repo, run)) = self.find_run(run_id) else { return };
        let (dir, name, run) = (repo.path.clone(), repo.name.clone(), run.clone());
        let key = job_id.unwrap_or(run_id);
        self.busy.insert(key);
        self.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let jobs = fetch_jobs(&dir, run.id)?;
                    let mut text = run_header(&run, &jobs);
                    let file = match job_id {
                        Some(jid) => {
                            let job = jobs.iter().find(|j| j.id == jid).ok_or("job no encontrado")?;
                            text.push_str(&job_section(&dir, job));
                            log_path(&name, &format!("run-{}-job-{jid}.log", run.id))
                        }
                        None => {
                            for job in &jobs {
                                text.push_str(&job_section(&dir, job));
                            }
                            log_path(&name, &format!("run-{}.log", run.id))
                        }
                    };
                    if let Some(p) = file.parent() {
                        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(&file, text).map_err(|e| e.to_string())?;
                    Ok::<_, String>((file, jobs))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy.remove(&key);
                match res {
                    Ok((file, jobs)) => {
                        this.jobs.insert(run_id, jobs);
                        cx.emit(ActionsPanelEvent::OpenLog(file));
                    }
                    Err(e) => this.message = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let active = self.repos.iter().flat_map(|r| &r.runs).filter(|r| r.active()).count();
        let loading = self.repos.iter().any(|r| r.loading);
        div()
            .id("actions-header")
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .flex_none()
            .pl(px(12.))
            .pr(px(8.))
            .border_t_1()
            .border_color(theme::rgb(theme::BORDER))
            .cursor_pointer()
            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.expanded = !this.expanded;
                if this.expanded && this.repos.iter().any(|r| !r.loaded) {
                    this.refresh(cx);
                }
                this.ensure_poll(cx);
                cx.notify();
            }))
            .child(gpui::svg().path(if self.expanded { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(10.)).text_color(theme::rgb(theme::TEXT_FAINT)))
            .child(div().flex_1().text_size(px(11.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child("GITHUB ACTIONS"))
            .when(active > 0, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .px(px(6.))
                        .rounded_full()
                        .bg(theme::rgb(0x3a3220))
                        .text_size(px(10.5))
                        .text_color(theme::rgb(theme::MODIFIED))
                        .child(div().size(px(6.)).rounded_full().bg(theme::rgb(theme::MODIFIED)))
                        .child(format!("{active} en curso")),
                )
            })
            .when(loading, |d| d.child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("…")))
            .child(icon_action("actions-refresh", "icons/fetch.svg", "Refrescar").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.expanded = true;
                this.refresh(cx);
                this.ensure_poll(cx);
            })))
            .into_any_element()
    }

    fn render_run(&self, run: &Run, indent: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = run.id;
        let (ic, color) = status_look(&run.status, &run.conclusion);
        let open = self.expanded_runs.contains(&id);
        let busy = self.busy.contains(&id);
        let jobs = self.jobs.get(&id);
        let meta = match jobs.filter(|j| run.active() && !j.is_empty()) {
            Some(jobs) => {
                let (done, ..) = job_counts(jobs);
                format!("{done}/{} · {}", jobs.len(), ago(&run.created))
            }
            None => format!("#{} · {}", run.number, ago(&run.created)),
        };
        let tip: SharedString = format!("#{} {}\n{} · {} · {}\n{}", run.number, run.title, run.branch, run.event, ago(&run.created), status_text(&run.status, &run.conclusion)).into();
        let url = run.url.clone();
        let group = SharedString::from(format!("run-g-{id}"));
        let mut col = div().flex().flex_col();
        let mut actions = div().flex().flex_none().items_center().invisible().group_hover(group.clone(), |s| s.visible());
        actions = actions.child(icon_action(SharedString::from(format!("run-log-{id}")), "icons/doc.svg", "Ver logs / detalle").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            this.open_log(id, None, cx);
        })));
        if run.active() {
            actions = actions.child(icon_action(SharedString::from(format!("run-cancel-{id}")), "icons/action-stop.svg", "Abortar").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.run_command(id, "cancel", cx);
            })));
        } else {
            actions = actions.child(icon_action(SharedString::from(format!("run-rerun-{id}")), "icons/action-rerun.svg", "Re-ejecutar").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.run_command(id, "rerun", cx);
            })));
        }
        actions = actions.child(icon_action(SharedString::from(format!("run-web-{id}")), "icons/external.svg", "Abrir en GitHub").on_click(move |_: &ClickEvent, _, cx| {
            cx.stop_propagation();
            let _ = Command::new("open").arg(&url).spawn();
        }));
        let row = div()
            .id(SharedString::from(format!("run-{id}")))
            .group(group.clone())
            .relative()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(26.))
            .pl(px(if indent { 22. } else { 12. }))
            .pr(px(4.))
            .mx(px(4.))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|st| st.bg(theme::rgb(theme::HOVER)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_run(id, cx)))
            .tooltip(move |_, cx| cx.new(|_| DynTooltip(tip.clone())).into())
            .child(gpui::svg().path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(9.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).child(run.workflow.clone()))
            .child(gpui::svg().path(ic).size(px(13.)).flex_none().text_color(theme::rgb(color)))
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(theme::rgb(if run.active() { theme::MODIFIED } else { theme::TEXT_FAINT }))
                    .group_hover(group.clone(), |s| s.invisible())
                    .child(if busy { "…".to_string() } else { meta }),
            )
            .child(div().absolute().right(px(4.)).top_0().bottom_0().flex().items_center().rounded(px(5.)).bg(theme::rgb(theme::HOVER)).invisible().group_hover(group, |s| s.visible()).child(actions));
        col = col.child(row);
        if open {
            match jobs {
                None => col = col.child(div().pl(px(38.)).h(px(22.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Cargando jobs…")),
                Some(jobs) if jobs.is_empty() => col = col.child(div().pl(px(38.)).h(px(22.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Sin jobs")),
                Some(jobs) => {
                    let (done, pending, ok, failed) = job_counts(jobs);
                    col = col.child(
                        div()
                            .pl(px(38.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .text_size(px(10.5))
                            .text_color(theme::rgb(theme::TEXT_MUTED))
                            .child(format!("{done}/{} terminados · {pending} pendientes · {ok} exitosos · {failed} fallidos", jobs.len())),
                    );
                    for job in jobs {
                        let jid = job.id;
                        let (jic, jcolor) = status_look(&job.status, &job.conclusion);
                        let running_step = job.steps.iter().find(|s| s.status == "in_progress").map(|s| s.name.clone());
                        let detail = match running_step {
                            Some(st) => format!("{} · {}", duration(&job.started, &job.completed), st),
                            None => duration(&job.started, &job.completed),
                        };
                        let jbusy = self.busy.contains(&jid);
                        col = col.child(
                            div()
                                .id(SharedString::from(format!("job-{jid}")))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .h(px(24.))
                                .pl(px(36.))
                                .pr(px(8.))
                                .mx(px(4.))
                                .rounded(px(5.))
                                .cursor_pointer()
                                .hover(|st| st.bg(theme::rgb(theme::HOVER)))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_log(id, Some(jid), cx)))
                                .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Ver detalle y log del job")).into())
                                .child(gpui::svg().path(jic).size(px(12.)).flex_none().text_color(theme::rgb(jcolor)))
                                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).child(job.name.clone()))
                                .child(div().flex_none().max_w(px(110.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(if jbusy { "…".to_string() } else { detail })),
                        );
                    }
                }
            }
        }
        col.into_any_element()
    }
}

impl Render for ActionsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().flex().flex_col().size_full().bg(theme::rgb(theme::SIDEBAR_BG)).child(self.render_header(cx));
        if !self.expanded {
            return root;
        }
        let mut body = div().id("actions-body").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().pb(px(8.));
        if let Some(m) = &self.message {
            body = body.child(div().px(px(14.)).py(px(4.)).text_size(px(11.)).text_color(theme::rgb(theme::DELETED)).child(m.clone()));
        }
        let multi = self.repos.len() > 1;
        for ix in 0..self.repos.len() {
            let repo = &self.repos[ix];
            let path = repo.path.clone();
            let collapsed = multi && !self.expanded_repos.contains(&path);
            let latest = latest_per_workflow(&repo.runs);
            if multi {
                let active = latest.iter().filter(|r| r.active()).count();
                let summary = (active == 0 && !latest.is_empty()).then(|| run_summary(&latest));
                body = body.child(
                    div()
                        .id(("actions-repo", ix))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .h(px(24.))
                        .pl(px(12.))
                        .pr(px(8.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if !this.expanded_repos.remove(&path) {
                                this.expanded_repos.insert(path.clone());
                            }
                            cx.notify();
                        }))
                        .child(gpui::svg().path(if collapsed { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" }).size(px(10.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                        .child(gpui::svg().path("icons/repo.svg").size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).font_weight(gpui::FontWeight::MEDIUM).child(repo.name.clone()))
                        .when(!repo.branch.is_empty(), |d| {
                            d.child(div().flex_shrink_0().max_w(px(140.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(repo.branch.clone()))
                        })
                        .when(active > 0, |d| d.child(div().text_size(px(10.5)).text_color(theme::rgb(theme::MODIFIED)).child(format!("{active} en curso"))))
                        .when_some(summary, |d, (ok, total, failed)| {
                            let (icon, color) = if failed > 0 { ("icons/close.svg", theme::DELETED) } else { ("icons/check.svg", theme::ADDED) };
                            d.child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(3.))
                                    .px(px(5.))
                                    .h(px(16.))
                                    .rounded(px(8.))
                                    .bg(theme::rgb(theme::SELECTED))
                                    .child(gpui::svg().path(icon).size(px(9.)).text_color(theme::rgb(color)))
                                    .child(div().text_size(px(10.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{ok}/{total}"))),
                            )
                        }),
                );
            }
            if collapsed {
                continue;
            }
            if let Some(e) = &repo.error {
                body = body.child(div().px(px(22.)).py(px(4.)).text_size(px(11.)).text_color(theme::rgb(theme::DELETED)).child(e.clone()));
            }
            if !repo.loaded {
                body = body.child(div().px(px(22.)).h(px(24.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Cargando ejecuciones…"));
                continue;
            }
            if !multi && !repo.branch.is_empty() {
                body = body.child(div().px(px(14.)).h(px(20.)).text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("Rama {}", repo.branch)));
            }
            if repo.runs.is_empty() && repo.error.is_none() {
                let msg = if repo.branch.is_empty() { "Sin ejecuciones".to_string() } else { format!("Sin ejecuciones en {}", repo.branch) };
                body = body.child(div().px(px(22.)).h(px(24.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(msg));
                continue;
            }
            for run in &latest {
                body = body.child(self.render_run(run, multi, cx));
            }
        }
        root = root.child(body);
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_time() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-11-23T04:19:52Z"), Some(1795407592));
    }

    #[test]
    fn cleans_log_lines() {
        let raw = "\u{feff}2026-11-19T06:43:19.2929254Z ##[group]Runner Image\n2026-11-19T06:43:19.2930284Z \u{1b}[36mHello\u{1b}[0m\n2026-11-19T06:43:19.2930284Z ##[endgroup]\n2026-11-19T06:43:20.0000000Z ##[error]Boom\n";
        assert_eq!(clean_log(raw), "06:43:19 ▸ Runner Image\n06:43:19 Hello\n06:43:20 ✗ ERROR: Boom\n");
    }

    #[test]
    fn counts_jobs() {
        let j = |st: &str, c: &str| Job { id: 0, name: String::new(), status: st.into(), conclusion: c.into(), started: String::new(), completed: String::new(), url: String::new(), steps: vec![] };
        assert_eq!(job_counts(&[j("completed", "success"), j("in_progress", ""), j("queued", ""), j("completed", "failure")]), (2, 2, 1, 1));
    }
}
