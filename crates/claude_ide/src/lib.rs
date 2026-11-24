//! Claude Code IDE integration (the protocol the VS Code / JetBrains extensions speak).
//!
//! * A WebSocket MCP server on 127.0.0.1:<port>, announced through the lockfile
//!   `~/.claude/ide/<port>.lock` (`{pid, workspaceFolders, ideName, transport:"ws", authToken}`).
//! * Terminals get `CLAUDE_CODE_SSE_PORT=<port>`, so `claude` started inside juan-terminal
//!   auto-connects to this instance (the port match makes it valid regardless of cwd).
//! * Claude authenticates with the `X-Claude-Code-Ide-Authorization` header (subprotocol `mcp`).
//! * We push `selection_changed` notifications (active file + selection) and answer tools:
//!   getDiagnostics, openFile, openDiff (blocks until Accept/Reject), close_tab, …
//! * `POST /hook` (same port, token header) receives Claude Code hook events (Stop,
//!   Notification) for "Claude finished / needs you" alerts.
//!
//! Protocol details were verified against the shipped `claude` 2.1.330 binary.

pub mod ws;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use serde_json::{Value, json};

pub const IDE_NAME: &str = "Juan Terminal";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub file: PathBuf,
    /// 0-based (line, UTF-16-ish character; we use chars) like LSP/VS Code.
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    /// 0-based.
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub message: String,
    /// "Error" | "Warning" | "Information" | "Hint"
    pub severity: &'static str,
    pub source: String,
    pub code: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenEditor {
    pub path: PathBuf,
    pub active: bool,
    pub dirty: bool,
    pub language_id: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// How the user resolved a proposed edit.
#[derive(Debug, PartialEq, Eq)]
pub enum DiffOutcome {
    /// Accepted; carries the final contents.
    Saved(String),
    Rejected,
    Closed,
}

/// Work the UI must do.
pub enum IdeRequest {
    OpenFile { path: PathBuf, line: Option<usize> },
    OpenDiff { path: PathBuf, new_contents: String, tab_name: String, reply: oneshot::Sender<DiffOutcome> },
    CloseTab { tab_name: String },
    SaveDocument { path: PathBuf, reply: oneshot::Sender<bool> },
    /// Claude Code hook event (from our injected hooks).
    Hook { terminal: Option<String>, event: String, message: Option<String>, cwd: Option<String> },
    /// A Claude session connected (true) or went away (false).
    Connection { connected: bool },
    /// Claude Code statusLine payload (model, effort, context window, session id…).
    Status { terminal: Option<String>, data: Value },
}

#[derive(Default)]
struct State {
    folders: Vec<PathBuf>,
    editors: Vec<OpenEditor>,
    selection: Option<Selection>,
}

struct Client {
    id: u64,
    writer: Mutex<TcpStream>,
    ready: AtomicBool,
}

struct Inner {
    port: u16,
    token: String,
    lockfile: PathBuf,
    state: Mutex<State>,
    clients: Mutex<Vec<Arc<Client>>>,
    requests: UnboundedSender<IdeRequest>,
    shutdown: AtomicBool,
    next_id: AtomicU64,
}

pub struct IdeServer {
    inner: Arc<Inner>,
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// `~/.claude/ide` (or `$CLAUDE_CONFIG_DIR/ide`), where Claude Code looks for lockfiles.
pub fn lock_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("ide"),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude/ide"),
    }
}

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn path_from_uri(s: &str) -> PathBuf {
    PathBuf::from(s.strip_prefix("file://").unwrap_or(s).replace("%20", " "))
}

impl IdeServer {
    /// Bind, write the lockfile, and start accepting. `lock_dir` is injectable for tests.
    pub fn start_in(lock_dir: &Path) -> anyhow::Result<(IdeServer, UnboundedReceiver<IdeRequest>)> {
        remove_stale_lockfiles(lock_dir);
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = unbounded();
        let inner = Arc::new(Inner {
            port,
            token: random_token(),
            lockfile: lock_dir.join(format!("{port}.lock")),
            state: Mutex::new(State::default()),
            clients: Mutex::new(Vec::new()),
            requests: tx,
            shutdown: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        });
        write_lockfile(&inner)?;
        let acc = inner.clone();
        std::thread::Builder::new().name("claude-ide-accept".into()).spawn(move || {
            for stream in listener.incoming() {
                if acc.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let inner = acc.clone();
                let _ = std::thread::Builder::new().name("claude-ide-conn".into()).spawn(move || handle_connection(inner, stream));
            }
        })?;
        Ok((IdeServer { inner }, rx))
    }

