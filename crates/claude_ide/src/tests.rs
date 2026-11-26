use super::*;
use futures::StreamExt;
use std::io::BufReader;
use std::time::Duration;

struct TestClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl TestClient {
    fn connect(port: u16, token: &str) -> std::io::Result<TestClient> {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        write!(
            s,
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mcp\r\nX-Claude-Code-Ide-Authorization: {token}\r\n\r\n"
        )?;
        let mut reader = BufReader::new(s.try_clone()?);
        let resp = ws::read_request(&mut reader)?; // same "head" parser works for responses
        if !resp.method.contains("HTTP/1.1") || resp.path != "101" {
            return Err(std::io::Error::other(format!("handshake failed: {} {}", resp.method, resp.path)));
        }
        assert_eq!(resp.header("sec-websocket-accept"), Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        assert_eq!(resp.header("sec-websocket-protocol"), Some("mcp"));
        Ok(TestClient { reader, writer: s })
    }

    fn send(&mut self, v: Value) {
        // Clients must mask frames.
        let payload = v.to_string().into_bytes();
        let mask = [9u8, 8, 7, 6];
        let mut f = vec![0x81u8];
        if payload.len() < 126 {
            f.push(0x80 | payload.len() as u8);
        } else {
            f.push(0x80 | 126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.writer.write_all(&f).unwrap();
    }

    fn recv(&mut self) -> Value {
        match ws::read_message(&mut self.reader).unwrap() {
            ws::Message::Text(t) => serde_json::from_str(&t).unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Next message with this id (skipping notifications).
    fn reply(&mut self, id: i64) -> Value {
        loop {
            let m = self.recv();
            if m.get("id") == Some(&json!(id)) {
                return m;
            }
        }
    }

    fn call(&mut self, id: i64, name: &str, args: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}}));
        self.reply(id)
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("jt-ide-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn lockfile_matches_what_claude_reads() {
    let dir = tmpdir("lock");
    let (srv, _rx) = IdeServer::start_in(&dir).unwrap();
    srv.set_workspace_folders(vec![PathBuf::from("/tmp/proj")]);
    let lock = dir.join(format!("{}.lock", srv.port()));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    assert_eq!(v["pid"], json!(std::process::id()));
    assert_eq!(v["transport"], "ws");
    assert_eq!(v["ideName"], IDE_NAME);
    assert_eq!(v["authToken"], srv.token());
    assert_eq!(v["workspaceFolders"], json!(["/tmp/proj"]));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777, 0o600);
    drop(srv);
    assert!(!lock.exists(), "lockfile removed on drop");
}

#[test]
fn rejects_bad_token() {
    let dir = tmpdir("auth");
    let (srv, _rx) = IdeServer::start_in(&dir).unwrap();
    assert!(TestClient::connect(srv.port(), "wrong").is_err());
}

#[test]
fn mcp_session_selection_diagnostics_and_diff() {
    let dir = tmpdir("mcp");
    let (srv, mut rx) = IdeServer::start_in(&dir).unwrap();
    let file = dir.join("a.ts");
    std::fs::write(&file, "const a = 1;\nconst b: string = 2;\n").unwrap();
    srv.set_selection(Some(Selection { file: file.clone(), start: (1, 0), end: (1, 5), text: "const".into() }));
    srv.set_editors(vec![OpenEditor {
        path: file.clone(),
        active: true,
        dirty: false,
        language_id: "typescript".into(),
        diagnostics: vec![Diagnostic { start: (1, 6), end: (1, 7), message: "Type 'number' is not assignable to type 'string'.".into(), severity: "Error", source: "tsgo".into(), code: Some("2322".into()) }],
    }]);

    let mut c = TestClient::connect(srv.port(), srv.token()).unwrap();
    c.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code"}}}));
    let init = c.reply(1);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    // The current selection is pushed right after the session is ready.
    let n = c.recv();
    assert_eq!(n["method"], "selection_changed");
    assert_eq!(n["params"]["selection"]["start"]["line"], 1);
    assert_eq!(n["params"]["text"], "const");
    assert!(matches!(futures::executor::block_on(rx.next()), Some(IdeRequest::Connection { connected: true })));
    assert_eq!(srv.connected(), 1);

    // A selection change is broadcast.
    srv.set_selection(Some(Selection { file: file.clone(), start: (0, 0), end: (0, 0), text: String::new() }));
    let n = c.recv();
    assert_eq!(n["params"]["selection"]["isEmpty"], true);

    c.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let names: Vec<String> = c.reply(2)["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert!(names.contains(&"getDiagnostics".to_string()) && names.contains(&"openDiff".to_string()));

    // getDiagnostics in the shape Claude parses: content[0].text = JSON [{uri, diagnostics}].
    let r = c.call(3, "getDiagnostics", json!({"uri": format!("file://{}", file.display())}));
    let parsed: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed[0]["uri"], format!("file://{}", file.display()));
    assert_eq!(parsed[0]["diagnostics"][0]["severity"], "Error");
    assert_eq!(parsed[0]["diagnostics"][0]["range"]["start"]["character"], 6);

    // openFile with startText resolves the line.
    let r = c.call(4, "openFile", json!({"filePath": file.display().to_string(), "startText": "const b"}));
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().starts_with("Opened file"));
    match futures::executor::block_on(rx.next()) {
        Some(IdeRequest::OpenFile { line, .. }) => assert_eq!(line, Some(1)),
        _ => panic!("expected OpenFile"),
    }

    // openDiff blocks until the UI answers; accepted → FILE_SAVED + contents.
    c.send(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "openDiff", "arguments": {"old_file_path": file.display().to_string(), "new_file_path": file.display().to_string(), "new_file_contents": "const a = 2;\n", "tab_name": "✻ [Claude Code] a.ts"}}}));
    match futures::executor::block_on(rx.next()) {
        Some(IdeRequest::OpenDiff { new_contents, tab_name, reply, .. }) => {
            assert_eq!(new_contents, "const a = 2;\n");
            assert!(tab_name.contains("a.ts"));
            reply.send(DiffOutcome::Saved(new_contents)).unwrap();
        }
        _ => panic!("expected OpenDiff"),
    }
    let r = c.reply(5);
    assert_eq!(r["result"]["content"][0]["text"], "FILE_SAVED");
    assert_eq!(r["result"]["content"][1]["text"], "const a = 2;\n");

    // Rejected.
    c.send(json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "openDiff", "arguments": {"old_file_path": "x", "new_file_path": "x", "new_file_contents": "", "tab_name": "t"}}}));
    if let Some(IdeRequest::OpenDiff { reply, .. }) = futures::executor::block_on(rx.next()) {
        reply.send(DiffOutcome::Rejected).unwrap();
    }
    assert_eq!(c.reply(6)["result"]["content"][0]["text"], "DIFF_REJECTED");

    // Unknown method → JSON-RPC error.
    c.send(json!({"jsonrpc": "2.0", "id": 7, "method": "nope"}));
    assert_eq!(c.reply(7)["error"]["code"], -32601);

    drop(c);
    assert!(matches!(futures::executor::block_on(rx.next()), Some(IdeRequest::Connection { connected: false })));
}

