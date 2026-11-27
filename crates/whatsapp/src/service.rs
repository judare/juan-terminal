//! The WhatsApp connection: one per app process, on its own tokio runtime. Events from the
//! phone are written to the cache and announced to the UI as [`Update`]s; UI actions are
//! plain non-blocking method calls.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use whatsapp_rust::download::MediaType;
use whatsapp_rust::media::{AudioOptions, DocumentOptions, ImageOptions, VideoOptions};
use whatsapp_rust::prelude::*;
use whatsapp_rust::wacore_binary::jid::JidExt;
use whatsapp_rust::types::events::*;
use whatsapp_rust::waproto::buffa::Message as _;
use whatsapp_rust::wacore::download::Downloadable;
use whatsapp_rust::wacore::types::presence::{ChatPresence, ReceiptType};

use crate::convert::{self, Parsed};
use crate::ipc::{self, SendRequest};
use crate::model::{GroupMember, MsgKind, MsgStatus, Status, Update};
use crate::store::{ChatMeta, NewMessage, Store, hidden_chat, now};

/// Sender id stored for our own messages.
pub const ME: &str = "me";
/// Minimum gap between two sends requested by Claude.
const CLAUDE_SEND_GAP: Duration = Duration::from_secs(3);

/// What the app did with a send Claude requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendAnswer {
    Send,
    Reject,
    /// Put in the chat's composer for the user to review and send.
    Drafted,
}

struct Shared {
    store: Arc<Store>,
    dir: PathBuf,
    subs: Mutex<Vec<UnboundedSender<Update>>>,
    status: Mutex<Status>,
    client: Mutex<Option<Arc<Client>>>,
    bot: Mutex<Option<BotHandle>>,
    /// Chats open in a tab: incoming messages there are read right away.
    open: Mutex<HashSet<String>>,
    downloading: Mutex<HashSet<(String, String)>>,
    pending: Mutex<HashMap<u64, std::sync::mpsc::Sender<SendAnswer>>>,
    next_id: AtomicU64,
    last_claude_send: Mutex<Option<Instant>>,
    avatars_running: Mutex<bool>,
    /// People whose picture was already requested this session (see `person_avatar`).
    avatar_tried: Mutex<HashSet<String>>,
    /// Voice notes queued / being transcribed (see `transcribe`).
    transcribing: Mutex<HashSet<(String, String)>>,
    /// One whisper at a time (it already uses every core).
    whisper: tokio::sync::Mutex<()>,
    /// Group participants fetched this session (see `group_members`).
    members: Mutex<HashMap<String, Vec<GroupMember>>>,
    /// Background media prefetch: a few downloads at a time (see `prefetch`).
    prefetch_slots: tokio::sync::Semaphore,
}

impl Shared {
    fn emit(&self, u: Update) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.unbounded_send(u.clone()).is_ok());
    }

    fn set_status(&self, s: Status) {
        *self.status.lock().unwrap() = s.clone();
        self.emit(Update::Status(s));
    }

    fn client(&self) -> Option<Arc<Client>> {
        self.client.lock().unwrap().clone()
    }

    fn media_dir(&self, chat: &str) -> PathBuf {
        self.dir.join("media").join(safe(chat))
    }
}

pub struct WhatsApp {
    shared: Arc<Shared>,
    rt: tokio::runtime::Runtime,
}

static GLOBAL: OnceLock<WhatsApp> = OnceLock::new();

/// `~/Library/Application Support/juan-terminal/whatsapp`.
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("JUAN_TERMINAL_STATE_DIR") {
        return PathBuf::from(d).join("whatsapp");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/juan-terminal/whatsapp")
}

fn safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

fn parse_jid(s: &str) -> Option<Jid> {
    s.parse::<Jid>().ok()
}

fn kind_of_path(path: &Path) -> MsgKind {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "heic" => MsgKind::Image,
        "mp4" | "mov" | "m4v" => MsgKind::Video,
        "mp3" | "m4a" | "aac" | "ogg" | "opus" | "wav" => MsgKind::Audio,
        _ => MsgKind::Document,
    }
}

fn mime_of(path: &Path) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg; codecs=opus",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Small JPEG preview + pixel size for an image file.
fn image_thumb(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    let t = img.thumbnail(96, 96).to_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    t.write_to(&mut out, image::ImageFormat::Jpeg).ok()?;
    Some((out.into_inner(), w, h))
}