    pub fn start() -> anyhow::Result<(IdeServer, UnboundedReceiver<IdeRequest>)> {
        Self::start_in(&lock_dir())
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    pub fn token(&self) -> &str {
        &self.inner.token
    }

    pub fn connected(&self) -> usize {
        self.inner.clients.lock().unwrap().iter().filter(|c| c.ready.load(Ordering::Relaxed)).count()
    }

    /// Project folders (Claude uses them to validate cwd for sessions not started by us).
    pub fn set_workspace_folders(&self, folders: Vec<PathBuf>) {
        let changed = {
            let mut st = self.inner.state.lock().unwrap();
            let c = st.folders != folders;
            st.folders = folders;
            c
        };
        if changed {
            let _ = write_lockfile(&self.inner);
        }
    }

    pub fn set_editors(&self, editors: Vec<OpenEditor>) {
        self.inner.state.lock().unwrap().editors = editors;
    }

    /// Update the active selection; notifies connected sessions when it changed.
    pub fn set_selection(&self, sel: Option<Selection>) {
        {
            let mut st = self.inner.state.lock().unwrap();
            if st.selection == sel {
                return;
            }
            st.selection = sel.clone();
        }
        broadcast(&self.inner, &selection_notification(sel.as_ref()));
    }
}

impl Drop for IdeServer {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.inner.lockfile);
        // Wake the accept loop.
        let _ = TcpStream::connect(("127.0.0.1", self.inner.port));
        for c in self.inner.clients.lock().unwrap().drain(..) {
            let _ = c.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Lockfiles left by juan-terminal instances that were killed (no Drop ran).
fn remove_stale_lockfiles(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "lock") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if v.get("ideName").and_then(|n| n.as_str()) != Some(IDE_NAME) {
            continue; // never touch other IDEs' files
        }
        let pid = v.get("pid").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
        // kill(pid, 0) fails with ESRCH when the process is gone.
        let alive = pid > 0 && (unsafe { kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1));
        if !alive {
            let _ = std::fs::remove_file(&p);
        }
    }
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn write_lockfile(inner: &Inner) -> anyhow::Result<()> {
    let folders: Vec<String> = inner.state.lock().unwrap().folders.iter().map(|p| p.display().to_string()).collect();
    let body = json!({
        "pid": std::process::id(),
        "workspaceFolders": folders,
        "ideName": IDE_NAME,
        "transport": "ws",
        "runningInWindows": false,
        "authToken": inner.token,
    });
    let dir = inner.lockfile.parent().expect("lockfile has a parent");
    std::fs::create_dir_all(dir)?;
    let tmp = inner.lockfile.with_extension("lock.tmp");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(body.to_string().as_bytes())?;
    }
    std::fs::rename(tmp, &inner.lockfile)?;
    Ok(())
}

fn selection_notification(sel: Option<&Selection>) -> Value {
    let params = match sel {
        Some(s) => json!({
            "text": s.text,
            "filePath": s.file.display().to_string(),
            "fileUrl": file_url(&s.file),
            "selection": {
                "start": {"line": s.start.0, "character": s.start.1},
                "end": {"line": s.end.0, "character": s.end.1},
                "isEmpty": s.start == s.end,
            }
        }),
        None => json!({"text": "", "filePath": null, "selection": null}),
    };
    json!({"jsonrpc": "2.0", "method": "selection_changed", "params": params})
}

fn send(client: &Client, msg: &Value) -> bool {
    let mut w = client.writer.lock().unwrap();
    ws::write_frame(&mut *w, 0x1, msg.to_string().as_bytes()).is_ok()
}

fn broadcast(inner: &Inner, msg: &Value) {
    let clients: Vec<Arc<Client>> = inner.clients.lock().unwrap().clone();
    for c in clients.iter().filter(|c| c.ready.load(Ordering::Relaxed)) {
        send(c, msg);
    }
}

