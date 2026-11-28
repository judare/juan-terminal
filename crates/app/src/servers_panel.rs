//! Sidebar "Servers" mode: the project's SSH servers (Termius-like). Add/edit servers
//! (name, host, port, user, key), open an SSH terminal, and browse the server over SFTP:
//! open/edit remote files (uploaded back on save), upload, download, new folder, rename,
//! delete. All remote work runs in the background through `ssh_remote`.
//! Also "2FA CODES": TOTP accounts with live codes and countdowns (see `totp`).

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
    RunDev(crate::ports::DevCommand),
    StopDev(PathBuf),
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
    dev: Vec<crate::ports::DevCommand>,
    dev_running: HashSet<PathBuf>,
    /// Listening ports of running dev servers (by folder).
    dev_ports: HashMap<PathBuf, Vec<u16>>,
    dev_poll: Option<gpui::Task<()>>,
    servers: Vec<SshServer>,
    open: HashSet<String>,
    browsers: HashMap<String, Browser>,
    form: Option<Form>,
    prompt: Option<NamePrompt>,
    totp: Totp,
}

/// "2FA CODES" section: TOTP accounts with live codes (secrets in the Keychain).
#[derive(Default)]
struct Totp {
    loaded: bool,
    accounts: Vec<crate::totp::Account>,
    secrets: HashMap<String, Vec<u8>>,
    /// Accounts whose secret couldn't be read from the Keychain.
    missing: HashSet<String>,
    form: Option<TotpForm>,
    /// Account id whose code was just copied, and when.
    copied: Option<(String, std::time::Instant)>,
    tick: Option<gpui::Task<()>>,
    /// Workspace projects (id, name), in sidebar order: group headers and the right-click menu.
    projects: Vec<(String, String, Option<std::path::PathBuf>)>,
    /// Right-click menu on an account: (position, account id).
    menu: Option<(gpui::Point<gpui::Pixels>, String)>,
}

