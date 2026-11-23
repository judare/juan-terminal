//! Moli (https://github.com/lexmount/moli) integration: locate or install the `moli` binary and
//! install Moli's official Claude Code skills (`moli-webfetch`, `moli-websearch`,
//! `moli-cdp-server`) into `~/.claude/skills`. Moli has no MCP server: Claude drives it through
//! the CLI (`moli fetch`) or its CDP server (`moli serve`), guided by those skills. This app never
//! starts Moli itself.
//!
//! Also cleans up the previous Obscura integration (user-scope MCP server + downloaded binaries).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub const REPO: &str = "lexmount/moli";
pub const RELEASE_SIZE_HINT: &str = "≈42 MB";
pub const SKILLS: &[&str] = &["moli-webfetch", "moli-websearch", "moli-cdp-server"];
/// Written into each installed skill dir: the git ref the skill was fetched from.
const SKILL_MARKER: &str = ".juan-terminal-ref";

/// What Claude gets (one line per skill).
pub const SKILL_INFO: &[(&str, &str)] = &[
    ("moli-webfetch", "moli fetch: JS-rendered page as markdown / semantic tree / JSON, screenshot, PDF, --eval"),
    ("moli-websearch", "Keyword and reverse-image search across engines, then verify sources with moli fetch"),
    ("moli-cdp-server", "moli serve on 127.0.0.1:9222 for Playwright / Puppeteer / raw CDP automation"),
];

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Same place the official installer uses, so the skills (which resolve `moli` from `PATH`, then
/// `~/.local/bin/moli`) always find it.
pub fn install_dir() -> PathBuf {
    home().join(".local/bin")
}

pub fn skills_dir() -> PathBuf {
    home().join(".claude/skills")
}

pub fn locate() -> Option<PathBuf> {
    code_quality::which("moli").into_iter().chain([install_dir().join("moli")]).find(|p| p.is_file())
}

/// `moli 1.1.27` → `1.1.27`.
pub fn version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").stdin(Stdio::null()).output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let v = v.strip_prefix("moli ").unwrap_or(&v).trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn release_asset() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => bail!("unsupported architecture: {other}"),
    };
    Ok(format!("moli-{arch}-apple-darwin.tar.gz"))
}

pub fn release_url() -> Result<String> {
    Ok(format!("https://github.com/{REPO}/releases/latest/download/{}", release_asset()?))
}