fn http_response(stream: &mut TcpStream, status: &str) {
    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn handle_connection(inner: Arc<Inner>, mut stream: TcpStream) {
    if inner.shutdown.load(Ordering::Relaxed) {
        return;
    }
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let Ok(req) = ws::read_request(&mut stream) else { return };

    // Hook / statusLine events from `curl` (see the injected Claude settings).
    if req.method == "POST" && (req.path.starts_with("/hook") || req.path.starts_with("/statusline")) {
        if req.header("x-jt-token") != Some(inner.token.as_str()) {
            return http_response(&mut stream, "401 Unauthorized");
        }
        let len: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0).min(1 << 20);
        let mut body = req.rest.clone();
        while body.len() < len {
            let mut chunk = vec![0u8; len - body.len()];
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let terminal = req.header("x-jt-terminal").filter(|t| !t.is_empty()).map(str::to_string);
        if req.path.starts_with("/statusline") {
            let _ = inner.requests.unbounded_send(IdeRequest::Status { terminal, data: v });
            return http_response(&mut stream, "204 No Content");
        }
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let _ = inner.requests.unbounded_send(IdeRequest::Hook {
            terminal,
            event: s("hook_event_name").unwrap_or_default(),
            message: s("message"),
            cwd: s("cwd"),
        });
        return http_response(&mut stream, "204 No Content");
    }

    let upgrade = req.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if !upgrade {
        return http_response(&mut stream, "404 Not Found");
    }
    if req.header("x-claude-code-ide-authorization") != Some(inner.token.as_str()) {
        return http_response(&mut stream, "401 Unauthorized");
    }
    let Some(key) = req.header("sec-websocket-key") else { return http_response(&mut stream, "400 Bad Request") };
    let proto = req.header("sec-websocket-protocol").filter(|p| p.split(',').any(|x| x.trim() == "mcp")).map(|_| "Sec-WebSocket-Protocol: mcp\r\n").unwrap_or("");
    if write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n{proto}\r\n", ws::accept_key(key)).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(None);
    let Ok(writer) = stream.try_clone() else { return };
    let client = Arc::new(Client { id: inner.next_id.fetch_add(1, Ordering::Relaxed), writer: Mutex::new(writer), ready: AtomicBool::new(false) });
    inner.clients.lock().unwrap().push(client.clone());

    let mut reader = std::io::BufReader::new(stream);
    loop {
        match ws::read_message(&mut reader) {
            Ok(ws::Message::Text(t)) => {
                let Ok(msg) = serde_json::from_str::<Value>(&t) else { continue };
                // Tool calls can block (openDiff waits for the user): answer on their own thread.
                let (inner2, client2) = (inner.clone(), client.clone());
                if msg.get("method").and_then(|m| m.as_str()) == Some("tools/call") {
                    std::thread::spawn(move || handle_message(&inner2, &client2, msg));
                } else {
                    handle_message(&inner2, &client2, msg);
                }
            }
            Ok(ws::Message::Ping(p)) => {
                let mut w = client.writer.lock().unwrap();
                let _ = ws::write_frame(&mut *w, 0xA, &p);
            }
            Ok(ws::Message::Close) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let was_ready = client.ready.load(Ordering::Relaxed);
    inner.clients.lock().unwrap().retain(|c| c.id != client.id);
    if was_ready {
        let _ = inner.requests.unbounded_send(IdeRequest::Connection { connected: false });
    }
    let _ = client.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
}

fn handle_message(inner: &Inner, client: &Client, msg: Value) {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "juan-terminal", "version": env!("CARGO_PKG_VERSION")},
        })),
        "notifications/initialized" | "ide_connected" => {
            if !client.ready.swap(true, Ordering::Relaxed) {
                let _ = inner.requests.unbounded_send(IdeRequest::Connection { connected: true });
                // Let the new session know what's selected right now.
                let sel = inner.state.lock().unwrap().selection.clone();
                send(client, &selection_notification(sel.as_ref()));
            }
            return;
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_list()})),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(inner, name, &args).map(|content| json!({"content": content}))
        }
        m if m.starts_with("notifications/") => return,
        _ => Err((-32601, format!("Method not found: {method}"))),
    };
    let Some(id) = id else { return };
    let reply = match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    };
    send(client, &reply);
}

fn text(t: impl Into<String>) -> Value {
    json!([{"type": "text", "text": t.into()}])
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": props, "required": required}})
}

