//! Plugins: features that can be switched on/off and configured from the Plugins view.
//!
//! * Built-in plugins ship with the app (music/videos, WhatsApp, 2FA, SSH servers, AI usage,
//!   website blocker). They are installed and on by default; their version is the app's and
//!   they update with it.
//! * External plugins live in `<state>/plugins/<folder>/plugin.json` (see docs/PLUGINS.md).
//!   They are installed from a git URL (or copied there by hand), updated with `git pull` and
//!   can add commands (shell commands run in a new terminal tab, listed in ⌘⇧P) and an MCP
//!   server for the Claude sessions started in the app.
//! * State (on/off and settings) is in `<state>/plugins.json`. Read through a process-wide
//!   cache, so checks are cheap anywhere (UI, background threads, the `--mcp-servers` child,
//!   which reads the file itself).
//! * Turning the website blocker (or one of its settings) off takes 24 h, like removing a
//!   blocked site: no impulse unblocking. Turning it on is immediate.

//!
//! Layout: one folder per built-in plugin, `plugins/<plugin>/mod.rs` with its `manifest()`, an
//! `ABOUT` text for its page and its code (`media/`, `whatsapp/`, `google/`, `http/`,
//! `clipboard/`, `two_factor/`, `ssh/`, `ai_usage/`, `website_blocker/`); `panel.rs` is the
//! Plugins list in the sidebar and `detail.rs` the page each plugin opens in a tab. The host (workspace, launcher, MCP server) asks
//! `plugins::enabled(id)` / `plugins::setting(id, key)` at each integration point.

pub mod ai_usage;
pub mod clipboard;
pub mod detail;
pub mod google;
pub mod http;
pub mod media;
pub mod panel;
pub mod ssh;
pub mod two_factor;
pub mod website_blocker;
pub mod whatsapp;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

pub const MEDIA: &str = "media";
pub const WHATSAPP: &str = "whatsapp";
pub const TWO_FACTOR: &str = "2fa";
pub const SSH: &str = "ssh";
pub const AI_USAGE: &str = "ai-usage";
pub const WEBSITE_BLOCKER: &str = "website-blocker";
pub const CLIPBOARD: &str = "clipboard";
pub const GOOGLE: &str = "google";
pub const HTTP: &str = "http";

/// Turning these off only takes effect after this long.
const OFF_DELAY_SECS: i64 = 24 * 3600;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettingSpec {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub help: String,
    #[serde(default = "yes")]
    pub default: bool,
    /// A choice instead of an on/off switch: the values it can take (the first one is the
    /// default unless `default_choice` says otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_choice: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl SettingSpec {
    pub fn is_choice(&self) -> bool {
        !self.choices.is_empty()
    }

    fn default_value(&self) -> String {
        if self.choices.iter().any(|c| c.value == self.default_choice) { self.default_choice.clone() } else { self.choices.first().map(|c| c.value.clone()).unwrap_or_default() }
    }
}

fn yes() -> bool {
    true
}

/// A command an external plugin adds to the command palette: `run` goes to `sh -c` in a new
/// terminal tab (project folder as cwd).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PluginCommand {
    pub title: String,
    pub run: String,
}

/// MCP server an external plugin offers to Claude sessions started in the app.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PluginMcp {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    /// Icon path among the app's icons (built-ins); external plugins get a generic one.
    #[serde(skip)]
    pub icon: &'static str,
    #[serde(default)]
    pub settings: Vec<SettingSpec>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub mcp: Option<PluginMcp>,
    #[serde(default = "yes")]
    pub default_enabled: bool,
    #[serde(skip)]
    pub builtin: bool,
    /// Turning off waits 24 h (website blocker).
    #[serde(skip)]
    pub delayed_off: bool,
    /// Folder of an external plugin.
    #[serde(skip)]
    pub dir: Option<PathBuf>,
}

/// A boolean setting, on by default.
pub(crate) fn spec(key: &str, label: &str, help: &str) -> SettingSpec {
    SettingSpec { key: key.into(), label: label.into(), help: help.into(), default: true, choices: Vec::new(), default_choice: String::new() }
}