#[test]
fn hook_endpoint_needs_token_and_forwards_events() {
    let dir = tmpdir("hook");
    let (srv, mut rx) = IdeServer::start_in(&dir).unwrap();
    let post = |token: &str| {
        let body = r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash","cwd":"/tmp/p"}"#;
        let mut s = TcpStream::connect(("127.0.0.1", srv.port())).unwrap();
        write!(s, "POST /hook HTTP/1.1\r\nHost: x\r\nX-JT-Token: {token}\r\nX-JT-Terminal: term-7\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    };
    assert!(post("bad").starts_with("HTTP/1.1 401"));
    assert!(post(srv.token()).starts_with("HTTP/1.1 204"));
    match futures::executor::block_on(rx.next()) {
        Some(IdeRequest::Hook { terminal, event, message, cwd, .. }) => {
            assert_eq!(terminal.as_deref(), Some("term-7"));
            assert_eq!(event, "Notification");
            assert_eq!(message.as_deref(), Some("Claude needs your permission to use Bash"));
            assert_eq!(cwd.as_deref(), Some("/tmp/p"));
        }
        _ => panic!("expected Hook"),
    }
}

#[test]
fn statusline_posts_are_forwarded() {
    let dir = tmpdir("status");
    let (srv, mut rx) = IdeServer::start_in(&dir).unwrap();
    let body = r#"{"session_id":"abc","model":{"id":"claude-opus-5-5","display_name":"Opus 5.5"},"context_window":{"used_percentage":42.5},"effort":{"level":"medium"}}"#;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).unwrap();
    write!(s, "POST /statusline HTTP/1.1\r\nX-JT-Token: {}\r\nX-JT-Terminal: t9\r\nContent-Length: {}\r\n\r\n{body}", srv.token(), body.len()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 204"));
    match futures::executor::block_on(rx.next()) {
        Some(IdeRequest::Status { terminal, data }) => {
            assert_eq!(terminal.as_deref(), Some("t9"));
            assert_eq!(data["context_window"]["used_percentage"], 42.5);
        }
        _ => panic!("expected Status"),
    }
}

#[test]
fn hook_settings_are_valid_claude_settings() {
    let v: Value = serde_json::from_str(&hook_settings_json()).unwrap();
    for ev in ["Stop", "Notification", "SessionEnd", "UserPromptSubmit"] {
        let cmd = v["hooks"][ev][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("/hook") && cmd.contains("$JT_HOOK_TOKEN"));
    }
    assert!(v["statusLine"]["command"].as_str().unwrap().contains("/statusline"));
}

#[test]
fn stale_own_lockfiles_are_removed_others_kept() {
    let dir = tmpdir("stale");
    std::fs::write(dir.join("1111.lock"), json!({"pid": 999_999, "ideName": IDE_NAME}).to_string()).unwrap();
    std::fs::write(dir.join("2222.lock"), json!({"pid": 999_999, "ideName": "Visual Studio Code"}).to_string()).unwrap();
    std::fs::write(dir.join("3333.lock"), json!({"pid": std::process::id(), "ideName": IDE_NAME}).to_string()).unwrap();
    let (_srv, _rx) = IdeServer::start_in(&dir).unwrap();
    assert!(!dir.join("1111.lock").exists());
    assert!(dir.join("2222.lock").exists());
    assert!(dir.join("3333.lock").exists());
}

#[test]
#[ignore]
fn dump_hook_settings() {
    if let Ok(p) = std::env::var("JT_DUMP_SETTINGS") {
        std::fs::write(p, hook_settings_json()).unwrap();
    }
}

#[test]
fn tool_summaries() {
    let t = |v: &str| tool_summary(&serde_json::from_str(v).unwrap());
    assert_eq!(t(r#"{"tool_name":"Edit","tool_input":{"file_path":"/a/b/lib.rs"}}"#).as_deref(), Some("Edit · lib.rs"));
    assert_eq!(t(r#"{"tool_name":"Bash","tool_input":{"command":"cargo test\nmore"}}"#).as_deref(), Some("Bash · cargo test"));
    assert_eq!(t(r#"{"tool_name":"Bash","tool_input":{"command":"x","description":"Run tests"}}"#).as_deref(), Some("Bash · Run tests"));
    assert_eq!(t(r#"{"tool_name":"mcp__juan-terminal__ssh_exec","tool_input":{}}"#).as_deref(), Some("ssh_exec"));
    assert_eq!(t(r#"{"hook_event_name":"Stop"}"#), None);
}

#[test]
fn idle_reminder_is_not_a_request() {
    let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
    assert!(is_idle_notification(&v(r#"{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"x"}"#)));
    assert!(is_idle_notification(&v(r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input"}"#)));
    assert!(!is_idle_notification(&v(r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#)));
    assert!(!is_idle_notification(&v(r#"{"hook_event_name":"Stop"}"#)));
}
