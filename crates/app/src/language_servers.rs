//! Language servers per project: TypeScript Go (`tsgo --lsp --stdio`) and `oxlint --lsp`.
//! Started lazily when the first relevant file is opened; all are stopped (processes killed)
//! when the project is switched, because the map and the editors holding them are dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Context, Entity, WeakEntity};
use lsp_client::LspServer;

use crate::editor_view::EditorView;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServerKind {
    TypeScript,
    Oxlint,
}

enum Slot {
    Starting(Vec<WeakEntity<EditorView>>),
    Ready(Arc<LspServer>),
    Failed,
}

#[derive(Default)]
pub struct LanguageServers {
    slots: HashMap<(ServerKind, PathBuf), Slot>,
}

impl LanguageServers {
    pub fn clear(&mut self) {
        self.slots.clear();
    }

    pub fn running(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .slots
            .iter()
            .filter(|(_, s)| matches!(s, Slot::Ready(_)))
            .map(|((kind, root), _)| {
                let name = match kind {
                    ServerKind::TypeScript => "tsgo",
                    ServerKind::Oxlint => "oxlint",
                };
                format!("{name} · {}", root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            })
            .collect();
        v.sort();
        v
    }
}

pub fn app_tools_dir() -> PathBuf {
    workspace_state::state_dir().join("tools")
}

/// tsgo binary: project-local first, then the app-managed install, then PATH. Prefers the
/// native binary over the npm `node` wrapper (one process less, ~40 MB less RAM).
pub fn locate_tsgo(root: &Path, repo: &Path) -> Option<PathBuf> {
    let native = |nm: PathBuf| nm.join("@typescript/native-preview-darwin-arm64/lib/tsgo");
    let app_nm = app_tools_dir().join("tsgo/node_modules");
    [root.join("node_modules"), repo.join("node_modules"), app_nm]
        .into_iter()
        .flat_map(|nm| [native(nm.clone()), nm.join(".bin/tsgo")])
        .find(|p| p.is_file())
        .or_else(|| code_quality::which("tsgo"))
}

pub fn install_tsgo() -> anyhow::Result<PathBuf> {
    let dir = app_tools_dir().join("tsgo");
    std::fs::create_dir_all(&dir)?;
    let npm = code_quality::which("npm").ok_or_else(|| anyhow::anyhow!("npm not found on PATH"))?;
    let out = std::process::Command::new(npm)
        .arg("install")
        .arg("--prefix")
        .arg(&dir)
        .args(["--no-audit", "--no-fund", "@typescript/native-preview"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let bin = dir.join("node_modules/.bin/tsgo");
    anyhow::ensure!(bin.is_file(), "tsgo was not installed");
    Ok(bin)
}

/// TypeScript project root for a file: nearest folder with tsconfig/jsconfig, within the repo.
pub fn ts_root(file: &Path, repo: &Path) -> Option<PathBuf> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d.join("tsconfig.json").is_file() || d.join("jsconfig.json").is_file() {
            return Some(d.to_path_buf());
        }
        if d == repo {
            break;
        }
        dir = d.parent();
    }
    let pkg = std::fs::read_to_string(repo.join("package.json")).unwrap_or_default();
    pkg.contains("\"typescript\"").then(|| repo.to_path_buf())
}

pub fn ts_language_id(path: &Path) -> Option<&'static str> {
    Some(match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        _ => return None,
    })
}

pub fn language_id(path: &Path) -> &'static str {
    ts_language_id(path).unwrap_or_else(|| if path.extension().is_some_and(|e| e == "vue") { "vue" } else { "plaintext" })
}

impl Workspace {
    pub(crate) fn sync_server_status(&mut self, cx: &mut Context<Self>) {
        let running = self.lsp.running();
        if let Some(tp) = self.tools_panel_entity() {
            tp.update(cx, |t, cx| t.set_servers(running, cx));
        }
    }

    /// Attach (starting if needed) a language server to an editor.
    pub(crate) fn ensure_server(&mut self, kind: ServerKind, root: PathBuf, program: PathBuf, args: Vec<&'static str>, view: &Entity<EditorView>, cx: &mut Context<Self>) {
        let key = (kind, root.clone());
        match self.lsp.slots.get_mut(&key) {
            Some(Slot::Ready(server)) => {
                let server = server.clone();
                view.update(cx, |v, cx| v.attach_lsp(server, cx));
                return;
            }
            Some(Slot::Starting(waiters)) => {
                waiters.push(view.downgrade());
                return;
            }
            Some(Slot::Failed) => return,
            None => {}
        }
        self.lsp.slots.insert(key.clone(), Slot::Starting(vec![view.downgrade()]));
        let name = match kind {
            ServerKind::TypeScript => "tsgo",
            ServerKind::Oxlint => "oxlint",
        };
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let result = cx.background_executor().spawn(async move { LspServer::start(name, &program, &args, &r) }).await;
            let _ = this.update(cx, |this, cx| {
                let waiters = match this.lsp.slots.remove(&key) {
                    Some(Slot::Starting(w)) => w,
                    // Project switched while starting: drop the server.
                    _ => return,
                };
                match result {
                    Ok(server) => {
                        for w in waiters {
                            if let Some(v) = w.upgrade() {
                                let s = server.clone();
                                v.update(cx, |v, cx| v.attach_lsp(s, cx));
                            }
                        }
                        this.lsp.slots.insert(key, Slot::Ready(server));
                        this.sync_server_status(cx);
                    }
                    Err(e) => {
                        this.error = Some(format!("{name} language server failed: {e:#}").into());
                        this.lsp.slots.insert(key, Slot::Failed);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Decide which servers a freshly opened file needs.
    pub(crate) fn attach_language_servers(&mut self, view: &Entity<EditorView>, repo: &Path, tools: &crate::editor_view::FileTools, cx: &mut Context<Self>) {
        let Some(path) = view.read(cx).path().cloned() else { return };
        if ts_language_id(&path).is_some() {
            if let Some(root) = ts_root(&path, repo) {
                if let Some(tsgo) = locate_tsgo(&root, repo) {
                    self.ensure_server(ServerKind::TypeScript, root, tsgo, vec!["--lsp", "--stdio"], view, cx);
                }
            }
        }
        if code_quality::lintable(&path) {
            if let Some(ox) = &tools.oxlint {
                self.ensure_server(ServerKind::Oxlint, ox.cwd.clone(), ox.bin.clone(), vec!["--lsp"], view, cx);
            }
        }
    }
}
