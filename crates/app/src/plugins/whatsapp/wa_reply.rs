//! "Sugerir respuesta" in a WhatsApp chat: gathers everything about the chat (who it is,
//! labels, recent messages with voice-note transcriptions) and asks a headless `claude -p`
//! for the reply. The text goes into the chat's composer, never to a Claude terminal.
//!
//! When a label of the chat is linked to a project, Claude runs in that project's folders and
//! may read its code, its SSH servers (read-only commands) and the user's MCP servers/connectors
//! (read-only tools), all enforced by the `wa_gate` PreToolUse hook.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use whatsapp::{MsgKind, WhatsApp};

/// Recent messages given to Claude up front (it can page back with whatsapp_read_chat).
const HISTORY: usize = 40;
/// Voice notes near the end transcribed on the spot if the background queue hasn't yet.
const TRANSCRIBE_LAST: usize = 5;

const READ_TOOLS: &str = "mcp__juan-terminal__whatsapp_read_chat,mcp__juan-terminal__whatsapp_search,mcp__juan-terminal__whatsapp_get_media,mcp__juan-terminal__whatsapp_list_chats";

/// Projects linked (through labels) to chat `jid`.
pub fn linked_projects(jid: &str) -> Vec<workspace_state::Project> {
    let store = WhatsApp::global().store();
    let Some(chat) = store.chat(jid) else { return Vec::new() };
    let ids: Vec<String> = store.labels().into_iter().filter(|l| chat.labels.contains(&l.id)).filter_map(|l| l.project).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<workspace_state::Project> = workspace_state::StateStore::default_location().load().projects.into_iter().filter(|p| ids.contains(&p.id)).collect();
    out.sort_by_key(|p| ids.iter().position(|i| *i == p.id));
    out
}

/// Folders of the projects (root + repositories), existing and without duplicates.
pub fn project_folders(projects: &[workspace_state::Project]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in projects {
        for d in p.root.iter().cloned().chain(p.repositories.iter().map(|r| r.path.clone())) {
            if d.is_dir() && !out.iter().any(|o| d.starts_with(o)) {
                out.push(d);
            }
        }
    }
    out
}

/// What Claude should know about the linked projects and how it may look into them.
fn project_brief(projects: &[workspace_state::Project], read_only: bool) -> String {
    if projects.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n--- PROYECTO ASOCIADO A ESTE CHAT ---\n");
    for p in projects {
        out.push_str(&format!("Proyecto: {}\n", p.name));
        for d in project_folders(std::slice::from_ref(p)) {
            out.push_str(&format!("  Carpeta: {}\n", d.display()));
        }
        for s in &p.servers {
            let dest = if s.user.is_empty() { s.host.clone() } else { format!("{}@{}", s.user, s.host) };
            out.push_str(&format!("  Servidor SSH: {} ({dest})\n", if s.name.is_empty() { &s.host } else { &s.name }));
        }
    }
    out.push_str(
        "\nAntes de responder, investiga en el proyecto todo lo necesario para dar una respuesta correcta y concreta:\n\
         - Lee el código, la documentación (README, CLAUDE.md, docs/) y el historial de git de esas carpetas (Read, Grep, Glob, `git log`).\n\
         - Si la pregunta depende del estado real (logs, procesos, datos), consulta los servidores con list_servers y ssh_exec usando SOLO comandos de lectura (cat, tail, grep, ls, ps, df, docker ps/logs, systemctl status, SELECT…).\n\
         - Usa también los servidores MCP y conectores disponibles (bases de datos, Drive, Gmail, Notion, Linear…) únicamente para consultar.\n",
    );
    if read_only {
        out.push_str("- Todo es de SOLO LECTURA: no edites archivos, no ejecutes comandos que cambien nada, no envíes mensajes ni crees registros. Las acciones de escritura serán bloqueadas.\n");
    } else {
        out.push_str("- En los servidores y conectores limítate a consultar salvo que mi instrucción pida explícitamente cambiar algo.\n");
    }
    out
}