fn tool_list() -> Vec<Value> {
    vec![
        tool("getDiagnostics", "Get language diagnostics (tsgo, oxlint) from juan-terminal", json!({"uri": {"type": "string"}}), &[]),
        tool("openFile", "Open a file in the editor", json!({"filePath": {"type": "string"}, "preview": {"type": "boolean"}, "startText": {"type": "string"}, "endText": {"type": "string"}, "makeFrontmost": {"type": "boolean"}}), &["filePath"]),
        tool("openDiff", "Show a proposed edit for the user to accept or reject", json!({"old_file_path": {"type": "string"}, "new_file_path": {"type": "string"}, "new_file_contents": {"type": "string"}, "tab_name": {"type": "string"}}), &["old_file_path", "new_file_path", "new_file_contents", "tab_name"]),
        tool("close_tab", "Close a diff tab", json!({"tab_name": {"type": "string"}}), &["tab_name"]),
        tool("closeAllDiffTabs", "Close all diff tabs", json!({}), &[]),
        tool("getCurrentSelection", "Get the current editor selection", json!({}), &[]),
        tool("getLatestSelection", "Get the most recent editor selection", json!({}), &[]),
        tool("getOpenEditors", "List open editor tabs", json!({}), &[]),
        tool("getWorkspaceFolders", "List workspace folders", json!({}), &[]),
        tool("checkDocumentDirty", "Whether a document has unsaved changes", json!({"filePath": {"type": "string"}}), &["filePath"]),
        tool("saveDocument", "Save a document", json!({"filePath": {"type": "string"}}), &["filePath"]),
    ]
}