impl WhatsApp {
    /// The process-wide service (created on first use; reconnects if a session exists).
    pub fn global() -> &'static WhatsApp {
        GLOBAL.get_or_init(|| {
            let dir = data_dir();
            let _ = std::fs::create_dir_all(&dir);
            let store = Arc::new(Store::open(&dir.join("cache.db")).expect("whatsapp cache"));
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("whatsapp").enable_all().build().expect("tokio runtime");
            let shared = Arc::new(Shared {
                store,
                dir,
                subs: Mutex::new(Vec::new()),
                status: Mutex::new(Status::Idle),
                client: Mutex::new(None),
                bot: Mutex::new(None),
                open: Mutex::new(HashSet::new()),
                downloading: Mutex::new(HashSet::new()),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                last_claude_send: Mutex::new(None),
                avatars_running: Mutex::new(false),
                avatar_tried: Mutex::new(HashSet::new()),
                transcribing: Mutex::new(HashSet::new()),
                members: Mutex::new(HashMap::new()),
                prefetch_slots: tokio::sync::Semaphore::new(4),
                whisper: tokio::sync::Mutex::new(()),
            });
            let wa = WhatsApp { shared, rt };
            wa.start_ipc();
            if wa.is_paired() {
                wa.start();
            }
            wa
        })
    }

    /// Already created (doesn't start the service).
    pub fn try_global() -> Option<&'static WhatsApp> {
        GLOBAL.get()
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.shared.store
    }

    pub fn dir(&self) -> &Path {
        &self.shared.dir
    }

    pub fn subscribe(&self) -> UnboundedReceiver<Update> {
        let (tx, rx) = unbounded();
        self.shared.subs.lock().unwrap().push(tx);
        rx
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap().clone()
    }

    /// A device was linked before (the session may still have been revoked on the phone).
    pub fn is_paired(&self) -> bool {
        self.shared.store.meta("paired").as_deref() == Some("1") && self.shared.dir.join("session.db").exists()
    }

    /// Connect (or show a QR when not linked yet).
    pub fn start(&self) {
        if self.shared.bot.lock().unwrap().is_some() {
            return;
        }
        let shared = self.shared.clone();
        shared.set_status(if self.is_paired() { Status::Connecting } else { Status::Starting });
        self.rt.spawn(async move {
            if let Err(e) = run_bot(shared.clone()).await {
                shared.set_status(Status::Error(format!("No se pudo iniciar WhatsApp: {e}")));
            }
        });
    }

    /// Internet came back: restart the connection unless it's up or needs the user (QR, logout…).
    pub fn reconnect(&self) {
        if !self.is_paired() || !matches!(self.status(), Status::Connecting | Status::Error(_)) {
            return;
        }
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            stop_bot(&shared).await;
            shared.set_status(Status::Connecting);
            if let Err(e) = run_bot(shared.clone()).await {
                shared.set_status(Status::Error(format!("No se pudo iniciar WhatsApp: {e}")));
            }
        });
    }

    /// Unlink this device and wipe the local cache.
    pub fn logout(&self) {
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            if let Some(c) = shared.client() {
                c.logout().await;
            }
            stop_bot(&shared).await;
            forget_session(&shared);
            shared.set_status(Status::Idle);
            shared.emit(Update::Chats);
        });
    }

    pub fn set_open(&self, chat: &str, open: bool) {
        let mut set = self.shared.open.lock().unwrap();
        if open {
            set.insert(chat.to_string());
        } else {
            set.remove(chat);
        }
    }

    /// Mark everything in `chat` read (receipts to the senders) — called when it's shown.
    /// Toggle the local "important" flag (only important chats count toward badges).
    pub fn set_important(&self, chat: &str, important: bool) {
        self.shared.store.set_important(chat, important);
        self.shared.emit(Update::Chats);
    }

    /// Archive / unarchive a chat here and on the phone (app-state sync).
    pub fn set_archived(&self, chat: &str, archived: bool) {
        self.shared.store.upsert_chat(chat, &ChatMeta { archived: Some(archived), ..Default::default() });
        self.shared.emit(Update::Chats);
        let Some(client) = self.shared.client() else { return };
        let Some(jid) = parse_jid(chat) else { return };
        self.rt.spawn(async move {
            let actions = client.chat_actions();
            let r = if archived { actions.archive_chat(&jid, None).await } else { actions.unarchive_chat(&jid, None).await };
            if let Err(e) = r {
                eprintln!("whatsapp: archive {jid}: {e}");
            }
        });
    }

    pub fn create_label(&self, name: &str) -> Option<i64> {
        let id = self.shared.store.create_label(name);
        self.shared.emit(Update::Chats);
        id
    }

    pub fn delete_label(&self, id: i64) {
        self.shared.store.delete_label(id);
        self.shared.emit(Update::Chats);
    }

    pub fn set_label_project(&self, id: i64, project: Option<&str>) {
        self.shared.store.set_label_project(id, project);
        self.shared.emit(Update::Chats);
    }

    pub fn set_chat_label(&self, chat: &str, label: i64, on: bool) {
        self.shared.store.set_chat_label(chat, label, on);
        self.shared.emit(Update::Chats);
    }

    /// Our own JID (phone-number form), once connected.
    pub fn own_jid(&self) -> Option<String> {
        self.shared.client().and_then(|c| c.pn().map(|j| j.to_non_ad().to_string()))
    }

    /// Whether a phone number (digits with country code) has WhatsApp. Resolves to its chat JID
    /// (phone-number form) when registered, `None` when not, offline, or on error.
    pub fn check_number(&self, digits: &str) -> futures::channel::oneshot::Receiver<Option<String>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        let Some(client) = self.shared.client() else {
            let _ = tx.send(None);
            return rx;
        };
        let digits = digits.to_string();
        self.rt.spawn(async move {
            let jid = Jid::pn(&digits);
            let found = match client.contacts().is_on_whatsapp(&[jid]).await {
                Ok(r) => r.into_iter().find(|r| r.is_registered).map(|r| r.pn_jid.unwrap_or(r.jid).to_non_ad().to_string()),
                Err(e) => {
                    log::warn!("whatsapp check_number {digits}: {e}");
                    None
                }
            };
            let _ = tx.send(found);
        });
        rx
    }

    /// Small profile picture of a person (group participant, or `ME` for ourselves). Cached on
    /// disk next to the chat pictures; when missing it's requested once per session in the
    /// background and `Update::Avatar(<that jid>)` is emitted when it arrives.
    pub fn person_avatar(&self, jid: &str) -> Option<PathBuf> {
        let jid = if jid == ME { self.own_jid()? } else { jid.to_string() };
        let dest = self.shared.dir.join("avatars").join(format!("{}.jpg", safe(&jid)));
        if dest.exists() {
            return Some(dest);
        }
        let client = self.shared.client()?;
        let j = parse_jid(&jid)?;
        if !self.shared.avatar_tried.lock().unwrap().insert(jid.clone()) {
            return None;
        }
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            let Ok(Some(p)) = client.contacts().get_profile_picture_with_timeout(&j, true, Some(Duration::from_secs(15))).await else { return };
            let url = p.url.clone();
            let ok = tokio::task::spawn_blocking(move || -> Option<()> {
                let mut resp = ureq::get(&url).call().ok()?;
                let bytes = resp.body_mut().read_to_vec().ok()?;
                let _ = std::fs::create_dir_all(dest.parent()?);
                std::fs::write(&dest, bytes).ok()
            })
            .await
            .ok()
            .flatten()
            .is_some();
            if ok {
                shared.emit(Update::Avatar(jid));
            }
        });
        None
    }

    pub fn mark_read(&self, chat: &str) {
        let shared = self.shared.clone();
        let chat = chat.to_string();
        self.rt.spawn(async move { mark_read(&shared, &chat).await });
    }

    pub fn typing(&self, chat: &str, composing: bool) {
        let Some(client) = self.shared.client() else { return };
        let Some(jid) = parse_jid(chat) else { return };
        self.rt.spawn(async move {
            let cs = client.chatstate();
            let _ = if composing { cs.send_composing(&jid).await } else { cs.send_paused(&jid).await };
        });
    }

    pub fn send_text(&self, chat: &str, text: &str, quote: Option<&str>) {
        let (shared, chat, text, quote) = (self.shared.clone(), chat.to_string(), text.to_string(), quote.map(str::to_string));
        self.rt.spawn(async move {
            if let Err(e) = send_text(&shared, &chat, &text, quote.as_deref()).await {
                shared.emit(Update::SendFailed { chat: chat.clone(), error: e });
            }
        });
    }

    /// Send files (one message each; `caption` goes on the first one).
    pub fn send_files(&self, chat: &str, files: Vec<PathBuf>, caption: &str, quote: Option<&str>) {
        let (shared, chat, caption, quote) = (self.shared.clone(), chat.to_string(), caption.to_string(), quote.map(str::to_string));
        self.rt.spawn(async move {
            for (i, f) in files.iter().enumerate() {
                let cap = if i == 0 { caption.as_str() } else { "" };
                if let Err(e) = send_file(&shared, &chat, f, cap, if i == 0 { quote.as_deref() } else { None }, None).await {
                    shared.emit(Update::SendFailed { chat: chat.clone(), error: e });
                }
            }
        });
    }

    /// Send a recorded voice note (Ogg/Opus file).
    pub fn send_voice(&self, chat: &str, file: PathBuf, seconds: u32, waveform: Vec<u8>, quote: Option<&str>) {
        let (shared, chat, quote) = (self.shared.clone(), chat.to_string(), quote.map(str::to_string));
        self.rt.spawn(async move {
            if let Err(e) = send_file(&shared, &chat, &file, "", quote.as_deref(), Some((seconds, waveform))).await {
                shared.emit(Update::SendFailed { chat: chat.clone(), error: e });
            }
        });
    }

    /// Forward message `id` of `from` to each chat in `to` (marked "Reenviado"; media is reused,
    /// not uploaded again).
    pub fn forward(&self, from: &str, id: &str, to: Vec<String>) {
        let (shared, from, id) = (self.shared.clone(), from.to_string(), id.to_string());
        self.rt.spawn(async move {
            for chat in to {
                if let Err(e) = forward(&shared, &from, &id, &chat).await {
                    shared.emit(Update::SendFailed { chat: chat.clone(), error: e });
                }
            }
        });
    }

    pub fn send_sticker(&self, chat: &str, sticker_id: &str) {
        let (shared, chat, id) = (self.shared.clone(), chat.to_string(), sticker_id.to_string());
        self.rt.spawn(async move {
            if let Err(e) = send_sticker(&shared, &chat, &id).await {
                shared.emit(Update::SendFailed { chat: chat.clone(), error: e });
            }
        });
    }

    pub fn react(&self, chat: &str, id: &str, emoji: &str) {
        let (shared, chat, id, emoji) = (self.shared.clone(), chat.to_string(), id.to_string(), emoji.to_string());
        self.rt.spawn(async move {
            let Some(client) = shared.client() else { return };
            let Some((sender, from_me)) = shared.store.message_origin(&chat, &id) else { return };
            let Some(jid) = parse_jid(&chat) else { return };
            let key = wa::MessageKey {
                remote_jid: Some(chat.clone()),
                from_me: Some(from_me),
                id: Some(id.clone()),
                participant: (chat.ends_with("@g.us") && !from_me).then_some(sender),
            };
            if client.send_reaction(jid, key, &emoji).await.is_ok() {
                shared.store.set_reaction(&chat, &id, ME, &emoji, true);
                // A reaction to the last message takes the chat out of "Sin respuesta".
                shared.emit(Update::Chats);
                shared.emit(Update::Messages(chat));
            }
        });
    }

    /// Members of a group as last fetched (None until `fetch_group_members` answers).
    pub fn group_members(&self, chat: &str) -> Option<Vec<GroupMember>> {
        self.shared.members.lock().unwrap().get(chat).cloned()
    }

    /// Ask the server for a group's participants; emits `Update::Group(chat)` when they arrive.
    pub fn fetch_group_members(&self, chat: &str) {
        let Some(client) = self.shared.client() else { return };
        let Some(j) = parse_jid(chat) else { return };
        let (shared, chat) = (self.shared.clone(), chat.to_string());
        let me = self.own_jid();
        self.rt.spawn(async move {
            let meta = match client.groups().get_metadata(&j).await {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("whatsapp group members {chat}: {e}");
                    return;
                }
            };
            let store = &shared.store;
            let mut members: Vec<GroupMember> = meta
                .participants
                .iter()
                .map(|p| {
                    let lid = p.lid.clone().or_else(|| p.jid.is_lid().then(|| p.jid.clone())).map(|j| j.to_non_ad_string());
                    let pn = p.phone_number.clone().or_else(|| (!p.jid.is_lid()).then(|| p.jid.clone())).map(|j| j.to_non_ad_string());
                    if let (Some(l), Some(n)) = (&lid, &pn) {
                        store.set_alias(l, n);
                    }
                    let jid = pn.clone().or(lid).map(|j| store.canonical(&j)).unwrap_or_else(|| p.jid.to_string());
                    let mut name = store.display_name(&jid);
                    let phone = crate::store::phone_label(&jid);
                    if name == phone && let Some(u) = p.username.as_ref().filter(|u| !u.is_empty()) {
                        name = format!("~{u}");
                    }
                    GroupMember { me: me.as_deref() == Some(jid.as_str()), name, phone, admin: p.is_admin(), jid }
                })
                .collect();
            // Me first, then admins, then saved contacts, then the rest; by name.
            let rank = |m: &GroupMember| (!m.me, !m.admin, m.name.starts_with('~') || m.name == m.phone, m.name.to_lowercase());
            members.sort_by_key(rank);
            shared.members.lock().unwrap().insert(chat.clone(), members);
            shared.emit(Update::Group(chat));
        });
    }

    /// Delete one of my messages for everyone (revoke): it shows "Se eliminó este mensaje".
    pub fn delete_for_everyone(&self, chat: &str, id: &str) {
        let (shared, chat, id) = (self.shared.clone(), chat.to_string(), id.to_string());
        self.rt.spawn(async move {
            let Some(client) = shared.client() else { return };
            let Some(jid) = parse_jid(&chat) else { return };
            match client.revoke_message(jid, id.clone(), whatsapp_rust::RevokeType::Sender).await {
                Ok(()) => {
                    shared.store.revoke(&chat, &id);
                    shared.emit(Update::Messages(chat.clone()));
                    shared.emit(Update::Chats);
                }
                Err(e) => shared.emit(Update::SendFailed { chat: chat.clone(), error: format!("No se pudo eliminar: {e}") }),
            }
        });
    }

    /// Delete a message only on my devices (synced to the phone via app state).
    pub fn delete_for_me(&self, chat: &str, id: &str, ts: i64) {
        let (shared, chat, id) = (self.shared.clone(), chat.to_string(), id.to_string());
        // Gone from this view right away; the app-state sync follows.
        let origin = shared.store.message_origin(&chat, &id);
        shared.store.delete_message(&chat, &id);
        shared.emit(Update::Messages(chat.clone()));
        shared.emit(Update::Chats);
        self.rt.spawn(async move {
            let Some(client) = shared.client() else { return };
            let Some(jid) = parse_jid(&chat) else { return };
            let (sender, from_me) = origin.unwrap_or_default();
            let participant = (chat.ends_with("@g.us") && !from_me).then(|| parse_jid(&sender)).flatten();
            if let Err(e) = client.chat_actions().delete_message_for_me(&jid, participant.as_ref(), &id, from_me, true, Some(ts)).await {
                log::warn!("whatsapp delete_for_me {id}: {e}");
            }
        });
    }

    /// Where the full-size profile picture of `chat` is cached (see `fetch_full_avatar`).
    pub fn full_avatar_path(&self, chat: &str) -> PathBuf {
        self.shared.dir.join("avatars").join(format!("full-{}.jpg", safe(chat)))
    }

    /// Download the full-size profile picture (contact info); emits `Update::Avatar` when ready.
    pub fn fetch_full_avatar(&self, chat: &str) {
        let Some(client) = self.shared.client() else { return };
        let Some(j) = parse_jid(chat) else { return };
        let (shared, chat, dest) = (self.shared.clone(), chat.to_string(), self.full_avatar_path(chat));
        self.rt.spawn(async move {
            let Ok(Some(p)) = client.contacts().get_profile_picture_with_timeout(&j, false, Some(Duration::from_secs(20))).await else { return };
            let url = p.url.clone();
            let ok = tokio::task::spawn_blocking(move || -> Option<()> {
                let mut resp = ureq::get(&url).call().ok()?;
                let bytes = resp.body_mut().read_to_vec().ok()?;
                let _ = std::fs::create_dir_all(dest.parent()?);
                std::fs::write(&dest, bytes).ok()
            })
            .await
            .ok()
            .flatten()
            .is_some();
            if ok {
                shared.emit(Update::Avatar(chat));
            }
        });
    }

    /// Download the media of a message into the cache (no-op if already there / in flight).
    pub fn download(&self, chat: &str, id: &str) {
        let key = (chat.to_string(), id.to_string());
        if !self.shared.downloading.lock().unwrap().insert(key.clone()) {
            return;
        }
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            let res = download(&shared, &key.0, &key.1).await;
            shared.downloading.lock().unwrap().remove(&key);
            if let Err(e) = res {
                log::warn!("whatsapp download {}: {e}", key.1);
            }
            shared.emit(Update::Messages(key.0));
        });
    }

    /// Download these messages' media in the background, in order, a few at a time, so photos
    /// are ready before they're scrolled into view. A `download` for something still queued
    /// here isn't held back: whichever starts first does it.
    pub fn prefetch(&self, chat: &str, ids: Vec<String>) {
        for id in ids {
            let (shared, chat) = (self.shared.clone(), chat.to_string());
            self.rt.spawn(async move {
                let Ok(_slot) = shared.prefetch_slots.acquire().await else { return };
                let key = (chat, id);
                if !shared.downloading.lock().unwrap().insert(key.clone()) {
                    return;
                }
                let had = shared.store.message(&key.0, &key.1).is_some_and(|m| m.media.is_some());
                let res = download(&shared, &key.0, &key.1).await;
                shared.downloading.lock().unwrap().remove(&key);
                match res {
                    Ok(_) if !had => shared.emit(Update::Messages(key.0)),
                    Ok(_) => {}
                    Err(e) => log::warn!("whatsapp prefetch {}: {e}", key.1),
                }
            });
        }
    }

    /// Transcribe a voice note with whisper (downloading it first) and store the text, so it's
    /// done once. No-op if already transcribed, queued, or whisper isn't installed.
    pub fn transcribe(&self, chat: &str, id: &str) {
        transcribe_note(&self.shared, self.rt.handle(), chat, id);
    }

    /// Add/remove a received sticker (already downloaded) from the favourites.
    pub fn toggle_favorite_sticker(&self, chat: &str, id: &str) -> bool {
        let store = &self.shared.store;
        let fav_id = format!("{}-{}", safe(chat), safe(id));
        if store.is_favorite_sticker(&fav_id) {
            store.remove_sticker(&fav_id);
            return false;
        }
        let Some(msg) = store.message(chat, id) else { return false };
        let (Some(media), Some(proto)) = (msg.media, store.proto(chat, id)) else { return false };
        let dir = self.shared.dir.join("stickers");
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join(format!("{fav_id}.webp"));
        if std::fs::copy(&media, &dest).is_err() {
            return false;
        }
        store.add_sticker(&fav_id, &dest, &proto);
        true
    }

    pub fn is_favorite_sticker(&self, chat: &str, id: &str) -> bool {
        self.shared.store.is_favorite_sticker(&format!("{}-{}", safe(chat), safe(id)))
    }

    /// The user's answer to a send requested by Claude.
    pub fn answer_send(&self, id: u64, answer: SendAnswer) {
        if let Some(tx) = self.shared.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(answer);
        }
    }

    fn start_ipc(&self) {
        let path = ipc::socket_path(&self.shared.dir);
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) else { return };
        let shared = self.shared.clone();
        let handle = self.rt.handle().clone();
        std::thread::Builder::new()
            .name("whatsapp-ipc".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (shared, handle) = (shared.clone(), handle.clone());
                    std::thread::spawn(move || serve_ipc(stream, &shared, &handle));
                }
            })
            .ok();
    }
}

