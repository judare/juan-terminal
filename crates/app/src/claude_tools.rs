//! What Claude Code gets inside juan-terminal, grouped (Brave, WhatsApp, SSH, Moli, other MCP
//! servers, skills), and switching groups off so their tools — and their context — are gone
//! from new Claude sessions:
//! - our own MCP tools: the `--mcp-servers` server stops listing them (and their instructions);
//! - other MCP servers: `permissions.deny: ["mcp__<server>"]` in the settings the `claude`
//!   wrapper passes (only sessions started in juan-terminal);
//! - skills: their folder moves out of `~/.claude/skills` (global, reversible).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Tools of juan-terminal's own MCP server.
    Builtin,
    /// Another MCP server configured for Claude.
    Mcp,
    /// Skill folder(s) in `~/.claude/skills`.
    Skill,
}

#[derive(Clone)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub desc: String,
    pub kind: Kind,
    /// Tool names (builtin), the server name (MCP) or skill folder names.
    pub items: Vec<String>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn state_file() -> PathBuf {
    workspace_state::state_dir().join("claude-tools.json")
}

pub fn skills_dir() -> PathBuf {
    home().join(".claude/skills")
}

/// Where switched-off skills wait.
pub fn disabled_skills_dir() -> PathBuf {
    workspace_state::state_dir().join("skills-disabled")
}

pub fn disabled() -> BTreeSet<String> {
    std::fs::read_to_string(state_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("disabled").and_then(|d| d.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
        .unwrap_or_default()
}

fn save(set: &BTreeSet<String>) -> std::io::Result<()> {
    let p = state_file();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, json!({"disabled": set}).to_string())
}

fn names(tools: Vec<Value>) -> Vec<String> {
    tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from)).collect()
}

fn builtin_groups() -> Vec<Group> {
    let g = |id: &str, name: &str, desc: &str, items: Vec<String>| Group { id: id.into(), name: name.into(), desc: desc.into(), kind: Kind::Builtin, items };
    vec![
        g("brave", "Brave", "Your browser with your logins: tabs, open, screenshot, run JS", names(crate::brave::tools())),
        g("whatsapp", "WhatsApp", "List, read and search chats, media, send (with approval)", names(crate::wa_mcp::tools())),
        g("ssh", "SSH servers", "The project's servers: list them and run commands", vec!["list_servers".into(), "ssh_exec".into()]),
        g("dev-servers", "Dev servers", "Local ports of the project, so Claude uses localhost", vec!["list_dev_servers".into()]),
    ]
}

/// Tool names of our MCP server that are switched off (read by `--mcp-servers`).
pub fn hidden_builtin_tools() -> Vec<String> {
    let off = disabled();
    // Plugins switched off hide their tools too (WhatsApp, SSH servers).
    let plugin_off = |id: &str| match id {
        "whatsapp" => !crate::plugins::enabled(crate::plugins::WHATSAPP),
        "ssh" => !crate::plugins::enabled(crate::plugins::SSH),
        _ => false,
    };
    builtin_groups().into_iter().filter(|g| off.contains(&g.id) || plugin_off(&g.id)).flat_map(|g| g.items).collect()
}

fn skill_dirs(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("SKILL.md").is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    v.sort();
    v
}

fn skill_desc(name: &str) -> String {
    let md = [skills_dir(), disabled_skills_dir()].iter().find_map(|d| std::fs::read_to_string(d.join(name).join("SKILL.md")).ok()).unwrap_or_default();
    md.lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().trim_matches('"').to_string())
        .map(|d| if d.chars().count() > 110 { format!("{}…", d.chars().take(110).collect::<String>()) } else { d })
        .unwrap_or_default()
}

fn skill_groups() -> Vec<Group> {
    let mut all: BTreeSet<String> = skill_dirs(&skills_dir()).into_iter().collect();
    all.extend(skill_dirs(&disabled_skills_dir()));
    let moli: Vec<String> = all.iter().filter(|s| crate::moli::SKILLS.contains(&s.as_str())).cloned().collect();
    let mut out = Vec::new();
    if !moli.is_empty() {
        out.push(Group { id: "skill:moli".into(), name: "Moli".into(), desc: "Headless browser: fetch pages, web search, CDP server".into(), kind: Kind::Skill, items: moli });
    }
    for s in all.iter().filter(|s| !crate::moli::SKILLS.contains(&s.as_str())) {
        out.push(Group { id: format!("skill:{s}"), name: s.clone(), desc: skill_desc(s), kind: Kind::Skill, items: vec![s.clone()] });
    }
    out
}

