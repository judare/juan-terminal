//! Release tags section under the file tree (below GitHub Actions). Lists the tags of each
//! repo of the project (newest version first), whether they're on `origin` and have a GitHub
//! release, and creates the next `vMAJOR.FEATURES.FIXES` tag (annotated, pushed, with its
//! GitHub release).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use gpui::{ClickEvent, ClipboardItem, Context, Entity, EventEmitter, Focusable, PromptLevel, Render, SharedString, Subscription, Window, div, prelude::*, px};
use serde_json::Value;

use crate::git_panel::{DynTooltip, icon_action};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

const TAGS_SHOWN: usize = 40;

pub enum TagsPanelEvent {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bump {
    Major,
    Feature,
    Fix,
}

impl Bump {
    fn label(self) -> &'static str {
        match self {
            Bump::Major => "mayor",
            Bump::Feature => "feature",
            Bump::Fix => "corrección",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `v3.4.31` / `3.4.31` → Version (anything else, e.g. `v1.2.3-rc1`, is not a release).
    pub fn parse(tag: &str) -> Option<Self> {
        let t = tag.strip_prefix('v').or_else(|| tag.strip_prefix('V')).unwrap_or(tag);
        let mut it = t.split('.');
        let p = |s: Option<&str>| s.filter(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit())).and_then(|x| x.parse::<u64>().ok());
        let v = Version(p(it.next())?, p(it.next())?, p(it.next())?);
        it.next().is_none().then_some(v)
    }

    pub fn bump(self, b: Bump) -> Self {
        match b {
            Bump::Major => Version(self.0 + 1, 0, 0),
            Bump::Feature => Version(self.0, self.1 + 1, 0),
            Bump::Fix => Version(self.0, self.1, self.2 + 1),
        }
    }

    pub fn tag(self) -> String {
        format!("v{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Tag {
    name: String,
    commit: String,
    date: String,
    subject: String,
    body: String,
    annotated: bool,
}

#[derive(Clone, Debug, Default)]
struct Release {
    draft: bool,
    prerelease: bool,
    latest: bool,
}

#[derive(Default)]
struct Snapshot {
    tags: Vec<Tag>,
    head: String,
    /// Tags on origin (None: no origin / unreachable).
    remote: Option<HashSet<String>>,
    /// GitHub releases by tag (None: not GitHub or `gh` unavailable).
    releases: Option<HashMap<String, Release>>,
    web: Option<String>,
}

struct RepoTags {
    name: String,
    path: PathBuf,
    snap: Snapshot,
    loaded: bool,
    loading: bool,
    error: Option<String>,
    /// New-tag form open with this bump.
    creating: Option<Bump>,
    input: Entity<TextInput>,
    _sub: Subscription,
}

pub struct TagsPanel {
    repos: Vec<RepoTags>,
    expanded_repos: HashSet<PathBuf>,
    /// Section open (else only its header shows).
    pub expanded: bool,
    busy: HashSet<String>,
    message: Option<(String, bool)>,
}

impl EventEmitter<TagsPanelEvent> for TagsPanel {}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").args(args).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("git {} falló", args.first().unwrap_or(&"")) } else { err })
    }
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

fn has_origin(dir: &Path) -> bool {
    git(dir, &["remote", "get-url", "origin"]).is_ok()
}

fn parse_tags(out: &str) -> Vec<Tag> {
    out.split('\u{1e}')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            if rec.is_empty() {
                return None;
            }
            let f: Vec<&str> = rec.split('\u{1f}').collect();
            (f.len() >= 6).then(|| Tag {
                name: f[0].to_string(),
                annotated: f[1] == "tag",
                date: f[2].to_string(),
                commit: f[3].to_string(),
                subject: f[4].to_string(),
                body: f[5].trim().to_string(),
            })
        })
        .collect()
}

