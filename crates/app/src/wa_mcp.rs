//! MCP tools giving Claude Code access to the user's WhatsApp (in the `--mcp-servers`
//! process). Reads come straight from the local cache; sending and downloading go through
//! the running app, which owns the connection and asks the user to approve every send.

use std::path::PathBuf;

use base64::Engine;
use chrono::{Local, TimeZone};
use serde_json::{Value, json};
use whatsapp::{ChatMessage, MsgKind, Store};

pub const INSTRUCTIONS: &str = " The user's WhatsApp is available through whatsapp_list_chats, whatsapp_read_chat, whatsapp_search, whatsapp_get_media and whatsapp_send (text and file attachments; the user approves each send in juan-terminal). Chats can be referred to by name, phone number or JID. Never send messages the user didn't ask for.";

pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "whatsapp_list_chats",
            "description": "List the user's WhatsApp chats (newest first, archived excluded): name, JID, labels, unread count and last message.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Filter by chat name, phone or JID"},
                "label": {"type": "string", "description": "Only chats with this label (name, case-insensitive)"},
                "unread_only": {"type": "boolean", "description": "Only chats with unread messages"},
                "limit": {"type": "integer", "description": "Default 30, max 200"},
            }},
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "whatsapp_read_chat",
            "description": "Read the messages of one WhatsApp chat (oldest to newest), with sender, time, message id, replies, reactions and media info. Use `before` (unix seconds) to page back.",
            "inputSchema": {"type": "object", "properties": {
                "chat": {"type": "string", "description": "Chat name, phone number or JID"},
                "limit": {"type": "integer", "description": "Messages to return (default 40, max 300)"},
                "before": {"type": "integer", "description": "Only messages older than this unix timestamp"},
            }, "required": ["chat"]},
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "whatsapp_search",
            "description": "Search the text of WhatsApp messages (all chats or one), newest first.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string"},
                "chat": {"type": "string", "description": "Optional chat name, phone or JID to search in"},
                "limit": {"type": "integer", "description": "Default 30, max 200"},
            }, "required": ["query"]},
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "whatsapp_get_media",
            "description": "Get the media of a WhatsApp message (image, sticker, video, audio, document): downloads it if needed and returns the local file path; images are also returned inline so you can see them.",
            "inputSchema": {"type": "object", "properties": {
                "chat": {"type": "string", "description": "Chat name, phone number or JID"},
                "message_id": {"type": "string", "description": "Message id from whatsapp_read_chat"},
            }, "required": ["chat", "message_id"]},
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "whatsapp_send",
            "description": "Send a WhatsApp message from the user's account: text and/or files (absolute paths; images, videos and audio are sent as media, anything else as a document). The user must approve it in juan-terminal before it is sent. Optionally reply to a message id.",
            "inputSchema": {"type": "object", "properties": {
                "chat": {"type": "string", "description": "Chat name, phone number (with country code) or JID"},
                "text": {"type": "string", "description": "Message text (caption of the first file when files are attached)"},
                "files": {"type": "array", "items": {"type": "string"}, "description": "Absolute paths of files to attach"},
                "reply_to": {"type": "string", "description": "Message id to quote"},
            }, "required": ["chat"]},
        }),
    ]
}

fn text(s: impl Into<String>) -> Value {
    json!({"type": "text", "text": s.into()})
}

fn store() -> Result<Store, String> {
    let path = whatsapp::data_dir().join("cache.db");
    if !path.exists() {
        return Err("WhatsApp no está vinculado en juan-terminal (abre la pestaña WhatsApp en la barra lateral y escanea el QR).".into());
    }
    Store::open_read_only(&path).map_err(|e| e.to_string())
}