fn serve_ipc(stream: std::os::unix::net::UnixStream, shared: &Arc<Shared>, handle: &tokio::runtime::Handle) {
    use std::io::{BufRead, BufReader, Write};
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = |ok: bool, text: String| {
        let resp = if ok { ipc::Response { ok, message: text, error: String::new() } } else { ipc::Response { ok, message: String::new(), error: text } };
        let mut s = &stream;
        let _ = s.write_all(format!("{}\n", serde_json::to_string(&resp).unwrap_or_default()).as_bytes());
    };
    let op = serde_json::from_str::<serde_json::Value>(line.trim()).ok().and_then(|v| v.get("op").and_then(|o| o.as_str()).map(str::to_string));
    if op.as_deref() == Some("download") {
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
        let (chat, id) = (v["chat"].as_str().unwrap_or_default().to_string(), v["id"].as_str().unwrap_or_default().to_string());
        let chat = shared.store.resolve_chat(&chat).unwrap_or(chat);
        return match handle.block_on(download(shared, &chat, &id)) {
            Ok(p) => {
                shared.emit(Update::Messages(chat));
                reply(true, p.display().to_string())
            }
            Err(e) => reply(false, format!("No se pudo descargar: {e}")),
        };
    }
    let Ok(mut req) = serde_json::from_str::<SendRequest>(line.trim()) else {
        return reply(false, "Petición inválida".into());
    };
    if shared.client().is_none() || !matches!(*shared.status.lock().unwrap(), Status::Connected | Status::Connecting) {
        return reply(false, "WhatsApp no está conectado en Fragua (vincula el dispositivo en la pestaña WhatsApp).".into());
    }
    let Some(chat) = shared.store.resolve_chat(&req.chat) else {
        return reply(false, format!("No encontré el chat \"{}\". Usa whatsapp_list_chats para ver los JID.", req.chat));
    };
    if hidden_chat(&chat) {
        return reply(false, "Ese chat no admite envíos desde aquí.".into());
    }
    if req.text.trim().is_empty() && req.files.is_empty() {
        return reply(false, "Nada que enviar: falta el texto o los archivos.".into());
    }
    for f in &req.files {
        if !f.is_absolute() || !f.is_file() {
            return reply(false, format!("No existe el archivo {} (usa rutas absolutas).", f.display()));
        }
    }
    {
        let last = shared.last_claude_send.lock().unwrap();
        if let Some(t) = *last
            && t.elapsed() < CLAUDE_SEND_GAP
        {
            std::thread::sleep(CLAUDE_SEND_GAP - t.elapsed());
        }
    }
    req.chat = chat.clone();
    req.chat_name = shared.store.display_name(&chat);
    req.id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    shared.pending.lock().unwrap().insert(req.id, tx);
    shared.emit(Update::ConfirmSend(req.clone()));
    let answer = rx.recv_timeout(Duration::from_secs(600)).unwrap_or(SendAnswer::Reject);
    shared.pending.lock().unwrap().remove(&req.id);
    match answer {
        SendAnswer::Send => {}
        SendAnswer::Drafted => {
            return reply(true, format!("No se envió: quedó como borrador en el compositor del chat {} para que el usuario lo revise y lo envíe él mismo.", req.chat_name));
        }
        SendAnswer::Reject => return reply(false, "El usuario rechazó el envío en Fragua (o no respondió).".into()),
    }
    *shared.last_claude_send.lock().unwrap() = Some(Instant::now());
    let result: Result<(), String> = handle.block_on(async {
        let mut quote = req.quote.as_deref();
        let mut caption = req.text.as_str();
        if req.files.is_empty() {
            return send_text(shared, &chat, &req.text, quote).await;
        }
        // Text longer than a caption goes as its own message first.
        if caption.len() > 1000 {
            send_text(shared, &chat, caption, quote).await?;
            caption = "";
            quote = None;
        }
        for (i, f) in req.files.iter().enumerate() {
            send_file(shared, &chat, f, if i == 0 { caption } else { "" }, if i == 0 { quote } else { None }, None).await?;
        }
        Ok(())
    });
    match result {
        Ok(()) => reply(true, format!("Enviado a {}.", req.chat_name)),
        Err(e) => reply(false, format!("No se pudo enviar: {e}")),
    }
}