fn fetch(dir: &Path) -> Result<Snapshot, String> {
    let fmt = "--format=%(refname:short)%1f%(objecttype)%1f%(creatordate:iso-strict)%1f%(if)%(*objectname)%(then)%(*objectname:short)%(else)%(objectname:short)%(end)%1f%(contents:subject)%1f%(contents:body)%1e";
    let tags = parse_tags(&git(dir, &["for-each-ref", "refs/tags", "--sort=-v:refname", fmt])?);
    let head = git(dir, &["rev-parse", "--short", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default();
    let web = crate::launcher::web_url(dir);
    let remote = has_origin(dir)
        .then(|| git(dir, &["ls-remote", "--tags", "origin"]).ok())
        .flatten()
        .map(|out| out.lines().filter_map(|l| l.split('\t').nth(1)?.strip_prefix("refs/tags/").map(|t| t.trim_end_matches("^{}").to_string())).collect());
    let releases = web.as_ref().filter(|w| w.contains("github.com")).and_then(|_| {
        let out = gh(dir, &["release", "list", "-L", "200", "--json", "tagName,isDraft,isPrerelease,isLatest"]).ok()?;
        let v: Value = serde_json::from_str(&out).ok()?;
        Some(
            v.as_array()?
                .iter()
                .map(|r| {
                    let b = |k: &str| r.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
                    (r.get("tagName").and_then(|x| x.as_str()).unwrap_or("").to_string(), Release { draft: b("isDraft"), prerelease: b("isPrerelease"), latest: b("isLatest") })
                })
                .collect(),
        )
    });
    Ok(Snapshot { tags, head, remote, releases, web })
}

/// Highest `vX.Y.Z` among the tags.
fn latest_version(tags: &[Tag]) -> Option<Version> {
    tags.iter().filter_map(|t| Version::parse(&t.name)).max()
}

fn short_date(iso: &str) -> String {
    iso.get(..10).unwrap_or(iso).to_string()
}

impl TagsPanel {
    pub fn new() -> Self {
        Self { repos: Vec::new(), expanded_repos: HashSet::new(), expanded: false, busy: HashSet::new(), message: None }
    }

    pub fn has_any(&self) -> bool {
        !self.repos.is_empty()
    }

    pub fn set_repos(&mut self, repos: Vec<(String, PathBuf)>, cx: &mut Context<Self>) {
        let with: Vec<(String, PathBuf)> = repos.into_iter().filter(|(_, p)| p.join(".git").exists()).collect();
        let same = with.len() == self.repos.len() && with.iter().zip(&self.repos).all(|((_, p), r)| *p == r.path);
        if same {
            return;
        }
        self.repos = with
            .into_iter()
            .map(|(name, path)| {
                let input = cx.new(|cx| TextInput::new("Descripción del release (Enter publica)", cx));
                let p = path.clone();
                let sub = cx.subscribe(&input, move |this, input, e, cx| match e {
                    TextInputEvent::Submit { .. } => {
                        let msg = input.read(cx).content.trim().to_string();
                        this.create_tag(&p, msg, cx);
                    }
                    TextInputEvent::Cancel => {
                        if let Some(r) = this.repos.iter_mut().find(|r| r.path == p) {
                            r.creating = None;
                            cx.notify();
                        }
                    }
                    TextInputEvent::Changed => {}
                });
                RepoTags { name, path, snap: Snapshot::default(), loaded: false, loading: false, error: None, creating: None, input, _sub: sub }
            })
            .collect();
        self.message = None;
        if self.expanded {
            self.refresh(cx);
        }
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        for ix in 0..self.repos.len() {
            let path = self.repos[ix].path.clone();
            self.refresh_repo(&path, cx);
        }
    }

    fn refresh_repo(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.iter_mut().find(|r| r.path == path) else { return };
        if repo.loading {
            return;
        }
        repo.loading = true;
        let dir = repo.path.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let res = cx.background_executor().spawn(async move { fetch(&d) }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(repo) = this.repos.iter_mut().find(|r| r.path == dir) else { return };
                repo.loading = false;
                repo.loaded = true;
                match res {
                    Ok(s) => {
                        repo.error = None;
                        repo.snap = s;
                    }
                    Err(e) => repo.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a blocking op for `tag` in the background, then show its result and refresh.
    fn run(&mut self, path: PathBuf, key: String, cx: &mut Context<Self>, op: impl FnOnce(&Path) -> Result<String, String> + Send + 'static) {
        self.busy.insert(key.clone());
        self.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let d = path.clone();
            let res = cx.background_executor().spawn(async move { op(&d) }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy.remove(&key);
                this.message = Some(match res {
                    Ok(m) => (m, false),
                    Err(e) => (e, true),
                });
                this.refresh_repo(&path, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn next_tag(&self, path: &Path, bump: Bump) -> String {
        let v = self.repos.iter().find(|r| r.path == path).and_then(|r| latest_version(&r.snap.tags)).unwrap_or_default();
        v.bump(bump).tag()
    }

    fn open_form(&mut self, path: &Path, bump: Bump, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repos.iter_mut().find(|r| r.path == path) else { return };
        repo.creating = Some(bump);
        let input = repo.input.clone();
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    fn create_tag(&mut self, path: &Path, msg: String, cx: &mut Context<Self>) {
        let Some(bump) = self.repos.iter().find(|r| r.path == path).and_then(|r| r.creating) else { return };
        let tag = self.next_tag(path, bump);
        if let Some(r) = self.repos.iter_mut().find(|r| r.path == path) {
            r.creating = None;
            r.input.update(cx, |i, cx| i.set_text("", cx));
        }
        let msg = if msg.is_empty() { format!("Release {tag}") } else { msg };
        let t = tag.clone();
        self.run(path.to_path_buf(), tag.clone(), cx, move |d| {
            git(d, &["tag", "-a", &t, "-m", &msg])?;
            if !has_origin(d) {
                return Ok(format!("Tag {t} creado (sin remoto origin)"));
            }
            git(d, &["push", "origin", &format!("refs/tags/{t}")]).map_err(|e| format!("Tag {t} creado, pero el push falló: {e}"))?;
            if !crate::launcher::web_url(d).is_some_and(|w| w.contains("github.com")) {
                return Ok(format!("Tag {t} creado y subido (origin no es GitHub: sin release)"));
            }
            let title = format!("{t} — {}", msg.lines().next().unwrap_or_default());
            match gh(d, &["release", "create", &t, "--verify-tag", "--latest", "--title", &title, "--notes", &msg]) {
                Ok(_) => Ok(format!("Tag y release {t} publicados")),
                Err(e) => Err(format!("Tag {t} creado y subido, pero el release falló: {e}")),
            }
        });
    }

    fn push_tag(&mut self, path: PathBuf, tag: String, cx: &mut Context<Self>) {
        let t = tag.clone();
        self.run(path, tag, cx, move |d| git(d, &["push", "origin", &format!("refs/tags/{t}")]).map(|_| format!("Tag {t} subido")));
    }

    fn create_release(&mut self, path: PathBuf, tag: String, cx: &mut Context<Self>) {
        let t = tag.clone();
        self.run(path, tag, cx, move |d| {
            let out = gh(d, &["release", "create", &t, "--verify-tag", "--notes-from-tag", "--title", &t])
                .or_else(|_| gh(d, &["release", "create", &t, "--verify-tag", "--generate-notes", "--title", &t]))?;
            Ok(format!("Release {t} creado {}", out.trim()))
        });
    }

    fn confirm_delete(&mut self, path: PathBuf, tag: String, window: &mut Window, cx: &mut Context<Self>) {
        let (on_remote, has_release) = self
            .repos
            .iter()
            .find(|r| r.path == path)
            .map(|r| (r.snap.remote.as_ref().is_some_and(|s| s.contains(&tag)), r.snap.releases.as_ref().is_some_and(|s| s.contains_key(&tag))))
            .unwrap_or_default();
        let detail = match (on_remote, has_release) {
            (_, true) => "Se borrará el release de GitHub, el tag en origin y el tag local.",
            (true, false) => "Se borrará el tag local y en origin.",
            _ => "Se borrará el tag local.",
        };
        let answer = window.prompt(PromptLevel::Critical, &format!("¿Borrar el tag “{tag}”?"), Some(detail), &["Borrar", "Cancelar"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let t = tag.clone();
                this.run(path, tag, cx, move |d| {
                    if has_release {
                        gh(d, &["release", "delete", &t, "--yes", "--cleanup-tag"])?;
                    } else if on_remote {
                        git(d, &["push", "origin", "--delete", &format!("refs/tags/{t}")])?;
                    }
                    let _ = git(d, &["tag", "-d", &t]);
                    Ok(format!("Tag {t} borrado"))
                });
            });
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let loading = self.repos.iter().any(|r| r.loading);
        let latest = (self.repos.len() == 1).then(|| latest_version(&self.repos[0].snap.tags)).flatten();
        div()
            .id("tags-header")
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
                if this.expanded {
                    this.refresh(cx);
                }
                cx.notify();
            }))
            .child(gpui::svg().path(if self.expanded { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(10.)).text_color(theme::rgb(theme::TEXT_FAINT)))
            .child(div().flex_1().text_size(px(11.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child("RELEASE TAGS"))
            .when_some(latest, |d, v| d.child(div().px(px(6.)).rounded_full().bg(theme::rgb(theme::SELECTED)).text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_MUTED)).child(v.tag())))
            .when(loading, |d| d.child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("…")))
            .child(icon_action("tags-refresh", "icons/fetch.svg", "Refrescar").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.expanded = true;
                this.refresh(cx);
            })))
            .into_any_element()
    }

    fn render_new(&self, ix: usize, repo: &RepoTags, cx: &mut Context<Self>) -> gpui::AnyElement {
        let latest = latest_version(&repo.snap.tags).unwrap_or_default();
        let mut col = div().flex().flex_col().px(px(12.)).py(px(4.)).gap(px(4.));
        let mut chips = div().flex().items_center().gap(px(4.)).child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Nuevo:"));
        for (bx, bump) in [Bump::Fix, Bump::Feature, Bump::Major].into_iter().enumerate() {
            let sel = repo.creating == Some(bump);
            let path = repo.path.clone();
            let tip: SharedString = format!("Nuevo tag de {} desde HEAD ({})", bump.label(), repo.snap.head).into();
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("tag-new-{ix}-{bx}")))
                    .px(px(6.))
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(theme::rgb(if sel { theme::ACCENT } else { theme::BORDER }))
                    .bg(theme::rgb(if sel { theme::SELECTED } else { theme::SIDEBAR_BG }))
                    .text_size(px(10.5))
                    .text_color(theme::rgb(if sel { theme::TEXT } else { theme::TEXT_MUTED }))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                    .tooltip(move |_, cx| cx.new(|_| DynTooltip(tip.clone())).into())
                    .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.open_form(&path, bump, w, cx)))
                    .child(latest.bump(bump).tag()),
            );
        }
        col = col.child(chips);
        if let Some(bump) = repo.creating {
            col = col
                .child(
                    div()
                        .flex()
                        .px(px(6.))
                        .py(px(2.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(theme::rgb(theme::ACCENT))
                        .bg(theme::rgb(theme::EDITOR_BG))
                        .child(repo.input.clone()),
                )
                .child(div().text_size(px(10.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("{} ({}) · Enter crea tag + release · Esc cancela", latest.bump(bump).tag(), bump.label())));
        }
        col.into_any_element()
    }

    fn render_tag(&self, ix: usize, repo: &RepoTags, tag: &Tag, cx: &mut Context<Self>) -> gpui::AnyElement {
        let name = tag.name.clone();
        let busy = self.busy.contains(&name);
        let on_remote = repo.snap.remote.as_ref().map(|s| s.contains(&name));
        let release = repo.snap.releases.as_ref().map(|r| r.get(&name).cloned());
        let at_head = !repo.snap.head.is_empty() && tag.commit == repo.snap.head;
        let mut tip = format!("{}  ·  {}  ·  {}", tag.name, short_date(&tag.date), tag.commit);
        if !tag.subject.is_empty() && tag.annotated {
            tip.push_str(&format!("\n\n{}", tag.subject));
            if !tag.body.is_empty() {
                tip.push_str(&format!("\n{}", tag.body));
            }
        }
        if !tag.annotated {
            tip.push_str("\n(tag ligero)");
        }
        match on_remote {
            Some(false) => tip.push_str("\nSolo local: no está en origin"),
            Some(true) => {}
            None => {}
        }
        if let Some(Some(r)) = &release {
            tip.push_str(if r.draft { "\nRelease (borrador)" } else if r.prerelease { "\nPre-release" } else { "\nRelease publicado" });
        }
        let tip: SharedString = tip.into();
        let group = SharedString::from(format!("tag-g-{ix}-{name}"));
        let path = repo.path.clone();
        let mut actions = div().flex().flex_none().items_center();
        if on_remote == Some(false) {
            let (p, n) = (path.clone(), name.clone());
            actions = actions.child(icon_action(SharedString::from(format!("tag-push-{ix}-{name}")), "icons/push.svg", "Subir a origin").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.push_tag(p.clone(), n.clone(), cx);
            })));
        }
        if matches!(release, Some(None)) && on_remote == Some(true) {
            let (p, n) = (path.clone(), name.clone());
            actions = actions.child(icon_action(SharedString::from(format!("tag-rel-{ix}-{name}")), "icons/package.svg", "Crear release en GitHub").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.create_release(p.clone(), n.clone(), cx);
            })));
        }
        {
            let n = name.clone();
            actions = actions.child(icon_action(SharedString::from(format!("tag-copy-{ix}-{name}")), "icons/copy.svg", "Copiar nombre").on_click(move |_: &ClickEvent, _, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(ClipboardItem::new_string(n.clone()));
            }));
        }
        if let Some(web) = repo.snap.web.clone().filter(|_| on_remote != Some(false)) {
            let url = if matches!(release, Some(Some(_))) { format!("{web}/releases/tag/{name}") } else { format!("{web}/tree/{name}") };
            actions = actions.child(icon_action(SharedString::from(format!("tag-web-{ix}-{name}")), "icons/external.svg", "Abrir en GitHub").on_click(move |_: &ClickEvent, _, cx| {
                cx.stop_propagation();
                let _ = Command::new("open").arg(&url).spawn();
            }));
        }
        {
            let (p, n) = (path.clone(), name.clone());
            actions = actions.child(icon_action(SharedString::from(format!("tag-del-{ix}-{name}")), "icons/trash.svg", "Borrar tag").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                cx.stop_propagation();
                this.confirm_delete(p.clone(), n.clone(), w, cx);
            })));
        }
        let (badge, badge_color) = match (&release, on_remote) {
            (Some(Some(r)), _) if r.latest => (Some("latest"), theme::ADDED),
            (Some(Some(r)), _) if r.draft => (Some("draft"), theme::TEXT_FAINT),
            (Some(Some(r)), _) if r.prerelease => (Some("pre"), theme::MODIFIED),
            (Some(Some(_)), _) => (Some("release"), theme::TEXT_MUTED),
            (_, Some(false)) => (Some("local"), theme::MODIFIED),
            _ => (None, 0),
        };
        div()
            .id(SharedString::from(format!("tag-{ix}-{name}")))
            .group(group.clone())
            .relative()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(24.))
            .pl(px(if self.repos.len() > 1 { 22. } else { 12. }))
            .pr(px(6.))
            .mx(px(4.))
            .rounded(px(5.))
            .hover(|st| st.bg(theme::rgb(theme::HOVER)))
            .tooltip(move |_, cx| cx.new(|_| DynTooltip(tip.clone())).into())
            .child(gpui::svg().path("icons/tag.svg").size(px(12.)).flex_none().text_color(theme::rgb(if at_head { theme::ACCENT } else { theme::TEXT_FAINT })))
            .child(div().flex_none().text_size(px(12.)).font_weight(gpui::FontWeight::MEDIUM).child(name.clone()))
            .when_some(badge, |d, b| d.child(div().flex_none().px(px(4.)).rounded(px(3.)).bg(theme::rgb(theme::SELECTED)).text_size(px(9.5)).text_color(theme::rgb(badge_color)).child(b)))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(if tag.annotated { tag.subject.clone() } else { String::new() }))
            .child(div().flex_none().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).group_hover(group.clone(), |s| s.invisible()).child(if busy { "…".to_string() } else { short_date(&tag.date) }))
            .child(div().absolute().right(px(4.)).top_0().bottom_0().flex().items_center().rounded(px(5.)).bg(theme::rgb(theme::HOVER)).invisible().group_hover(group, |s| s.visible()).child(actions))
            .into_any_element()
    }
}

