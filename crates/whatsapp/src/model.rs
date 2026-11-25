//! Plain data shared with the UI and the MCP tools (no protobuf types).

use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ChatSummary {
    pub jid: String,
    pub name: String,
    pub is_group: bool,
    /// Unix seconds of the last message (0 = unknown).
    pub last_ts: i64,
    /// Preview of the last message: its text, or the kind's label ("Foto") without caption.
    pub last_text: String,
    /// Kind of the last message (the UI draws its icon before `last_text`).
    pub last_kind: MsgKind,
    /// Who sent the last message in a group (empty otherwise / when it's ours).
    pub last_sender: String,
    pub last_from_me: bool,
    /// Delivery state of the last message when it's ours.
    pub last_status: MsgStatus,
    pub unread: u32,
    pub pinned: bool,
    pub muted: bool,
    /// Marked important locally: only these count toward the unread badges.
    pub important: bool,
    /// Ids of the local labels set on this chat.
    pub labels: Vec<i64>,
    /// Downloaded profile picture (small), if any.
    pub avatar: Option<std::path::PathBuf>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum MsgStatus {
    #[default]
    None,
    Pending,
    Sent,
    Delivered,
    Read,
    Played,
    Failed,
}

impl MsgStatus {
    pub fn from_i64(v: i64) -> Self {
        match v {
            1 => Self::Pending,
            2 => Self::Sent,
            3 => Self::Delivered,
            4 => Self::Read,
            5 => Self::Played,
            6 => Self::Failed,
            _ => Self::None,
        }
    }
    pub fn as_i64(self) -> i64 {
        match self {
            Self::None => 0,
            Self::Pending => 1,
            Self::Sent => 2,
            Self::Delivered => 3,
            Self::Read => 4,
            Self::Played => 5,
            Self::Failed => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum MsgKind {
    Text,
    Image,
    Video,
    Audio,
    /// Voice note (push-to-talk).
    Voice,
    Sticker,
    Document,
    Location,
    Contact,
    Poll,
    /// Deleted for everyone.
    Revoked,
    /// Anything else (shown as a short label).
    Other,
}

impl MsgKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Voice => "voice",
            Self::Sticker => "sticker",
            Self::Document => "document",
            Self::Location => "location",
            Self::Contact => "contact",
            Self::Poll => "poll",
            Self::Revoked => "revoked",
            Self::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "text" => Self::Text,
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "voice" => Self::Voice,
            "sticker" => Self::Sticker,
            "document" => Self::Document,
            "location" => Self::Location,
            "contact" => Self::Contact,
            "poll" => Self::Poll,
            "revoked" => Self::Revoked,
            _ => Self::Other,
        }
    }
    pub fn has_media(self) -> bool {
        matches!(self, Self::Image | Self::Video | Self::Audio | Self::Voice | Self::Sticker | Self::Document)
    }
    /// Label used in chat previews and for media without caption.
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "",
            Self::Image => "Foto",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Voice => "Nota de voz",
            Self::Sticker => "Sticker",
            Self::Document => "Documento",
            Self::Location => "Ubicación",
            Self::Contact => "Contacto",
            Self::Poll => "Encuesta",
            Self::Revoked => "Se eliminó este mensaje",
            Self::Other => "Mensaje no soportado",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Reaction {
    pub sender: String,
    pub sender_name: String,
    pub emoji: String,
    pub from_me: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Quoted {
    pub id: String,
    pub sender_name: String,
    pub text: String,
    pub kind: MsgKind,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatMessage {
    pub chat: String,
    pub id: String,
    pub sender: String,
    pub sender_name: String,
    pub from_me: bool,
    pub ts: i64,
    pub kind: MsgKind,
    /// Text or caption (or the file name for documents without caption).
    pub text: String,
    pub status: MsgStatus,
    pub edited: bool,
    /// Downloaded media file, if already in the cache.
    pub media: Option<std::path::PathBuf>,
    pub mime: Option<String>,
    pub file_name: Option<String>,
    /// Seconds (audio/video).
    pub duration: Option<u32>,
    /// Pixel size (image/video/sticker).
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub quoted: Option<Quoted>,
    pub reactions: Vec<Reaction>,
    /// Whisper transcription of a voice note (None = not done yet; "" = nothing understood).
    pub transcript: Option<String>,
}

impl ChatMessage {
    /// Short single-line text for previews / tools.
    pub fn preview(&self) -> String {
        let label = self.kind.label();
        let text = self.text.replace('\n', " ");
        let base = match (label.is_empty(), text.is_empty()) {
            (true, _) => text,
            (false, true) => label.to_string(),
            (false, false) => format!("{label}: {text}"),
        };
        match self.transcript.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => format!("{base} (transcripción: {})", t.replace('\n', " ")),
            None => base,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Service not started (no session yet, user hasn't asked to link).
    Idle,
    Starting,
    /// Waiting for the phone to scan this QR payload.
    Qr(String),
    /// QR codes ran out; the user has to ask for a new one.
    QrExpired,
    Connecting,
    Connected,
    /// Session ended on the phone (or "Cerrar sesión"); a new QR is needed.
    LoggedOut(String),
    /// Temporary ban or account locked: the service stopped on purpose.
    Blocked(String),
    Error(String),
}

#[derive(Clone, Debug)]
pub enum Update {
    Status(Status),
    /// Chat list changed (new message, read, rename…).
    Chats,
    /// Messages of this chat changed (new, edited, receipts, reactions, media downloaded).
    Messages(String),
    /// Profile picture downloaded.
    Avatar(String),
    /// Somebody is typing / recording in a chat (None = stopped).
    Typing { chat: String, who: Option<String> },
    /// Incoming message worth a notification (chat not muted, not from me).
    Notify { chat: String, title: String, body: String },
    /// Sending from the app failed (shown in the chat).
    SendFailed { chat: String, error: String },
    /// Claude (through the MCP tool) wants to send something: the app must ask the user.
    ConfirmSend(crate::ipc::SendRequest),
    /// Group members fetched (see `WhatsApp::group_members`).
    Group(String),
}

/// A participant of a group chat.
#[derive(Clone, Debug)]
pub struct GroupMember {
    /// Phone-number JID when known, else the LID.
    pub jid: String,
    pub name: String,
    pub phone: String,
    pub admin: bool,
    pub me: bool,
}

/// Local chat label ("Trabajo", "Clientes"…) to filter the chat list.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: u32,
    /// juan-terminal project id linked to this label (context for suggested replies).
    pub project: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StickerFav {
    pub id: String,
    pub path: std::path::PathBuf,
}