// ---------------------------------------------------------------- connection

async fn run_bot(shared: Arc<Shared>) -> anyhow::Result<()> {
    let session = shared.dir.join("session.db");
    let backend = SqliteStore::new(&session.to_string_lossy()).await?;
    let ev_shared = shared.clone();
    let bot = Bot::builder()
        .with_backend(backend)
        .with_device_props(
            // Shown on the phone under "Linked devices" (sent when linking): a Mac desktop app.
            whatsapp_rust::wacore::store::DevicePropsOverride::new().with_os("Mac OS").with_platform_type(wa::device_props::PlatformType::Desktop),
        )
        .on_event(move |event, client| {
            let shared = ev_shared.clone();
            async move { on_event(&shared, &event, &client).await }
        })
        .build()
        .await?;
    let handle = bot.spawn();
    *shared.client.lock().unwrap() = Some(handle.client());
    *shared.bot.lock().unwrap() = Some(handle);
    Ok(())
}

async fn stop_bot(shared: &Arc<Shared>) {
    let bot = shared.bot.lock().unwrap().take();
    *shared.client.lock().unwrap() = None;
    if let Some(bot) = bot {
        bot.shutdown().await;
    }
}

fn forget_session(shared: &Shared) {
    for f in ["session.db", "session.db-wal", "session.db-shm"] {
        let _ = std::fs::remove_file(shared.dir.join(f));
    }
    shared.store.clear();
    let _ = std::fs::remove_dir_all(shared.dir.join("media"));
    let _ = std::fs::remove_dir_all(shared.dir.join("avatars"));
}