impl Render for TagsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div().flex().flex_col().size_full().bg(theme::rgb(theme::SIDEBAR_BG)).child(self.render_header(cx));
        if !self.expanded {
            return root;
        }
        let mut body = div().id("tags-body").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().pb(px(8.));
        if let Some((m, err)) = &self.message {
            body = body.child(
                div()
                    .id("tags-msg")
                    .px(px(14.))
                    .py(px(4.))
                    .text_size(px(11.))
                    .text_color(theme::rgb(if *err { theme::DELETED } else { theme::ADDED }))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.message = None;
                        cx.notify();
                    }))
                    .child(m.clone()),
            );
        }
        let multi = self.repos.len() > 1;
        for ix in 0..self.repos.len() {
            let repo = &self.repos[ix];
            let path = repo.path.clone();
            let collapsed = multi && !self.expanded_repos.contains(&path);
            if multi {
                let latest = latest_version(&repo.snap.tags);
                body = body.child(
                    div()
                        .id(("tags-repo", ix))
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
                        .when_some(latest, |d, v| d.child(div().flex_none().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(v.tag()))),
                );
            }
            if collapsed {
                continue;
            }
            if let Some(e) = &repo.error {
                body = body.child(div().px(px(22.)).py(px(4.)).text_size(px(11.)).text_color(theme::rgb(theme::DELETED)).child(e.clone()));
            }
            if !repo.loaded {
                body = body.child(div().px(px(22.)).h(px(24.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Cargando tags…"));
                continue;
            }
            body = body.child(self.render_new(ix, repo, cx));
            if repo.snap.tags.is_empty() {
                body = body.child(div().px(px(22.)).h(px(24.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Sin tags"));
                continue;
            }
            for tag in repo.snap.tags.iter().take(TAGS_SHOWN) {
                body = body.child(self.render_tag(ix, repo, tag, cx));
            }
            if repo.snap.tags.len() > TAGS_SHOWN {
                body = body.child(div().px(px(22.)).h(px(20.)).text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("… y {} más", repo.snap.tags.len() - TAGS_SHOWN)));
            }
        }
        root.child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_bumps_versions() {
        assert_eq!(Version::parse("v3.4.31"), Some(Version(3, 4, 31)));
        assert_eq!(Version::parse("1.0.0"), Some(Version(1, 0, 0)));
        assert_eq!(Version::parse("v1.2"), None);
        assert_eq!(Version::parse("v1.2.3-rc1"), None);
        assert_eq!(Version::parse("v1.2.3.4"), None);
        let v = Version(3, 4, 31);
        assert_eq!(v.bump(Bump::Fix).tag(), "v3.4.32");
        assert_eq!(v.bump(Bump::Feature).tag(), "v3.5.0");
        assert_eq!(v.bump(Bump::Major).tag(), "v4.0.0");
    }

    #[test]
    fn latest_is_numeric_not_lexical() {
        let t = |n: &str| Tag { name: n.into(), commit: String::new(), date: String::new(), subject: String::new(), body: String::new(), annotated: true };
        assert_eq!(latest_version(&[t("v1.9.0"), t("v1.10.0"), t("nightly"), t("v1.2.30")]), Some(Version(1, 10, 0)));
    }

    #[test]
    fn parses_for_each_ref_output() {
        let out = "v1.0.1\u{1f}tag\u{1f}2026-11-20T10:00:00-05:00\u{1f}abc1234\u{1f}Fix login\u{1f}- detail\n\u{1e}\nv1.0.0\u{1f}commit\u{1f}2026-11-01T10:00:00-05:00\u{1f}def5678\u{1f}msg\u{1f}\u{1e}\n";
        let tags = parse_tags(out);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name, "v1.0.1");
        assert!(tags[0].annotated);
        assert_eq!(tags[0].body, "- detail");
        assert!(!tags[1].annotated);
        assert_eq!(tags[1].commit, "def5678");
    }
}
