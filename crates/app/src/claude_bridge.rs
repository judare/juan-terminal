//! Glue between the Claude Code IDE server (`claude_ide`) and the workspace: keeps Claude's
//! view of open editors / selection / diagnostics current, shows proposed edits as an
//! Accept/Reject diff, and turns hook events into tab badges + macOS notifications.

use std::path::PathBuf;

use claude_ide::{DiffOutcome, IdeRequest, IdeServer, OpenEditor, Selection};
use futures::StreamExt;
use futures::channel::oneshot;
use gpui::{AppContext, Context, Entity, Focusable, Window};

use crate::diff_view::{DiffSource, DiffView, DiffViewEvent};
use crate::editor_view::EditorView;
use crate::terminal_view::{Attention, TerminalView};
use crate::workspace::Workspace;

/// A Claude edit waiting for Accept/Reject.
pub struct PendingDiff {
    pub tab_name: String,
    pub new_contents: String,
    pub reply: oneshot::Sender<DiffOutcome>,
    pub view: Entity<DiffView>,
}

/// Selected text sent to Claude is capped (Claude only needs to know what's selected).
const MAX_SELECTION_TEXT: usize = 100_000;

pub fn start(window: &mut Window, cx: &mut Context<Workspace>) -> Option<IdeServer> {
    let (server, mut rx) = match IdeServer::start() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("claude ide server failed to start: {e:#}");
            return None;
        }
    };
    cx.spawn_in(window, async move |this, cx| {
        while let Some(req) = rx.next().await {
            if this.update_in(cx, |this, window, cx| this.handle_ide_request(req, window, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    Some(server)
}

fn diag_to_ide(d: &code_quality::Diagnostic) -> claude_ide::Diagnostic {
    let line = d.line.saturating_sub(1);
    let col = d.column.saturating_sub(1);
    let code = d.code.clone();
    claude_ide::Diagnostic {
        start: (line, col),
        end: (line, col + d.length.max(1)),
        message: d.message.clone(),
        severity: match d.severity {
            code_quality::Severity::Error => "Error",
            code_quality::Severity::Warning => "Warning",
            code_quality::Severity::Advice => "Information",
        },
        source: if code.as_deref().is_some_and(|c| c.starts_with("ts")) { "tsgo".into() } else { "oxlint".into() },
        code,
    }
}

/// Terminals waiting on the user, per workspace window (keyed by entity id): summed into the
/// Dock icon badge.
static DOCK_COUNTS: std::sync::Mutex<Vec<(u64, usize)>> = std::sync::Mutex::new(Vec::new());

/// Report this window's count; repaints the Dock badge only when the total changes.
pub fn set_dock_count(owner: u64, n: usize) {
    let Ok(mut counts) = DOCK_COUNTS.lock() else { return };
    let before: usize = counts.iter().map(|c| c.1).sum();
    counts.retain(|c| c.0 != owner);
    if n > 0 {
        counts.push((owner, n));
    }
    let after: usize = counts.iter().map(|c| c.1).sum();
    if before != after {
        set_dock_badge(if after == 0 { String::new() } else { after.to_string() });
    }
}

#[allow(unexpected_cfgs)] // objc 0.2 macros
fn set_dock_badge(label: String) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    let Ok(c) = std::ffi::CString::new(label) else { return };
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let tile: *mut Object = msg_send![app, dockTile];
        let s: *mut Object = msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
        let _: () = msg_send![tile, setBadgeLabel: s];
    }
}

impl Workspace {
    /// Environment that lets `claude` in this terminal find us and report hook events.
    pub(crate) fn claude_terminal_env(&self, terminal_id: &str) -> Vec<(String, String)> {
        let Some(ide) = &self.ide else { return Vec::new() };
        vec![
            ("CLAUDE_CODE_SSE_PORT".into(), ide.port().to_string()),
            ("JT_HOOK_TOKEN".into(), ide.token().to_string()),
            ("JT_TERMINAL_ID".into(), terminal_id.to_string()),
        ]
    }

    /// Push open editors, the active selection and project folders to the IDE server.
    pub(crate) fn sync_ide(&mut self, cx: &mut Context<Self>) {
        let Some(ide) = &self.ide else { return };
        let folders: Vec<PathBuf> = self
            .active_project()
            .map(|p| p.repositories.iter().map(|r| r.path.clone()).chain(p.root.clone()).collect())
            .unwrap_or_default();
        ide.set_workspace_folders(folders);

        let active = self.active_editor().cloned();
        let tabs: Vec<Entity<EditorView>> = self.editors.get(&self.active_key()).map(|t| t.tabs.clone()).unwrap_or_default();
        let editors: Vec<OpenEditor> = tabs
            .iter()
            .filter_map(|e| {
                let v = e.read(cx);
                let path = v.path()?.clone();
                Some(OpenEditor {
                    language_id: crate::language_servers::language_id(&path).to_string(),
                    path,
                    active: active.as_ref() == Some(e),
                    dirty: v.is_dirty(),
                    diagnostics: v.diagnostics().iter().map(diag_to_ide).collect(),
                })
            })
            .collect();
        ide.set_editors(editors);

        let selection = active.and_then(|e| {
            let v = e.read(cx);
            let path = v.path()?.clone();
            if v.asset.is_some() {
                return None;
            }
            let r = v.doc.selection.range();
            let start = v.doc.line_col(r.start);
            let end = v.doc.line_col(r.end);
            let mut text = if r.is_empty() { String::new() } else { v.doc.selected_text() };
            if text.len() > MAX_SELECTION_TEXT {
                let mut cut = MAX_SELECTION_TEXT;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
            }
            Some(Selection { file: path, start, end, text })
        });
        ide.set_selection(selection);
    }

    fn handle_ide_request(&mut self, req: IdeRequest, window: &mut Window, cx: &mut Context<Self>) {
        match req {
            IdeRequest::OpenFile { path, line } => {
                self.open_file(&path, true, window, cx);
                if let (Some(line), Some(e)) = (line, self.active_editor().cloned()) {
                    e.update(cx, |e, cx| e.go_to(line, 0, cx));
                }
                self.persist(cx);
            }
            IdeRequest::OpenDiff { path, new_contents, tab_name, reply } => self.show_claude_diff(path, new_contents, tab_name, reply, window, cx),
            IdeRequest::CloseTab { tab_name } => {
                let matches = self.claude_pending.as_ref().is_some_and(|p| tab_name.is_empty() || p.tab_name == tab_name);
                if matches {
                    self.finish_claude_diff(DiffOutcomeKind::Closed, cx);
                }
            }
            IdeRequest::SaveDocument { path, reply } => {
                let editor = self.editors.values().flat_map(|t| t.tabs.iter()).find(|e| e.read(cx).path() == Some(&path)).cloned();
                let ok = editor.is_some_and(|e| e.update(cx, |e, cx| e.save(cx)));
                let _ = reply.send(ok);
            }
            IdeRequest::Hook { terminal, event, message, cwd, prompt, tool } => self.on_claude_hook(terminal, &event, message, cwd, prompt, tool, window, cx),
            IdeRequest::Status { terminal, data } => {
                if let Some((_, t)) = terminal.as_deref().and_then(|id| self.find_terminal(id, cx)) {
                    let st = crate::terminal_view::ClaudeStatus::from_json(&data);
                    t.update(cx, |t, cx| t.set_claude_status(st, cx));
                }
            }
            IdeRequest::Connection { connected } => {
                self.claude_sessions = if connected { self.claude_sessions + 1 } else { self.claude_sessions.saturating_sub(1) };
                if connected {
                    self.sync_ide(cx);
                }
                cx.notify();
            }
        }
    }

    fn show_claude_diff(&mut self, path: PathBuf, new_contents: String, tab_name: String, reply: oneshot::Sender<DiffOutcome>, window: &mut Window, cx: &mut Context<Self>) {
        // Only one proposal at a time (Claude asks sequentially); a stale one is closed.
        self.finish_claude_diff(DiffOutcomeKind::Closed, cx);
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        let name = self
            .active_project()
            .and_then(|p| p.repositories.iter().map(|r| r.path.clone()).chain(p.root.clone()).find(|r| path.starts_with(r)))
            .and_then(|root| path.strip_prefix(root).ok().map(|p| p.display().to_string()))
            .unwrap_or_else(|| path.display().to_string());
        let files = match git_engine::diff_contents(&name, &old, &new_contents) {
            Ok(text) => git_engine::parse_diff(&text),
            Err(e) => {
                self.error = Some(format!("Could not diff Claude's edit: {e:#}").into());
                let _ = reply.send(DiffOutcome::Closed);
                cx.notify();
                return;
            }
        };
        let title = format!("Claude · {name}");
        let source = DiffSource::ClaudeProposal;
        let view = cx.new(|cx| DiffView::new(title, source, path.parent().map(|p| p.to_path_buf()), files, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, event, _, cx| {
            if let DiffViewEvent::Resolve(accept) = event {
                this.finish_claude_diff(if *accept { DiffOutcomeKind::Saved } else { DiffOutcomeKind::Rejected }, cx);
            }
        });
        self._subscriptions.push(sub);
        // Keep keyboard focus where it is (the user may answer in the terminal instead).
        self.diff = Some(view.clone());
        self.diff_active = true;
        self.claude_pending = Some(PendingDiff { tab_name, new_contents, reply, view });
        cx.notify();
    }

    pub(crate) fn finish_claude_diff(&mut self, kind: DiffOutcomeKind, cx: &mut Context<Self>) {
        let Some(p) = self.claude_pending.take() else { return };
        let outcome = match kind {
            DiffOutcomeKind::Saved => DiffOutcome::Saved(p.new_contents),
            DiffOutcomeKind::Rejected => DiffOutcome::Rejected,
            DiffOutcomeKind::Closed => DiffOutcome::Closed,
        };
        let _ = p.reply.send(outcome);
        if self.diff.as_ref() == Some(&p.view) {
            self.diff = None;
            self.diff_active = false;
        }
        cx.notify();
    }

    fn find_terminal(&self, id: &str, cx: &Context<Self>) -> Option<(String, Entity<TerminalView>)> {
        self.terminals.iter().find_map(|(key, tabs)| tabs.tabs.iter().find(|t| t.read(cx).jt_id == id).map(|t| (key.clone(), t.clone())))
    }

    #[allow(clippy::too_many_arguments)]
    fn on_claude_hook(&mut self, terminal: Option<String>, event: &str, message: Option<String>, cwd: Option<String>, prompt: Option<String>, tool: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let found = terminal.as_deref().and_then(|id| self.find_terminal(id, cx));
        // Sessions board: track what this terminal's Claude is doing.
        if let Some((_, t)) = &found {
            let changed = t.update(cx, |t, _| t.activity.apply(event, prompt, tool, message.clone()));
            if changed {
                // Other windows show this session on their board / badge too.
                cx.defer(|cx| cx.refresh_windows());
            }
        }
        let attention = match event {
            "Stop" => Attention::Done,
            "Notification" => Attention::NeedsInput,
            // Session closed or the user answered: that terminal no longer waits on them.
            "SessionEnd" | "UserPromptSubmit" => {
                if let Some((_, t)) = &found {
                    t.update(cx, |t, cx| t.clear_attention(cx));
                }
                cx.notify();
                return;
            }
            _ => return,
        };
        let looking_at_it = window.is_window_active() && found.as_ref().is_some_and(|(_, t)| t.read(cx).focus_handle(cx).is_focused(window));
        if let Some((_, t)) = &found {
            if !looking_at_it {
                t.update(cx, |t, cx| {
                    t.attention = Some(attention);
                    cx.notify();
                });
            }
        }
        if !looking_at_it {
            let place = cwd
                .as_deref()
                .and_then(|c| std::path::Path::new(c).file_name().map(|n| n.to_string_lossy().into_owned()))
                .or_else(|| found.as_ref().and_then(|(_, t)| t.read(cx).repository.clone()))
                .unwrap_or_default();
            let (title, body) = match attention {
                Attention::Done => ("Claude terminó", "Listo para tu siguiente mensaje".to_string()),
                Attention::NeedsInput => ("Claude te necesita", message.unwrap_or_else(|| "Esperando tu respuesta".into())),
            };
            crate::notify::notify(crate::notify::Source::Claude, title, &place, &body);
        }
        cx.notify();
    }
}

#[derive(Clone, Copy)]
pub enum DiffOutcomeKind {
    Saved,
    Rejected,
    Closed,
}