fn diagnostics_json(editors: &[OpenEditor], only: Option<&Path>) -> Value {
    let list: Vec<Value> = editors
        .iter()
        .filter(|e| only.is_none_or(|p| e.path == p))
        .map(|e| {
            json!({
                "uri": file_url(&e.path),
                "diagnostics": e.diagnostics.iter().map(|d| json!({
                    "message": d.message,
                    "severity": d.severity,
                    "source": d.source,
                    "code": d.code,
                    "range": {"start": {"line": d.start.0, "character": d.start.1}, "end": {"line": d.end.0, "character": d.end.1}},
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Value::Array(list)
}

fn selection_json(sel: Option<&Selection>) -> Value {
    match sel {
        Some(s) => json!({
            "success": true,
            "text": s.text,
            "filePath": s.file.display().to_string(),
            "fileUrl": file_url(&s.file),
            "selection": {"start": {"line": s.start.0, "character": s.start.1}, "end": {"line": s.end.0, "character": s.end.1}, "isEmpty": s.start == s.end},
        }),
        None => json!({"success": false, "message": "No active editor found"}),
    }
}

fn call_tool(inner: &Inner, name: &str, args: &Value) -> Result<Value, (i64, String)> {
    let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let st = || inner.state.lock().unwrap();
    match name {
        "getDiagnostics" => {
            let only = arg("uri").map(|u| path_from_uri(&u));
            Ok(text(diagnostics_json(&st().editors, only.as_deref()).to_string()))
        }
        "getCurrentSelection" | "getLatestSelection" => Ok(text(selection_json(st().selection.as_ref()).to_string())),
        "getOpenEditors" => {
            let tabs: Vec<Value> = st()
                .editors
                .iter()
                .map(|e| json!({"uri": file_url(&e.path), "isActive": e.active, "label": e.path.file_name().map(|n| n.to_string_lossy().into_owned()), "languageId": e.language_id, "isDirty": e.dirty}))
                .collect();
            Ok(text(json!({"tabs": tabs}).to_string()))
        }
        "getWorkspaceFolders" => {
            let f = st().folders.clone();
            let folders: Vec<Value> = f.iter().map(|p| json!({"name": p.file_name().map(|n| n.to_string_lossy().into_owned()), "uri": file_url(p), "path": p.display().to_string()})).collect();
            Ok(text(json!({"success": true, "folders": folders, "rootPath": f.first().map(|p| p.display().to_string())}).to_string()))
        }
        "checkDocumentDirty" => {
            let p = PathBuf::from(arg("filePath").unwrap_or_default());
            let dirty = st().editors.iter().find(|e| e.path == p).map(|e| e.dirty);
            Ok(text(match dirty {
                Some(d) => json!({"success": true, "filePath": p.display().to_string(), "isDirty": d, "isUntitled": false}).to_string(),
                None => json!({"success": false, "message": format!("Document not open: {}", p.display())}).to_string(),
            }))
        }
        "openFile" => {
            let path = PathBuf::from(arg("filePath").ok_or((-32602, "filePath required".to_string()))?);
            // startText: open at the first line that contains it.
            let line = arg("startText").filter(|t| !t.is_empty()).and_then(|t| std::fs::read_to_string(&path).ok().and_then(|c| c.lines().position(|l| l.contains(&t))));
            let _ = inner.requests.unbounded_send(IdeRequest::OpenFile { path: path.clone(), line });
            Ok(text(format!("Opened file: {}", path.display())))
        }
        "openDiff" => {
            let path = PathBuf::from(arg("new_file_path").or_else(|| arg("old_file_path")).ok_or((-32602, "file path required".to_string()))?);
            let (tx, rx) = oneshot::channel();
            let req = IdeRequest::OpenDiff { path, new_contents: arg("new_file_contents").unwrap_or_default(), tab_name: arg("tab_name").unwrap_or_default(), reply: tx };
            inner.requests.unbounded_send(req).map_err(|_| (-32603, "IDE is shutting down".to_string()))?;
            // Blocks this call's thread until the user decides (or the tab is closed).
            Ok(match futures::executor::block_on(rx) {
                Ok(DiffOutcome::Saved(contents)) => json!([{"type": "text", "text": "FILE_SAVED"}, {"type": "text", "text": contents}]),
                Ok(DiffOutcome::Rejected) => text("DIFF_REJECTED"),
                Ok(DiffOutcome::Closed) | Err(_) => text("TAB_CLOSED"),
            })
        }
        "close_tab" => {
            let _ = inner.requests.unbounded_send(IdeRequest::CloseTab { tab_name: arg("tab_name").unwrap_or_default() });
            Ok(text("TAB_CLOSED"))
        }
        "closeAllDiffTabs" => {
            let _ = inner.requests.unbounded_send(IdeRequest::CloseTab { tab_name: String::new() });
            Ok(text("CLOSED_DIFF_TABS"))
        }
        "saveDocument" => {
            let p = PathBuf::from(arg("filePath").unwrap_or_default());
            let (tx, rx) = oneshot::channel();
            let _ = inner.requests.unbounded_send(IdeRequest::SaveDocument { path: p.clone(), reply: tx });
            let saved = futures::executor::block_on(rx).unwrap_or(false);
            Ok(text(json!({"success": saved, "filePath": p.display().to_string(), "saved": saved, "message": if saved { "Document saved" } else { "Document not open or save failed" }}).to_string()))
        }
        _ => Err((-32602, format!("Unknown tool: {name}"))),
    }
}

/// Claude Code settings (passed with `--settings`) whose hooks report Stop / Notification /
/// SessionEnd / UserPromptSubmit events back to this server, tagged with the terminal they came from.
pub fn hook_settings_json() -> String {
    let cmd = r#"[ -n "$CLAUDE_CODE_SSE_PORT" ] && [ -n "$JT_HOOK_TOKEN" ] && curl -s -m 2 -X POST -H "X-JT-Token: $JT_HOOK_TOKEN" -H "X-JT-Terminal: $JT_TERMINAL_ID" -H 'Content-Type: application/json' --data-binary @- "http://127.0.0.1:$CLAUDE_CODE_SSE_PORT/hook" >/dev/null 2>&1; exit 0"#;
    let hook = json!([{"hooks": [{"type": "command", "command": cmd, "timeout": 5}]}]);
    // statusLine: hand the payload to juan-terminal (which draws its own bar) and print nothing.
    let status = r#"[ -n "$CLAUDE_CODE_SSE_PORT" ] && [ -n "$JT_HOOK_TOKEN" ] && curl -s -m 1 -X POST -H "X-JT-Token: $JT_HOOK_TOKEN" -H "X-JT-Terminal: $JT_TERMINAL_ID" -H 'Content-Type: application/json' --data-binary @- "http://127.0.0.1:$CLAUDE_CODE_SSE_PORT/statusline" >/dev/null 2>&1; exit 0"#;
    json!({
        "hooks": {"Stop": hook, "Notification": hook, "SessionEnd": hook, "UserPromptSubmit": hook},
        "statusLine": {"type": "command", "command": status, "padding": 0},
    })
    .to_string()
}

#[cfg(test)]
mod tests;