fn when(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

pub(crate) fn describe(m: &ChatMessage) -> String {
    let mut line = format!("[{}] {}: ", when(m.ts), if m.from_me { "Yo".to_string() } else { m.sender_name.trim_start_matches('~').to_string() });
    if let Some(q) = &m.quoted {
        let qt: String = q.text.chars().take(80).collect();
        line.push_str(&format!("(respondiendo a {} \"{}\") ", q.sender_name.trim_start_matches('~'), qt.replace('\n', " ")));
    }
    if m.kind.has_media() {
        let what = match m.kind {
            MsgKind::Image => "imagen",
            MsgKind::Video => "video",
            MsgKind::Audio => "audio",
            MsgKind::Voice => "nota de voz",
            MsgKind::Sticker => "sticker",
            _ => "documento",
        };
        let mut info = what.to_string();
        if let Some(d) = m.duration {
            info.push_str(&format!(" {d}s"));
        }
        if let Some(f) = &m.file_name {
            info.push_str(&format!(" \"{f}\""));
        }
        line.push_str(&format!("<{info}{}> ", if m.media.is_some() { ", descargado" } else { "" }));
        if m.kind != MsgKind::Document || m.file_name.as_deref() != Some(m.text.as_str()) {
            line.push_str(&m.text);
        }
        if let Some(t) = m.transcript.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            line.push_str(&format!("(transcripción: \"{}\")", t.replace('\n', " ")));
        }
    } else if m.kind == MsgKind::Text {
        line.push_str(&m.text);
    } else {
        line.push_str(&m.preview());
    }
    if m.edited {
        line.push_str(" (editado)");
    }
    if !m.reactions.is_empty() {
        let r: Vec<String> = m.reactions.iter().map(|r| format!("{} {}", r.emoji, r.sender_name.trim_start_matches('~'))).collect();
        line.push_str(&format!(" [reacciones: {}]", r.join(", ")));
    }
    line.push_str(&format!("  (id: {})", m.id));
    line
}

fn arg_str<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
}

fn limit(args: &Value, default: usize, max: usize) -> usize {
    args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(default).clamp(1, max)
}

