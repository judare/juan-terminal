//! Minimal Language Server Protocol client over stdio: JSON-RPC framing, initialize, document
//! sync (full text), pull diagnostics and go-to-definition. One reader thread per server; the
//! server process is killed when the client is dropped.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::channel::oneshot;
use serde_json::{Value, json};

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value>>>>>;

pub struct LspServer {
    pub name: String,
    pub root: PathBuf,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Pending,
    /// Whether the server supports pull diagnostics (`textDocument/diagnostic`).
    pub pull_diagnostics: bool,
    /// Latest pushed diagnostics per URI (servers without pull support).
    pushed: Arc<Mutex<HashMap<String, Value>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspDiagnostic {
    /// 0-based, UTF-16 columns (as LSP specifies).
    pub line: usize,
    pub character: usize,
    pub end_line: usize,
    pub end_character: usize,
    /// 1 error, 2 warning, 3 info, 4 hint.
    pub severity: u8,
    pub message: String,
    pub code: Option<String>,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub line: usize,
    pub character: usize,
}

pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

fn write_message(stdin: &mut impl Write, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:").or_else(|| line.strip_prefix("content-length:")) {
            len = Some(v.trim().parse::<usize>()?);
        }
    }
    let len = len.context("missing Content-Length")?;
    let mut buf = vec![0; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

impl LspServer {
    /// Spawn `program args…` in `root` and complete the initialize handshake (blocking, with a
    /// timeout). Call from a background thread.
    pub fn start(name: &str, program: &Path, args: &[&str], root: &Path) -> Result<Arc<Self>> {
        Self::start_with_timeout(name, program, args, root, Duration::from_secs(20))
    }

    pub fn start_with_timeout(name: &str, program: &Path, args: &[&str], root: &Path, init_timeout: Duration) -> Result<Arc<Self>> {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new(program)
            .args(args)
            // Own process group: wrappers (node → native binary) are all stopped on drop.
            .process_group(0)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to start {}", program.display()))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pushed = Arc::new(Mutex::new(HashMap::new()));

        let mut server = LspServer {
            name: name.to_string(),
            root: root.to_path_buf(),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending: pending.clone(),
            pull_diagnostics: false,
            pushed: pushed.clone(),
        };

        // Reader: route responses, answer server requests, record pushed diagnostics.
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Value>();
        std::thread::Builder::new().name(format!("lsp-{name}")).spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(msg)) = read_message(&mut reader) {
                let method = msg.get("method").and_then(|m| m.as_str());
                match (msg.get("id"), method) {
                    (Some(id), Some(method)) => {
                        // Server → client request.
                        let result = match method {
                            "workspace/configuration" => {
                                let n = msg["params"]["items"].as_array().map(|a| a.len()).unwrap_or(1);
                                Value::Array(vec![json!({}); n])
                            }
                            _ => Value::Null,
                        };
                        let _ = reply_tx.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                    }
                    (Some(id), None) => {
                        let Some(id) = id.as_i64() else { continue };
                        if let Some(tx) = pending.lock().unwrap().remove(&id) {
                            let r = match msg.get("error") {
                                Some(e) => Err(anyhow!("{}", e.get("message").and_then(|m| m.as_str()).unwrap_or("LSP error"))),
                                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = tx.send(r);
                        }
                    }
                    (None, Some("textDocument/publishDiagnostics")) => {
                        if let Some(uri) = msg["params"]["uri"].as_str() {
                            pushed.lock().unwrap().insert(uri.to_string(), msg["params"]["diagnostics"].clone());
                        }
                    }
                    _ => {}
                }
            }
            // Fail anything still waiting.
            for (_, tx) in pending.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!("language server exited")));
            }
        })?;

        let root_uri = path_to_uri(root);
        let init = server.request_blocking(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()}],
                "capabilities": {
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "publishDiagnostics": {},
                        "diagnostic": {"dynamicRegistration": false},
                        "definition": {"linkSupport": true}
                    },
                    "workspace": {"configuration": true, "workspaceFolders": true}
                }
            }),
            init_timeout,
            &reply_rx,
        )?;
        server.pull_diagnostics = !init["capabilities"]["diagnosticProvider"].is_null();
        server.notify("initialized", json!({}))?;

        // Keep answering server requests for the rest of the session.
        let stdin_writer = Arc::new(server);
        let weak = Arc::downgrade(&stdin_writer);
        std::thread::Builder::new().name(format!("lsp-{name}-replies")).spawn(move || {
            while let Ok(reply) = reply_rx.recv() {
                let Some(s) = weak.upgrade() else { break };
                let _ = s.send(&reply);
            }
        })?;
        Ok(stdin_writer)
    }

    fn send(&self, value: &Value) -> Result<()> {
        write_message(&mut *self.stdin.lock().unwrap(), value)
    }

    fn request_blocking(&self, method: &str, params: Value, timeout: Duration, replies: &std::sync::mpsc::Receiver<Value>) -> Result<Value> {
        let rx = self.request(method, params)?;
        let deadline = std::time::Instant::now() + timeout;
        let mut rx = rx;
        loop {
            // Answer server requests that arrive during the handshake.
            while let Ok(reply) = replies.try_recv() {
                self.send(&reply)?;
            }
            match rx.try_recv() {
                Ok(Some(r)) => return r,
                Ok(None) => {}
                Err(_) => bail!("{} closed during {method}", self.name),
            }
            if std::time::Instant::now() > deadline {
                bail!("{} timed out during {method}", self.name);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn request(&self, method: &str, params: Value) -> Result<oneshot::Receiver<Result<Value>>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(rx)
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    pub fn did_open(&self, path: &Path, language_id: &str, version: i64, text: &str) -> Result<()> {
        self.notify("textDocument/didOpen", json!({"textDocument": {"uri": path_to_uri(path), "languageId": language_id, "version": version, "text": text}}))
    }

    /// Full-text change (valid for both full and incremental sync servers).
    pub fn did_change(&self, path: &Path, version: i64, text: &str) -> Result<()> {
        self.notify("textDocument/didChange", json!({"textDocument": {"uri": path_to_uri(path), "version": version}, "contentChanges": [{"text": text}]}))
    }

    pub fn did_save(&self, path: &Path) -> Result<()> {
        self.notify("textDocument/didSave", json!({"textDocument": {"uri": path_to_uri(path)}}))
    }

    pub fn did_close(&self, path: &Path) -> Result<()> {
        self.notify("textDocument/didClose", json!({"textDocument": {"uri": path_to_uri(path)}}))
    }

    /// Current diagnostics for a document (pull if supported, else the last pushed set).
    pub async fn diagnostics(&self, path: &Path) -> Result<Vec<LspDiagnostic>> {
        let uri = path_to_uri(path);
        let items = if self.pull_diagnostics {
            let rx = self.request("textDocument/diagnostic", json!({"textDocument": {"uri": uri}}))?;
            let v = rx.await.map_err(|_| anyhow!("cancelled"))??;
            v.get("items").cloned().unwrap_or(Value::Array(vec![]))
        } else {
            self.pushed.lock().unwrap().get(&uri).cloned().unwrap_or(Value::Array(vec![]))
        };
        Ok(parse_diagnostics(&items))
    }

    pub async fn definition(&self, path: &Path, line: usize, character: usize) -> Result<Vec<Location>> {
        let rx = self.request(
            "textDocument/definition",
            json!({"textDocument": {"uri": path_to_uri(path)}, "position": {"line": line, "character": character}}),
        )?;
        let v = rx.await.map_err(|_| anyhow!("cancelled"))??;
        let arr = match v {
            Value::Array(a) => a,
            Value::Null => vec![],
            other => vec![other],
        };
        Ok(arr
            .iter()
            .filter_map(|l| {
                let uri = l.get("targetUri").or_else(|| l.get("uri"))?.as_str()?;
                let range = l.get("targetSelectionRange").or_else(|| l.get("range"))?;
                Some(Location {
                    path: uri_to_path(uri)?,
                    line: range["start"]["line"].as_u64()? as usize,
                    character: range["start"]["character"].as_u64()? as usize,
                })
            })
            .collect())
    }
}

pub fn parse_diagnostics(items: &Value) -> Vec<LspDiagnostic> {
    items
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let r = &d["range"];
                    Some(LspDiagnostic {
                        line: r["start"]["line"].as_u64()? as usize,
                        character: r["start"]["character"].as_u64()? as usize,
                        end_line: r["end"]["line"].as_u64()? as usize,
                        end_character: r["end"]["character"].as_u64()? as usize,
                        severity: d["severity"].as_u64().unwrap_or(1) as u8,
                        message: d["message"].as_str()?.to_string(),
                        code: match &d["code"] {
                            Value::String(s) => Some(s.clone()),
                            Value::Number(n) => Some(n.to_string()),
                            _ => None,
                        },
                        source: d["source"].as_str().map(|s| s.to_string()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Drop for LspServer {
    fn drop(&mut self) {
        let _ = self.send(&json!({"jsonrpc": "2.0", "id": 0, "method": "shutdown"}));
        let _ = self.send(&json!({"jsonrpc": "2.0", "method": "exit"}));
        if let Ok(mut c) = self.child.lock() {
            let pid = c.id() as i32;
            // SAFETY: signalling the process group we created for this server.
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
            let _ = c.kill();
            let _ = c.wait();
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let p = Path::new("/Users/a b/código/x.ts");
        let uri = path_to_uri(p);
        assert!(uri.starts_with("file:///Users/a%20b/c%C3%B3digo/x.ts"));
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn framing_roundtrip() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"a": 1})).unwrap();
        let mut r = BufReader::new(&buf[..]);
        assert_eq!(read_message(&mut r).unwrap().unwrap(), json!({"a": 1}));
    }

    #[test]
    fn parses_diagnostics() {
        let v = json!([{"range": {"start": {"line": 8, "character": 6}, "end": {"line": 8, "character": 8}}, "severity": 1, "message": "Type error", "code": 2322, "source": "ts"}]);
        let d = parse_diagnostics(&v);
        assert_eq!(d[0].line, 8);
        assert_eq!(d[0].code.as_deref(), Some("2322"));
    }

    #[test]
    fn drop_kills_whole_process_group() {
        // A fake "server" that spawns a grandchild and never answers initialize.
        let dir = std::env::temp_dir().join(format!("jt-lsp-pg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("srv.sh");
        let marker = dir.join("child.pid");
        std::fs::write(&script, format!("#!/bin/sh\nsleep 300 &\necho $! > {}\nwait\n", marker.display())).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        // start() fails on initialize timeout, which drops the server → group must die.
        let started = std::thread::spawn({
            let script = script.clone();
            let dir = dir.clone();
            move || LspServer::start_with_timeout("fake", &script, &[], &dir, Duration::from_secs(2)).is_err()
        });
        std::thread::sleep(Duration::from_millis(500));
        let grandchild: i32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
        assert!(started.join().unwrap());
        std::thread::sleep(Duration::from_millis(200));
        let alive = unsafe { libc::kill(grandchild, 0) } == 0;
        assert!(!alive, "grandchild {grandchild} survived");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Real servers: set JT_TSGO (tsgo binary) and JT_LSP_REPO (a TS project) to run.
    #[test]
    fn real_tsgo_reports_errors_without_saving() {
        let (Some(tsgo), Some(repo)) = (std::env::var_os("JT_TSGO"), std::env::var_os("JT_LSP_REPO")) else { return };
        let repo = PathBuf::from(repo);
        let server = LspServer::start("tsgo", Path::new(&tsgo), &["--lsp", "--stdio"], &repo).unwrap();
        let file = repo.join("src/__jt_probe.ts");
        server.did_open(&file, "typescript", 1, "const a: number = 'x';\n").unwrap();
        let diags = futures::executor::block_on(server.diagnostics(&file)).unwrap();
        assert!(diags.iter().any(|d| d.severity == 1 && d.line == 0), "{diags:?}");
        server.did_change(&file, 2, "const a: number = 1;\nexport {}\n").unwrap();
        let diags = futures::executor::block_on(server.diagnostics(&file)).unwrap();
        assert!(!diags.iter().any(|d| d.severity == 1), "{diags:?}");
    }
}
