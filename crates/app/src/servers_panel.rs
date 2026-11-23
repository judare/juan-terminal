//! Sidebar "Servers" mode: the project's SSH servers (Termius-like). Add/edit servers
//! (name, host, port, user, key), open an SSH terminal, and browse the server over SFTP:
//! open/edit remote files (uploaded back on save), upload, download, new folder, rename,
//! delete. All remote work runs in the background through `ssh_remote`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{App, ClickEvent, Context, Entity, EventEmitter, Focusable, PathPromptOptions, PromptLevel, Render, SharedString, Subscription, Window, div, prelude::*, px};
use ssh_remote::RemoteEntry;
use workspace_state::SshServer;

use crate::git_panel::{Tooltip, icon_action};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

pub enum ServersPanelEvent {
    /// Server list edited: persist it in the project.
    Changed(Vec<SshServer>),
    OpenTerminal(SshServer),
    /// A remote file was downloaded to `local`; open it (uploads back on save).
    OpenRemoteFile { server: SshServer, remote: String, local: PathBuf },
}

#[derive(Default)]
struct Browser {
    root: Option<String>,
    expanded: HashSet<String>,
    children: HashMap<String, Vec<RemoteEntry>>,
    loading: HashSet<String>,
    error: Option<String>,
    busy: Option<String>,
}

struct Form {
    editing: Option<String>, // server id
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    key: Entity<TextInput>,
    start_dir: Entity<TextInput>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

enum PromptKind {
    NewFolder { dir: String },
    Rename { path: String },
}

struct NamePrompt {
    server: String,
    kind: PromptKind,
    input: Entity<TextInput>,
    _sub: Subscription,
}

pub struct ServersPanel {
    servers: Vec<SshServer>,
    open: HashSet<String>,
    browsers: HashMap<String, Browser>,
    form: Option<Form>,
    prompt: Option<NamePrompt>,
}

impl EventEmitter<ServersPanelEvent> for ServersPanel {}

/// Local cache path for a remote file (kept per server so names can't collide).
pub fn cache_path(server: &SshServer, remote: &str) -> PathBuf {
    let rel = remote.trim_start_matches('/');
    workspace_state::state_dir().join("remote").join(&server.id).join(if rel.is_empty() { "_" } else { rel })
}

fn human_size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / 1024.),
        1_048_576..=1_073_741_823 => format!("{:.1} MB", n as f64 / 1_048_576.),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.),
    }
}

impl ServersPanel {
    pub fn new() -> Self {
        Self { servers: Vec::new(), open: HashSet::new(), browsers: HashMap::new(), form: None, prompt: None }
    }

    pub fn set_servers(&mut self, servers: Vec<SshServer>, cx: &mut Context<Self>) {
        if servers != self.servers {
            let ids: HashSet<String> = servers.iter().map(|s| s.id.clone()).collect();
            self.browsers.retain(|k, _| ids.contains(k));
            self.open.retain(|k| ids.contains(k));
            self.servers = servers;
            cx.notify();
        }
    }

    fn server(&self, id: &str) -> Option<SshServer> {
        self.servers.iter().find(|s| s.id == id).cloned()
    }

    // ---- form --------------------------------------------------------------------------

