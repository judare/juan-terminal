//! Sidebar "Ports" mode: TCP ports in LISTEN state, the process holding each one, and kill
//! buttons (one / all). Uses `lsof` + `ps` (no root: only the user's own processes are visible,
//! which are also the only ones we could kill).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use gpui::{ClickEvent, Context, PromptLevel, Render, SharedString, Task, Timer, Window, div, prelude::*, px};

use crate::git_panel::icon_action;
use crate::theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    pub pid: i32,
    /// Short process name from lsof (`node`, `postgres`…).
    pub name: String,
    /// Full command line from `ps`.
    pub command: String,
    pub cwd: Option<PathBuf>,
    /// Ports with their bind address (`*`, `127.0.0.1`, `[::1]`).
    pub ports: Vec<(u16, String)>,
    /// macOS services (AirPlay receiver, rapportd…): hidden by default, never in "kill all".
    pub system: bool,
    /// Started from juan-terminal (one of its terminals is an ancestor).
    pub ours: bool,
    /// Human name: the `.app` bundle or the executable ("Figma Agent", "node").
    pub app: String,
    /// Executable path (from `ps -o comm`), used for the app name and system detection.
    pub exe: String,
}

/// "…/Visual Studio Code 2.app/Contents/Frameworks/Code Helper.app/…" → "Visual Studio Code 2".
pub fn app_name(exe: &str, fallback: &str) -> String {
    if let Some(i) = exe.find(".app/") {
        let head = &exe[..i];
        return head.rsplit('/').next().unwrap_or(head).to_string();
    }
    let base = exe.rsplit('/').next().unwrap_or(exe);
    // Some CLIs (claude) report their version as the process name; prefer the executable.
    if base.is_empty() || base.chars().all(|c| c.is_ascii_digit() || c == '.') { fallback.to_string() } else { base.to_string() }
}

/// pid → (ppid, exe) for every process.
fn process_table() -> HashMap<i32, (i32, String)> {
    let mut t = HashMap::new();
    if let Ok(o) = Command::new("/bin/ps").args(["-A", "-ww", "-o", "pid=,ppid=,comm="]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut it = line.trim_start().splitn(2, char::is_whitespace);
            let (Some(pid), Some(rest)) = (it.next(), it.next()) else { continue };
            let rest = rest.trim_start();
            let mut it2 = rest.splitn(2, char::is_whitespace);
            let (Some(ppid), Some(exe)) = (it2.next(), it2.next()) else { continue };
            if let (Ok(pid), Ok(ppid)) = (pid.parse(), ppid.parse()) {
                t.insert(pid, (ppid, exe.trim().to_string()));
            }
        }
    }
    t
}

fn descends_from(pid: i32, ancestor: i32, table: &HashMap<i32, (i32, String)>) -> bool {
    let mut p = pid;
    for _ in 0..64 {
        match table.get(&p) {
            Some((ppid, _)) if *ppid == ancestor => return true,
            Some((ppid, _)) if *ppid > 1 => p = *ppid,
            _ => return false,
        }
    }
    false
}