async fn on_event(shared: &Arc<Shared>, event: &Event, client: &Arc<Client>) {
    match event {
        Event::PairingQrCode(q) => shared.set_status(Status::Qr(q.code.clone())),
        Event::PairingQrCodesExhausted(_) => {
            shared.set_status(Status::QrExpired);
            let s = shared.clone();
            tokio::spawn(async move { stop_bot(&s).await });
        }
        Event::PairSuccess(_) => {
            shared.store.clear();
            shared.store.set_meta("paired", "1");
            shared.set_status(Status::Connecting);
        }
        Event::PairError(e) => shared.set_status(Status::Error(format!("No se pudo vincular: {}", e.error))),
        Event::Connected(_) => {
            shared.store.set_meta("paired", "1");
            shared.set_status(Status::Connected);
            let (s, c) = (shared.clone(), client.clone());
            tokio::spawn(async move { after_connect(&s, &c).await });
        }
        Event::Disconnected(_) => {
            if matches!(*shared.status.lock().unwrap(), Status::Connected) {
                shared.set_status(Status::Connecting);
            }
        }
        Event::LoggedOut(l) => {
            let reason = match &l.logout_message {
                Some(m) => format!("{m:?}"),
                None => format!("{:?}", l.reason),
            };
            let s = shared.clone();
            tokio::spawn(async move {
                stop_bot(&s).await;
                forget_session(&s);
                s.set_status(Status::LoggedOut(format!("Se cerró la sesión desde el teléfono ({reason}).")));
                s.emit(Update::Chats);
            });
        }
        Event::TemporaryBan(b) => {
            let msg = format!(
                "WhatsApp bloqueó temporalmente la cuenta ({:?}) por {} min. Se desconectó para no empeorarlo.",
                b.code,
                b.expire.num_minutes()
            );
            shared.set_status(Status::Blocked(msg));
            let s = shared.clone();
            tokio::spawn(async move { stop_bot(&s).await });
        }
        Event::StreamReplaced(_) => {
            shared.set_status(Status::Error("La sesión se abrió en otro lugar con las mismas claves.".into()));
            let s = shared.clone();
            tokio::spawn(async move { stop_bot(&s).await });
        }
        Event::ClientOutdated(_) => {
            shared.set_status(Status::Error("WhatsApp rechazó la versión del cliente: hay que actualizar whatsapp-rust.".into()));
        }
        Event::ConnectFailure(f) => {
            log::warn!("whatsapp connect failure: {:?} {:?}", f.reason, f.message);
        }
        Event::HistorySync(h) => {
            on_history(shared, client, h).await;
            // Chats arrive here after Connected (fresh pairing): fetch their pictures now.
            fetch_avatars(shared.clone(), client.clone());
        }
        Event::Messages(batch) => {
            let live = matches!(batch.origin, BatchOrigin::Live);
            let mut chats = HashSet::new();
            for m in batch.messages.iter() {
                if let Some(chat) = on_message(shared, client, &m.message, &m.info, live).await {
                    chats.insert(chat);
                }
            }
            for c in chats {
                shared.emit(Update::Messages(c));
            }
            shared.emit(Update::Chats);
            fetch_avatars(shared.clone(), client.clone());
        }
        Event::Receipt(r) => {
            let chat = canonical(shared, client, &r.source.chat).await;
            let status = match r.r#type {
                ReceiptType::Delivered => Some(MsgStatus::Delivered),
                ReceiptType::Read => Some(MsgStatus::Read),
                ReceiptType::Played => Some(MsgStatus::Played),
                ReceiptType::ReadSelf => {
                    // Read on the phone: clear the unread badge here too.
                    shared.store.mark_read(&chat);
                    shared.emit(Update::Chats);
                    None
                }
                _ => None,
            };
            if let Some(st) = status {
                let ids: Vec<String> = r.message_ids.iter().map(|i| i.to_string()).collect();
                shared.store.set_status(&chat, &ids, st);
                shared.emit(Update::Messages(chat));
                shared.emit(Update::Chats);
            }
        }
        Event::ChatPresence(p) => {
            let chat = canonical(shared, client, &p.source.chat).await;
            let who = match p.state {
                ChatPresence::Composing => {
                    let sender = canonical(shared, client, &p.source.sender).await;
                    Some(if chat.ends_with("@g.us") { shared.store.display_name(&sender) } else { String::new() })
                }
                _ => None,
            };
            shared.emit(Update::Typing { chat, who });
        }
        Event::ContactUpdate(c) => {
            let jid = canonical(shared, client, &c.jid).await;
            if let Some(name) = c.action.full_name.clone().or_else(|| c.action.first_name.clone()) {
                shared.store.set_contact_name(&jid, &name);
            }
            if let (Some(lid), Some(pn)) = (c.action.lid_jid.as_deref(), c.action.pn_jid.as_deref()) {
                shared.store.set_alias(lid, pn);
            }
            if !c.from_full_sync {
                shared.emit(Update::Chats);
            }
        }
        Event::PushNameUpdate(p) => {
            let jid = canonical(shared, client, &p.jid).await;
            shared.store.set_push_name(&jid, &p.new_push_name);
        }
        Event::PictureUpdate(p) => {
            let jid = canonical(shared, client, &p.jid).await;
            shared.store.forget_avatar(&jid);
            fetch_avatars(shared.clone(), client.clone());
        }
        Event::ArchiveUpdate(a) => {
            let jid = canonical(shared, client, &a.jid).await;
            shared.store.upsert_chat(&jid, &ChatMeta { archived: a.action.archived, ..Default::default() });
            shared.emit(Update::Chats);
        }
        Event::PinUpdate(p) => {
            let jid = canonical(shared, client, &p.jid).await;
            shared.store.upsert_chat(&jid, &ChatMeta { pinned: p.action.pinned, ..Default::default() });
            shared.emit(Update::Chats);
        }
        Event::MuteUpdate(m) => {
            let jid = canonical(shared, client, &m.jid).await;
            let until = if m.action.muted.unwrap_or(false) { mute_until(m.action.mute_end_timestamp) } else { 0 };
            shared.store.upsert_chat(&jid, &ChatMeta { muted_until: Some(until), ..Default::default() });
            shared.emit(Update::Chats);
        }
        Event::MarkChatAsReadUpdate(m) => {
            let jid = canonical(shared, client, &m.jid).await;
            if m.action.read.unwrap_or(false) {
                shared.store.mark_read(&jid);
            } else {
                shared.store.upsert_chat(&jid, &ChatMeta { unread: Some(1), ..Default::default() });
            }
            shared.emit(Update::Chats);
        }
        Event::DeleteChatUpdate(d) => {
            let jid = canonical(shared, client, &d.jid).await;
            shared.store.delete_chat(&jid);
            shared.emit(Update::Chats);
        }
        Event::ClearChatUpdate(c) => {
            let jid = canonical(shared, client, &c.jid).await;
            shared.store.clear_chat(&jid);
            shared.emit(Update::Messages(jid));
            shared.emit(Update::Chats);
        }
        Event::DeleteMessageForMeUpdate(d) => {
            let chat = canonical(shared, client, &d.chat_jid).await;
            shared.store.delete_message(&chat, &d.message_id);
            shared.emit(Update::Messages(chat));
        }
        Event::GroupUpdate(_) => {
            let (s, c) = (shared.clone(), client.clone());
            tokio::spawn(async move { refresh_groups(&s, &c).await });
        }
        _ => {}
    }
}

/// `mute_end_timestamp` is in ms; -1 = forever.
fn mute_until(ts: Option<i64>) -> i64 {
    match ts {
        Some(t) if t < 0 => -1,
        Some(t) if t > 10_000_000_000 => t / 1000,
        Some(t) => t,
        None => -1,
    }
}

async fn after_connect(shared: &Arc<Shared>, client: &Arc<Client>) {
    flush_outbox(shared, client).await;
    refresh_groups(shared, client).await;
    fetch_avatars(shared.clone(), client.clone());
}

/// Send what was written while offline, in order.
async fn flush_outbox(shared: &Arc<Shared>, client: &Arc<Client>) {
    for (chat, id, proto) in shared.store.outbox() {
        let (Some(jid), Some(msg)) = (parse_jid(&chat), convert::decode(&proto)) else { continue };
        if let Err(e) = send_queued(shared, client, &chat, &id, jid, msg).await {
            log::warn!("whatsapp outbox {id}: {e}");
        }
    }
}

async fn refresh_groups(shared: &Arc<Shared>, client: &Arc<Client>) {
    if let Ok(groups) = client.groups().get_participating().await {
        for (jid, meta) in groups {
            shared.store.upsert_chat(&jid.to_string(), &ChatMeta { name: Some(meta.subject.clone()), is_group: Some(true), ..Default::default() });
        }
        shared.emit(Update::Chats);
    }
}