/// Everything Claude should know about the chat, as text.
fn context(jid: &str) -> String {
    let wa = WhatsApp::global();
    let store = wa.store();
    let mut msgs = store.messages(jid, None, HISTORY);
    // Make sure the latest voice notes have their text.
    if whatsapp::transcribe::available() {
        let work = whatsapp::data_dir().join("tmp");
        for m in msgs.iter_mut().rev().filter(|m| m.kind == MsgKind::Voice).take(TRANSCRIBE_LAST) {
            if let (None, Some(media)) = (&m.transcript, &m.media)
                && let Ok(t) = whatsapp::transcribe::transcribe(media, &work)
            {
                store.set_transcript(jid, &m.id, &t);
                m.transcript = Some(t);
            }
        }
    }
    let summary = store.chat(jid);
    let name = store.display_name(jid);
    let mut out = format!("Chat: {name}\nJID: {jid}\n");
    if let Some(phone) = jid.strip_suffix("@s.whatsapp.net") {
        out.push_str(&format!("Teléfono: +{phone}\n"));
    }
    if let Some(c) = &summary {
        out.push_str(&format!("Tipo: {}\n", if c.is_group { "grupo" } else { "conversación individual" }));
        let labels = store.labels();
        let names: Vec<&str> = c.labels.iter().filter_map(|id| labels.iter().find(|l| l.id == *id)).map(|l| l.name.as_str()).collect();
        if !names.is_empty() {
            out.push_str(&format!("Etiquetas: {}\n", names.join(", ")));
        }
        let flags: Vec<&str> = [(c.important, "favorito"), (c.pinned, "fijado"), (c.muted, "silenciado"), (store.is_archived(jid), "archivado")].into_iter().filter(|(on, _)| *on).map(|(_, f)| f).collect();
        if !flags.is_empty() {
            out.push_str(&format!("Marcas: {}\n", flags.join(", ")));
        }
        if c.unread > 0 {
            out.push_str(&format!("Sin leer: {}\n", c.unread));
        }
    }
    out.push_str(&format!("\nÚltimos {} mensajes (del más antiguo al más reciente; \"Yo\" soy yo):\n", msgs.len()));
    for m in &msgs {
        out.push_str(&crate::wa_mcp::describe(m));
        out.push('\n');
    }
    out
}

fn prompt(jid: &str, instruction: &str, projects: &[workspace_state::Project]) -> String {
    let mut p = String::from(
        "Eres mi asistente para WhatsApp. Redacta el mensaje que yo (\"Yo\") debería enviar AHORA en este chat, respondiendo a lo más reciente que quede pendiente.\n\
         Reglas:\n\
         - Responde ÚNICAMENTE con el texto del mensaje, listo para enviar: sin comillas, sin explicaciones, sin opciones, sin prefijos como \"Respuesta:\".\n\
         - Imita mi estilo en este chat: idioma, tuteo/usted, longitud, emojis, saludos. Natural y breve, como un mensaje de WhatsApp real.\n\
         - Las notas de voz vienen con su transcripción; tenlas en cuenta como texto.\n\
         - Si necesitas más contexto puedes usar whatsapp_read_chat (mensajes anteriores con `before`), whatsapp_search o whatsapp_get_media (para ver imágenes). Nunca envíes nada tú.\n\
         - No inventes datos concretos (fechas, cifras, compromisos) que no salgan del chat o del proyecto; si hacen falta, deja la respuesta abierta o pregunta.\n",
    );
    p.push_str(&project_brief(projects, true));
    if !instruction.is_empty() {
        p.push_str(&format!("\nIndicación mía para esta respuesta (o borrador a mejorar): {instruction}\n"));
    }
    p.push_str("\n--- CONTEXTO DEL CHAT ---\n");
    p.push_str(&context(jid));
    p
}

/// MCP config exposing our read-only WhatsApp tools to the headless Claude.
fn mcp_config() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = workspace_state::state_dir().join("mcp");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("whatsapp-reply.json");
    let body = serde_json::json!({"mcpServers": {"juan-terminal": {
        "type": "stdio",
        "command": exe.display().to_string(),
        "args": ["--mcp-servers"],
        "env": {"JT_APP_PID": std::process::id().to_string()},
    }}})
    .to_string();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
        std::fs::write(&path, body).ok()?;
    }
    Some(path)
}

/// Settings file installing `wa_gate` as a PreToolUse hook for every tool.
fn gate_settings() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = workspace_state::state_dir().join("mcp");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("whatsapp-gate-settings.json");
    let cmd = format!("'{}' --wa-tool-gate", exe.display().to_string().replace('\'', "'\\''"));
    let body = serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": cmd, "timeout": 10}]}]}}).to_string();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
        std::fs::write(&path, body).ok()?;
    }
    Some(path)
}