/// A boolean setting, off by default.
pub(crate) fn spec_off(key: &str, label: &str, help: &str) -> SettingSpec {
    SettingSpec { default: false, ..spec(key, label, help) }
}

/// A choice setting: `choices` are (value, label); `default` is one of the values.
pub(crate) fn choice(key: &str, label: &str, help: &str, choices: &[(&str, &str)], default: &str) -> SettingSpec {
    SettingSpec {
        choices: choices.iter().map(|(v, l)| Choice { value: v.to_string(), label: l.to_string() }).collect(),
        default_choice: default.into(),
        ..spec(key, label, help)
    }
}

/// Manifest of a plugin that ships with the app (its version is the app's).
pub(crate) fn builtin(id: &str, name: &str, icon: &'static str, description: &str, settings: Vec<SettingSpec>) -> Manifest {
    Manifest {
        id: id.into(),
        name: name.into(),
        description: description.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        author: "Fragua".into(),
        icon,
        settings,
        commands: Vec::new(),
        mcp: None,
        default_enabled: true,
        builtin: true,
        delayed_off: false,
        dir: None,
    }
}

/// Built-in plugins, in the order the Plugins view lists them.
pub fn builtins() -> Vec<Manifest> {
    vec![
        media::manifest(),
        whatsapp::manifest(),
        google::manifest(),
        http::manifest(),
        clipboard::manifest(),
        two_factor::manifest(),
        ssh::manifest(),
        ai_usage::manifest(),
        website_blocker::manifest(),
    ]
}

pub fn plugins_dir() -> PathBuf {
    workspace_state::state_dir().join("plugins")
}

fn state_file() -> PathBuf {
    workspace_state::state_dir().join("plugins.json")
}

/// External plugins found in `plugins_dir()` (folders with a valid `plugin.json`).
pub fn external() -> Vec<Manifest> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(plugins_dir()) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        if let Some(m) = read_manifest(&dir) {
            if !builtins().iter().any(|b| b.id == m.id) && !out.iter().any(|o: &Manifest| o.id == m.id) {
                out.push(m);
            }
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

pub fn read_manifest(dir: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(dir.join("plugin.json")).ok()?;
    let mut m: Manifest = serde_json::from_str(&text).ok()?;
    if m.id.trim().is_empty() || !m.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    m.icon = "icons/package.svg";
    m.builtin = false;
    m.dir = Some(dir.to_path_buf());
    Some(m)
}

/// Long description of a built-in plugin for its page (Markdown).
pub fn about(id: &str) -> &'static str {
    match id {
        MEDIA => media::ABOUT,
        WHATSAPP => whatsapp::ABOUT,
        TWO_FACTOR => two_factor::ABOUT,
        SSH => ssh::ABOUT,
        AI_USAGE => ai_usage::ABOUT,
        WEBSITE_BLOCKER => website_blocker::ABOUT,
        CLIPBOARD => clipboard::ABOUT,
        GOOGLE => google::ABOUT,
        HTTP => http::ABOUT,
        _ => "",
    }
}

/// The plugin's own settings view, shown under its switches on its page.
pub fn config_view(id: &str, ws: gpui::WeakEntity<crate::workspace::Workspace>, window: &mut gpui::Window, cx: &mut gpui::App) -> Option<gpui::AnyView> {
    use gpui::AppContext;
    match id {
        GOOGLE => Some(cx.new(|cx| google::config::GoogleConfig::new(window, cx)).into()),
        HTTP => Some(cx.new(|cx| http::config::HttpConfig::new(ws, cx)).into()),
        CLIPBOARD => Some(cx.new(clipboard::ClipboardConfig::new).into()),
        MEDIA => Some(cx.new(media::MediaConfig::new).into()),
        _ => None,
    }
}

/// The plugin needs setting up before it does anything (its page opens on Configuración).
pub fn needs_setup(id: &str) -> bool {
    id == GOOGLE && !google::auth::connected()
}