/// Download small profile pictures for the visible chats, a few at a time.
fn fetch_avatars(shared: Arc<Shared>, client: Arc<Client>) {
    {
        let mut running = shared.avatars_running.lock().unwrap();
        if *running {
            return;
        }
        *running = true;
    }
    tokio::spawn(async move {
        let dir = shared.dir.join("avatars");
        let _ = std::fs::create_dir_all(&dir);
        loop {
            let batch = shared.store.avatars_to_fetch(10);
            if batch.is_empty() {
                // Re-check under the lock so a trigger that arrived meanwhile isn't lost.
                let mut running = shared.avatars_running.lock().unwrap();
                if shared.store.avatars_to_fetch(1).is_empty() {
                    *running = false;
                    break;
                }
                continue;
            }
            for jid in batch {
                let Some(j) = parse_jid(&jid) else {
                    shared.store.set_avatar(&jid, None);
                    continue;
                };
                let pic = client.contacts().get_profile_picture_with_timeout(&j, true, Some(Duration::from_secs(15))).await;
                let path = match pic {
                    Ok(Some(p)) => {
                        let url = p.url.clone();
                        let dest = dir.join(format!("{}.jpg", safe(&jid)));
                        let d2 = dest.clone();
                        let ok = tokio::task::spawn_blocking(move || -> Option<()> {
                            let mut resp = ureq::get(&url).call().ok()?;
                            let bytes = resp.body_mut().read_to_vec().ok()?;
                            std::fs::write(&d2, bytes).ok()
                        })
                        .await
                        .ok()
                        .flatten()
                        .is_some();
                        ok.then_some(dest)
                    }
                    _ => None,
                };
                shared.store.set_avatar(&jid, path.as_deref());
                if path.is_some() {
                    shared.emit(Update::Avatar(jid));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            shared.emit(Update::Chats);
        }
    });
}

/// Chat/sender JID as stored: phone-number form for users when the LID mapping is known,
/// without device suffix.
async fn canonical(shared: &Shared, client: &Client, jid: &Jid) -> String {
    let base = jid.to_non_ad();
    let s = base.to_string();
    if !base.is_lid() {
        return s;
    }
    let known = shared.store.canonical(&s);
    if known != s {
        return known;
    }
    if let Ok(Some(e)) = client.get_lid_pn_entry(&base).await {
        let pn = format!("{}@s.whatsapp.net", e.phone_number);
        shared.store.set_alias(&s, &pn);
        return pn;
    }
    s
}

async fn on_history(shared: &Arc<Shared>, client: &Arc<Client>, h: &LazyHistorySync) {
    let Some(hs) = h.get() else { return };
    for p in &hs.pushnames {
        if let (Some(id), Some(name)) = (p.id.as_deref(), p.pushname.as_deref()) {
            let jid = shared.store.canonical(id);
            shared.store.set_push_name(&jid, name);
        }
    }
    for m in &hs.phone_number_to_lid_mappings {
        if let (Some(pn), Some(lid)) = (m.pn_jid.as_deref(), m.lid_jid.as_deref()) {
            shared.store.set_alias(lid, pn);
        }
    }
    for conv in &hs.conversations {
        if let (Some(lid), Some(pn)) = (conv.lid_jid.as_deref(), conv.pn_jid.as_deref()) {
            shared.store.set_alias(lid, pn);
        }
        let raw = conv.id.clone();
        if hidden_chat(&raw) {
            continue;
        }
        let chat = match parse_jid(&raw) {
            Some(j) => canonical(shared, client, &j).await,
            None => continue,
        };
        let ts = conv.conversation_timestamp.or(conv.last_msg_timestamp).unwrap_or(0) as i64;
        let muted = conv.mute_end_time.map(|t| mute_until(Some(t as i64))).unwrap_or(0);
        shared.store.upsert_chat(
            &chat,
            &ChatMeta {
                name: conv.name.clone().or_else(|| conv.display_name.clone()),
                is_group: Some(chat.ends_with("@g.us")),
                unread: conv.unread_count.or(conv.marked_as_unread.filter(|u| *u).map(|_| 1)),
                archived: conv.archived,
                pinned: conv.pinned.map(|p| p > 0),
                muted_until: Some(muted),
                last_ts: Some(ts),
            },
        );
        for hm in &conv.messages {
            let Some(w) = hm.message.as_option() else { continue };
            let Some(msg) = w.message.as_option() else { continue };
            let Some(key) = w.key.as_option() else { continue };
            let Some(id) = key.id.clone() else { continue };
            let from_me = key.from_me.unwrap_or(false);
            let sender_raw = key.participant.clone().or_else(|| w.participant.clone()).unwrap_or_else(|| raw.clone());
            let sender = if from_me {
                ME.to_string()
            } else {
                match parse_jid(&sender_raw) {
                    Some(j) => canonical(shared, client, &j).await,
                    None => sender_raw,
                }
            };
            if let Some(pn) = w.push_name.as_deref().filter(|_| !from_me) {
                shared.store.set_push_name(&sender, pn);
            }
            let ts = w.message_timestamp.unwrap_or(0) as i64;
            let status = if from_me {
                match w.status.map(|s| s as i32).unwrap_or(0) {
                    0 => MsgStatus::Failed,
                    1 => MsgStatus::Pending,
                    2 => MsgStatus::Sent,
                    3 => MsgStatus::Delivered,
                    4 => MsgStatus::Read,
                    5 => MsgStatus::Played,
                    _ => MsgStatus::Sent,
                }
            } else {
                MsgStatus::None
            };
            store_parsed(shared, &chat, &id, &sender, from_me, ts, status, false, msg).await;
        }
    }
    shared.emit(Update::Chats);
}

#[allow(clippy::too_many_arguments)]
async fn store_parsed(shared: &Shared, chat: &str, id: &str, sender: &str, from_me: bool, ts: i64, status: MsgStatus, unseen: bool, msg: &wa::Message) -> bool {
    match convert::parse(msg, true) {
        Parsed::Message(c) => {
            let quoted_sender = c.quoted_sender.as_deref().map(|s| shared.store.canonical(s.split(':').next().unwrap_or(s)));
            shared.store.insert_message(&NewMessage {
                chat: chat.to_string(),
                id: id.to_string(),
                sender: sender.to_string(),
                from_me,
                ts,
                kind: c.kind,
                text: c.text,
                status,
                unseen,
                quoted_id: c.quoted_id,
                quoted_sender,
                quoted_text: c.quoted_text,
                quoted_kind: c.quoted_kind,
                forwarded: c.forwarded,
                mime: c.mime,
                file_name: c.file_name,
                duration: c.duration,
                width: c.width,
                height: c.height,
                proto: c.proto,
            })
        }
        Parsed::Reaction { target, emoji } => {
            shared.store.set_reaction(chat, &target, sender, &emoji, from_me);
            false
        }
        Parsed::Revoke { target } => {
            shared.store.revoke(chat, &target);
            false
        }
        Parsed::Edit { target, text } => {
            shared.store.edit_text(chat, &target, &text);
            false
        }
        Parsed::Ignore => false,
    }
}

/// Store an incoming (live or offline) message. Returns the chat it belongs to.
async fn on_message(shared: &Arc<Shared>, client: &Arc<Client>, msg: &wa::Message, info: &MessageInfo, live: bool) -> Option<String> {
    let src = &info.source;
    if src.chat.is_status_broadcast() || src.chat.is_newsletter() || src.chat.is_broadcast_list() {
        return None;
    }
    let chat = canonical(shared, client, &src.chat).await;
    let from_me = src.is_from_me;
    let sender = if from_me { ME.to_string() } else { canonical(shared, client, &src.sender).await };
    if let (Some(alt), false) = (src.sender_alt.as_ref(), from_me) {
        let (a, b) = (src.sender.to_non_ad().to_string(), alt.to_non_ad().to_string());
        if a.ends_with("@lid") && b.ends_with("@s.whatsapp.net") {
            shared.store.set_alias(&a, &b);
        } else if b.ends_with("@lid") && a.ends_with("@s.whatsapp.net") {
            shared.store.set_alias(&b, &a);
        }
    }
    if !from_me && !info.push_name.is_empty() {
        shared.store.set_push_name(&sender, &info.push_name);
    }
    let unseen = !from_me;
    let ts = info.timestamp.timestamp();
    let status = if from_me { MsgStatus::Sent } else { MsgStatus::None };
    let is_new = store_parsed(shared, &chat, &info.id, &sender, from_me, ts, status, unseen, msg).await;
    if from_me {
        // Replied from the phone: the chat is read there.
        shared.store.mark_read(&chat);
    }
    if is_new && live && !from_me && shared.store.message(&chat, &info.id).is_some_and(|m| m.kind == MsgKind::Voice) {
        transcribe_note(shared, &tokio::runtime::Handle::current(), &chat, &info.id);
    }
    if is_new && live && !from_me && !shared.store.is_muted(&chat) && !shared.store.is_archived(&chat) {
        if let Some(m) = shared.store.message(&chat, &info.id) {
            let title = shared.store.display_name(&chat);
            let body = if chat.ends_with("@g.us") { format!("{}: {}", m.sender_name.trim_start_matches('~'), m.preview()) } else { m.preview() };
            shared.emit(Update::Notify { chat: chat.clone(), title, body });
        }
    }
    if is_new && !from_me && matches!(shared.store.message(&chat, &info.id).map(|m| m.kind), Some(MsgKind::Sticker | MsgKind::Image)) {
        // Small media is fetched eagerly so the chat shows it without a click.
        let (s, c, id) = (shared.clone(), chat.clone(), info.id.to_string());
        tokio::spawn(async move {
            if download(&s, &c, &id).await.is_ok() {
                s.emit(Update::Messages(c));
            }
        });
    }
    Some(chat)
}

async fn mark_read(shared: &Arc<Shared>, chat: &str) {
    let was_unread = shared.store.unread(chat) > 0;
    let pending = shared.store.mark_read(chat);
    shared.emit(Update::Chats);
    let Some(client) = shared.client() else { return };
    let Some(chat_jid) = parse_jid(chat) else { return };
    if pending.is_empty() && !was_unread {
        return;
    }
    // Read on the phone and the other linked devices too (app-state sync; the receipts below
    // only tell the senders).
    if let Some(last) = shared.store.messages(chat, None, 1).pop() {
        let participant = if chat.ends_with("@g.us") && !last.from_me { parse_jid(&last.sender) } else { None };
        let key = whatsapp_rust::message_key(last.id.clone(), &chat_jid, last.from_me, participant.as_ref());
        let range = whatsapp_rust::message_range(last.ts, None, vec![(key, last.ts)]);
        if let Err(e) = client.chat_actions().mark_chat_as_read(&chat_jid, true, Some(range)).await {
            log::warn!("whatsapp mark_chat_as_read {chat}: {e}");
        }
    }
    let mut by_sender: HashMap<String, Vec<String>> = HashMap::new();
    for (id, sender) in pending {
        by_sender.entry(sender).or_default().push(id);
    }
    for (sender, ids) in by_sender {
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let sender_jid = if chat.ends_with("@g.us") { parse_jid(&sender) } else { None };
        let _ = client.mark_as_read(&chat_jid, sender_jid.as_ref(), &ids).await;
    }
}

// ---------------------------------------------------------------- sending

fn quote_context(shared: &Shared, chat: &str, quote: Option<&str>) -> Option<Box<wa::ContextInfo>> {
    let id = quote?;
    let (sender, from_me) = shared.store.message_origin(chat, id)?;
    let original = shared.store.message(chat, id)?;
    let quoted = match shared.store.proto(chat, id).and_then(|p| convert::decode(&p)) {
        Some(m) => m.prepare_for_quote(),
        None => Box::new(wa::Message::text(original.text.clone())),
    };
    let participant = if from_me {
        // Our own JID: the phone number of this account.
        shared.client().and_then(|c| c.pn().map(|j| j.to_non_ad().to_string()))
    } else {
        Some(sender)
    };
    Some(Box::new(wa::ContextInfo {
        stanza_id: Some(id.to_string()),
        participant,
        quoted_message: whatsapp_rust::waproto::buffa::MessageField::from_box(quoted),
        ..Default::default()
    }))
}

/// Show a message as pending right away, then send it and record the result.
/// Id for a message written while there's no client (same shape as WhatsApp's own).
fn offline_message_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("3EB0{:016X}{:04X}", h.finish(), std::process::id() & 0xffff)
}

fn is_connected(shared: &Shared) -> bool {
    matches!(*shared.status.lock().unwrap(), Status::Connected) && shared.client().is_some()
}

/// Save our message (Pending) and send it. Offline it stays Pending in the outbox and goes out
/// on the next `Connected` (see `flush_outbox`), so writing never fails for lack of internet.
async fn deliver(shared: &Shared, chat: &str, msg: wa::Message, local: NewMessage, media: Option<PathBuf>) -> Result<(), String> {
    let jid = parse_jid(chat).ok_or("JID inválido")?;
    let client = shared.client();
    let id = client.as_ref().map(|c| c.generate_message_id()).unwrap_or_else(offline_message_id);
    let mut row = local;
    row.chat = chat.to_string();
    row.id = id.clone();
    row.sender = ME.to_string();
    row.from_me = true;
    row.ts = now();
    row.status = MsgStatus::Pending;
    if row.proto.is_none() {
        row.proto = Some(msg.encode_to_vec());
    }
    shared.store.insert_message(&row);
    if let Some(f) = media {
        shared.store.set_media(chat, &id, &f);
    }
    shared.emit(Update::Messages(chat.to_string()));
    shared.emit(Update::Chats);
    let Some(client) = client.filter(|_| is_connected(shared)) else {
        return Ok(()); // queued
    };
    send_queued(shared, &client, chat, &id, jid, msg).await
}

async fn send_queued(shared: &Shared, client: &Arc<Client>, chat: &str, id: &str, jid: Jid, msg: wa::Message) -> Result<(), String> {
    let id = id.to_string();
    let res = client.send_message_with_options(jid, msg, SendOptions::default().with_message_id(id.clone())).await;
    match res {
        Ok(_) => {
            shared.store.set_status(chat, std::slice::from_ref(&id), MsgStatus::Sent);
            shared.emit(Update::Messages(chat.to_string()));
            shared.emit(Update::Chats);
            Ok(())
        }
        Err(e) => {
            shared.store.replace_status(chat, &id, MsgStatus::Failed);
            shared.emit(Update::Messages(chat.to_string()));
            Err(e.to_string())
        }
    }
}

async fn send_text(shared: &Shared, chat: &str, text: &str, quote: Option<&str>) -> Result<(), String> {
    let ctx = quote_context(shared, chat, quote);
    let msg = match ctx {
        Some(ci) => wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some(text.to_string()),
                context_info: MessageField::some(*ci),
                ..Default::default()
            }),
            ..Default::default()
        },
        None => wa::Message::text(text),
    };
    let (q_text, q_kind, q_sender) = quoted_preview(shared, chat, quote);
    deliver(
        shared,
        chat,
        msg,
        NewMessage { kind: Some(MsgKind::Text), text: text.to_string(), quoted_id: quote.map(str::to_string), quoted_text: q_text, quoted_kind: q_kind, quoted_sender: q_sender, ..Default::default() },
        None,
    )
    .await
}