/// Suggested reply for `jid` (blocking: tens of seconds, minutes when it digs into a project).
/// `instruction` is whatever the user had typed in the composer.
pub fn suggest(jid: &str, instruction: &str) -> Result<String, String> {
    let claude = crate::moli::claude_bin().ok_or("No encontré `claude` instalado.")?;
    let projects = linked_projects(jid);
    let prompt = prompt(jid, instruction, &projects);
    let folders = project_folders(&projects);
    let mut cmd = Command::new(claude);
    cmd.args(["-p", "--output-format", "text"]);
    if projects.is_empty() {
        cmd.args(["--allowedTools", READ_TOOLS]);
        if let Some(cfg) = mcp_config() {
            cmd.arg("--mcp-config").arg(cfg).arg("--strict-mcp-config");
        }
        cmd.current_dir(whatsapp::data_dir());
    } else {
        // The project's own MCP servers and the user's connectors load as usual (no strict
        // config); the gate hook decides every tool call, so nothing can write.
        let settings = gate_settings().ok_or("No pude preparar el modo solo lectura.")?;
        cmd.arg("--settings").arg(settings);
        cmd.args(["--disallowedTools", "Edit,Write,NotebookEdit,MultiEdit,mcp__juan-terminal__whatsapp_send"]);
        cmd.args(["--max-turns", "60"]);
        if let Some(cfg) = crate::servers_mcp_config(&projects[0]) {
            cmd.arg("--mcp-config").arg(cfg);
        }
        for d in folders.iter().skip(1) {
            cmd.arg("--add-dir").arg(d);
        }
        cmd.current_dir(folders.first().cloned().unwrap_or_else(whatsapp::data_dir));
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("No se pudo ejecutar claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(format!("claude falló: {}", if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err.to_string() }));
    }
    let text = clean(&String::from_utf8_lossy(&out.stdout));
    if text.is_empty() {
        return Err("Claude no devolvió ninguna respuesta.".into());
    }
    Ok(text)
}

/// "Pedir a Claude…" on a chat: an interactive Claude Code session with the chat (and its
/// linked project) as context and the user's instruction as the task.
pub struct AskPlan {
    /// File holding the first prompt.
    pub prompt_file: PathBuf,
    pub cwd: PathBuf,
    pub add_dirs: Vec<PathBuf>,
    /// MCP config of the linked project (its SSH servers), if any.
    pub mcp_config: Option<PathBuf>,
    pub project_name: Option<String>,
}

/// Blocking (may transcribe voice notes).
pub fn ask_plan(jid: &str, instruction: &str) -> Result<AskPlan, String> {
    let projects = linked_projects(jid);
    let folders = project_folders(&projects);
    let mut p = String::from(
        "Te paso una conversación de mi WhatsApp y una instrucción sobre ella. Haz lo que pide la instrucción.\n\
         - \"Yo\" soy yo. Las notas de voz vienen transcritas.\n\
         - Para más contexto usa whatsapp_read_chat (con `before` para mensajes anteriores), whatsapp_search o whatsapp_get_media.\n\
         - Si la instrucción pide escribir o responder un mensaje, usa whatsapp_send: no se envía, queda en el compositor del chat para que yo lo revise y lo envíe.\n",
    );
    p.push_str(&format!("\nINSTRUCCIÓN: {instruction}\n"));
    p.push_str(&project_brief(&projects, false));
    p.push_str("\n--- CONTEXTO DEL CHAT ---\n");
    p.push_str(&context(jid));
    let dir = workspace_state::state_dir().join("wa-ask");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Keep only the latest few prompt files.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = rd.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
        files.sort();
        for (_, f) in files.iter().rev().skip(10) {
            let _ = std::fs::remove_file(f);
        }
    }
    let prompt_file = dir.join(format!("{}.md", whatsapp::store::now()));
    std::fs::write(&prompt_file, p).map_err(|e| e.to_string())?;
    Ok(AskPlan {
        prompt_file,
        cwd: folders.first().cloned().unwrap_or_else(whatsapp::data_dir),
        add_dirs: folders.iter().skip(1).cloned().collect(),
        mcp_config: projects.first().and_then(crate::servers_mcp_config),
        project_name: projects.first().map(|p| p.name.clone()),
    })
}

/// Drop wrapping quotes / code fences Claude sometimes adds despite the instructions.
fn clean(raw: &str) -> String {
    let mut t = raw.trim();
    if let Some(inner) = t.strip_prefix("```").and_then(|s| s.strip_suffix("```")) {
        t = inner.split_once('\n').map(|(_, rest)| rest).unwrap_or(inner).trim();
    }
    for (a, b) in [('"', '"'), ('“', '”'), ('«', '»')] {
        if t.len() > 2 && t.starts_with(a) && t.ends_with(b) && !t[a.len_utf8()..t.len() - b.len_utf8()].contains(['"', '“', '”']) {
            t = &t[a.len_utf8()..t.len() - b.len_utf8()];
        }
    }
    t.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn strips_wrappers() {
        assert_eq!(clean("  \"Hola, ¿cómo vas?\"\n"), "Hola, ¿cómo vas?");
        assert_eq!(clean("```\nDale, mañana te aviso\n```"), "Dale, mañana te aviso");
        assert_eq!(clean("Él dijo \"sí\" ayer"), "Él dijo \"sí\" ayer");
    }
}