/// Download the latest macOS release into `~/.local/bin/moli`. Blocking.
pub fn install() -> Result<PathBuf> {
    let url = release_url()?;
    let dir = install_dir();
    std::fs::create_dir_all(&dir)?;
    let work = dir.join(".moli-download");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = (|| {
        let archive = work.join("moli.tar.gz");
        let status = Command::new("curl").args(["-fsSL", "-o"]).arg(&archive).arg(&url).status().context("curl failed")?;
        if !status.success() {
            bail!("Download failed ({url})");
        }
        let status = Command::new("tar").arg("xzf").arg(&archive).arg("-C").arg(&work).status()?;
        if !status.success() {
            bail!("Could not extract the archive");
        }
        let found = find_file(&work, "moli").context("`moli` not found in the release archive")?;
        let dest = dir.join("moli");
        std::fs::rename(&found, &dest)?;
        Command::new("chmod").arg("+x").arg(&dest).status()?;
        version(&dest).context("installed binary does not run")?;
        Ok(dest)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
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

// ---- skills ------------------------------------------------------------------------------

/// Git ref the installed skills came from (`v1.1.27`), `Some("")` if installed by hand,
/// `None` if any skill is missing.
pub fn skills_ref() -> Option<String> {
    let mut found = None;
    for s in SKILLS {
        let dir = skills_dir().join(s);
        if !dir.join("SKILL.md").is_file() {
            return None;
        }
        let r = std::fs::read_to_string(dir.join(SKILL_MARKER)).unwrap_or_default().trim().to_string();
        found.get_or_insert(r);
    }
    found
}

/// Ref to fetch skills from: the tag matching the installed binary, so skill docs and CLI agree.
pub fn ref_for(version: Option<&str>) -> String {
    version.map(|v| format!("v{v}")).unwrap_or_else(|| "main".into())
}

fn curl(url: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl").args(["-fsSL", "-H", "Accept: application/vnd.github+json", url]).stdin(Stdio::null()).output().context("curl failed")?;
    if !out.status.success() {
        bail!("GET {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// `skills/<name>/...` blob paths for our skills from a git tree listing.
fn skill_files(tree: &serde_json::Value) -> Vec<String> {
    tree["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "blob")
        .filter_map(|e| e["path"].as_str())
        .filter(|p| SKILLS.iter().any(|s| p.starts_with(&format!("skills/{s}/"))))
        .map(str::to_string)
        .collect()
}

/// Download Moli's skills at `git_ref` into `~/.claude/skills/<name>/`, replacing old copies.
/// Staged first so a failed download leaves the previous install intact. Blocking.
pub fn install_skills(git_ref: &str) -> Result<String> {
    let tree: serde_json::Value = serde_json::from_slice(&curl(&format!("https://api.github.com/repos/{REPO}/git/trees/{git_ref}?recursive=1"))?)?;
    let files = skill_files(&tree);
    for s in SKILLS {
        if !files.iter().any(|f| *f == format!("skills/{s}/SKILL.md")) {
            bail!("skill `{s}` not found in {REPO}@{git_ref}");
        }
    }
    let root = skills_dir();
    std::fs::create_dir_all(&root)?;
    let stage = root.join(".moli-staging");
    let _ = std::fs::remove_dir_all(&stage);
    let result = (|| {
        for f in &files {
            let rel = f.strip_prefix("skills/").unwrap_or(f);
            if rel.split('/').any(|c| c == ".." || c.is_empty()) {
                bail!("unexpected path in repo: {f}");
            }
            let bytes = curl(&format!("https://raw.githubusercontent.com/{REPO}/{git_ref}/{f}"))?;
            let dest = stage.join(rel);
            std::fs::create_dir_all(dest.parent().unwrap())?;
            std::fs::write(dest, bytes)?;
        }
        for s in SKILLS {
            std::fs::write(stage.join(s).join(SKILL_MARKER), git_ref)?;
            let dest = root.join(s);
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::rename(stage.join(s), &dest)?;
        }
        Ok(format!("Installed {} skills ({git_ref}) into {}", SKILLS.len(), root.display()))
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

pub fn remove_skills() -> Result<String> {
    for s in SKILLS {
        let dir = skills_dir().join(s);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
    }
    Ok("Removed Moli skills".into())
}

// ---- legacy Obscura cleanup -----------------------------------------------------------

const OBSCURA_MCP: &str = "obscura";

fn app_bin_dir() -> PathBuf {
    workspace_state::state_dir().join("bin")
}

pub fn claude_bin() -> Option<PathBuf> {
    code_quality::which("claude").into_iter().chain([home().join(".local/bin/claude")]).find(|p| p.is_file())
}

/// Whether anything from the old Obscura integration is left (MCP registration or binaries).
pub fn obscura_leftovers() -> bool {
    let bin = app_bin_dir();
    if bin.join("obscura").exists() || bin.join("obscura-worker").exists() {
        return true;
    }
    claude_bin().is_some_and(|c| Command::new(c).args(["mcp", "get", OBSCURA_MCP]).stdin(Stdio::null()).output().is_ok_and(|o| o.status.success()))
}

/// `claude mcp remove -s user obscura` + delete the binaries this app downloaded.
pub fn remove_obscura() -> Result<String> {
    let mut done = Vec::new();
    if let Some(claude) = claude_bin() {
        let out = Command::new(claude).args(["mcp", "remove", "-s", "user", OBSCURA_MCP]).stdin(Stdio::null()).output()?;
        if out.status.success() {
            done.push("MCP server removed");
        }
    }
    for name in ["obscura", "obscura-worker"] {
        let p = app_bin_dir().join(name);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
            done.push(if name == "obscura" { "binary deleted" } else { "worker deleted" });
        }
    }
    if done.is_empty() {
        return Ok("Nothing to remove".into());
    }
    Ok(format!("Obscura: {}", done.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_only_our_skill_blobs() {
        let tree = serde_json::json!({"tree": [
            {"path": "skills/moli-webfetch", "type": "tree"},
            {"path": "skills/moli-webfetch/SKILL.md", "type": "blob"},
            {"path": "skills/moli-webfetch/references/fetch-recipes.md", "type": "blob"},
            {"path": "skills/moli-websearch-extra/SKILL.md", "type": "blob"},
            {"path": "skills/other/SKILL.md", "type": "blob"},
            {"path": "README.md", "type": "blob"},
        ]});
        assert_eq!(skill_files(&tree), vec!["skills/moli-webfetch/SKILL.md", "skills/moli-webfetch/references/fetch-recipes.md"]);
    }

    #[test]
    fn ref_follows_binary_version() {
        assert_eq!(ref_for(Some("1.1.27")), "v1.1.27");
        assert_eq!(ref_for(None), "main");
    }

    #[test]
    fn finds_nested_binary() {
        let dir = std::env::temp_dir().join(format!("jt-moli-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("moli-v1-aarch64/x")).unwrap();
        std::fs::write(dir.join("moli-v1-aarch64/x/moli"), "x").unwrap();
        assert_eq!(find_file(&dir, "moli"), Some(dir.join("moli-v1-aarch64/x/moli")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Network: `HOME=$(mktemp -d) cargo test -p juan-terminal moli::tests::live -- --ignored`
    #[test]
    #[ignore]
    fn live_install_binary_and_skills() {
        let bin = install().unwrap();
        let v = version(&bin).unwrap();
        install_skills(&ref_for(Some(&v))).unwrap();
        assert_eq!(skills_ref().as_deref(), Some(format!("v{v}").as_str()));
        remove_skills().unwrap();
        assert_eq!(skills_ref(), None);
    }
}
