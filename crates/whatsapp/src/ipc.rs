//! Unix socket between the MCP process (spawned by Claude) and the app, which owns the only
//! WhatsApp connection. One JSON request per line, one JSON response per line.
//!
//! Request:  {"op":"send","chat":"…","text":"…","files":["/abs/path"],"quote":"msg id"}
//! Response: {"ok":true,"message":"…"} / {"ok":false,"error":"…"}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SendRequest {
    /// Assigned by the app when the request arrives.
    #[serde(default)]
    pub id: u64,
    pub chat: String,
    /// Resolved display name (filled by the app).
    #[serde(default)]
    pub chat_name: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub quote: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub error: String,
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("whatsapp.sock")
}

fn roundtrip(dir: &Path, body: serde_json::Value, timeout: Duration) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket_path(dir))
        .map_err(|_| "Fragua no está abierto o WhatsApp no está conectado en la app.".to_string())?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.write_all(format!("{body}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| format!("Sin respuesta de la app: {e}"))?;
    let resp: Response = serde_json::from_str(resp.trim()).map_err(|e| format!("Respuesta inválida: {e}"))?;
    if resp.ok { Ok(resp.message) } else { Err(resp.error) }
}

/// Client side (MCP process): have the app download a message's media; returns the local path.
pub fn request_download(dir: &Path, chat: &str, id: &str) -> Result<String, String> {
    roundtrip(dir, serde_json::json!({"op": "download", "chat": chat, "id": id}), Duration::from_secs(120))
}

/// Client side (MCP process): ask the running app to send a message. Blocks until the user
/// approves or rejects it in the app (up to 10 minutes).
pub fn request_send(dir: &Path, req: &SendRequest) -> Result<String, String> {
    let mut body = serde_json::json!({"op": "send"});
    if let (Some(obj), Ok(serde_json::Value::Object(extra))) = (body.as_object_mut(), serde_json::to_value(req)) {
        obj.extend(extra);
    }
    roundtrip(dir, body, Duration::from_secs(600))
}