/// MCP servers from `~/.claude.json` (user scope and the projects' repos) and the repos' `.mcp.json`.
fn mcp_groups(repos: &[PathBuf]) -> Vec<Group> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let keys = |v: Option<&Value>| -> Vec<String> { v.and_then(|m| m.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default() };
    if let Some(cfg) = std::fs::read_to_string(home().join(".claude.json")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        found.extend(keys(cfg.get("mcpServers")));
        for r in repos {
            found.extend(keys(cfg.get("projects").and_then(|p| p.get(r.to_string_lossy().as_ref())).and_then(|p| p.get("mcpServers"))));
        }
    }
    for r in repos {
        if let Some(v) = std::fs::read_to_string(r.join(".mcp.json")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            found.extend(keys(v.get("mcpServers")));
        }
    }
    found.remove("juan-terminal");
    found.into_iter().map(|s| Group { id: format!("mcp:{s}"), name: s.clone(), desc: "MCP server".into(), kind: Kind::Mcp, items: vec![s] }).collect()
}

pub fn groups(repos: &[PathBuf]) -> Vec<Group> {
    let mut v = builtin_groups();
    v.extend(mcp_groups(repos));
    v.extend(skill_groups());
    v
}

/// Claude's tool-name form of an MCP server name.
fn mcp_prefix(server: &str) -> String {
    let s: String = server.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    format!("mcp__{s}")
}

/// Settings for the `claude` wrapper: our hooks plus deny rules for what's switched off.
pub fn settings_json() -> String {
    let mut v: Value = serde_json::from_str(&claude_ide::hook_settings_json()).unwrap_or_else(|_| json!({}));
    let off = disabled();
    let mut deny: Vec<String> = hidden_builtin_tools().iter().map(|t| format!("mcp__juan-terminal__{t}")).collect();
    deny.extend(off.iter().filter_map(|id| id.strip_prefix("mcp:")).map(mcp_prefix));
    if !deny.is_empty() {
        v["permissions"] = json!({"deny": deny});
    }
    v.to_string()
}

/// Rewrite the wrapper's settings file (if the shell integration created it).
fn refresh_settings() {
    let p = workspace_state::state_dir().join("claude-hooks.json");
    if p.exists() {
        let _ = std::fs::write(p, settings_json());
    }
}

fn move_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d)?;
    }
    if to.exists() {
        std::fs::remove_dir_all(to)?;
    }
    std::fs::rename(from, to)
}

/// Switch a group on/off. Applies to Claude sessions started afterwards.
pub fn set_enabled(group: &Group, enabled: bool) -> std::io::Result<()> {
    if group.kind == Kind::Skill {
        for s in &group.items {
            let (on, off) = (skills_dir().join(s), disabled_skills_dir().join(s));
            if enabled { move_dir(&off, &on)? } else { move_dir(&on, &off)? }
        }
    }
    let mut set = disabled();
    if enabled {
        set.remove(&group.id);
    } else {
        set.insert(group.id.clone());
    }
    save(&set)?;
    refresh_settings();
    Ok(())
}

pub fn is_enabled(group: &Group, off: &BTreeSet<String>) -> bool {
    match group.kind {
        // The folder is the truth (it can be reinstalled/removed from elsewhere).
        Kind::Skill => group.items.iter().any(|s| skills_dir().join(s).join("SKILL.md").is_file()),
        _ => !off.contains(&group.id),
    }
}

#[cfg(test)]
mod tests {
    use super::mcp_prefix;

    #[test]
    fn prefix() {
        assert_eq!(mcp_prefix("blokay"), "mcp__blokay");
        assert_eq!(mcp_prefix("my server.v2"), "mcp__my_server_v2");
    }
}