fn list_chats(args: &Value) -> Result<Vec<Value>, String> {
    let st = store()?;
    let q = arg_str(args, "query").map(|q| q.to_lowercase());
    let unread_only = args.get("unread_only").and_then(|v| v.as_bool()).unwrap_or(false);
    let labels = st.labels();
    let label_name = |id: i64| labels.iter().find(|l| l.id == id).map(|l| l.name.clone());
    let label_filter = match arg_str(args, "label") {
        Some(name) => Some(labels.iter().find(|l| l.name.to_lowercase() == name.to_lowercase()).map(|l| l.id).ok_or_else(|| {
            let all: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
            format!("No existe la etiqueta \"{name}\". Etiquetas: {}", if all.is_empty() { "(ninguna)".to_string() } else { all.join(", ") })
        })?),
        None => None,
    };
    let chats: Vec<_> = st
        .chats()
        .into_iter()
        .filter(|c| q.as_ref().is_none_or(|q| c.name.to_lowercase().contains(q) || c.jid.contains(q.as_str())))
        .filter(|c| !unread_only || c.unread > 0)
        .filter(|c| label_filter.is_none_or(|id| c.labels.contains(&id)))
        .take(limit(args, 30, 200))
        .collect();
    if chats.is_empty() {
        return Ok(vec![text("No hay chats que coincidan.")]);
    }
    let lines: Vec<String> = chats
        .iter()
        .map(|c| {
            let flags = [(c.unread > 0).then(|| format!("{} sin leer", c.unread)), c.pinned.then(|| "fijado".into()), c.important.then(|| "importante".into()), c.muted.then(|| "silenciado".into()), c.is_group.then(|| "grupo".into())]
                .into_iter()
                .flatten()
                .chain(c.labels.iter().filter_map(|&id| label_name(id)).map(|n| format!("#{n}")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("- {} <{}>{} — {} {}\"{}\"", c.name, c.jid, if flags.is_empty() { String::new() } else { format!(" ({flags})") }, when(c.last_ts), if c.last_from_me { "Yo: ".to_string() } else if c.last_sender.is_empty() { String::new() } else { format!("{}: ", c.last_sender) }, if c.last_kind != MsgKind::Text && c.last_text != c.last_kind.label() { format!("[{}] {}", c.last_kind.label(), c.last_text) } else { c.last_text.clone() })
        })
        .collect();
    Ok(vec![text(lines.join("\n"))])
}

fn read_chat(args: &Value) -> Result<Vec<Value>, String> {
    let st = store()?;
    let q = arg_str(args, "chat").ok_or("Falta `chat`.")?;
    let jid = st.resolve_chat(q).ok_or_else(|| format!("No encontré el chat \"{q}\"."))?;
    let msgs = st.messages(&jid, args.get("before").and_then(|v| v.as_i64()), limit(args, 40, 300));
    let name = st.display_name(&jid);
    if msgs.is_empty() {
        return Ok(vec![text(format!("{name} <{jid}>: no hay mensajes guardados (más antiguos de los que el teléfono sincronizó no están disponibles)."))]);
    }
    let mut out = format!("Chat: {name} <{jid}> — {} mensajes (del más antiguo al más reciente)\n", msgs.len());
    for m in &msgs {
        out.push_str(&describe(m));
        out.push('\n');
    }
    if let Some(first) = msgs.first() {
        out.push_str(&format!("\nPara ver mensajes anteriores usa before={}", first.ts));
    }
    Ok(vec![text(out)])
}

fn search(args: &Value) -> Result<Vec<Value>, String> {
    let st = store()?;
    let q = arg_str(args, "query").ok_or("Falta `query`.")?;
    let chat = match arg_str(args, "chat") {
        Some(c) => Some(st.resolve_chat(c).ok_or_else(|| format!("No encontré el chat \"{c}\"."))?),
        None => None,
    };
    let hits = st.search(q, chat.as_deref(), limit(args, 30, 200));
    if hits.is_empty() {
        return Ok(vec![text(format!("Sin resultados para \"{q}\"."))]);
    }
    let lines: Vec<String> = hits.iter().map(|m| format!("{} <{}> {}", st.display_name(&m.chat), m.chat, describe(m))).collect();
    Ok(vec![text(lines.join("\n"))])
}

fn get_media(args: &Value) -> Result<Vec<Value>, String> {
    let st = store()?;
    let q = arg_str(args, "chat").ok_or("Falta `chat`.")?;
    let id = arg_str(args, "message_id").ok_or("Falta `message_id`.")?;
    let jid = st.resolve_chat(q).ok_or_else(|| format!("No encontré el chat \"{q}\"."))?;
    let m = st.message(&jid, id).ok_or("No encontré ese mensaje.")?;
    if !m.kind.has_media() {
        return Err("Ese mensaje no tiene archivos.".into());
    }
    let path = match m.media {
        Some(p) => p,
        None => PathBuf::from(whatsapp::ipc::request_download(&whatsapp::data_dir(), &jid, id)?),
    };
    let mut out = vec![text(format!("Archivo: {}", path.display()))];
    if matches!(m.kind, MsgKind::Image | MsgKind::Sticker) {
        let mime = match path.extension().and_then(|e| e.to_str()) {
            Some("png") => "image/png",
            Some("webp") => "image/webp",
            Some("gif") => "image/gif",
            _ => "image/jpeg",
        };
        if let Ok(bytes) = std::fs::read(&path)
            && bytes.len() < 5_000_000
        {
            out.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType": mime}));
        }
    }
    Ok(out)
}

fn send(args: &Value) -> Result<Vec<Value>, String> {
    let chat = arg_str(args, "chat").ok_or("Falta `chat`.")?.to_string();
    let files: Vec<PathBuf> = args.get("files").and_then(|f| f.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).map(PathBuf::from).collect()).unwrap_or_default();
    let req = whatsapp::ipc::SendRequest {
        id: 0,
        chat,
        chat_name: String::new(),
        text: args.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        files,
        quote: arg_str(args, "reply_to").map(str::to_string),
    };
    whatsapp::ipc::request_send(&whatsapp::data_dir(), &req).map(|m| vec![text(m)])
}

pub fn call(name: &str, args: &Value) -> Option<(Vec<Value>, bool)> {
    let result = match name {
        "whatsapp_list_chats" => list_chats(args),
        "whatsapp_read_chat" => read_chat(args),
        "whatsapp_search" => search(args),
        "whatsapp_get_media" => get_media(args),
        "whatsapp_send" => send(args),
        _ => return None,
    };
    Some(match result {
        Ok(c) => (c, false),
        Err(e) => (vec![text(e)], true),
    })
}
