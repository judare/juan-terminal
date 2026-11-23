//! Stdio MCP server (`juan-terminal --mcp-servers`) giving Claude Code two tools for the
//! active project's SSH servers: `list_servers` and `ssh_exec`. Claude asks the user for
//! permission before each call like any MCP tool. Servers are read from the saved app state
//! for the project in `$JT_PROJECT_ID`, so edits in the Servers tab apply without restarts.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};
use workspace_state::{SshServer, StateStore};

fn servers(project_id: &str) -> Vec<SshServer> {
    StateStore::default_location().load().projects.into_iter().find(|p| p.id == project_id).map(|p| p.servers).unwrap_or_default()
}

fn find<'a>(list: &'a [SshServer], key: &str) -> Option<&'a SshServer> {
    list.iter().find(|s| s.name == key || s.id == key || s.host == key || s.destination() == key)
}

pub fn handle(project_id: &str, msg: &Value, list: impl Fn() -> Vec<SshServer>) -> Option<Value> {
    handle_with(project_id, msg, list, &|| String::from("Dev server listing is not available."))
}

/// `dev_servers`: report for the `list_dev_servers` tool (provided by the app).
pub fn handle_with(project_id: &str, msg: &Value, list: impl Fn() -> Vec<SshServer>, dev_servers: &dyn Fn() -> String) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "juan-terminal", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "You are running inside juan-terminal, the user's development workspace. When the user asks to see, screenshot, open or test their app/page (e.g. with a browser tool), first call list_dev_servers and use this project's local dev server URL (http://localhost:PORT) rather than the production site, unless they explicitly ask for production. If none is running, start it yourself in the background (list_dev_servers gives the repo's start command), wait until the URL responds, and then take the screenshot — don't fall back to production. For the project's remote machines use list_servers and ssh_exec.",
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [
            {
                "name": "list_dev_servers",
                "description": "List local dev servers (listening ports) of the current project, with their localhost URLs, process and working directory. Use it before looking at or screenshotting the user's app so you use the local version.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": {"readOnlyHint": true},
            },
            {
                "name": "list_servers",
                "description": "List the SSH servers configured for this project (name, user@host, port).",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": {"readOnlyHint": true},
            },
            {
                "name": "ssh_exec",
                "description": "Run a shell command on one of the project's SSH servers and return stdout, stderr and the exit code. Non-interactive: commands that prompt will fail or time out.",
                "inputSchema": {"type": "object", "properties": {
                    "server": {"type": "string", "description": "Server name (from list_servers)"},
                    "command": {"type": "string", "description": "Shell command to run on the server"},
                    "timeout_secs": {"type": "integer", "description": "Default 60, max 600"},
                }, "required": ["server", "command"]},
            },
        ]}),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let (text, is_error) = if name == "list_dev_servers" { (dev_servers(), false) } else { call(project_id, name, &args, &list) };
            json!({"content": [{"type": "text", "text": text}], "isError": is_error})
        }
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Method not found: {method}")}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn call(project_id: &str, name: &str, args: &Value, list: &impl Fn() -> Vec<SshServer>) -> (String, bool) {
    let servers = list();
    match name {
        "list_servers" => {
            if servers.is_empty() {
                return (format!("No SSH servers configured for this project ({project_id}). Add one in juan-terminal's Servers tab."), false);
            }
            let lines: Vec<String> = servers.iter().map(|s| format!("- {}: {} (port {})", s.name, s.destination(), s.port_or_default())).collect();
            (lines.join("\n"), false)
        }
        "ssh_exec" => {
            let key = args.get("server").and_then(|v| v.as_str()).unwrap_or_default();
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or_default();
            if command.trim().is_empty() {
                return ("command is required".into(), true);
            }
            let Some(server) = find(&servers, key) else {
                let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
                return (format!("Unknown server {key:?}. Available: {}", names.join(", ")), true);
            };
            let timeout = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(60).clamp(1, 600);
            match crate::exec(server, command, Duration::from_secs(timeout)) {
                Ok(out) => {
                    let status = match out.code {
                        Some(c) => format!("exit code: {c}"),
                        None => format!("timed out after {timeout}s (killed)"),
                    };
                    let mut text = format!("{status} · {}\n", server.destination());
                    if !out.stdout.is_empty() {
                        text.push_str(&format!("--- stdout ---\n{}", out.stdout));
                    }
                    if !out.stderr.is_empty() {
                        text.push_str(&format!("\n--- stderr ---\n{}", out.stderr));
                    }
                    if out.truncated {
                        text.push_str("\n[output truncated]");
                    }
                    (text, out.code != Some(0))
                }
                Err(e) => (format!("{e:#}"), true),
            }
        }
        _ => (format!("Unknown tool: {name}"), true),
    }
}

/// Serve MCP over stdin/stdout until EOF.
pub fn serve_stdio() {
    serve_stdio_with(&|| String::from("Dev server listing is not available."))
}

pub fn serve_stdio_with(dev_servers: &dyn Fn() -> String) {
    let project_id = std::env::var("JT_PROJECT_ID").unwrap_or_default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(reply) = handle_with(&project_id, &msg, || servers(&project_id), dev_servers) {
            let _ = writeln!(stdout, "{reply}");
            let _ = stdout.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<SshServer> {
        // Points at a closed local port: exec fails fast without network access.
        vec![SshServer { id: "a".into(), name: "local-closed".into(), host: "127.0.0.1".into(), port: 1, user: "nobody".into(), identity_file: None, start_dir: String::new() }]
    }

    #[test]
    fn mcp_lists_and_execs() {
        let r = handle("p", &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}), list).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        let r = handle("p", &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), list).unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 3);
        assert!(handle("p", &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), list).is_none());
        let r = handle("p", &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "list_servers", "arguments": {}}}), list).unwrap();
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("nobody@127.0.0.1"));
        let r = handle("p", &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "ssh_exec", "arguments": {"server": "nope", "command": "ls"}}}), list).unwrap();
        assert_eq!(r["result"]["isError"], true);
        // Real ssh against a closed port: a clean error, not a hang.
        let r = handle("p", &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "ssh_exec", "arguments": {"server": "local-closed", "command": "uptime", "timeout_secs": 10}}}), list).unwrap();
        assert_eq!(r["result"]["isError"], true);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("exit code: 255") && text.to_lowercase().contains("refused"), "{text}");
    }
}
