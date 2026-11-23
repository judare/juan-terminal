//! Obscura (https://github.com/h4ckf0r0day/obscura) integration: locate or install the binary
//! and register its stdio MCP server (`obscura mcp`) with Claude Code. Obscura is never started
//! by this app; Claude Code launches the MCP server on demand.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub const RELEASE_URL: &str = "https://github.com/h4ckf0r0day/obscura/releases/latest/download/obscura-aarch64-macos.tar.gz";
pub const RELEASE_SIZE_HINT: &str = "≈67 MB";
pub const MCP_NAME: &str = "obscura";

/// Tools exposed by `obscura mcp` (from the project's docs/Use-the-MCP-server.md).
pub const TOOLS: &[(&str, &str)] = &[
    ("browser_navigate", "Open a URL (waitUntil: load / domcontentloaded / networkidle0)"),
    ("browser_snapshot", "URL, title, visible text and element references"),
    ("browser_screenshot", "Current viewport as PNG"),
    ("browser_click / fill / type / press_key", "Interact with elements"),
    ("browser_evaluate", "Run JavaScript in the page"),
    ("browser_console_messages", "Console output of the page"),
    ("browser_network_requests", "Requests made by the page"),
    ("browser_markdown / links / extract", "Page content as markdown, links or structured data"),
    ("browser_tab_new / list / switch / close", "Tabs"),
    ("browser_pdf", "Current page as PDF"),
];

pub fn app_bin_dir() -> PathBuf {
    workspace_state::state_dir().join("bin")
}

pub fn locate() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    code_quality::which("obscura")
        .into_iter()
        .chain([app_bin_dir().join("obscura"), home.join(".local/bin/obscura")])
        .find(|p| p.is_file())
}

pub fn version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").stdin(Stdio::null()).output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

pub fn claude_bin() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    code_quality::which("claude").into_iter().chain([home.join(".local/bin/claude")]).find(|p| p.is_file())
}

/// Whether Claude Code has an MCP server named `obscura` (any scope).
pub fn registered() -> Result<bool> {
    let claude = claude_bin().context("`claude` CLI not found")?;
    let out = Command::new(claude).args(["mcp", "get", MCP_NAME]).stdin(Stdio::null()).output()?;
    Ok(out.status.success())
}

/// JSON config for `claude mcp add-json` (stdio server with its env).
/// Private networks are allowed so Claude can inspect local dev servers (localhost/LAN).
pub fn mcp_json(bin: &Path) -> String {
    serde_json::json!({
        "type": "stdio",
        "command": bin.display().to_string(),
        "args": ["mcp"],
        "env": {"OBSCURA_ALLOW_PRIVATE_NETWORK": "1"},
    })
    .to_string()
}

/// `claude mcp add-json -s user obscura '<json>'`. (Not `mcp add -e …`: `-e` is variadic
/// and swallows the server name.)
pub fn register(bin: &Path) -> Result<String> {
    let claude = claude_bin().context("`claude` CLI not found")?;
    let out = Command::new(claude).args(["mcp", "add-json", "-s", "user", MCP_NAME, &mcp_json(bin)]).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
        bail!("{msg}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn unregister() -> Result<String> {
    let claude = claude_bin().context("`claude` CLI not found")?;
    let out = Command::new(claude).args(["mcp", "remove", "-s", "user", MCP_NAME]).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Download the latest macOS arm64 release into the app's bin dir. Blocking.
pub fn install() -> Result<PathBuf> {
    let dir = app_bin_dir();
    let work = dir.join(".download");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let archive = work.join("obscura.tar.gz");
    let status = Command::new("curl").args(["-fsSL", "-o"]).arg(&archive).arg(RELEASE_URL).status().context("curl failed")?;
    if !status.success() {
        bail!("Download failed ({RELEASE_URL})");
    }
    let status = Command::new("tar").arg("xzf").arg(&archive).arg("-C").arg(&work).status()?;
    if !status.success() {
        bail!("Could not extract the archive");
    }
    let mut installed = None;
    for name in ["obscura", "obscura-worker"] {
        let found = find_file(&work, name).with_context(|| format!("`{name}` not found in the release archive"))?;
        let dest = dir.join(name);
        std::fs::rename(&found, &dest)?;
        Command::new("chmod").arg("+x").arg(&dest).status()?;
        if name == "obscura" {
            installed = Some(dest);
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    let bin = installed.context("install failed")?;
    version(&bin).context("installed binary does not run")?;
    Ok(bin)
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {

    #[test]
    fn mcp_json_is_a_stdio_server_with_env() {
        let v: serde_json::Value = serde_json::from_str(&super::mcp_json(std::path::Path::new("/a b/obscura"))).unwrap();
        assert_eq!(v["command"], "/a b/obscura");
        assert_eq!(v["args"][0], "mcp");
        assert_eq!(v["env"]["OBSCURA_ALLOW_PRIVATE_NETWORK"], "1");
    }

    use super::*;

    #[test]
    fn finds_nested_binary() {
        let dir = std::env::temp_dir().join(format!("jt-obs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/obscura"), "x").unwrap();
        assert_eq!(find_file(&dir, "obscura"), Some(dir.join("a/b/obscura")));
        assert_eq!(find_file(&dir, "obscura-worker"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