fn quoted_preview(shared: &Shared, chat: &str, quote: Option<&str>) -> (Option<String>, Option<MsgKind>, Option<String>) {
    let Some(m) = quote.and_then(|q| shared.store.message(chat, q)) else { return (None, None, None) };
    (Some(m.text.clone()), Some(m.kind), Some(m.sender.clone()))
}

/// Upload and send one file. `voice` = (seconds, waveform) sends an audio as a voice note.
async fn send_file(shared: &Shared, chat: &str, path: &Path, caption: &str, quote: Option<&str>, voice: Option<(u32, Vec<u8>)>) -> Result<(), String> {
    let client = shared.client().filter(|_| is_connected(shared)).ok_or("Sin conexión: los archivos y notas de voz se pueden enviar cuando vuelva internet (los textos sí quedan en cola).")?;
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut kind = if voice.is_some() { MsgKind::Voice } else { kind_of_path(path) };
    // Only Opus/Ogg plays as an audio bubble everywhere; other audio goes as a document.
    if kind == MsgKind::Audio && !matches!(path.extension().and_then(|e| e.to_str()), Some("ogg" | "opus" | "mp3" | "m4a" | "aac")) {
        kind = MsgKind::Document;
    }
    let mime = mime_of(path);
    let ctx = quote_context(shared, chat, quote);
    let caption_opt = (!caption.is_empty()).then(|| caption.to_string());
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    // Keep a copy in the media cache so the bubble shows it without downloading back.
    let dir = shared.media_dir(chat);
    let _ = std::fs::create_dir_all(&dir);
    let cached = dir.join(format!("sent-{}-{}", now(), file_name.clone().unwrap_or_else(|| "file".into())));
    let _ = std::fs::copy(path, &cached);
    let (media_type, mut row) = match kind {
        MsgKind::Image => (MediaType::Image, NewMessage { kind: Some(kind), text: caption.to_string(), mime: Some(mime.clone()), ..Default::default() }),
        MsgKind::Video => (MediaType::Video, NewMessage { kind: Some(kind), text: caption.to_string(), mime: Some(mime.clone()), ..Default::default() }),
        MsgKind::Audio | MsgKind::Voice => (MediaType::Audio, NewMessage { kind: Some(kind), mime: Some(mime.clone()), duration: voice.as_ref().map(|v| v.0), ..Default::default() }),
        _ => (
            MediaType::Document,
            NewMessage { kind: Some(MsgKind::Document), text: caption_opt.clone().or(file_name.clone()).unwrap_or_default(), mime: Some(mime.clone()), ..Default::default() },
        ),
    };
    let thumb = if kind == MsgKind::Image { image_thumb(&bytes) } else { None };
    if let Some((_, w, h)) = &thumb {
        row.width = Some(*w);
        row.height = Some(*h);
    }
    let upload = client.upload(bytes, media_type, Default::default()).await.map_err(|e| format!("Error subiendo {}: {e}", path.display()))?;
    let msg = match kind {
        MsgKind::Image => {
            let mut m = whatsapp_rust::media::image_message(
                upload,
                ImageOptions { caption: caption_opt, mimetype: Some(mime), jpeg_thumbnail: thumb.as_ref().map(|t| t.0.clone()), context_info: ctx },
            );
            if let (Some(im), Some((_, w, h))) = (m.image_message.as_option_mut(), thumb.as_ref()) {
                im.width = Some(*w);
                im.height = Some(*h);
            }
            m
        }
        MsgKind::Video => whatsapp_rust::media::video_message(upload, VideoOptions { caption: caption_opt, mimetype: Some(mime), context_info: ctx, ..Default::default() }),
        MsgKind::Audio | MsgKind::Voice => whatsapp_rust::media::audio_message(
            upload,
            AudioOptions {
                mimetype: Some(if voice.is_some() { "audio/ogg; codecs=opus".into() } else { mime }),
                duration_seconds: voice.as_ref().map(|v| v.0),
                ptt: Some(voice.is_some()),
                waveform: voice.map(|v| v.1),
                context_info: ctx,
            },
        ),
        _ => whatsapp_rust::media::document_message(
            upload,
            DocumentOptions { mimetype: Some(mime), file_name: file_name.clone(), title: file_name.clone(), caption: caption_opt, context_info: ctx, ..Default::default() },
        ),
    };
    let (q_text, q_kind, q_sender) = quoted_preview(shared, chat, quote);
    row.quoted_id = quote.map(str::to_string);
    row.quoted_text = q_text;
    row.quoted_kind = q_kind;
    row.quoted_sender = q_sender;
    if kind == MsgKind::Document {
        row.file_name = file_name;
    }
    deliver(shared, chat, msg, row, Some(cached)).await
}