/// Parse `lsof -nP -iTCP -sTCP:LISTEN -F pcn` output into pid → (name, [(port, addr)]).
pub fn parse_lsof(out: &str) -> BTreeMap<i32, (String, Vec<(u16, String)>)> {
    let mut map: BTreeMap<i32, (String, Vec<(u16, String)>)> = BTreeMap::new();
    let mut pid = None;
    for line in out.lines() {
        let (tag, val) = line.split_at(line.len().min(1));
        match tag {
            "p" => {
                pid = val.parse::<i32>().ok();
                if let Some(p) = pid {
                    map.entry(p).or_default();
                }
            }
            "c" => {
                if let Some(e) = pid.and_then(|p| map.get_mut(&p)) {
                    e.0 = val.to_string();
                }
            }
            "n" => {
                let Some(e) = pid.and_then(|p| map.get_mut(&p)) else { continue };
                if let Some((addr, port)) = val.rsplit_once(':') {
                    if let Ok(port) = port.parse::<u16>() {
                        // IPv4 + IPv6 sockets on the same port collapse into one entry.
                        if !e.1.iter().any(|(p, _)| *p == port) {
                            e.1.push((port, addr.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for e in map.values_mut() {
        e.1.sort();
    }
    map.retain(|_, e| !e.1.is_empty());
    map
}

fn is_system(command: &str) -> bool {
    ["/System/", "/usr/libexec/", "/usr/sbin/", "/Library/Apple/"].iter().any(|p| command.starts_with(p))
}

pub fn scan() -> anyhow::Result<Vec<Listener>> {
    scan_for(std::process::id() as i32)
}

/// `app_pid`: the juan-terminal process whose descendants count as "ours".
pub fn scan_for(app_pid: i32) -> anyhow::Result<Vec<Listener>> {
    let out = Command::new("/usr/sbin/lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"]).output()?;
    // lsof exits 1 when nothing matches.
    let map = parse_lsof(&String::from_utf8_lossy(&out.stdout));
    if map.is_empty() {
        return Ok(Vec::new());
    }
    let pids: Vec<String> = map.keys().map(|p| p.to_string()).collect();
    let mut commands: HashMap<i32, String> = HashMap::new();
    if let Ok(ps) = Command::new("/bin/ps").args(["-ww", "-o", "pid=,args=", "-p", &pids.join(",")]).output() {
        for line in String::from_utf8_lossy(&ps.stdout).lines() {
            let line = line.trim_start();
            if let Some((pid, args)) = line.split_once(' ') {
                if let Ok(pid) = pid.parse() {
                    commands.insert(pid, args.trim().to_string());
                }
            }
        }
    }
    let mut cwds: HashMap<i32, PathBuf> = HashMap::new();
    if let Ok(o) = Command::new("/usr/sbin/lsof").args(["-a", "-d", "cwd", "-F", "pn", "-p", &pids.join(",")]).output() {
        let mut pid = None;
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(p) = line.strip_prefix('p') {
                pid = p.parse().ok();
            } else if let (Some(n), Some(p)) = (line.strip_prefix('n'), pid) {
                cwds.insert(p, PathBuf::from(n));
            }
        }
    }
    let table = process_table();
    let me = app_pid;
    let mut v: Vec<Listener> = map
        .into_iter()
        .map(|(pid, (name, ports))| {
            let command = commands.remove(&pid).unwrap_or_else(|| name.clone());
            let exe = table.get(&pid).map(|(_, e)| e.clone()).unwrap_or_else(|| command.clone());
            let first = command.split_whitespace().next().unwrap_or(&name).rsplit('/').next().unwrap_or(&name).to_string();
            let app = app_name(&exe, &first);
            Listener { pid, system: is_system(&exe) || is_system(&command), ours: descends_from(pid, me, &table), app, exe, name, command, cwd: cwds.remove(&pid), ports }
        })
        .collect();
    v.sort_by_key(|l| (!l.ours, l.system, l.ports.first().map(|p| p.0).unwrap_or(0)));
    Ok(v)
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// SIGTERM, then SIGKILL if it's still alive after ~1.5 s.
pub fn kill(pid: i32) -> anyhow::Result<()> {
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        anyhow::bail!("kill {pid}: {}", std::io::Error::last_os_error());
    }
    for _ in 0..15 {
        std::thread::sleep(Duration::from_millis(100));
        if !alive(pid) {
            return Ok(());
        }
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    Ok(())
}

pub struct PortsPanel {
    listeners: Vec<Listener>,
    error: Option<String>,
    show_system: bool,
    /// Active project's folders, to tag "this project" processes.
    roots: Vec<(String, PathBuf)>,
    busy: bool,
    loaded: bool,
    /// Set by the workspace: polling pauses while the panel isn't visible.
    pub active: bool,
    _poll: Option<Task<()>>,
}

impl PortsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self { listeners: Vec::new(), error: None, show_system: false, roots: Vec::new(), busy: false, loaded: false, active: true, _poll: None };
        this.refresh(cx);
        // Cheap (two lsof + one ps); only repaints when the list changed.
        this._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_secs(3)).await;
                let r = this.update(cx, |this, cx| {
                    if this.active {
                        this.refresh(cx)
                    }
                });
                if r.is_err() {
                    break;
                }
            }
        }));
        this
    }

    pub fn set_roots(&mut self, roots: Vec<(String, PathBuf)>, cx: &mut Context<Self>) {
        self.roots = roots;
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async { scan() }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.loaded = true;
                match r {
                    Ok(l) if l != this.listeners || this.error.is_some() => {
                        this.listeners = l;
                        this.error = None;
                        cx.notify();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        this.error = Some(format!("{e:#}"));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn project_of(&self, cwd: &Option<PathBuf>) -> Option<String> {
        let cwd = cwd.as_ref()?;
        self.roots.iter().filter(|(_, p)| cwd.starts_with(p)).max_by_key(|(_, p)| p.as_os_str().len()).map(|(n, _)| n.clone())
    }

    fn kill_pids(&mut self, pids: Vec<i32>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let errors: Vec<String> = cx.background_executor().spawn(async move { pids.into_iter().filter_map(|p| kill(p).err().map(|e| format!("{e:#}"))).collect() }).await;
            let _ = this.update(cx, |this, cx| {
                this.error = (!errors.is_empty()).then(|| errors.join("; "));
                this.busy = false;
                this.refresh(cx);
            });
        })
        .detach();
    }

    fn confirm_kill(&mut self, pids: Vec<i32>, what: String, window: &mut Window, cx: &mut Context<Self>) {
        if pids.is_empty() {
            return;
        }
        let answer = window.prompt(PromptLevel::Warning, &format!("Kill {what}?"), Some("Sends SIGTERM, then SIGKILL if it doesn't exit. Unsaved work in those processes is lost."), &["Kill", "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| this.kill_pids(pids, cx));
            }
        })
        .detach();
    }
}

