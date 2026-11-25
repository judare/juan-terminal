//! Read-only gate for the headless Claude that drafts WhatsApp replies with a project's context.
//! Runs as a Claude Code `PreToolUse` hook (`juan-terminal --wa-tool-gate`): reads the hook JSON
//! on stdin and allows only tools that look at things (files, read-only shell commands, read-only
//! `ssh_exec`, MCP/connector tools named get/list/search/read…, SELECT queries). Everything else
//! is denied, whatever the user's own permission rules say.

use serde_json::{Value, json};

/// Entry point for `--wa-tool-gate`.
pub fn run() {
    let mut input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let tool = v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
    let args = v.get("tool_input").cloned().unwrap_or(Value::Null);
    let (allow, reason) = match decide(tool, &args) {
        Ok(()) => (true, "lectura permitida".to_string()),
        Err(r) => (false, r),
    };
    println!(
        "{}",
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": if allow { "allow" } else { "deny" },
            "permissionDecisionReason": reason,
        }})
    );
}

const READ_TOOLS: &[&str] = &["Read", "Grep", "Glob", "LS", "WebFetch", "WebSearch", "TodoWrite", "ToolSearch", "ListMcpResourcesTool", "ReadMcpResourceTool", "Task", "Agent"];

const OURS_OK: &[&str] = &["whatsapp_read_chat", "whatsapp_search", "whatsapp_get_media", "whatsapp_list_chats", "list_servers", "list_dev_servers"];

/// First word of an MCP tool name that marks it as read-only.
const READ_VERBS: &[&str] = &[
    "get", "list", "search", "read", "query", "describe", "fetch", "find", "view", "show", "lookup", "count", "retrieve", "download", "inspect", "explain", "preview", "browse", "workspace",
];

pub fn decide(tool: &str, args: &Value) -> Result<(), String> {
    if READ_TOOLS.contains(&tool) {
        return Ok(());
    }
    if tool == "Bash" {
        let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
        return read_only_shell(cmd);
    }
    if let Some(rest) = tool.strip_prefix("mcp__") {
        let (server, name) = rest.split_once("__").unwrap_or(("", rest));
        if server == "juan-terminal" {
            if OURS_OK.contains(&name) {
                return Ok(());
            }
            if name == "ssh_exec" {
                let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
                return read_only_shell(cmd);
            }
            return Err(format!("{name} no está permitido: modo solo lectura."));
        }
        let lower = name.to_ascii_lowercase();
        if lower.contains("sql") {
            return match args.as_object().into_iter().flat_map(|o| o.values()).filter_map(|v| v.as_str()).find(|s| looks_like_sql(s)) {
                Some(sql) if read_only_sql(sql) => Ok(()),
                _ => Err("Solo se permiten consultas SELECT (modo solo lectura).".into()),
            };
        }
        let first = lower.split(['_', '-']).next().unwrap_or("");
        if READ_VERBS.contains(&first) {
            return Ok(());
        }
        return Err(format!("{name} puede modificar datos: modo solo lectura."));
    }
    Err(format!("{tool} no está permitido: modo solo lectura."))
}

fn looks_like_sql(s: &str) -> bool {
    let w = s.trim_start().split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    ["select", "with", "show", "explain", "describe", "desc", "insert", "update", "delete", "drop", "alter", "create", "truncate", "replace", "merge", "grant", "call", "set"].contains(&w.as_str())
}

fn read_only_sql(sql: &str) -> bool {
    let s = sql.trim().trim_end_matches(';').to_ascii_lowercase();
    if s.contains(';') {
        return false;
    }
    let first = s.split_whitespace().next().unwrap_or("");
    if !["select", "with", "show", "explain", "describe", "desc"].contains(&first) {
        return false;
    }
    let words: Vec<&str> = s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').collect();
    !words.iter().any(|w| ["insert", "update", "delete", "drop", "alter", "create", "truncate", "grant", "revoke", "merge", "replace", "into", "call", "lock"].contains(w))
}

/// Commands allowed as the program of each pipeline segment.
const SHELL_OK: &[&str] = &[
    "cat", "head", "tail", "grep", "egrep", "fgrep", "rg", "ls", "ll", "find", "wc", "du", "df", "free", "uptime", "ps", "pgrep", "pwd", "whoami", "hostname", "date", "echo", "printf", "stat", "file", "sort", "uniq", "cut", "awk", "sed", "jq", "tr", "column", "printenv", "uname", "id", "which", "type", "journalctl", "netstat", "ss", "lsof", "cd", "true", "nl", "zcat", "zgrep", "less", "more", "tree", "realpath", "basename", "dirname", "diff", "md5sum", "sha256sum", "getent", "dig", "nslookup", "ping", "vmstat", "iostat", "top", "w", "last", "env", "curl", "git", "docker", "systemctl", "service", "kubectl", "pm2", "supervisorctl", "nginx", "crontab", "redis-cli", "php", "node", "npm", "composer", "mysql", "psql",
];