    fn open_form(&mut self, editing: Option<SshServer>, window: &mut Window, cx: &mut Context<Self>) {
        let s = editing.clone().unwrap_or_default();
        let mk = |ph: &'static str, val: String, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut t = TextInput::new(ph, cx);
                t.set_text(&val, cx);
                t
            })
        };
        let name = mk("Name (e.g. production)", s.name.clone(), cx);
        let host = mk("Host or IP", s.host.clone(), cx);
        let port = mk("22", if s.port == 0 { String::new() } else { s.port.to_string() }, cx);
        let user = mk("User", s.user.clone(), cx);
        let key = mk("~/.ssh/id_ed25519 (optional)", s.identity_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default(), cx);
        let start_dir = mk("Start folder (optional, e.g. /var/www)", s.start_dir.clone(), cx);
        let subs = [&name, &host, &port, &user, &key, &start_dir]
            .into_iter()
            .map(|i| {
                cx.subscribe_in(i, window, |this, _, e, _, cx| match e {
                    TextInputEvent::Submit { .. } => this.save_form(cx),
                    TextInputEvent::Cancel => {
                        this.form = None;
                        cx.notify();
                    }
                    TextInputEvent::Changed => {}
                })
            })
            .collect();
        window.focus(&name.read(cx).focus_handle(cx));
        self.form = Some(Form { editing: editing.map(|s| s.id), name, host, port, user, key, start_dir, error: None, _subs: subs });
        cx.notify();
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        let get = |e: &Entity<TextInput>, cx: &App| e.read(cx).content.trim().to_string();
        let host = get(&f.host, cx);
        if host.is_empty() {
            f.error = Some("Host is required".into());
            cx.notify();
            return;
        }
        let port_s = get(&f.port, cx);
        let port: u16 = if port_s.is_empty() {
            0
        } else {
            match port_s.parse() {
                Ok(p) => p,
                Err(_) => {
                    f.error = Some("Port must be a number".into());
                    cx.notify();
                    return;
                }
            }
        };
        let key_s = get(&f.key, cx);
        let identity_file = (!key_s.is_empty()).then(|| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(if let Some(r) = key_s.strip_prefix("~/") { format!("{home}/{r}") } else { key_s.clone() })
        });
        if let Some(k) = &identity_file {
            if !k.is_file() {
                f.error = Some(format!("Key file not found: {}", k.display()));
                cx.notify();
                return;
            }
        }
        let mut name = get(&f.name, cx);
        if name.is_empty() {
            name = host.clone();
        }
        let id = f.editing.clone().unwrap_or_else(|| format!("srv-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)));
        let server = SshServer { id: id.clone(), name, host, port, user: get(&f.user, cx), identity_file, start_dir: get(&f.start_dir, cx) };
        match self.servers.iter_mut().find(|s| s.id == id) {
            Some(s) => *s = server,
            None => self.servers.push(server),
        }
        // Connection details may have changed: reset the browser.
        self.browsers.remove(&id);
        self.form = None;
        cx.emit(ServersPanelEvent::Changed(self.servers.clone()));
        cx.notify();
    }

    fn pick_key(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose private key".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| {
                if let Some(f) = &this.form {
                    f.key.update(cx, |k, cx| k.set_text(&p.display().to_string(), cx));
                }
            });
        })
        .detach();
    }

    fn confirm_remove_server(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.server(&id) else { return };
        let answer = window.prompt(PromptLevel::Warning, &format!("Remove server “{}”?", s.name), Some("Only removes it from this project. Nothing on the server or your keys is touched."), &["Remove", "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| {
                    this.servers.retain(|x| x.id != id);
                    this.browsers.remove(&id);
                    cx.emit(ServersPanelEvent::Changed(this.servers.clone()));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ---- remote browser ----------------------------------------------------------------

    fn toggle_server(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.open.remove(id) {
            self.open.insert(id.to_string());
            if self.browsers.get(id).is_none_or(|b| b.root.is_none()) {
                self.load_root(id.to_string(), cx);
            }
        }
        cx.notify();
    }

    fn load_root(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(server) = self.server(&id) else { return };
        let b = self.browsers.entry(id.clone()).or_default();
        b.error = None;
        b.busy = Some("Connecting…".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let s2 = server.clone();
            let r = cx
                .background_executor()
                .spawn(async move {
                    let root = if s2.start_dir.trim().is_empty() { ssh_remote::home_dir(&s2)? } else { s2.start_dir.trim().to_string() };
                    let entries = ssh_remote::list_dir(&s2, &root)?;
                    anyhow::Ok((root, entries))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let b = this.browsers.entry(id).or_default();
                b.busy = None;
                match r {
                    Ok((root, entries)) => {
                        b.children.insert(root.clone(), entries);
                        b.root = Some(root);
                    }
                    Err(e) => b.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_dir(&mut self, id: String, dir: String, cx: &mut Context<Self>) {
        let Some(server) = self.server(&id) else { return };
        let b = self.browsers.entry(id.clone()).or_default();
        if !b.loading.insert(dir.clone()) {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (s2, d2) = (server.clone(), dir.clone());
            let r = cx.background_executor().spawn(async move { ssh_remote::list_dir(&s2, &d2) }).await;
            let _ = this.update(cx, |this, cx| {
                let b = this.browsers.entry(id).or_default();
                b.loading.remove(&dir);
                match r {
                    Ok(entries) => {
                        b.children.insert(dir, entries);
                    }
                    Err(e) => b.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_dir(&mut self, id: &str, dir: &str, cx: &mut Context<Self>) {
        let b = self.browsers.entry(id.to_string()).or_default();
        if !b.expanded.remove(dir) {
            b.expanded.insert(dir.to_string());
            if !b.children.contains_key(dir) {
                self.load_dir(id.to_string(), dir.to_string(), cx);
            }
        }
        cx.notify();
    }

    fn refresh_dir(&mut self, id: &str, dir: &str, cx: &mut Context<Self>) {
        self.load_dir(id.to_string(), dir.to_string(), cx);
    }

    /// Run a remote operation, show progress/errors, then refresh `refresh` (a directory).
    fn remote_op(&mut self, id: String, label: &str, refresh: String, cx: &mut Context<Self>, op: impl FnOnce(&SshServer) -> anyhow::Result<()> + Send + 'static) {
        let Some(server) = self.server(&id) else { return };
        let b = self.browsers.entry(id.clone()).or_default();
        b.busy = Some(label.to_string());
        b.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { op(&server) }).await;
            let _ = this.update(cx, |this, cx| {
                let b = this.browsers.entry(id.clone()).or_default();
                b.busy = None;
                if let Err(e) = r {
                    b.error = Some(format!("{e:#}"));
                }
                this.load_dir(id, refresh, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_file(&mut self, id: String, remote: String, cx: &mut Context<Self>) {
        let Some(server) = self.server(&id) else { return };
        let local = cache_path(&server, &remote);
        let b = self.browsers.entry(id.clone()).or_default();
        b.busy = Some(format!("Downloading {}…", remote.rsplit('/').next().unwrap_or("")));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (s2, r2, l2) = (server.clone(), remote.clone(), local.clone());
            let res = cx.background_executor().spawn(async move { ssh_remote::download(&s2, &r2, &l2) }).await;
            let _ = this.update(cx, |this, cx| {
                let b = this.browsers.entry(id).or_default();
                b.busy = None;
                match res {
                    Ok(()) => cx.emit(ServersPanelEvent::OpenRemoteFile { server, remote, local }),
                    Err(e) => b.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn download_to(&mut self, id: String, remote: String, is_dir: bool, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Download to".into()) });
        let parent = ssh_remote::parent_remote(&remote);
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(dest) = paths.into_iter().next() else { return };
            let name = remote.trim_end_matches('/').rsplit('/').next().unwrap_or("download").to_string();
            let _ = this.update(cx, |this, cx| {
                this.remote_op(id, "Downloading…", parent, cx, move |s| {
                    if is_dir {
                        // scp -r creates dest/<name>.
                        download_dir(s, &remote, &dest)
                    } else {
                        ssh_remote::download(s, &remote, &dest.join(&name))
                    }
                });
            });
        })
        .detach();
    }

    fn upload_into(&mut self, id: String, dir: String, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: true, multiple: true, prompt: Some("Upload".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                let d2 = dir.clone();
                this.browsers.entry(id.clone()).or_default().expanded.insert(dir.clone());
                this.remote_op(id, "Uploading…", dir, cx, move |s| {
                    for p in paths {
                        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        ssh_remote::upload(s, &p, &ssh_remote::join_remote(&d2, &name))?;
                    }
                    Ok(())
                });
            });
        })
        .detach();
    }

    fn start_prompt(&mut self, server: String, kind: PromptKind, window: &mut Window, cx: &mut Context<Self>) {
        let initial = match &kind {
            PromptKind::Rename { path } => path.rsplit('/').next().unwrap_or("").to_string(),
            PromptKind::NewFolder { .. } => String::new(),
        };
        let input = cx.new(|cx| {
            let mut t = TextInput::new("Name", cx);
            t.set_text(&initial, cx);
            t
        });
        let sub = cx.subscribe_in(&input, window, |this, input, e, _, cx| match e {
            TextInputEvent::Submit { .. } => {
                let name = input.read(cx).content.trim().to_string();
                if let Some(p) = this.prompt.take() {
                    if !name.is_empty() && !name.contains('/') {
                        match p.kind {
                            PromptKind::NewFolder { dir } => {
                                let target = ssh_remote::join_remote(&dir, &name);
                                this.browsers.entry(p.server.clone()).or_default().expanded.insert(dir.clone());
                                this.remote_op(p.server, "Creating folder…", dir, cx, move |s| ssh_remote::mkdir(s, &target));
                            }
                            PromptKind::Rename { path } => {
                                let parent = ssh_remote::parent_remote(&path);
                                let to = ssh_remote::join_remote(&parent, &name);
                                this.remote_op(p.server, "Renaming…", parent, cx, move |s| ssh_remote::rename(s, &path, &to));
                            }
                        }
                    }
                }
                cx.notify();
            }
            TextInputEvent::Cancel => {
                this.prompt = None;
                cx.notify();
            }
            TextInputEvent::Changed => {}
        });
        window.focus(&input.read(cx).focus_handle(cx));
        input.update(cx, |i, cx| i.select_all_text(cx));
        self.prompt = Some(NamePrompt { server, kind, input, _sub: sub });
        cx.notify();
    }

    fn confirm_delete(&mut self, id: String, path: String, is_dir: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.server(&id) else { return };
        let what = if is_dir { "folder and everything inside it" } else { "file" };
        let answer = window.prompt(PromptLevel::Critical, &format!("Delete {path} on {}?", s.name), Some(&format!("This permanently deletes the {what} on the server. It can't be undone.")), &["Delete", "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| {
                    let parent = ssh_remote::parent_remote(&path);
                    this.remote_op(id, "Deleting…", parent, cx, move |s| ssh_remote::remove(s, &path, is_dir));
                });
            }
        })
        .detach();
    }

    // ---- rendering ---------------------------------------------------------------------

    fn render_form(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let f = self.form.as_ref()?;
        let field = |label: &'static str, input: &Entity<TextInput>| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(label))
                .child(div().px(px(8.)).h(px(26.)).flex().items_center().rounded(px(5.)).bg(theme::rgb(theme::EDITOR_BG)).border_1().border_color(theme::rgb(theme::BORDER)).child(input.clone()))
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .mx(px(10.))
                .my(px(6.))
                .p(px(10.))
                .rounded(px(8.))
                .bg(theme::rgb(0x202227))
                .border_1()
                .border_color(theme::rgb(0x33363d))
                .child(div().text_size(px(12.)).font_weight(gpui::FontWeight::SEMIBOLD).child(if f.editing.is_some() { "Edit server" } else { "New server" }))
                .child(field("NAME", &f.name))
                .child(field("HOST", &f.host))
                .child(div().flex().gap(px(8.)).child(div().w(px(70.)).child(field("PORT", &f.port))).child(div().flex_1().child(field("USER", &f.user))))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(6.))
                        .child(div().flex_1().min_w_0().child(field("PRIVATE KEY", &f.key)))
                        .child(icon_action("pick-key", "icons/folder.svg", "Choose key file…").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pick_key(cx)))),
                )
                .child(field("START FOLDER", &f.start_dir))
                .when_some(f.error.clone(), |d, e| d.child(div().text_size(px(11.)).text_color(theme::rgb(0xf0b3b8)).child(e)))
                .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Only the key's path is saved. Passwords are never stored: connect in a terminal to type one."))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(6.))
                        .child(
                            div().id("srv-cancel").px(px(10.)).h(px(24.)).flex().items_center().rounded(px(5.)).cursor_pointer().bg(theme::rgb(theme::HOVER)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child("Cancel").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.form = None;
                                cx.notify();
                            })),
                        )
                        .child(div().id("srv-save").px(px(12.)).h(px(24.)).flex().items_center().rounded(px(5.)).cursor_pointer().bg(theme::rgb(0x2b4a7a)).hover(|s| s.bg(theme::rgb(0x355a92))).text_size(px(12.)).child("Save").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_form(cx)))),
                )
                .into_any_element(),
        )
    }

    fn render_tree(&self, id: &str, dir: &str, depth: usize, out: &mut Vec<gpui::AnyElement>, cx: &mut Context<Self>) {
        let Some(b) = self.browsers.get(id) else { return };
        let Some(entries) = b.children.get(dir) else {
            if b.loading.contains(dir) {
                out.push(div().pl(px(28. + depth as f32 * 14.)).h(px(22.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Loading…").into_any_element());
            }
            return;
        };
        if entries.is_empty() {
            out.push(div().pl(px(28. + depth as f32 * 14.)).h(px(22.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Empty folder").into_any_element());
        }
        for e in entries {
            let path = ssh_remote::join_remote(dir, &e.name);
            let expanded = e.is_dir && b.expanded.contains(&path);
            let group: SharedString = format!("rf-{id}-{path}").into();
            let (icon, color) = if e.is_dir { (if expanded { "icons/folder-open.svg" } else { "icons/folder.svg" }, 0x7aa2f7) } else { crate::assets::file_icon(&e.name) };
            let (id1, id2, id3, id4, id5, id6) = (id.to_string(), id.to_string(), id.to_string(), id.to_string(), id.to_string(), id.to_string());
            let (p1, p2, p3, p4, p5, p6) = (path.clone(), path.clone(), path.clone(), path.clone(), path.clone(), path.clone());
            let is_dir = e.is_dir;
            let mut row = div()
                .id(SharedString::from(format!("re-{id}-{path}")))
                .group(group.clone())
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(24.))
                .pl(px(16. + depth as f32 * 14.))
                .pr(px(6.))
                .mx(px(4.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if is_dir {
                        this.toggle_dir(&id1, &p1, cx);
                    } else {
                        this.open_file(id1.clone(), p1.clone(), cx);
                    }
                }))
                .child(if is_dir {
                    gpui::svg().path(if expanded { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(9.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)).into_any_element()
                } else {
                    div().w(px(9.)).flex_none().into_any_element()
                })
                .child(gpui::svg().path(icon).size(px(13.)).flex_none().text_color(theme::rgb(color)))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).text_color(theme::rgb(if e.name.starts_with('.') { theme::TEXT_MUTED } else { theme::TEXT })).child(e.name.clone()))
                .when(!is_dir, |d| d.child(div().flex_none().text_size(px(10.)).text_color(theme::rgb(theme::TEXT_FAINT)).group_hover(group.clone(), |s| s.invisible()).child(human_size(e.size))));
            // Hover actions.
            let mut actions = div().flex().flex_none().invisible().group_hover(group.clone(), |s| s.visible());
            if is_dir {
                actions = actions
                    .child(icon_action(SharedString::from(format!("up-{path}")), "icons/push.svg", "Upload here…").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.upload_into(id2.clone(), p2.clone(), cx)
                    })))
                    .child(icon_action(SharedString::from(format!("nf-{path}")), "icons/new-folder.svg", "New folder").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.start_prompt(id3.clone(), PromptKind::NewFolder { dir: p3.clone() }, w, cx)
                    })));
            }
            actions = actions
                .child(icon_action(SharedString::from(format!("dl-{path}")), "icons/pull.svg", "Download…").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.download_to(id4.clone(), p4.clone(), is_dir, cx)
                })))
                .child(icon_action(SharedString::from(format!("rn-{path}")), "icons/edit.svg", "Rename").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                    cx.stop_propagation();
                    this.start_prompt(id5.clone(), PromptKind::Rename { path: p5.clone() }, w, cx)
                })))
                .child(icon_action(SharedString::from(format!("rm-{path}")), "icons/trash.svg", "Delete").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                    cx.stop_propagation();
                    this.confirm_delete(id6.clone(), p6.clone(), is_dir, w, cx)
                })));
            row = row.child(actions);
            out.push(row.into_any_element());
            if let Some(p) = self.prompt.as_ref().filter(|p| p.server == id && matches!(&p.kind, PromptKind::Rename { path: x } if *x == path) || p.server == id && matches!(&p.kind, PromptKind::NewFolder { dir } if *dir == path)) {
                out.push(prompt_row(&p.input, depth + 1));
            }
            if expanded {
                self.render_tree(id, &path, depth + 1, out, cx);
            }
        }
    }
}