/// Context of a forwarded message (drops any quote the original had).
fn forwarded_context(score: u32) -> MessageField<wa::ContextInfo> {
    MessageField::some(wa::ContextInfo { is_forwarded: Some(true), forwarding_score: Some(score), ..Default::default() })
}

async fn forward(shared: &Shared, from: &str, id: &str, chat: &str) -> Result<(), String> {
    let m = shared.store.message(from, id).ok_or("Mensaje no encontrado")?;
    if m.kind == MsgKind::Revoked {
        return Err("No se puede reenviar un mensaje eliminado".into());
    }
    let original = shared.store.proto(from, id).and_then(|p| convert::decode(&p)).map(|p| p.get_base_message().clone());
    // Media keeps its upload (url / keys); everything else goes as forwarded text.
    let media_msg = original.filter(|_| m.kind.has_media()).and_then(|mut o| {
        macro_rules! fwd {
            ($($f:ident),*) => {
                $(if let Some(x) = o.$f.as_option_mut() {
                    let score = x.context_info.as_option().and_then(|c| c.forwarding_score).unwrap_or(0) + 1;
                    x.context_info = forwarded_context(score);
                    return Some(wa::Message { $f: o.$f.clone(), ..Default::default() });
                })*
            };
        }
        fwd!(image_message, video_message, audio_message, document_message, sticker_message);
        None
    });
    let msg = media_msg.unwrap_or_else(|| wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage { text: Some(m.text.clone()), context_info: forwarded_context(1), ..Default::default() }),
        ..Default::default()
    });
    let as_text = msg.extended_text_message.is_set();
    let media = if as_text { None } else { m.media.clone() };
    let row = NewMessage {
        kind: Some(if as_text { MsgKind::Text } else { m.kind }),
        forwarded: m.forwarded + 1,
        text: m.text.clone(),
        mime: m.mime.clone(),
        file_name: m.file_name.clone(),
        duration: m.duration,
        width: m.width,
        height: m.height,
        ..Default::default()
    };
    deliver(shared, chat, msg, row, media).await
}

async fn send_sticker(shared: &Shared, chat: &str, fav_id: &str) -> Result<(), String> {
    let client = shared.client().ok_or("WhatsApp no está conectado")?;
    let path = shared.store.stickers().into_iter().find(|s| s.id == fav_id).map(|s| s.path).ok_or("Sticker no encontrado")?;
    let original = shared.store.sticker_proto(fav_id).and_then(|p| convert::decode(&p));
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let upload = client.upload(bytes, MediaType::Sticker, Default::default()).await.map_err(|e| e.to_string())?;
    let orig = original.as_ref().and_then(|m| m.sticker_message.as_option().cloned()).unwrap_or_default();
    let sticker = wa::message::StickerMessage {
        url: Some(upload.url),
        direct_path: Some(upload.direct_path),
        media_key: Some(upload.media_key.to_vec()),
        file_sha256: Some(upload.file_sha256.to_vec()),
        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
        file_length: Some(upload.file_length),
        media_key_timestamp: Some(upload.media_key_timestamp),
        mimetype: Some("image/webp".into()),
        width: orig.width.or(Some(512)),
        height: orig.height.or(Some(512)),
        is_animated: orig.is_animated,
        png_thumbnail: orig.png_thumbnail.clone(),
        ..Default::default()
    };
    let msg = wa::Message { sticker_message: MessageField::some(sticker), ..Default::default() };
    deliver(
        shared,
        chat,
        msg,
        NewMessage { kind: Some(MsgKind::Sticker), mime: Some("image/webp".into()), width: orig.width, height: orig.height, ..Default::default() },
        Some(path),
    )
    .await
}

// ---------------------------------------------------------------- media

fn transcribe_note(shared: &Arc<Shared>, rt: &tokio::runtime::Handle, chat: &str, id: &str) {
    if !crate::transcribe::available() {
        return;
    }
    if shared.store.message(chat, id).is_none_or(|m| m.transcript.is_some() || !matches!(m.kind, MsgKind::Voice | MsgKind::Audio)) {
        return;
    }
    let key = (chat.to_string(), id.to_string());
    if !shared.transcribing.lock().unwrap().insert(key.clone()) {
        return;
    }
    let shared = shared.clone();
    rt.spawn(async move {
        let (chat, id) = (key.0.clone(), key.1.clone());
        let media = match shared.store.message(&chat, &id).and_then(|m| m.media) {
            Some(p) => Ok(p),
            None => download(&shared, &chat, &id).await,
        };
        let path = match media {
            Ok(p) => p,
            Err(e) => {
                // Not downloadable yet (offline…): retried next time the note is shown.
                log::warn!("whatsapp transcribe {id}: {e}");
                shared.transcribing.lock().unwrap().remove(&key);
                return;
            }
        };
        let _turn = shared.whisper.lock().await;
        let work = shared.dir.join("tmp");
        match tokio::task::spawn_blocking(move || crate::transcribe::transcribe(&path, &work)).await.unwrap_or_else(|e| Err(e.to_string())) {
            Ok(text) => {
                shared.store.set_transcript(&chat, &id, &text);
                shared.transcribing.lock().unwrap().remove(&key);
                shared.emit(Update::Messages(chat));
            }
            // Key stays in `transcribing`: no retry loop this session for a file whisper rejects.
            Err(e) => log::warn!("whatsapp transcribe {id}: {e}"),
        }
    });
}

async fn download(shared: &Shared, chat: &str, id: &str) -> Result<PathBuf, String> {
    let client = shared.client().ok_or("WhatsApp no está conectado")?;
    let msg = shared.store.message(chat, id).ok_or("mensaje no encontrado")?;
    if let Some(p) = msg.media.clone() {
        return Ok(p);
    }
    let proto = shared.store.proto(chat, id).ok_or("sin datos de media")?;
    let m = convert::decode(&proto).ok_or("proto inválido")?;
    let m = m.get_base_message();
    let item: &dyn Downloadable = if let Some(x) = m.image_message.as_option() {
        x
    } else if let Some(x) = m.video_message.as_option() {
        x
    } else if let Some(x) = m.audio_message.as_option() {
        x
    } else if let Some(x) = m.sticker_message.as_option() {
        x
    } else if let Some(x) = m.document_message.as_option() {
        x
    } else {
        return Err("no es media".into());
    };
    let bytes = client.download(item).await.map_err(|e| e.to_string())?;
    let dir = shared.media_dir(chat);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ext = convert::extension(msg.kind, msg.mime.as_deref(), msg.file_name.as_deref());
    let name = match (&msg.file_name, msg.kind) {
        (Some(n), MsgKind::Document) => format!("{}-{}", safe(id), safe(n)),
        _ => format!("{}.{ext}", safe(id)),
    };
    let dest = dir.join(name);
    std::fs::write(&dest, bytes).map_err(|e| e.to_string())?;
    shared.store.set_media(chat, id, &dest);
    Ok(dest)
}