struct TotpForm {
    secret: Entity<TextInput>,
    name: Entity<TextInput>,
    error: Option<String>,
    busy: Option<String>,
    _subs: Vec<Subscription>,
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
        Self { dev: Vec::new(), dev_running: HashSet::new(), dev_ports: HashMap::new(), dev_poll: None, servers: Vec::new(), open: HashSet::new(), browsers: HashMap::new(), form: None, prompt: None, totp: Totp::default() }
    }

    /// Dev scripts of the project and which of them run now (their terminal tabs).
    pub fn set_dev(&mut self, dev: Vec<crate::ports::DevCommand>, running: HashSet<PathBuf>, cx: &mut Context<Self>) {
        let changed = dev != self.dev || running != self.dev_running;
        self.dev = dev;
        self.dev_running = running;
        self.dev_ports.retain(|k, _| self.dev_running.contains(k));
        if self.dev_running.is_empty() {
            self.dev_poll = None;
        } else if self.dev_poll.is_none() {
            // Find the ports they listen on (they take a moment to bind).
            self.dev_poll = Some(cx.spawn(async move |this, cx| {
                loop {
                    let dirs: Vec<PathBuf> = match this.update(cx, |this, _| this.dev_running.iter().cloned().collect()) {
                        Ok(d) => d,
                        Err(_) => break,
                    };
                    let found = cx
                        .background_executor()
                        .spawn(async move {
                            let mut map: HashMap<PathBuf, Vec<u16>> = HashMap::new();
                            for l in crate::ports::scan().unwrap_or_default() {
                                let Some(cwd) = &l.cwd else { continue };
                                // Deepest matching folder wins (front vs repo root).
                                if let Some(d) = dirs.iter().filter(|d| cwd.starts_with(d)).max_by_key(|d| d.as_os_str().len()) {
                                    map.entry(d.clone()).or_default().extend(l.ports.iter().map(|p| p.0));
                                }
                            }
                            map
                        })
                        .await;
                    if this
                        .update(cx, |this, cx| {
                            if found != this.dev_ports {
                                this.dev_ports = found;
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                    gpui::Timer::after(std::time::Duration::from_secs(3)).await;
                }
            }));
        }
        if changed {
            cx.notify();
        }
    }

    fn render_dev(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.dev.is_empty() {
            return None;
        }
        let mut list = div().flex().flex_col().pb(px(6.)).child(
            div()
                .flex()
                .items_center()
                .pl(px(14.))
                .pr(px(8.))
                .h(px(30.))
                .child(div().text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child("DEV SERVERS")),
        );
        for (ix, d) in self.dev.iter().enumerate() {
            let running = self.dev_running.contains(&d.dir);
            let ports = self.dev_ports.get(&d.dir).cloned().unwrap_or_default();
            let (run_cmd, stop_dir) = (d.clone(), d.dir.clone());
            let mut row = div()
                .id(("dev-row", ix))
                .flex()
                .items_center()
                .gap(px(7.))
                .h(px(34.))
                .pl(px(14.))
                .pr(px(6.))
                .mx(px(4.))
                .rounded(px(5.))
                .hover(|st| st.bg(theme::rgb(theme::HOVER)))
                .child(div().flex_none().size(px(7.)).rounded_full().bg(theme::rgb(if running { theme::ADDED } else { 0x3a3d44 })))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).font_weight(gpui::FontWeight::MEDIUM).child(d.label.clone()))
                        .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("{} · {}", d.command_line(), d.detail))),
                );
            for p in ports.iter().take(2) {
                let port = *p;
                row = row.child(
                    div()
                        .id(SharedString::from(format!("dev-port-{ix}-{port}")))
                        .flex_none()
                        .px(px(5.))
                        .h(px(18.))
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .bg(theme::rgb(theme::BRAND_TINT))
                        .hover(|st| st.bg(theme::rgb(theme::BRAND_DEEP)))
                        .text_size(px(11.))
                        .font_family(theme::MONO_FONT)
                        .text_color(theme::rgb(theme::ACCENT))
                        .tooltip(|_, cx| cx.new(|_| Tooltip("Open in browser")).into())
                        .on_click(move |_, _, cx| cx.open_url(&format!("http://localhost:{port}")))
                        .child(format!(":{port}")),
                );
            }
            row = row.child(if running {
                div()
                    .id(("dev-stop", ix))
                    .flex_none()
                    .px(px(8.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .bg(theme::rgb(0x3a1f22))
                    .hover(|st| st.bg(theme::rgb(0x4a2226)))
                    .text_size(px(11.))
                    .text_color(theme::rgb(0xf0b3b8))
                    .child("■ Stop")
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(ServersPanelEvent::StopDev(stop_dir.clone()))))
                    .into_any_element()
            } else {
                div()
                    .id(("dev-run", ix))
                    .flex_none()
                    .px(px(8.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .bg(theme::rgb(0x1d2b22))
                    .hover(|st| st.bg(theme::rgb(0x23402a)))
                    .text_size(px(11.))
                    .text_color(theme::rgb(0xa8d8a0))
                    .child("▶ Run")
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(ServersPanelEvent::RunDev(run_cmd.clone()))))
                    .into_any_element()
            });
            list = list.child(row);
        }
        Some(list.border_b_1().border_color(theme::rgb(theme::BORDER)).mb(px(4.)).into_any_element())
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

    // ---- 2FA (TOTP) --------------------------------------------------------------------

    /// Load accounts + Keychain secrets once, in the background.
    fn ensure_totp(&mut self, cx: &mut Context<Self>) {
        if self.totp.loaded {
            return;
        }
        self.totp.loaded = true;
        cx.spawn(async move |this, cx| {
            let (accounts, secrets, projects) = cx
                .background_executor()
                .spawn(async move {
                    let accounts = crate::totp::load_accounts();
                    let secrets: Vec<(String, anyhow::Result<Vec<u8>>)> = accounts.iter().map(|a| (a.id.clone(), crate::totp::load_secret(&a.id))).collect();
                    (accounts, secrets, load_projects())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.totp.accounts = accounts;
                this.totp.projects = projects;
                for (id, r) in secrets {
                    match r {
                        Ok(s) => {
                            this.totp.secrets.insert(id, s);
                        }
                        Err(_) => {
                            this.totp.missing.insert(id);
                        }
                    }
                }
                this.ensure_tick(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-render every second while there are codes to show.
    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if self.totp.accounts.is_empty() {
            self.totp.tick = None;
            return;
        }
        if self.totp.tick.is_some() {
            return;
        }
        self.totp.tick = Some(cx.spawn(async move |this, cx| {
            loop {
                // Wake right after the next second boundary so countdowns stay in step.
                let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_millis()).unwrap_or(0);
                gpui::Timer::after(std::time::Duration::from_millis(1000 - ms as u64 + 15)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    fn open_totp_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let secret = cx.new(|cx| TextInput::new("Secret key or otpauth:// link", cx));
        let name = cx.new(|cx| TextInput::new("Name (optional, e.g. AWS root)", cx));
        let subs = [&secret, &name]
            .into_iter()
            .map(|i| {
                cx.subscribe_in(i, window, |this, _, e, _, cx| match e {
                    TextInputEvent::Submit { .. } => this.save_totp_form(cx),
                    TextInputEvent::Cancel => {
                        this.totp.form = None;
                        cx.notify();
                    }
                    TextInputEvent::Changed => {}
                })
            })
            .collect();
        window.focus(&secret.read(cx).focus_handle(cx));
        self.totp.form = Some(TotpForm { secret, name, error: None, busy: None, _subs: subs });
        cx.notify();
    }

    fn totp_form_error(&mut self, e: String, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.totp.form {
            f.busy = None;
            f.error = Some(e);
        }
        cx.notify();
    }

    fn save_totp_form(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &self.totp.form else { return };
        let input = f.secret.read(cx).content.trim().to_string();
        if input.is_empty() {
            return self.totp_form_error("Paste a secret key or an otpauth:// link, or scan a QR code".into(), cx);
        }
        self.add_totp(input, cx);
    }

    /// Parse `input`, store its secret in the Keychain and add the account.
    fn add_totp(&mut self, input: String, cx: &mut Context<Self>) {
        let parsed = match crate::totp::parse_input(&input) {
            Ok(p) => p,
            Err(e) => return self.totp_form_error(format!("{e:#}"), cx),
        };
        let name = self.totp.form.as_ref().map(|f| f.name.read(cx).content.trim().to_string()).unwrap_or_default();
        let id = format!("totp-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        let account = crate::totp::Account {
            id: id.clone(),
            label: if name.is_empty() { parsed.label.clone() } else { name },
            issuer: parsed.issuer.clone(),
            digits: parsed.digits,
            period: parsed.period,
            algorithm: parsed.algorithm,
            project: None,
        };
        if let Some(f) = &mut self.totp.form {
            f.busy = Some("Saving to Keychain…".into());
            f.error = None;
        }
        cx.notify();
        let mut accounts = self.totp.accounts.clone();
        accounts.push(account);
        let secret = parsed.secret;
        cx.spawn(async move |this, cx| {
            let (acc2, id2, sec2) = (accounts.clone(), id.clone(), secret.clone());
            let r = cx
                .background_executor()
                .spawn(async move {
                    crate::totp::store_secret(&id2, &sec2)?;
                    if let Err(e) = crate::totp::save_accounts(&acc2) {
                        crate::totp::delete_secret(&id2);
                        return Err(e);
                    }
                    anyhow::Ok(())
                })
                .await;
            let _ = this.update(cx, |this, cx| match r {
                Ok(()) => {
                    this.totp.accounts = accounts;
                    this.totp.secrets.insert(id, secret);
                    this.totp.form = None;
                    this.ensure_tick(cx);
                    cx.notify();
                }
                Err(e) => this.totp_form_error(format!("{e:#}"), cx),
            });
        })
        .detach();
    }

    /// Decode a QR code in the background (screen selection, image file or clipboard), then add it.
    fn totp_from_qr(&mut self, label: &str, cx: &mut Context<Self>, decode: impl FnOnce() -> anyhow::Result<Option<String>> + Send + 'static) {
        if let Some(f) = &mut self.totp.form {
            f.busy = Some(label.to_string());
            f.error = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { decode() }).await;
            let _ = this.update(cx, |this, cx| match r {
                Ok(Some(text)) => this.add_totp(text, cx),
                Ok(None) => {
                    if let Some(f) = &mut this.totp.form {
                        f.busy = None;
                    }
                    cx.notify();
                }
                Err(e) => this.totp_form_error(format!("{e:#}"), cx),
            });
        })
        .detach();
    }

    fn totp_qr_file(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose QR code image".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| this.totp_from_qr("Reading QR code…", cx, move || crate::totp::decode_qr_file(&p).map(Some)));
        })
        .detach();
    }

    fn totp_qr_clipboard(&mut self, cx: &mut Context<Self>) {
        let item = cx.read_from_clipboard();
        let image = item.as_ref().and_then(|i| i.entries().iter().find_map(|e| if let gpui::ClipboardEntry::Image(img) = e { Some(img.bytes.clone()) } else { None }));
        if let Some(bytes) = image {
            return self.totp_from_qr("Reading QR code…", cx, move || crate::totp::decode_qr_bytes(&bytes).map(Some));
        }
        // Text on the clipboard: a secret or otpauth:// link.
        if let Some(text) = item.and_then(|i| i.text()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
            return self.add_totp(text, cx);
        }
        self.totp_form_error("The clipboard has no image or text. Copy a QR screenshot (⌃⇧⌘4) first".into(), cx);
    }

    fn copy_totp(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(a) = self.totp.accounts.iter().find(|a| a.id == id) else { return };
        let Some(secret) = self.totp.secrets.get(id) else { return };
        let code = crate::totp::code(secret, a.algorithm, a.digits, a.period, crate::totp::now_secs());
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code));
        self.totp.copied = Some((id.to_string(), std::time::Instant::now()));
        cx.notify();
    }

    fn confirm_remove_totp(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.totp.accounts.iter().find(|a| a.id == id) else { return };
        let title = totp_title(a);
        let answer = window.prompt(PromptLevel::Warning, &format!("Remove 2FA “{title}”?"), Some("Deletes the secret from the Keychain. Make sure you can still sign in (another authenticator or recovery codes)."), &["Remove", "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.totp.accounts.retain(|a| a.id != id);
                this.totp.secrets.remove(&id);
                this.totp.missing.remove(&id);
                let accounts = this.totp.accounts.clone();
                cx.background_executor()
                    .spawn(async move {
                        crate::totp::delete_secret(&id);
                        let _ = crate::totp::save_accounts(&accounts);
                    })
                    .detach();
                this.ensure_tick(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn set_totp_project(&mut self, id: &str, project: Option<String>, cx: &mut Context<Self>) {
        let Some(a) = self.totp.accounts.iter_mut().find(|a| a.id == id) else { return };
        a.project = project;
        let accounts = self.totp.accounts.clone();
        cx.background_executor()
            .spawn(async move {
                let _ = crate::totp::save_accounts(&accounts);
            })
            .detach();
        cx.notify();
    }

    fn render_totp_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use crate::whatsapp_panel::{menu_box, menu_row};
        let (pos, id) = self.totp.menu.clone()?;
        let current = self.totp.accounts.iter().find(|a| a.id == id)?.project.clone();
        let mut list = menu_box(cx.listener(|this, _, _, cx| {
            this.totp.menu = None;
            cx.notify();
        }))
        .child(div().px(px(12.)).py(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("PROYECTO"));
        let options = std::iter::once((None, "Ninguno".to_string())).chain(self.totp.projects.iter().map(|(pid, name, _)| (Some(pid.clone()), name.clone())));
        for (i, (pid, name)) in options.enumerate() {
            let on = pid == current;
            let id = id.clone();
            list = list.child(
                menu_row(("totp-project", i))
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(div().w(px(14.)).flex_none().text_color(theme::rgb(theme::ADDED)).when(on, |d| d.child("✓")))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.totp.menu = None;
                        this.set_totp_project(&id, pid.clone(), cx);
                    })),
            );
        }
        Some(gpui::deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element())
    }

    fn render_totp(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut col = div().flex().flex_col().mt(px(6.)).pt(px(2.)).border_t_1().border_color(theme::rgb(theme::BORDER)).child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(14.))
                .pr(px(8.))
                .h(px(30.))
                .child(div().text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child("2FA CODES"))
                .child(div().flex_1())
                .child(icon_action("totp-add", "icons/plus.svg", "Add 2FA account (secret key or QR code)").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.open_totp_form(w, cx)))),
        );
        if let Some(f) = &self.totp.form {
            let field = |label: &'static str, input: &Entity<TextInput>| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(label))
                    .child(div().px(px(8.)).h(px(26.)).flex().items_center().rounded(px(5.)).bg(theme::rgb(theme::EDITOR_BG)).border_1().border_color(theme::rgb(theme::BORDER)).child(input.clone()))
            };
            let qr_btn = |id: &'static str, text: &'static str, tip: &'static str| {
                div()
                    .id(id)
                    .flex_1()
                    .px(px(6.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .bg(theme::rgb(theme::BRAND_TINT))
                    .hover(|s| s.bg(theme::rgb(theme::BRAND_DEEP)))
                    .text_size(px(11.))
                    .text_color(theme::rgb(theme::ACCENT))
                    .tooltip(move |_, cx| cx.new(|_| Tooltip(tip)).into())
                    .child(text)
            };
            col = col.child(
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
                    .child(div().text_size(px(12.)).font_weight(gpui::FontWeight::SEMIBOLD).child("New 2FA account"))
                    .child(field("SECRET KEY", &f.secret))
                    .child(field("NAME", &f.name))
                    .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("…or read the QR code:"))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(qr_btn("totp-scan", "Scan screen", "Drag a box around the QR code on screen").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.totp_from_qr("Select the QR code on screen…", cx, crate::totp::scan_screen))))
                            .child(qr_btn("totp-file", "Image…", "Open a QR code image file").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.totp_qr_file(cx))))
                            .child(qr_btn("totp-paste", "Paste", "Use the QR image (or secret / link) on the clipboard").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.totp_qr_clipboard(cx)))),
                    )
                    .when_some(f.busy.clone(), |d, b| d.child(div().text_size(px(11.)).text_color(theme::rgb(theme::ACCENT)).child(b)))
                    .when_some(f.error.clone(), |d, e| d.child(div().text_size(px(11.)).text_color(theme::rgb(0xf0b3b8)).child(e)))
                    .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Secrets are stored in the macOS Keychain, never in plain files."))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(6.))
                            .child(div().id("totp-cancel").px(px(10.)).h(px(24.)).flex().items_center().rounded(px(5.)).cursor_pointer().bg(theme::rgb(theme::HOVER)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child("Cancel").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.totp.form = None;
                                cx.notify();
                            })))
                            .child(div().id("totp-save").px(px(12.)).h(px(24.)).flex().items_center().rounded(px(5.)).cursor_pointer().bg(theme::rgb(theme::BRAND_DEEP)).hover(|s| s.bg(theme::rgb(theme::BRAND_DEEP_HOVER))).text_size(px(12.)).child("Add").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_totp_form(cx)))),
                    ),
            );
        }
        if self.totp.loaded && self.totp.accounts.is_empty() && self.totp.form.is_none() {
            col = col.child(div().px(px(16.)).py(px(6.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("No 2FA accounts. Add one with + (secret key or QR code); click a code to copy it."));
        }
        let now = crate::totp::now_secs();
        // Grouped by project (sidebar order), then the unassigned ones; headers only once
        // some account has a project.
        let project_ix = |a: &crate::totp::Account| a.project.as_deref().and_then(|p| self.totp.projects.iter().position(|(id, _, _)| id == p));
        let mut ordered: Vec<(Option<usize>, &crate::totp::Account)> = self.totp.accounts.iter().map(|a| (project_ix(a), a)).collect();
        ordered.sort_by_key(|(ix, _)| ix.unwrap_or(usize::MAX));
        let grouped = ordered.iter().any(|(ix, _)| ix.is_some());
        let mut last_group: Option<Option<usize>> = None;
        for (ix, a) in ordered {
            if grouped && last_group != Some(ix) {
                last_group = Some(ix);
                let title = ix.map(|i| self.totp.projects[i].1.clone()).unwrap_or_else(|| "Sin proyecto".into());
                let icon_el = match ix.and_then(|i| self.totp.projects[i].2.clone()) {
                    Some(path) => gpui::img(path).size(px(16.)).flex_none().rounded(px(3.)).object_fit(gpui::ObjectFit::Contain).into_any_element(),
                    None => gpui::svg().path("icons/project-empty.svg").size(px(16.)).flex_none().text_color(theme::rgb(theme::TEXT_FAINT)).into_any_element(),
                };
                col = col.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .pl(px(14.))
                        .pr(px(8.))
                        .pt(px(10.))
                        .pb(px(4.))
                        .child(icon_el)
                        .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child(title)),
                );
            }
            let id = a.id.clone();
            let i3 = id.clone();
            let group: SharedString = format!("totp-{id}").into();
            let (i1, i2) = (id.clone(), id.clone());
            let left = crate::totp::remaining(a.period, now);
            let urgent = left <= 5;
            let copied = self.totp.copied.as_ref().is_some_and(|(c, t)| *c == id && t.elapsed().as_millis() < 1500);
            let code = self.totp.secrets.get(&id).map(|s| crate::totp::code(s, a.algorithm, a.digits, a.period, now));
            let pretty = code.as_deref().map(|c| {
                let mid = c.len() / 2;
                format!("{} {}", &c[..mid], &c[mid..])
            });
            let subtitle = if !a.issuer.is_empty() && !a.label.is_empty() && a.label != a.issuer { Some(a.label.clone()) } else { None };
            let frac = left as f32 / a.period.max(1) as f32;
            let bar_color = if urgent { 0xe06c75 } else { 0x56b6c2 };
            col = col.child(
                div()
                    .id(SharedString::from(format!("totp-row-{id}")))
                    .group(group.clone())
                    .flex()
                    .flex_col()
                    .mx(px(4.))
                    .px(px(10.))
                    .pt(px(5.))
                    .pb(px(4.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(|st| st.bg(theme::rgb(theme::HOVER)))
                    .tooltip(|_, cx| cx.new(|_| Tooltip("Click to copy the code · right-click: project")).into())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.copy_totp(&i1, cx)))
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                            this.totp.projects = load_projects();
                            this.totp.menu = Some((e.position, i3.clone()));
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme::rgb(theme::TEXT_MUTED)).child(totp_title(a)))
                                    .when_some(subtitle, |d, s| d.child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(s)))
                                    .child(match pretty {
                                        Some(c) => div().text_size(px(19.)).font_family(theme::MONO_FONT).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(if urgent { 0xe06c75 } else { theme::TEXT })).child(c),
                                        None => div().text_size(px(11.)).text_color(theme::rgb(0xf0b3b8)).child(if self.totp.missing.contains(&id) { "Secret missing from the Keychain" } else { "Loading…" }),
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .invisible()
                                    .group_hover(group.clone(), |st| st.visible())
                                    .child(icon_action(SharedString::from(format!("totp-rm-{id}")), "icons/trash.svg", "Remove").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                        cx.stop_propagation();
                                        this.confirm_remove_totp(i2.clone(), w, cx)
                                    }))),
                            )
                            .child(if copied {
                                div().flex_none().text_size(px(11.)).text_color(theme::rgb(theme::ADDED)).child("Copied ✓")
                            } else {
                                div().flex_none().min_w(px(26.)).flex().justify_end().text_size(px(12.)).font_family(theme::MONO_FONT).text_color(theme::rgb(if urgent { 0xe06c75 } else { theme::TEXT_MUTED })).child(format!("{left}s"))
                            }),
                    )
                    .child(div().mt(px(3.)).h(px(2.)).w_full().rounded_full().bg(theme::rgb(0x2a2d33)).child(div().h_full().rounded_full().bg(theme::rgb(bar_color)).w(gpui::relative(frac)))),
            );
        }
        col.children(self.render_totp_menu(cx)).into_any_element()
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
                        .child(div().id("srv-save").px(px(12.)).h(px(24.)).flex().items_center().rounded(px(5.)).cursor_pointer().bg(theme::rgb(theme::BRAND_DEEP)).hover(|s| s.bg(theme::rgb(theme::BRAND_DEEP_HOVER))).text_size(px(12.)).child("Save").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_form(cx)))),
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
            let row_icon = if e.is_dir {
                crate::assets::folder_icon_el(&e.name, expanded, 13., None, false)
            } else {
                let (icon, color) = crate::assets::file_icon(&e.name);
                gpui::svg().path(icon).size(px(13.)).flex_none().text_color(theme::rgb(color)).into_any_element()
            };
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
                .child(row_icon)
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

fn load_projects() -> Vec<(String, String, Option<std::path::PathBuf>)> {
    workspace_state::StateStore::default_location().load().projects.into_iter().map(|p| { let icon = p.icon_path(); (p.id, p.name, icon) }).collect()
}

fn totp_title(a: &crate::totp::Account) -> String {
    if !a.issuer.is_empty() {
        a.issuer.clone()
    } else if !a.label.is_empty() {
        a.label.clone()
    } else {
        "2FA account".into()
    }
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
        if let Some(dev) = self.render_dev(cx) {
            list = list.child(dev);
        }
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
                            .bg(theme::rgb(theme::BRAND_TINT))
                            .hover(|st| st.bg(theme::rgb(theme::BRAND_DEEP)))
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
        self.ensure_totp(cx);
        list.child(self.render_totp(cx))
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
