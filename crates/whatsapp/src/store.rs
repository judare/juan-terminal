//! Local cache of chats, messages and contacts (`cache.db`), so the chat list and history open
//! instantly and survive restarts without asking the phone again. The app process writes it;
//! the MCP process (spawned by Claude) only reads it. WAL mode lets both coexist.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::model::{ChatMessage, ChatSummary, Label, MsgKind, MsgStatus, Quoted, Reaction, StickerFav};

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS chats (
    jid TEXT PRIMARY KEY,
    name TEXT,
    is_group INTEGER NOT NULL DEFAULT 0,
    last_ts INTEGER NOT NULL DEFAULT 0,
    last_id TEXT,
    unread INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    muted_until INTEGER NOT NULL DEFAULT 0,
    avatar TEXT,
    avatar_checked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS messages (
    chat TEXT NOT NULL,
    id TEXT NOT NULL,
    sender TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL DEFAULT '',
    status INTEGER NOT NULL DEFAULT 0,
    seen INTEGER NOT NULL DEFAULT 1,
    edited INTEGER NOT NULL DEFAULT 0,
    quoted_id TEXT,
    quoted_sender TEXT,
    quoted_text TEXT,
    quoted_kind TEXT,
    mime TEXT,
    file_name TEXT,
    duration INTEGER,
    width INTEGER,
    height INTEGER,
    media TEXT,
    proto BLOB,
    PRIMARY KEY (chat, id)
);
CREATE INDEX IF NOT EXISTS messages_chat_ts ON messages(chat, ts);
CREATE TABLE IF NOT EXISTS reactions (
    chat TEXT NOT NULL,
    target TEXT NOT NULL,
    sender TEXT NOT NULL,
    emoji TEXT NOT NULL,
    from_me INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (chat, target, sender)
);
CREATE TABLE IF NOT EXISTS contacts (
    jid TEXT PRIMARY KEY,
    name TEXT,
    push_name TEXT
);
CREATE TABLE IF NOT EXISTS aliases (
    lid TEXT PRIMARY KEY,
    pn TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS stickers (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    proto BLOB NOT NULL,
    added INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS labels (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    color INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_labels (
    jid TEXT NOT NULL,
    label INTEGER NOT NULL,
    PRIMARY KEY (jid, label)
);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT
);
";

/// A message as written by the service (one row of `messages`).
#[derive(Default, Clone, Debug)]
pub struct NewMessage {
    pub chat: String,
    pub id: String,
    pub sender: String,
    pub from_me: bool,
    pub ts: i64,
    pub kind: Option<MsgKind>,
    pub text: String,
    pub status: MsgStatus,
    /// Incoming and not read yet.
    pub unseen: bool,
    pub quoted_id: Option<String>,
    pub quoted_sender: Option<String>,
    pub quoted_text: Option<String>,
    pub quoted_kind: Option<MsgKind>,
    /// Forwarding score (0 = not forwarded).
    pub forwarded: u32,
    pub mime: Option<String>,
    pub file_name: Option<String>,
    pub duration: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub proto: Option<Vec<u8>>,
}

/// Chat metadata update: `None` fields are left untouched.
#[derive(Default, Clone, Debug)]
pub struct ChatMeta {
    pub name: Option<String>,
    pub is_group: Option<bool>,
    pub unread: Option<u32>,
    pub archived: Option<bool>,
    pub pinned: Option<bool>,
    pub muted_until: Option<i64>,
    pub last_ts: Option<i64>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

/// `jid` without the device part and with phone-number JIDs shown as `+57…` when nameless.
pub fn phone_label(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid);
    let user = user.split(':').next().unwrap_or(user);
    if jid.ends_with("@s.whatsapp.net") { format!("+{user}") } else { user.to_string() }
}

/// A name that is only a (possibly masked) phone number, e.g. `+57∙∙∙∙∙∙∙∙03` as WhatsApp's
/// history sync sends for some 1:1 chats. It must not hide the name saved in the address book.
pub fn phone_like(name: &str) -> bool {
    let s = name.trim();
    s.starts_with('+') && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')' | '.' | '∙' | '•' | '·' | '*'))
}

/// Chats hidden from the list: status updates, channels, broadcast lists.
pub fn hidden_chat(jid: &str) -> bool {
    jid == "status@broadcast" || jid.ends_with("@newsletter") || jid.ends_with("@broadcast")
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        // Added later: local-only "important" flag (only these count toward the Dock badge).
        let _ = conn.execute("ALTER TABLE chats ADD COLUMN important INTEGER NOT NULL DEFAULT 0", []);
        // Whisper transcriptions of voice notes (NULL = not transcribed yet).
        let _ = conn.execute("ALTER TABLE messages ADD COLUMN transcript TEXT", []);
        // Project (juan-terminal project id) a label is linked to: Claude's suggested replies
        // in those chats may read that project's code, servers and connectors.
        let _ = conn.execute("ALTER TABLE labels ADD COLUMN project TEXT", []);
        // Forwarding score (0 = not forwarded). Newly added: backfill from the stored protos.
        if conn.execute("ALTER TABLE messages ADD COLUMN forwarded INTEGER NOT NULL DEFAULT 0", []).is_ok() {
            Self::backfill_forwarded(&conn);
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn backfill_forwarded(conn: &Connection) {
        let rows: Vec<(String, String, Vec<u8>)> = conn
            .prepare("SELECT chat, id, proto FROM messages WHERE proto IS NOT NULL")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|rows| rows.flatten().collect()))
            .unwrap_or_default();
        for (chat, id, proto) in rows {
            let Some(msg) = crate::convert::decode(&proto) else { continue };
            if let crate::convert::Parsed::Message(c) = crate::convert::parse(&msg, false)
                && c.forwarded > 0
            {
                let _ = conn.execute("UPDATE messages SET forwarded=?3 WHERE chat=?1 AND id=?2", params![chat, id, c.forwarded]);
            }
        }
    }

    /// Read-only handle (MCP process). Fails if the cache doesn't exist yet.
    pub fn open_read_only(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wipe everything (after logging out: the next account starts clean).
    pub fn clear(&self) {
        let db = self.db();
        let _ = db.execute_batch("DELETE FROM chats; DELETE FROM messages; DELETE FROM reactions; DELETE FROM contacts; DELETE FROM aliases; DELETE FROM meta; DELETE FROM chat_labels; DELETE FROM labels;");
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.db().query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        let _ = self.db().execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value]);
    }

    // ---- aliases (LID ↔ phone number) ----

    pub fn set_alias(&self, lid: &str, pn: &str) {
        if lid == pn {
            return;
        }
        let db = self.db();
        let _ = db.execute("INSERT INTO aliases(lid,pn) VALUES(?1,?2) ON CONFLICT(lid) DO UPDATE SET pn=excluded.pn", params![lid, pn]);
        // A chat first seen under its LID moves to the phone-number JID.
        let moved: bool = db.query_row("SELECT 1 FROM chats WHERE jid=?1", [lid], |_| Ok(true)).optional().ok().flatten().unwrap_or(false);
        if moved {
            let tx = db.unchecked_transaction();
            if let Ok(tx) = tx {
                let _ = tx.execute("INSERT OR IGNORE INTO chats(jid,name,is_group) SELECT ?2,name,is_group FROM chats WHERE jid=?1", params![lid, pn]);
                let _ = tx.execute(
                    "UPDATE chats SET last_ts=MAX(last_ts,(SELECT last_ts FROM chats WHERE jid=?1)), unread=unread+(SELECT unread FROM chats WHERE jid=?1) WHERE jid=?2",
                    params![lid, pn],
                );
                let _ = tx.execute("UPDATE OR IGNORE messages SET chat=?2 WHERE chat=?1", params![lid, pn]);
                let _ = tx.execute("UPDATE OR IGNORE reactions SET chat=?2 WHERE chat=?1", params![lid, pn]);
                let _ = tx.execute("UPDATE OR IGNORE chat_labels SET jid=?2 WHERE jid=?1", params![lid, pn]);
                let _ = tx.execute("DELETE FROM chat_labels WHERE jid=?1", [lid]);
                let _ = tx.execute("DELETE FROM chats WHERE jid=?1", [lid]);
                let _ = tx.execute("DELETE FROM messages WHERE chat=?1", [lid]);
                let _ = tx.execute(
                    "UPDATE chats SET last_id=(SELECT id FROM messages WHERE chat=?1 ORDER BY ts DESC LIMIT 1), last_ts=MAX(last_ts, IFNULL((SELECT MAX(ts) FROM messages WHERE chat=?1),0)) WHERE jid=?1",
                    [pn],
                );
                let _ = tx.commit();
            }
        }
        let _ = db.execute("UPDATE messages SET sender=?2 WHERE sender=?1", params![lid, pn]);
    }

    /// Phone-number JID for a LID (or the JID itself).
    pub fn canonical(&self, jid: &str) -> String {
        if !jid.ends_with("@lid") {
            return jid.to_string();
        }
        self.db().query_row("SELECT pn FROM aliases WHERE lid=?1", [jid], |r| r.get(0)).optional().ok().flatten().unwrap_or_else(|| jid.to_string())
    }

    // ---- contacts ----

    pub fn set_contact_name(&self, jid: &str, name: &str) {
        if name.is_empty() {
            return;
        }
        let _ = self.db().execute("INSERT INTO contacts(jid,name) VALUES(?1,?2) ON CONFLICT(jid) DO UPDATE SET name=excluded.name", params![jid, name]);
    }

    pub fn set_push_name(&self, jid: &str, name: &str) {
        if name.is_empty() {
            return;
        }
        let _ = self.db().execute("INSERT INTO contacts(jid,push_name) VALUES(?1,?2) ON CONFLICT(jid) DO UPDATE SET push_name=excluded.push_name", params![jid, name]);
    }

    /// Best display name: saved contact name, chat name, push name, then the phone number.
    pub fn display_name(&self, jid: &str) -> String {
        Self::display_name_in(&self.db(), jid)
    }

    fn display_name_in(db: &Connection, jid: &str) -> String {
        let row: Option<(Option<String>, Option<String>)> =
            db.query_row("SELECT name, push_name FROM contacts WHERE jid=?1", [jid], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten();
        let chat: Option<String> = db.query_row("SELECT name FROM chats WHERE jid=?1", [jid], |r| r.get(0)).optional().ok().flatten().flatten();
        let chat = chat.filter(|s| !phone_like(s));
        let (name, push) = row.unwrap_or((None, None));
        name.filter(|s| !s.is_empty())
            .or(chat.filter(|s| !s.is_empty()))
            .or(push.filter(|s| !s.is_empty()).map(|p| format!("~{p}")))
            .unwrap_or_else(|| phone_label(jid))
    }

    // ---- chats ----

    pub fn upsert_chat(&self, jid: &str, meta: &ChatMeta) {
        let db = self.db();
        let _ = db.execute("INSERT OR IGNORE INTO chats(jid,is_group) VALUES(?1,?2)", params![jid, jid.ends_with("@g.us")]);
        if let Some(n) = meta.name.as_deref().filter(|n| !n.is_empty() && !phone_like(n)) {
            let _ = db.execute("UPDATE chats SET name=?2 WHERE jid=?1", params![jid, n]);
        }
        if let Some(v) = meta.is_group {
            let _ = db.execute("UPDATE chats SET is_group=?2 WHERE jid=?1", params![jid, v]);
        }
        if let Some(v) = meta.unread {
            let _ = db.execute("UPDATE chats SET unread=?2 WHERE jid=?1", params![jid, v]);
        }
        if let Some(v) = meta.archived {
            let _ = db.execute("UPDATE chats SET archived=?2 WHERE jid=?1", params![jid, v]);
        }
        if let Some(v) = meta.pinned {
            let _ = db.execute("UPDATE chats SET pinned=?2 WHERE jid=?1", params![jid, v]);
        }
        if let Some(v) = meta.muted_until {
            let _ = db.execute("UPDATE chats SET muted_until=?2 WHERE jid=?1", params![jid, v]);
        }
        if let Some(v) = meta.last_ts {
            let _ = db.execute("UPDATE chats SET last_ts=MAX(last_ts,?2) WHERE jid=?1", params![jid, v]);
        }
    }

    pub fn delete_chat(&self, jid: &str) {
        let db = self.db();
        let _ = db.execute("DELETE FROM chats WHERE jid=?1", [jid]);
        let _ = db.execute("DELETE FROM messages WHERE chat=?1", [jid]);
        let _ = db.execute("DELETE FROM reactions WHERE chat=?1", [jid]);
    }

    pub fn clear_chat(&self, jid: &str) {
        let db = self.db();
        let _ = db.execute("DELETE FROM messages WHERE chat=?1", [jid]);
        let _ = db.execute("DELETE FROM reactions WHERE chat=?1", [jid]);
        let _ = db.execute("UPDATE chats SET last_id=NULL, unread=0 WHERE jid=?1", [jid]);
    }

    pub fn is_muted(&self, jid: &str) -> bool {
        let until: i64 = self.db().query_row("SELECT muted_until FROM chats WHERE jid=?1", [jid], |r| r.get(0)).optional().ok().flatten().unwrap_or(0);
        until == -1 || until > now()
    }

    pub fn set_important(&self, jid: &str, important: bool) {
        let _ = self.db().execute("UPDATE chats SET important=?2 WHERE jid=?1", params![jid, important]);
    }

    pub fn is_archived(&self, jid: &str) -> bool {
        self.db().query_row("SELECT archived FROM chats WHERE jid=?1", [jid], |r| r.get::<_, bool>(0)).optional().ok().flatten().unwrap_or(false)
    }

    pub fn set_avatar(&self, jid: &str, path: Option<&Path>) {
        let _ = self.db().execute(
            "UPDATE chats SET avatar=?2, avatar_checked=?3 WHERE jid=?1",
            params![jid, path.map(|p| p.to_string_lossy().into_owned()), now()],
        );
    }

    /// Chats whose picture was never fetched (or long ago), most recent first.
    pub fn avatars_to_fetch(&self, limit: usize) -> Vec<String> {
        let db = self.db();
        let Ok(mut st) = db.prepare(
            "SELECT jid FROM chats WHERE archived=0 AND avatar_checked < ?1 AND last_ts>0 ORDER BY pinned DESC, last_ts DESC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        st.query_map(params![now() - 7 * 86400, limit as i64], |r| r.get(0)).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    pub fn forget_avatar(&self, jid: &str) {
        let _ = self.db().execute("UPDATE chats SET avatar_checked=0 WHERE jid=?1", [jid]);
    }

    /// Visible chats (not archived/hidden), pinned first, then by last message.
    pub fn chats(&self) -> Vec<ChatSummary> {
        self.chats_in(false)
    }

    /// Archived chats, by last message.
    pub fn archived_chats(&self) -> Vec<ChatSummary> {
        self.chats_in(true)
    }

    pub fn archived_count(&self) -> usize {
        let db = self.db();
        let Ok(mut st) = db.prepare("SELECT jid FROM chats WHERE archived=1 AND last_ts>0") else { return 0 };
        st.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.flatten().filter(|j| !hidden_chat(j)).count()).unwrap_or(0)
    }

    fn chats_in(&self, archived: bool) -> Vec<ChatSummary> {
        let db = self.db();
        let Ok(mut st) = db.prepare(
            "SELECT c.jid, c.name, c.is_group, c.last_ts, c.unread, c.pinned, c.muted_until, c.avatar,
                    m.kind, m.text, m.from_me, m.status, m.sender, c.important,
                    EXISTS(SELECT 1 FROM reactions r WHERE r.chat=c.jid AND r.target=c.last_id AND r.from_me=1 AND r.emoji<>'')
             FROM chats c LEFT JOIN messages m ON m.chat=c.jid AND m.id=c.last_id
             WHERE c.archived=?1 AND c.last_ts>0
             ORDER BY c.pinned DESC, c.last_ts DESC",
        ) else {
            return Vec::new();
        };
        let rows = st
            .query_map([archived], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<bool>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, Option<bool>>(13).ok().flatten().unwrap_or(false),
                    r.get::<_, bool>(14).unwrap_or(false),
                ))
            })
            .map(|rows| rows.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let now = now();
        let mut chat_labels: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
        if let Ok(mut st) = db.prepare("SELECT jid, label FROM chat_labels ORDER BY label") {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                for (jid, label) in rows.flatten() {
                    chat_labels.entry(jid).or_default().push(label);
                }
            }
        }
        rows.into_iter()
            .filter(|r| !hidden_chat(&r.0))
            .map(|(jid, name, is_group, last_ts, unread, pinned, muted, avatar, kind, text, from_me, status, sender, important, reacted)| {
                let kind = kind.as_deref().map(MsgKind::parse);
                let text = text.unwrap_or_default();
                let last_text = match kind {
                    Some(MsgKind::Revoked) => MsgKind::Revoked.label().to_string(),
                    Some(k) if text.is_empty() => k.label().to_string(),
                    _ => text,
                }
                .replace('\n', " ");
                let from_me = from_me.unwrap_or(false);
                let labels = chat_labels.remove(&jid).unwrap_or_default();
                let last_sender = match sender.as_deref().filter(|s| is_group && !from_me && !s.is_empty()) {
                    Some(s) => Self::display_name_in(&db, s).trim_start_matches('~').to_string(),
                    None => String::new(),
                };
                ChatSummary {
                    // 1:1 chats: the saved contact name wins over the chat name from history sync.
                    name: if is_group { name.filter(|n| !n.is_empty()).unwrap_or_else(|| Self::display_name_in(&db, &jid)) } else { Self::display_name_in(&db, &jid) },
                    jid,
                    is_group,
                    last_ts,
                    last_text,
                    last_kind: kind.unwrap_or(MsgKind::Text),
                    last_sender,
                    last_from_me: from_me,
                    last_reacted_by_me: reacted,
                    last_status: MsgStatus::from_i64(status.unwrap_or(0)),
                    unread: unread.max(0) as u32,
                    pinned: pinned > 0,
                    muted: muted == -1 || muted > now,
                    important,
                    labels,
                    avatar: avatar.map(PathBuf::from).filter(|p| p.exists()),
                }
            })
            .collect()
    }

    pub fn chat(&self, jid: &str) -> Option<ChatSummary> {
        self.chats().into_iter().find(|c| c.jid == jid).or_else(|| self.archived_chats().into_iter().find(|c| c.jid == jid))
    }

    /// Resolve what the user / Claude typed (JID, phone number or name) to a chat JID.
    pub fn resolve_chat(&self, query: &str) -> Option<String> {
        let q = query.trim();
        if q.contains('@') {
            return Some(self.canonical(q));
        }
        let digits: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 7 && digits.len() + 4 >= q.len() {
            let jid = format!("{digits}@s.whatsapp.net");
            return Some(jid);
        }
        let lower = q.to_lowercase();
        let chats = self.chats();
        chats
            .iter()
            .find(|c| c.name.to_lowercase() == lower)
            .or_else(|| chats.iter().find(|c| c.name.to_lowercase().contains(&lower)))
            .map(|c| c.jid.clone())
    }

    // ---- messages ----

    /// Insert (or, for an existing id, refresh) a message and bump its chat.
    /// Returns true when the message is new.
    pub fn insert_message(&self, m: &NewMessage) -> bool {
        let db = self.db();
        let exists: bool = db.query_row("SELECT 1 FROM messages WHERE chat=?1 AND id=?2", params![m.chat, m.id], |_| Ok(true)).optional().ok().flatten().unwrap_or(false);
        let kind = m.kind.unwrap_or(MsgKind::Other);
        if exists {
            // History can deliver a message we already got live: keep our status/seen, fill blanks.
            let _ = db.execute(
                "UPDATE messages SET proto=COALESCE(proto,?3), status=MAX(status,?4), forwarded=MAX(forwarded,?5) WHERE chat=?1 AND id=?2",
                params![m.chat, m.id, m.proto, m.status.as_i64(), m.forwarded],
            );
            return false;
        }
        let _ = db.execute(
            "INSERT INTO messages(chat,id,sender,from_me,ts,kind,text,status,seen,quoted_id,quoted_sender,quoted_text,quoted_kind,mime,file_name,duration,width,height,proto,forwarded)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            params![
                m.chat,
                m.id,
                m.sender,
                m.from_me,
                m.ts,
                kind.as_str(),
                m.text,
                m.status.as_i64(),
                !m.unseen,
                m.quoted_id,
                m.quoted_sender,
                m.quoted_text,
                m.quoted_kind.map(|k| k.as_str()),
                m.mime,
                m.file_name,
                m.duration,
                m.width,
                m.height,
                m.proto,
                m.forwarded,
            ],
        );
        let _ = db.execute("INSERT OR IGNORE INTO chats(jid,is_group) VALUES(?1,?2)", params![m.chat, m.chat.ends_with("@g.us")]);
        let _ = db.execute("UPDATE chats SET last_ts=?2, last_id=?3 WHERE jid=?1 AND last_ts<=?2", params![m.chat, m.ts, m.id]);
        if m.unseen {
            let _ = db.execute("UPDATE chats SET unread=unread+1 WHERE jid=?1", [&m.chat]);
        }
        true
    }

    pub fn set_status(&self, chat: &str, ids: &[String], status: MsgStatus) {
        let db = self.db();
        for id in ids {
            let _ = db.execute("UPDATE messages SET status=MAX(status,?3) WHERE chat=?1 AND id=?2", params![chat, id, status.as_i64()]);
        }
    }

    /// Force a status (e.g. Failed after Pending).
    /// Our messages written while offline, oldest first: (chat, id, encoded message).
    pub fn outbox(&self) -> Vec<(String, String, Vec<u8>)> {
        let db = self.db();
        let Ok(mut st) = db.prepare("SELECT chat, id, proto FROM messages WHERE from_me=1 AND status=?1 AND proto IS NOT NULL ORDER BY ts, rowid") else {
            return Vec::new();
        };
        st.query_map([MsgStatus::Pending.as_i64()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    pub fn replace_status(&self, chat: &str, id: &str, status: MsgStatus) {
        let _ = self.db().execute("UPDATE messages SET status=?3 WHERE chat=?1 AND id=?2", params![chat, id, status.as_i64()]);
    }

    /// Rename a locally-created pending message to the id the server assigned.
    pub fn rename_message(&self, chat: &str, old: &str, new: &str) {
        let db = self.db();
        let _ = db.execute("UPDATE messages SET id=?3 WHERE chat=?1 AND id=?2", params![chat, old, new]);
        let _ = db.execute("UPDATE chats SET last_id=?3 WHERE jid=?1 AND last_id=?2", params![chat, old, new]);
    }

    pub fn edit_text(&self, chat: &str, id: &str, text: &str) {
        let _ = self.db().execute("UPDATE messages SET text=?3, edited=1 WHERE chat=?1 AND id=?2", params![chat, id, text]);
    }

    pub fn revoke(&self, chat: &str, id: &str) {
        let _ = self.db().execute(
            "UPDATE messages SET kind='revoked', text='', media=NULL, proto=NULL, quoted_id=NULL WHERE chat=?1 AND id=?2",
            params![chat, id],
        );
    }

    pub fn delete_message(&self, chat: &str, id: &str) {
        let db = self.db();
        let _ = db.execute("DELETE FROM messages WHERE chat=?1 AND id=?2", params![chat, id]);
        let _ = db.execute("UPDATE chats SET last_id=(SELECT id FROM messages WHERE chat=?1 ORDER BY ts DESC LIMIT 1) WHERE jid=?1", [chat]);
    }

    pub fn set_reaction(&self, chat: &str, target: &str, sender: &str, emoji: &str, from_me: bool) {
        let db = self.db();
        if emoji.is_empty() {
            let _ = db.execute("DELETE FROM reactions WHERE chat=?1 AND target=?2 AND sender=?3", params![chat, target, sender]);
        } else {
            let _ = db.execute(
                "INSERT INTO reactions(chat,target,sender,emoji,from_me) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(chat,target,sender) DO UPDATE SET emoji=excluded.emoji",
                params![chat, target, sender, emoji, from_me],
            );
        }
    }

    pub fn set_transcript(&self, chat: &str, id: &str, text: &str) {
        let _ = self.db().execute("UPDATE messages SET transcript=?3 WHERE chat=?1 AND id=?2", params![chat, id, text]);
    }

    pub fn set_media(&self, chat: &str, id: &str, path: &Path) {
        let _ = self.db().execute("UPDATE messages SET media=?3 WHERE chat=?1 AND id=?2", params![chat, id, path.to_string_lossy()]);
    }

    pub fn proto(&self, chat: &str, id: &str) -> Option<Vec<u8>> {
        self.db().query_row("SELECT proto FROM messages WHERE chat=?1 AND id=?2", params![chat, id], |r| r.get(0)).optional().ok().flatten().flatten()
    }

    /// (sender, from_me) of a message, for quoting / reacting.
    pub fn message_origin(&self, chat: &str, id: &str) -> Option<(String, bool)> {
        self.db().query_row("SELECT sender, from_me FROM messages WHERE chat=?1 AND id=?2", params![chat, id], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten()
    }

    /// Mark the chat read: returns the incoming messages that still need a read receipt
    /// as (id, sender).
    /// Unread count stored for a chat (what the phone said when the chat was last synced).
    pub fn unread(&self, chat: &str) -> u32 {
        self.db().query_row("SELECT unread FROM chats WHERE jid=?1", [chat], |r| r.get::<_, i64>(0)).optional().ok().flatten().unwrap_or(0).max(0) as u32
    }

    pub fn mark_read(&self, chat: &str) -> Vec<(String, String)> {
        let db = self.db();
        let pending: Vec<(String, String)> = db
            .prepare("SELECT id, sender FROM messages WHERE chat=?1 AND from_me=0 AND seen=0 ORDER BY ts")
            .and_then(|mut st| st.query_map([chat], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()))
            .unwrap_or_default();
        let _ = db.execute("UPDATE messages SET seen=1 WHERE chat=?1 AND seen=0", [chat]);
        let _ = db.execute("UPDATE chats SET unread=0 WHERE jid=?1", [chat]);
        pending
    }

    fn reactions_for(db: &Connection, chat: &str, target: &str) -> Vec<Reaction> {
        db.prepare("SELECT sender, emoji, from_me FROM reactions WHERE chat=?1 AND target=?2")
            .and_then(|mut st| {
                st.query_map(params![chat, target], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))
                    .map(|rows| rows.flatten().collect::<Vec<_>>())
            })
            .unwrap_or_default()
            .into_iter()
            .map(|(sender, emoji, from_me)| Reaction {
                sender_name: if from_me { "Tú".into() } else { Self::display_name_in(db, &sender) },
                sender,
                emoji,
                from_me,
            })
            .collect()
    }

    const MSG_COLS: &'static str = "chat,id,sender,from_me,ts,kind,text,status,edited,quoted_id,quoted_sender,quoted_text,quoted_kind,mime,file_name,duration,width,height,media,transcript,forwarded";

    fn row_to_msg(db: &Connection, r: &rusqlite::Row<'_>) -> rusqlite::Result<ChatMessage> {
        let chat: String = r.get(0)?;
        let id: String = r.get(1)?;
        let sender: String = r.get(2)?;
        let from_me: bool = r.get(3)?;
        let quoted_id: Option<String> = r.get(9)?;
        let quoted = quoted_id.map(|qid| {
            let qs: Option<String> = r.get(10).ok().flatten();
            let qk: Option<String> = r.get(12).ok().flatten();
            Quoted {
                id: qid,
                sender_name: qs.as_deref().map(|s| Self::display_name_in(db, s)).unwrap_or_default(),
                text: r.get::<_, Option<String>>(11).ok().flatten().unwrap_or_default(),
                kind: qk.as_deref().map(MsgKind::parse).unwrap_or(MsgKind::Text),
            }
        });
        let media: Option<String> = r.get(18)?;
        Ok(ChatMessage {
            sender_name: if from_me { "Tú".into() } else { Self::display_name_in(db, &sender) },
            reactions: Self::reactions_for(db, &chat, &id),
            chat,
            id,
            sender,
            from_me,
            ts: r.get(4)?,
            kind: MsgKind::parse(&r.get::<_, String>(5)?),
            text: r.get(6)?,
            status: MsgStatus::from_i64(r.get(7)?),
            edited: r.get(8)?,
            quoted,
            mime: r.get(13)?,
            file_name: r.get(14)?,
            duration: r.get(15)?,
            width: r.get(16)?,
            height: r.get(17)?,
            media: media.map(PathBuf::from).filter(|p| p.exists()),
            transcript: r.get::<_, Option<String>>(19).ok().flatten(),
            forwarded: r.get(20).unwrap_or(0),
        })
    }

    /// The `limit` messages before `before_ts` (all if None), oldest first.
    pub fn messages(&self, chat: &str, before_ts: Option<i64>, limit: usize) -> Vec<ChatMessage> {
        let db = self.db();
        let sql = format!("SELECT {} FROM messages WHERE chat=?1 AND ts<?2 ORDER BY ts DESC, rowid DESC LIMIT ?3", Self::MSG_COLS);
        let mut out: Vec<ChatMessage> = db
            .prepare(&sql)
            .and_then(|mut st| st.query_map(params![chat, before_ts.unwrap_or(i64::MAX), limit as i64], |r| Self::row_to_msg(&db, r)).map(|rows| rows.flatten().collect()))
            .unwrap_or_default();
        out.reverse();
        out
    }

    /// All messages at or after `from_ts`, oldest first.
    pub fn messages_from(&self, chat: &str, from_ts: i64) -> Vec<ChatMessage> {
        let db = self.db();
        let sql = format!("SELECT {} FROM messages WHERE chat=?1 AND ts>=?2 ORDER BY ts, rowid", Self::MSG_COLS);
        db.prepare(&sql)
            .and_then(|mut st| st.query_map(params![chat, from_ts], |r| Self::row_to_msg(&db, r)).map(|rows| rows.flatten().collect::<Vec<ChatMessage>>()))
            .unwrap_or_default()
    }

    pub fn message(&self, chat: &str, id: &str) -> Option<ChatMessage> {
        let db = self.db();
        let sql = format!("SELECT {} FROM messages WHERE chat=?1 AND id=?2", Self::MSG_COLS);
        db.query_row(&sql, params![chat, id], |r| Self::row_to_msg(&db, r)).optional().ok().flatten()
    }

    /// Messages whose text matches `query` (case-insensitive), newest first. `chat` limits
    /// the search to one conversation.
    pub fn search(&self, query: &str, chat: Option<&str>, limit: usize) -> Vec<ChatMessage> {
        let db = self.db();
        let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let sql = format!(
            "SELECT {} FROM messages m WHERE text LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR chat=?2)
             AND (?2 IS NOT NULL OR NOT EXISTS (SELECT 1 FROM chats c WHERE c.jid=m.chat AND c.archived=1))
             ORDER BY ts DESC LIMIT ?3",
            Self::MSG_COLS
        );
        db.prepare(&sql)
            .and_then(|mut st| st.query_map(params![like, chat, limit as i64], |r| Self::row_to_msg(&db, r)).map(|rows| rows.flatten().collect::<Vec<ChatMessage>>()))
            .unwrap_or_default()
            .into_iter()
            .filter(|m: &ChatMessage| !hidden_chat(&m.chat))
            .collect()
    }

    /// Messages of `chat` of the given kinds, newest first (contact info: media, documents).
    pub fn chat_media(&self, chat: &str, kinds: &[MsgKind], limit: usize) -> Vec<ChatMessage> {
        if kinds.is_empty() {
            return Vec::new();
        }
        let db = self.db();
        let list = kinds.iter().map(|k| format!("'{}'", k.as_str())).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT {} FROM messages WHERE chat=?1 AND kind IN ({list}) ORDER BY ts DESC, rowid DESC LIMIT ?2", Self::MSG_COLS);
        db.prepare(&sql)
            .and_then(|mut st| st.query_map(params![chat, limit as i64], |r| Self::row_to_msg(&db, r)).map(|rows| rows.flatten().collect::<Vec<ChatMessage>>()))
            .unwrap_or_default()
    }

    /// Messages of `chat` that contain a link, newest first.
    pub fn chat_links(&self, chat: &str, limit: usize) -> Vec<ChatMessage> {
        let db = self.db();
        let sql = format!(
            "SELECT {} FROM messages WHERE chat=?1 AND kind!='revoked' AND (text LIKE '%http://%' OR text LIKE '%https://%' OR text LIKE '%www.%') ORDER BY ts DESC, rowid DESC LIMIT ?2",
            Self::MSG_COLS
        );
        db.prepare(&sql)
            .and_then(|mut st| st.query_map(params![chat, limit as i64], |r| Self::row_to_msg(&db, r)).map(|rows| rows.flatten().collect::<Vec<ChatMessage>>()))
            .unwrap_or_default()
    }

    /// Saved contact name and push name ("~Nombre" the person chose) of a JID.
    pub fn contact_names(&self, jid: &str) -> (Option<String>, Option<String>) {
        self.db()
            .query_row("SELECT name, push_name FROM contacts WHERE jid=?1", [jid], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)))
            .optional()
            .ok()
            .flatten()
            .map(|(a, b)| (a.filter(|s| !s.is_empty()), b.filter(|s| !s.is_empty())))
            .unwrap_or((None, None))
    }

    pub fn oldest_ts(&self, chat: &str) -> Option<i64> {
        self.db().query_row("SELECT MIN(ts) FROM messages WHERE chat=?1", [chat], |r| r.get(0)).optional().ok().flatten().flatten()
    }

    // ---- favourite stickers ----

    pub fn add_sticker(&self, id: &str, path: &Path, proto: &[u8]) {
        let _ = self.db().execute(
            "INSERT OR REPLACE INTO stickers(id,path,proto,added) VALUES(?1,?2,?3,?4)",
            params![id, path.to_string_lossy(), proto, now()],
        );
    }

    pub fn remove_sticker(&self, id: &str) {
        let _ = self.db().execute("DELETE FROM stickers WHERE id=?1", [id]);
    }

    pub fn is_favorite_sticker(&self, id: &str) -> bool {
        self.db().query_row("SELECT 1 FROM stickers WHERE id=?1", [id], |_| Ok(true)).optional().ok().flatten().unwrap_or(false)
    }

    pub fn stickers(&self) -> Vec<StickerFav> {
        self.db()
            .prepare("SELECT id, path FROM stickers ORDER BY added DESC")
            .and_then(|mut st| {
                st.query_map([], |r| Ok(StickerFav { id: r.get(0)?, path: PathBuf::from(r.get::<_, String>(1)?) })).map(|rows| rows.flatten().collect::<Vec<_>>())
            })
            .unwrap_or_default()
    }

    pub fn sticker_proto(&self, id: &str) -> Option<Vec<u8>> {
        self.db().query_row("SELECT proto FROM stickers WHERE id=?1", [id], |r| r.get(0)).optional().ok().flatten()
    }

    // ---- labels (local only) ----

    pub fn labels(&self) -> Vec<Label> {
        self.db()
            .prepare("SELECT id, name, color, project FROM labels ORDER BY id")
            .and_then(|mut st| st.query_map([], |r| Ok(Label { id: r.get(0)?, name: r.get(1)?, color: r.get::<_, i64>(2)? as u32, project: r.get(3)? })).map(|rows| rows.flatten().collect::<Vec<_>>()))
            .unwrap_or_default()
    }

    /// New label (or the existing one with the same name, ignoring case). Returns its id.
    pub fn create_label(&self, name: &str) -> Option<i64> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        if let Some(l) = self.labels().into_iter().find(|l| l.name.to_lowercase() == name.to_lowercase()) {
            return Some(l.id);
        }
        const COLORS: [u32; 8] = [0x25d366, 0x53bdeb, 0xe5c07b, 0xc678dd, 0xe06c75, 0xd19a66, 0x56b6c2, 0x98c379];
        let db = self.db();
        let n: i64 = db.query_row("SELECT COUNT(*) FROM labels", [], |r| r.get(0)).unwrap_or(0);
        db.execute("INSERT INTO labels(name, color) VALUES(?1, ?2)", params![name, COLORS[n as usize % COLORS.len()] as i64]).ok()?;
        Some(db.last_insert_rowid())
    }

    pub fn delete_label(&self, id: i64) {
        let db = self.db();
        let _ = db.execute("DELETE FROM chat_labels WHERE label=?1", [id]);
        let _ = db.execute("DELETE FROM labels WHERE id=?1", [id]);
    }

    /// Link label `id` to a juan-terminal project (None unlinks it).
    pub fn set_label_project(&self, id: i64, project: Option<&str>) {
        let _ = self.db().execute("UPDATE labels SET project=?2 WHERE id=?1", params![id, project]);
    }

    pub fn set_chat_label(&self, jid: &str, label: i64, on: bool) {
        let db = self.db();
        let _ = if on {
            db.execute("INSERT OR IGNORE INTO chat_labels(jid, label) VALUES(?1, ?2)", params![jid, label])
        } else {
            db.execute("DELETE FROM chat_labels WHERE jid=?1 AND label=?2", params![jid, label])
        };
    }

    /// Unread messages in chats marked important (the rest don't count toward badges).
    pub fn total_unread(&self) -> u32 {
        self.chats().iter().filter(|c| c.important).map(|c| c.unread).sum()
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