/// Built-ins first, then external plugins.
pub fn all() -> Vec<Manifest> {
    let mut v = builtins();
    v.extend(external());
    v
}

pub fn manifest(id: &str) -> Option<Manifest> {
    all().into_iter().find(|m| m.id == id)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Stored {
    /// Explicit on/off; missing = the manifest's default.
    #[serde(default)]
    enabled: BTreeMap<String, bool>,
    /// plugin id → setting key → value.
    #[serde(default)]
    settings: BTreeMap<String, BTreeMap<String, bool>>,
    /// plugin id → choice setting key → chosen value.
    #[serde(default)]
    choices: BTreeMap<String, BTreeMap<String, String>>,
    /// "<id>" or "<id>.<key>" → Unix time it was switched off (delayed plugins only).
    #[serde(default)]
    off_since: BTreeMap<String, i64>,
}

static CACHE: RwLock<Option<Stored>> = RwLock::new(None);

fn load() -> Stored {
    std::fs::read_to_string(state_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn stored() -> Stored {
    if let Some(s) = CACHE.read().ok().and_then(|c| c.clone()) {
        return s;
    }
    let s = load();
    if let Ok(mut c) = CACHE.write() {
        *c = Some(s.clone());
    }
    s
}

fn save(s: &Stored) -> std::io::Result<()> {
    let path = state_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    if let Ok(mut c) = CACHE.write() {
        *c = Some(s.clone());
    }
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn is_delayed(id: &str) -> bool {
    id == WEBSITE_BLOCKER
}

fn default_enabled(id: &str) -> bool {
    is_builtin(id) || external().iter().any(|m| m.id == id && m.default_enabled)
}

fn effective(stored_value: Option<bool>, default: bool, off_key: &str, s: &Stored, delayed: bool) -> bool {
    match stored_value.unwrap_or(default) {
        true => true,
        false if delayed => s.off_since.get(off_key).is_some_and(|t| now() - t < OFF_DELAY_SECS),
        false => false,
    }
}

/// The switch as the user left it (what the Plugins view shows).
pub fn wanted(id: &str) -> bool {
    let s = stored();
    s.enabled.get(id).copied().unwrap_or_else(|| default_enabled(id))
}

fn is_builtin(id: &str) -> bool {
    [MEDIA, WHATSAPP, TWO_FACTOR, SSH, AI_USAGE, WEBSITE_BLOCKER, CLIPBOARD, GOOGLE, HTTP].contains(&id)
}

/// Whether the plugin is active right now.
pub fn enabled(id: &str) -> bool {
    let s = stored();
    match s.enabled.get(id).copied() {
        Some(v) => effective(Some(v), v, id, &s, is_delayed(id)),
        None => default_enabled(id),
    }
}

/// The setting as the user left it.
pub fn setting_wanted(id: &str, key: &str) -> bool {
    let s = stored();
    s.settings.get(id).and_then(|m| m.get(key)).copied().unwrap_or_else(|| setting_default(id, key))
}

fn setting_default(id: &str, key: &str) -> bool {
    let specs = if is_builtin(id) { builtins() } else { external() };
    specs.into_iter().find(|m| m.id == id).and_then(|m| m.settings.into_iter().find(|x| x.key == key)).map(|x| x.default).unwrap_or(true)
}

/// A setting of an active plugin (false when the plugin is off).
pub fn setting(id: &str, key: &str) -> bool {
    if !enabled(id) {
        return false;
    }
    let s = stored();
    match s.settings.get(id).and_then(|m| m.get(key)).copied() {
        Some(v) => effective(Some(v), v, &format!("{id}.{key}"), &s, is_delayed(id)),
        None => setting_default(id, key),
    }
}

fn setting_spec(id: &str, key: &str) -> Option<SettingSpec> {
    let specs = if is_builtin(id) { builtins() } else { external() };
    specs.into_iter().find(|m| m.id == id)?.settings.into_iter().find(|x| x.key == key)
}

/// Value of a choice setting (its default when unset or no longer offered).
pub fn choice_value(id: &str, key: &str) -> String {
    let spec = setting_spec(id, key);
    let stored = stored().choices.get(id).and_then(|m| m.get(key)).cloned();
    match (stored, spec) {
        (Some(v), Some(sp)) if sp.choices.iter().any(|c| c.value == v) => v,
        (_, Some(sp)) => sp.default_value(),
        (Some(v), None) => v,
        (None, None) => String::new(),
    }
}

/// A numeric choice (e.g. days); `None` for non-numeric values like "never".
pub fn choice_number(id: &str, key: &str) -> Option<u64> {
    choice_value(id, key).parse().ok()
}

pub fn set_choice(id: &str, key: &str, value: &str) -> std::io::Result<()> {
    let mut s = load();
    s.choices.entry(id.to_string()).or_default().insert(key.to_string(), value.to_string());
    save(&s)
}

/// When a delayed "off" switch takes effect (Unix time), if it's still pending.
pub fn pending_off(id: &str, key: Option<&str>) -> Option<i64> {
    let s = stored();
    let k = key.map(|k| format!("{id}.{k}")).unwrap_or_else(|| id.to_string());
    let wanted_off = match key {
        Some(k) => !setting_wanted(id, k),
        None => !wanted(id),
    };
    if !wanted_off || !is_delayed(id) {
        return None;
    }
    s.off_since.get(&k).map(|t| t + OFF_DELAY_SECS).filter(|t| *t > now())
}

pub fn set_enabled(id: &str, on: bool) -> std::io::Result<()> {
    let mut s = load();
    let was = s.enabled.get(id).copied().unwrap_or_else(|| default_enabled(id));
    s.enabled.insert(id.to_string(), on);
    if is_delayed(id) {
        if on {
            s.off_since.remove(id);
        } else if was {
            s.off_since.insert(id.to_string(), now());
        }
    }
    save(&s)
}

pub fn set_setting(id: &str, key: &str, on: bool) -> std::io::Result<()> {
    let mut s = load();
    let was = setting_wanted(id, key);
    s.settings.entry(id.to_string()).or_default().insert(key.to_string(), on);
    let k = format!("{id}.{key}");
    if is_delayed(id) {
        if on {
            s.off_since.remove(&k);
        } else if was {
            s.off_since.insert(k, now());
        }
    }
    save(&s)
}

/// Re-read `plugins.json` (it may have been edited by hand or by another process).
pub fn reload() {
    if let Ok(mut c) = CACHE.write() {
        *c = None;
    }
}

/// Environment for an external plugin's commands and MCP server.
pub fn plugin_env(m: &Manifest) -> Vec<(String, String)> {
    let mut env = vec![
        ("FRAGUA_PLUGIN_ID".to_string(), m.id.clone()),
        ("FRAGUA_PLUGIN_DIR".to_string(), m.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default()),
    ];
    for s in &m.settings {
        let name: String = s.key.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
        let value = if s.is_choice() { choice_value(&m.id, &s.key) } else if setting(&m.id, &s.key) { "1".into() } else { "0".into() };
        env.push((format!("FRAGUA_SETTING_{name}"), value));
    }
    env
}

/// MCP servers of the active external plugins (`mcpServers` entries, keyed `plugin-<id>`).
pub fn mcp_servers() -> Vec<(String, serde_json::Value)> {
    external()
        .into_iter()
        .filter(|m| enabled(&m.id))
        .filter_map(|m| {
            let mcp = m.mcp.clone()?;
            let dir = m.dir.clone().unwrap_or_default();
            // Relative commands run from the plugin folder.
            let command = if mcp.command.starts_with("./") { dir.join(&mcp.command).display().to_string() } else { mcp.command.clone() };
            let mut env: serde_json::Map<String, serde_json::Value> = plugin_env(&m).into_iter().map(|(k, v)| (k, v.into())).collect();
            env.extend(mcp.env.into_iter().map(|(k, v)| (k, v.into())));
            Some((format!("plugin-{}", m.id), serde_json::json!({"type": "stdio", "command": command, "args": mcp.args, "env": env, "cwd": dir.display().to_string()})))
        })
        .collect()
}

/// `git clone <url>` into the plugins folder. Returns the installed manifest.
pub fn install_from_git(url: &str) -> anyhow::Result<Manifest> {
    let url = url.trim();
    anyhow::ensure!(!url.is_empty() && !url.starts_with('-'), "Pega la URL git del plugin");
    let base = plugins_dir();
    std::fs::create_dir_all(&base)?;
    let stem = url.trim_end_matches('/').trim_end_matches(".git").rsplit(['/', ':']).next().unwrap_or("plugin");
    let stem: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    let stem = if stem.is_empty() { "plugin".to_string() } else { stem };
    let mut dir = base.join(&stem);
    let mut n = 2;
    while dir.exists() {
        dir = base.join(format!("{stem}-{n}"));
        n += 1;
    }
    let out = std::process::Command::new("git").args(["clone", "--depth", "1", "--", url]).arg(&dir).output()?;
    anyhow::ensure!(out.status.success(), "git clone: {}", String::from_utf8_lossy(&out.stderr).trim());
    match read_manifest(&dir) {
        Some(m) if builtins().iter().any(|b| b.id == m.id) || external().iter().filter(|o| o.id == m.id).count() > 1 => {
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::bail!("Ya hay un plugin con id “{}”", m.id)
        }
        Some(m) => Ok(m),
        None => {
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::bail!("El repositorio no tiene un plugin.json válido (ver docs/PLUGINS.md)")
        }
    }
}

/// `git pull --ff-only` in an external plugin's folder. Returns git's summary line.
pub fn update(m: &Manifest) -> anyhow::Result<String> {
    let dir = m.dir.clone().ok_or_else(|| anyhow::anyhow!("Los plugins incluidos se actualizan con Fragua"))?;
    anyhow::ensure!(dir.join(".git").exists(), "No se instaló desde git: actualízalo a mano en {}", dir.display());
    let out = std::process::Command::new("git").arg("-C").arg(&dir).args(["pull", "--ff-only"]).output()?;
    anyhow::ensure!(out.status.success(), "git pull: {}", String::from_utf8_lossy(&out.stderr).trim());
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().last().unwrap_or("").trim().to_string())
}

/// Remove an external plugin: its folder goes to the Trash.
pub fn uninstall(m: &Manifest) -> anyhow::Result<()> {
    let dir = m.dir.clone().ok_or_else(|| anyhow::anyhow!("Los plugins incluidos no se pueden quitar, solo desactivar"))?;
    let script = format!("tell application \"Finder\" to delete POSIX file \"{}\"", dir.display().to_string().replace('\\', "\\\\").replace('"', "\\\""));
    let out = std::process::Command::new("osascript").args(["-e", &script]).output()?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    let mut s = load();
    s.enabled.remove(&m.id);
    s.settings.remove(&m.id);
    s.choices.remove(&m.id);
    save(&s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_are_unique_and_on_by_default() {
        let b = builtins();
        let mut ids: Vec<&str> = b.iter().map(|m| m.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), b.len());
        assert!(b.iter().all(|m| m.default_enabled && is_builtin(&m.id)));
    }

    #[test]
    fn delayed_off_stays_on_for_a_day() {
        let mut s = Stored::default();
        s.off_since.insert("x".into(), now() - 60);
        assert!(effective(Some(false), true, "x", &s, true));
        s.off_since.insert("x".into(), now() - OFF_DELAY_SECS - 1);
        assert!(!effective(Some(false), true, "x", &s, true));
        assert!(!effective(Some(false), true, "x", &s, false));
        assert!(effective(None, true, "x", &s, true));
    }

    #[test]
    fn manifest_parses() {
        let m: Manifest = serde_json::from_str(r#"{"id":"hello","name":"Hello","commands":[{"title":"Hi","run":"echo hi"}],"settings":[{"key":"loud","label":"Loud","default":false}]}"#).unwrap();
        assert_eq!(m.commands[0].run, "echo hi");
        assert!(!m.settings[0].default);
        assert!(m.default_enabled);
        assert!(m.mcp.is_none());
    }
}