fn short_command(cmd: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let s = if !home.is_empty() { cmd.replace(home.as_ref(), "~") } else { cmd.to_string() };
    // Drop the interpreter path: "/opt/homebrew/bin/node ./x" → "node ./x".
    match s.split_once(' ') {
        Some((exe, rest)) => format!("{} {rest}", exe.rsplit('/').next().unwrap_or(exe)),
        None => s.rsplit('/').next().unwrap_or(&s).to_string(),
    }
}

impl PortsPanel {
    fn render_listener(&self, l: &Listener, home: &Path, cx: &mut Context<Self>) -> gpui::AnyElement {
        let pid = l.pid;
        let group: SharedString = format!("port-{pid}").into();
        let project = self.project_of(&l.cwd);
        let cwd = l.cwd.as_ref().filter(|c| c.as_os_str() != "/" && c.as_path() != home).map(|c| c.to_string_lossy().replacen(home.to_string_lossy().as_ref(), "~", 1));
        // Only show the command when it says more than the app name.
        let cmd = short_command(&l.command, home);
        let show_cmd = !l.command.contains(".app/") && cmd != l.app;
        let mut ports = div().flex().flex_wrap().justify_end().gap(px(3.)).flex_none().max_w(px(130.));
        for (port, addr) in &l.ports {
            let port = *port;
            let local = addr != "*" && !addr.starts_with("0.0.0.0") && addr != "[::]";
            ports = ports.child(
                div()
                    .id(SharedString::from(format!("p-{pid}-{port}")))
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .px(px(5.))
                    .h(px(18.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .bg(theme::rgb(if l.ours { 0x1e2a3d } else { theme::HOVER }))
                    .hover(|s| s.bg(theme::rgb(0x2b4a7a)))
                    .text_size(px(11.))
                    .font_family(theme::MONO_FONT)
                    .text_color(theme::rgb(if l.ours { theme::ACCENT } else { theme::TEXT_MUTED }))
                    .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(if local { "localhost only · click to open" } else { "open to the network · click to open" })).into())
                    .on_click(move |_, _, cx| cx.open_url(&format!("http://localhost:{port}")))
                    .when(!local, |d| d.child(div().size(px(4.)).rounded_full().bg(theme::rgb(theme::MODIFIED))))
                    .child(port.to_string()),
            );
        }
        div()
            .id(SharedString::from(format!("listener-{pid}")))
            .group(group.clone())
            .flex()
            .items_start()
            .gap(px(8.))
            .mx(px(6.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(6.))
            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .min_w_0()
                            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme::rgb(theme::TEXT)).child(l.app.clone()))
                            .when_some(project, |d, p| d.child(div().flex_none().px(px(5.)).rounded(px(3.)).bg(theme::rgb(0x1d2b22)).text_size(px(10.)).text_color(theme::rgb(0xa8d8a0)).child(p))),
                    )
                    .when(show_cmd, |d| d.child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).font_family(theme::MONO_FONT).text_color(theme::rgb(theme::TEXT_MUTED)).child(cmd)))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .min_w_0()
                            .text_size(px(10.5))
                            .text_color(theme::rgb(theme::TEXT_FAINT))
                            .child(div().flex_none().font_family(theme::MONO_FONT).child(format!("pid {pid}")))
                            .when_some(cwd, |d, c| d.child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(c))),
                    ),
            )
            .child(ports)
            .child(
                div()
                    .id(SharedString::from(format!("kill-{pid}")))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(20.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .hover(|s| s.bg(theme::rgb(0x3a1f22)))
                    .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Kill process")).into())
                    .child(gpui::svg().path("icons/close.svg").size(px(11.)).text_color(theme::rgb(0xf0b3b8)))
                    .on_click(cx.listener({
                        let what = format!("{} (pid {pid}) on {}", l.app, l.ports.iter().map(|p| format!(":{}", p.0)).collect::<Vec<_>>().join(" "));
                        move |this, _: &ClickEvent, w, cx| this.confirm_kill(vec![pid], what.clone(), w, cx)
                    })),
            )
            .into_any_element()
    }
}