fn prompt_row(input: &Entity<TextInput>, depth: usize) -> gpui::AnyElement {
    div()
        .ml(px(16. + depth as f32 * 14.))
        .mr(px(10.))
        .my(px(2.))
        .px(px(6.))
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .bg(theme::rgb(theme::EDITOR_BG))
        .border_1()
        .border_color(theme::rgb(theme::ACCENT))
        .child(input.clone())
        .into_any_element()
}

/// Recursive download (sftp `get -r`).
fn download_dir(s: &SshServer, remote: &str, dest_dir: &std::path::Path) -> anyhow::Result<()> {
    let mut args = ssh_remote::base_options(s, true);
    args.extend(["-r".into(), "-P".into(), s.port_or_default().to_string(), format!("{}:{}", s.destination(), remote), dest_dir.display().to_string()]);
    let st = std::process::Command::new("scp").args(&args).stdin(std::process::Stdio::null()).output()?;
    if !st.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&st.stderr).trim());
    }
    Ok(())
}

impl Render for ServersPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().id("servers-panel").flex().flex_col().size_full().overflow_y_scroll().pb(px(16.));
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(14.))
                .pr(px(8.))
                .h(px(30.))
                .child(div().text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child("SERVERS"))
                .child(div().flex_1())
                .child(icon_action("srv-add", "icons/plus.svg", "Add server").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.open_form(None, w, cx)))),
        );
        if let Some(f) = self.render_form(cx) {
            list = list.child(f);
        }
        if self.servers.is_empty() && self.form.is_none() {
            list = list.child(div().px(px(16.)).py(px(6.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("No servers in this project. Add one with + (host, user, port and an SSH key). Claude can then run commands on it with the ssh_exec tool."));
        }
        let servers = self.servers.clone();
        for s in servers {
            let id = s.id.clone();
            let open = self.open.contains(&id);
            let group: SharedString = format!("srv-{id}").into();
            let (i1, i2, i3, i4) = (id.clone(), id.clone(), id.clone(), id.clone());
            let s_term = s.clone();
            let s_edit = s.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("srv-row-{id}")))
                    .group(group.clone())
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(34.))
                    .pl(px(10.))
                    .pr(px(6.))
                    .mx(px(4.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme::rgb(theme::HOVER)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_server(&i1, cx)))
                    .child(gpui::svg().path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(9.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)))
                    .child(gpui::svg().path("icons/server.svg").size(px(14.)).flex_none().text_color(theme::rgb(0x56b6c2)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).font_weight(gpui::FontWeight::MEDIUM).child(s.name.clone()))
                            .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("{}{}", s.destination(), if s.port_or_default() != 22 { format!(":{}", s.port_or_default()) } else { String::new() }))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .invisible()
                            .group_hover(group, |st| st.visible())
                            .child(icon_action(SharedString::from(format!("srv-edit-{id}")), "icons/edit.svg", "Edit").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                cx.stop_propagation();
                                this.open_form(Some(s_edit.clone()), w, cx)
                            })))
                            .child(icon_action(SharedString::from(format!("srv-rm-{id}")), "icons/trash.svg", "Remove").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                cx.stop_propagation();
                                this.confirm_remove_server(i2.clone(), w, cx)
                            }))),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("srv-ssh-{id}")))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .px(px(7.))
                            .h(px(22.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .bg(theme::rgb(0x1e2a3d))
                            .hover(|st| st.bg(theme::rgb(0x2b4a7a)))
                            .text_size(px(11.))
                            .text_color(theme::rgb(theme::ACCENT))
                            .tooltip(|_, cx| cx.new(|_| Tooltip("Open an SSH terminal")).into())
                            .child(gpui::svg().path("icons/terminal.svg").size(px(11.)).text_color(theme::rgb(theme::ACCENT)))
                            .child("SSH")
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(ServersPanelEvent::OpenTerminal(s_term.clone()))
                            })),
                    ),
            );
            if !open {
                continue;
            }
            let b = self.browsers.get(&id);
            let root = b.and_then(|b| b.root.clone());
            if let Some(busy) = b.and_then(|b| b.busy.clone()) {
                list = list.child(div().pl(px(32.)).py(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::ACCENT)).child(busy));
            }
            if let Some(err) = b.and_then(|b| b.error.clone()) {
                list = list.child(
                    div()
                        .id(SharedString::from(format!("srv-err-{id}")))
                        .mx(px(12.))
                        .my(px(4.))
                        .px(px(8.))
                        .py(px(5.))
                        .rounded(px(5.))
                        .bg(theme::rgb(0x3a1f22))
                        .text_size(px(11.))
                        .text_color(theme::rgb(0xf0b3b8))
                        .cursor_pointer()
                        .child(err)
                        .child(div().mt(px(3.)).text_color(theme::rgb(theme::TEXT_MUTED)).child("Click to retry"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if this.browsers.get(&i3).is_some_and(|b| b.root.is_some()) {
                                if let Some(b) = this.browsers.get_mut(&i3) {
                                    b.error = None;
                                }
                                cx.notify();
                            } else {
                                this.load_root(i3.clone(), cx);
                            }
                        })),
                );
            }
            if let Some(root) = root {
                let r2 = root.clone();
                let r3 = root.clone();
                let i5 = id.clone();
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .pl(px(30.))
                        .pr(px(8.))
                        .h(px(24.))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_MUTED)).child(root.clone()))
                        .child(icon_action(SharedString::from(format!("srv-refresh-{id}")), "icons/fetch.svg", "Refresh").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.refresh_dir(&i4, &r2, cx))))
                        .child(icon_action(SharedString::from(format!("srv-up-{id}")), "icons/push.svg", "Upload here…").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.upload_into(i5.clone(), r3.clone(), cx)))),
                );
                let mut rows = Vec::new();
                self.render_tree(&id, &root, 0, &mut rows, cx);
                list = list.children(rows);
            }
        }
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_are_per_server_and_absolute_safe() {
        let s = SshServer { id: "srv-1".into(), ..Default::default() };
        let p = cache_path(&s, "/etc/nginx/nginx.conf");
        assert!(p.ends_with("remote/srv-1/etc/nginx/nginx.conf"), "{}", p.display());
        assert_eq!(human_size(1536), "1.5 KB");
    }
}