pub fn read_only_shell(cmd: &str) -> Result<(), String> {
    let deny = |why: &str| Err(format!("Comando bloqueado (solo lectura): {why}"));
    let cleaned = cmd.replace("2>&1", "").replace("2>/dev/null", "").replace(">/dev/null", "").replace("> /dev/null", "");
    if cleaned.contains('>') {
        return deny("redirección a archivo");
    }
    if cleaned.contains('`') || cleaned.contains("$(") || cleaned.contains("<(") {
        return deny("sustitución de comandos");
    }
    for seg in cleaned.split(['|', ';', '\n', '&']) {
        let mut words = seg.split_whitespace().skip_while(|w| w.contains('=') && !w.starts_with('-')).peekable();
        let Some(prog) = words.next() else { continue };
        let prog = prog.rsplit('/').next().unwrap_or(prog);
        let rest: Vec<&str> = words.collect();
        let has = |flags: &[&str]| rest.iter().any(|w| flags.iter().any(|f| w == f || w.starts_with(&format!("{f}="))));
        let sub = rest.iter().find(|w| !w.starts_with('-')).copied().unwrap_or("");
        let ok = match prog {
            p if !SHELL_OK.contains(&p) => false,
            "find" => !has(&["-delete", "-exec", "-execdir", "-ok", "-fprint", "-fprintf", "-fls"]),
            "sed" => !rest.iter().any(|w| w.starts_with("-i") || *w == "--in-place"),
            "awk" => !seg.contains("system") && !seg.contains("print >"),
            "env" => rest.is_empty(),
            "curl" => !has(&["-X", "--request", "-d", "--data", "--data-raw", "--data-binary", "--data-urlencode", "-F", "--form", "-T", "--upload-file", "-o", "--output", "-O"]),
            "git" => ["log", "show", "status", "diff", "branch", "remote", "rev-parse", "ls-files", "blame", "describe", "shortlog", "reflog", "grep", "config"].contains(&sub) && !(sub == "config" && !has(&["--get", "--list", "-l"])),
            "docker" => {
                let sub2 = rest.iter().filter(|w| !w.starts_with('-')).nth(1).copied().unwrap_or("");
                ["ps", "logs", "inspect", "images", "stats", "top", "version", "info"].contains(&sub) || (sub == "compose" && ["ps", "logs", "config", "images", "top"].contains(&sub2))
            }
            "systemctl" | "service" => ["status", "is-active", "is-enabled", "list-units", "list-timers", "show", "cat"].contains(&sub) || rest.last() == Some(&"status"),
            "kubectl" => ["get", "describe", "logs", "top", "explain", "version"].contains(&sub),
            "pm2" => ["list", "ls", "logs", "status", "show", "describe", "jlist", "info"].contains(&sub),
            "supervisorctl" => ["status", "tail", "avail"].contains(&sub),
            "nginx" => has(&["-t", "-T", "-v", "-V"]),
            "crontab" => has(&["-l"]),
            "redis-cli" => {
                let up = seg.to_ascii_uppercase();
                ["GET ", "KEYS ", "SCAN ", "INFO", "TTL ", "HGET", "LRANGE ", "SMEMBERS ", "EXISTS ", "TYPE ", "DBSIZE", "PING", "ZRANGE "].iter().any(|k| up.contains(k))
            }
            "php" | "node" | "npm" | "composer" => has(&["-v", "--version", "-V"]) || (prog == "npm" && ["ls", "list", "view", "outdated"].contains(&sub)) || (prog == "composer" && ["show", "outdated"].contains(&sub)),
            "mysql" | "psql" => {
                let q = rest.iter().position(|w| *w == "-e" || *w == "-c").and_then(|i| rest.get(i + 1..)).map(|r| r.join(" ")).unwrap_or_default();
                let q = q.trim_matches(['"', '\'']);
                !q.is_empty() && read_only_sql(q)
            }
            _ => true,
        };
        if !ok {
            return deny(seg.trim());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell() {
        assert!(read_only_shell("tail -n 200 /var/log/app.log | grep ERROR").is_ok());
        assert!(read_only_shell("docker logs api --tail 50 2>&1").is_ok());
        assert!(read_only_shell("git log --oneline -5").is_ok());
        assert!(read_only_shell("rm -rf /").is_err());
        assert!(read_only_shell("echo hi > x").is_err());
        assert!(read_only_shell("ls && rm x").is_err());
        assert!(read_only_shell("sed -i s/a/b/ f").is_err());
        assert!(read_only_shell("find . -delete").is_err());
        assert!(read_only_shell("docker restart api").is_err());
        assert!(read_only_shell("systemctl restart nginx").is_err());
        assert!(read_only_shell("systemctl status nginx").is_ok());
        assert!(read_only_shell("cat $(echo x)").is_err());
        assert!(read_only_shell("mysql -e \"SELECT id FROM users LIMIT 5\"").is_ok());
        assert!(read_only_shell("mysql -e \"DELETE FROM users\"").is_err());
        assert!(read_only_shell("curl -X POST https://x").is_err());
    }

    #[test]
    fn tools() {
        assert!(decide("Read", &Value::Null).is_ok());
        assert!(decide("Edit", &Value::Null).is_err());
        assert!(decide("Write", &Value::Null).is_err());
        assert!(decide("mcp__juan-terminal__whatsapp_send", &Value::Null).is_err());
        assert!(decide("mcp__juan-terminal__ssh_exec", &json!({"command": "df -h"})).is_ok());
        assert!(decide("mcp__juan-terminal__ssh_exec", &json!({"command": "reboot"})).is_err());
        assert!(decide("mcp__claude_ai_Gmail__search_threads", &Value::Null).is_ok());
        assert!(decide("mcp__claude_ai_Gmail__send_message", &Value::Null).is_err());
        assert!(decide("mcp__blokay__run_sql", &json!({"sql": "select * from t"})).is_ok());
        assert!(decide("mcp__blokay__run_sql", &json!({"sql": "update t set a=1"})).is_err());
    }
}