fn section(id: &'static str, title: &'static str, subtitle: &'static str, n: usize, first: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .pl(px(14.))
        .pr(px(8.))
        .h(px(30.))
        .when(!first, |d| d.mt(px(6.)).border_t_1().border_color(theme::rgb(theme::BORDER)))
        .child(div().flex_none().text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child(title))
        .child(div().flex_none().px(px(6.)).rounded_full().bg(theme::rgb(theme::HOVER)).text_size(px(10.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(n.to_string()))
        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(subtitle))
        .child(div().flex_1())
}

fn kill_all_button(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .px(px(6.))
        .h(px(20.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(theme::rgb(0xf0b3b8))
        .hover(|s| s.bg(theme::rgb(0x3a1f22)))
        .child("Kill all")
}

impl Render for PortsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let ours: Vec<Listener> = self.listeners.iter().filter(|l| l.ours).cloned().collect();
        let others: Vec<Listener> = self.listeners.iter().filter(|l| !l.ours && !l.system).cloned().collect();
        let system: Vec<Listener> = self.listeners.iter().filter(|l| !l.ours && l.system).cloned().collect();
        let show_system = self.show_system;

        let mut list = div().id("ports-panel").flex().flex_col().size_full().overflow_y_scroll().pb(px(16.));
        let ours_pids: Vec<i32> = ours.iter().map(|l| l.pid).collect();
        list = list.child(
            section("ports-ours", "JUAN-TERMINAL", "started from your terminals", ours.len(), true)
                .when(!ours_pids.is_empty(), |d| {
                    d.child(kill_all_button("ports-kill-ours").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        let n = ours_pids.len();
                        this.confirm_kill(ours_pids.clone(), format!("{n} process(es) started from juan-terminal"), w, cx)
                    })))
                })
                .child(icon_action("ports-refresh", "icons/fetch.svg", "Refresh").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx)))),
        );
        if let Some(e) = &self.error {
            list = list.child(div().mx(px(12.)).my(px(4.)).px(px(8.)).py(px(4.)).rounded(px(4.)).bg(theme::rgb(0x3a1f22)).text_size(px(11.5)).text_color(theme::rgb(0xf0b3b8)).child(e.clone()));
        }
        if ours.is_empty() && self.loaded {
            list = list.child(div().px(px(16.)).pb(px(6.)).text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("No dev servers running from juan-terminal."));
        }
        for l in &ours {
            list = list.child(self.render_listener(l, &home, cx));
        }

        let other_pids: Vec<i32> = others.iter().map(|l| l.pid).collect();
        list = list.child(section("ports-others", "OTHER APPS", "", others.len(), false).when(!other_pids.is_empty(), |d| {
            d.child(kill_all_button("ports-kill-others").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                let n = other_pids.len();
                this.confirm_kill(other_pids.clone(), format!("{n} process(es) from other apps"), w, cx)
            })))
        }));
        for l in &others {
            list = list.child(self.render_listener(l, &home, cx));
        }
        if !system.is_empty() {
            list = list.child(
                div()
                    .id("ports-system")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(14.))
                    .h(px(28.))
                    .mt(px(4.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .hover(|s| s.text_color(theme::rgb(theme::TEXT_MUTED)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.show_system = !this.show_system;
                        cx.notify();
                    }))
                    .child(gpui::svg().path(if show_system { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(8.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                    .child(format!("macOS services ({})", system.len())),
            );
            if show_system {
                for l in &system {
                    list = list.child(self.render_listener(l, &home, cx));
                }
            }
        }
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_commands_from_package_json() {
        let d = std::env::temp_dir().join(format!("jt-dev-{}", std::process::id()));
        std::fs::create_dir_all(d.join("web")).unwrap();
        std::fs::write(d.join("web/package.json"), r#"{"scripts":{"build":"vite build","dev":"vite --port 5173"}}"#).unwrap();
        std::fs::write(d.join("web/pnpm-lock.yaml"), "").unwrap();
        let c = dev_commands(&[("web".into(), d.join("web"))]);
        assert_eq!(c.len(), 1);
        // From the parent (monorepo layout) it's found one level down.
        let c2 = dev_commands(&[("mono".into(), d.clone())]);
        assert!(c2.iter().any(|x| x.contains("[mono/web]")), "{c2:?}");
        assert!(c[0].contains("pnpm run dev") && c[0].contains("vite --port 5173"), "{c:?}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn parses_lsof_and_dedupes_ipv4_ipv6() {
        let out = "p661\ncControlCenter\nf9\nn*:7000\nTST=LISTEN\nf10\nn*:7000\nf11\nn*:5000\np900\ncnode\nf20\nn127.0.0.1:5173\nf21\nn[::1]:5173\n";
        let m = parse_lsof(out);
        assert_eq!(m[&661].0, "ControlCenter");
        assert_eq!(m[&661].1, vec![(5000, "*".into()), (7000, "*".into())]);
        assert_eq!(m[&900].1, vec![(5173, "127.0.0.1".into())]);
    }

    #[test]
    fn system_and_short_command() {
        assert!(is_system("/System/Library/CoreServices/ControlCenter.app/Contents/MacOS/ControlCenter"));
        assert!(!is_system("/opt/homebrew/bin/node vite"));
        assert_eq!(app_name("/Applications/Visual Studio Code 2.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper", "x"), "Visual Studio Code 2");
        assert_eq!(app_name("claude", "claude"), "claude");
        assert_eq!(app_name("2.1.330", "claude"), "claude");
        assert_eq!(short_command("/opt/homebrew/bin/node /Users/x/app/node_modules/.bin/vite", Path::new("/Users/x")), "node ~/app/node_modules/.bin/vite");
    }

    #[test]
    fn scan_finds_a_real_listener_and_kill_works() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let found = scan().unwrap();
        let me = std::process::id() as i32;
        assert!(found.iter().any(|x| x.pid == me && x.ports.iter().any(|p| p.0 == port)), "port {port} not found");
        // A child of this process counts as "ours".
        let mut child = Command::new("/usr/bin/python3").args(["-c", "import socket,time;s=socket.socket();s.bind(('127.0.0.1',0));s.listen();time.sleep(30)"]).spawn().unwrap();
        let cpid = child.id() as i32;
        let ok = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            scan().unwrap().iter().any(|x| x.pid == cpid && x.ours)
        });
        assert!(ok, "child listener not reported as ours");
        kill(child.id() as i32).unwrap();
        let _ = child.wait();
        // Kill a child that listens.
        let mut child = Command::new("/usr/bin/python3").args(["-c", "import socket,time;s=socket.socket();s.bind(('127.0.0.1',0));s.listen();time.sleep(30)"]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        kill(child.id() as i32).unwrap();
        assert!(child.wait().is_ok());
    }
}

/// Text for Claude's `list_dev_servers` tool: local listeners, project ones first.
pub fn dev_servers_report(app_pid: i32, roots: &[(String, PathBuf)]) -> String {
    let list = match scan_for(app_pid) {
        Ok(l) => l,
        Err(e) => return format!("Could not list ports: {e:#}"),
    };
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let project_of = |cwd: &Option<PathBuf>| {
        let cwd = cwd.as_ref()?;
        roots.iter().filter(|(_, p)| cwd.starts_with(p)).max_by_key(|(_, p)| p.as_os_str().len()).map(|(n, _)| n.clone())
    };
    let line = |l: &Listener| {
        let urls: Vec<String> = l.ports.iter().map(|(p, _)| format!("http://localhost:{p}")).collect();
        let cwd = l.cwd.as_ref().map(|c| c.display().to_string()).unwrap_or_default();
        format!("{} — {} (pid {}, {}) cwd {}", urls.join(", "), l.app, l.pid, short_command(&l.command, &home), cwd)
    };
    let mut project = Vec::new();
    let mut other = Vec::new();
    for l in list.iter().filter(|l| !l.system) {
        match project_of(&l.cwd) {
            Some(name) => project.push(format!("- [{name}] {}", line(l))),
            None if l.ours => other.push(format!("- [started in juan-terminal] {}", line(l))),
            None => {}
        }
    }
    let mut out = String::new();
    if project.is_empty() {
        out.push_str("No dev server of this project is running. Don't fall back to production: start the right one yourself in the background (Bash with run_in_background), wait until its URL responds (e.g. poll with curl), then look at / screenshot http://localhost:PORT.\n");
        let cmds = dev_commands(roots);
        if !cmds.is_empty() {
            out.push_str("Start commands found in this project:\n");
            out.push_str(&cmds.join("\n"));
            out.push('\n');
        }
    } else {
        out.push_str("Dev servers of this project (use these localhost URLs to look at / screenshot / test the app instead of production):\n");
        out.push_str(&project.join("\n"));
        out.push('\n');
    }
    if !other.is_empty() {
        out.push_str("\nOther servers started from juan-terminal:\n");
        out.push_str(&other.join("\n"));
        out.push('\n');
    }
    out
}

/// `cd <repo> && <pm> run dev` for repos whose package.json has a dev/start script.
pub fn dev_commands(roots: &[(String, PathBuf)]) -> Vec<String> {
    // Monorepos keep apps in subfolders (front/, backend/, apps/web…): look two levels down.
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    for (name, root) in roots {
        dirs.push((name.clone(), root.clone()));
        let subs = |d: &PathBuf| -> Vec<PathBuf> {
            std::fs::read_dir(d)
                .map(|it| {
                    it.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| { let n = n.to_string_lossy(); !n.starts_with('.') && n != "node_modules" && n != "dist" && n != "target" }))
                        .collect()
                })
                .unwrap_or_default()
        };
        for a in subs(root) {
            for b in subs(&a) {
                if b.join("package.json").is_file() {
                    dirs.push((format!("{name}/{}/{}", a.file_name().unwrap().to_string_lossy(), b.file_name().unwrap().to_string_lossy()), b));
                }
            }
            if a.join("package.json").is_file() {
                dirs.push((format!("{name}/{}", a.file_name().unwrap().to_string_lossy()), a));
            }
        }
    }
    // Same folder reached from the project root and from the repo: keep the shorter label.
    dirs.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.len().cmp(&b.0.len())));
    dirs.dedup_by(|a, b| a.1 == b.1);
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (name, dir) in &dirs {
        let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let scripts = &v["scripts"];
        let Some(script) = ["dev", "start", "serve"].into_iter().find(|k| scripts.get(*k).is_some()) else { continue };
        let pm = if dir.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if dir.join("yarn.lock").exists() {
            "yarn"
        } else if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            "bun"
        } else {
            "npm"
        };
        let detail = scripts[script].as_str().unwrap_or_default();
        out.push(format!("- [{name}] cd {} && {pm} run {script}   ({detail})", dir.display()));
    }
    out
}
