//! A WhatsApp conversation shown as an editor tab: bubbles (text, images, stickers, videos,
//! documents, voice notes), replies, reactions, and a composer that sends text, files,
//! favourite stickers and recorded voice notes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle, InteractiveText, ListAlignment, ListOffset, ListState, MouseButton, MouseDownEvent,
    PathPromptOptions, Pixels, Point, Render, SharedString, StyledText, Subscription, Task, Timer, Window, div, list, prelude::*, px,
};
use whatsapp::{ChatMessage, MsgKind, Update, WhatsApp};

use crate::glass::GlassExt;
use crate::text_area::{TextArea, TextAreaEvent};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;
use crate::wa_ui;

const PAGE: usize = 60;
/// Emoji picker contents, by category.
const EMOJIS: &[(&str, &str)] = &[
    ("Caras", "😀 😃 😄 😁 😆 😅 🤣 😂 🙂 🙃 😉 😊 😇 🥰 😍 🤩 😘 😗 😚 😋 😛 😜 🤪 😝 🤑 🤗 🤭 🤫 🤔 🤐 🤨 😐 😑 😶 😏 😒 🙄 😬 😮‍💨 🤥 😌 😔 😪 🤤 😴 😷 🤒 🤕 🤢 🤮 🥵 🥶 🥴 😵 🤯 🤠 🥳 😎 🤓 🧐 😕 😟 🙁 😮 😯 😲 😳 🥺 🥹 😦 😧 😨 😰 😥 😢 😭 😱 😖 😣 😞 😓 😩 😫 🥱 😤 😡 😠 🤬 😈 💀 💩 🤡 👻 👽 🤖 😺 😸 😹 😻 🙈 🙉 🙊"),
    ("Gestos", "👋 🤚 ✋ 🖖 👌 🤌 🤏 ✌️ 🤞 🤟 🤘 🤙 👈 👉 👆 👇 ☝️ 👍 👎 ✊ 👊 🤛 🤜 👏 🙌 👐 🤲 🤝 🙏 ✍️ 💪 🫶 👀 🧠 🫡"),
    ("Corazones", "❤️ 🧡 💛 💚 💙 💜 🖤 🤍 🤎 💔 ❣️ 💕 💞 💓 💗 💖 💘 💝 💯 💢 💥 💫 💦 💨 💬 💤"),
    ("Cosas", "🎉 🎊 🎁 🎂 🍻 🥂 ☕ 🍕 🍔 🌮 🍺 🔥 ✨ ⭐ 🌟 ⚡ ☀️ 🌙 🌈 ⚽ 🏆 🎯 🎵 🎶 📱 💻 📷 💡 📌 📎 ✏️ 📝 📅 💰 💸 🚀 🚗 ✈️ 🏠 ⏰ ✅ ❌ ⚠️ ❓ ❗ 🆗 🇨🇴"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerTab {
    Emojis,
    Stickers,
}

gpui::actions!(chat_view, [SearchInChat, CopySelection]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new("cmd-f", SearchInChat, Some("ChatView")), gpui::KeyBinding::new("cmd-c", CopySelection, Some("ChatView"))]);
}

/// Search inside the open conversation (⌘F): matches newest first, Enter goes to older ones.
struct Search {
    input: Entity<TextInput>,
    query: String,
    hits: Vec<ChatMessage>,
    /// Index into `hits` of the message shown.
    current: usize,
    _sub: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InfoTab {
    Members,
    Media,
    Docs,
    Links,
}

/// Contact / group info sheet: picture, name, number and shared files.
struct Info {
    tab: InfoTab,
    media: Vec<Rc<ChatMessage>>,
    docs: Vec<Rc<ChatMessage>>,
    links: Vec<Rc<ChatMessage>>,
    /// Group participants (None while they're being fetched).
    members: Option<Vec<whatsapp::GroupMember>>,
    loaded: bool,
    copied: Option<Instant>,
}

/// Swipe-to-reply: how far the bubble follows (px) and the distance that triggers the reply.
const SWIPE_MAX: f32 = 90.;
const SWIPE_REPLY: f32 = 60.;
/// Bubble-sized preview of a PDF's first page (top part), above its file card.
const DOC_PREVIEW_W: f32 = 280.;
const DOC_PREVIEW_H: f32 = 150.;

const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

fn recent_reactions_path() -> PathBuf {
    whatsapp::data_dir().join("recent-reactions.txt")
}

/// Reactions offered first: the last ones used (newest first), topped up with the defaults.
fn quick_reactions() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_to_string(recent_reactions_path())
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    for e in QUICK_REACTIONS {
        if !out.iter().any(|x| x == e) {
            out.push(e.to_string());
        }
    }
    out.truncate(QUICK_REACTIONS.len());
    out
}

/// Moves `emoji` to the front of the recent reactions.
fn note_reaction(emoji: &str) {
    if emoji.is_empty() {
        return;
    }
    let mut list = quick_reactions();
    list.retain(|e| e != emoji);
    list.insert(0, emoji.to_string());
    list.truncate(QUICK_REACTIONS.len());
    let path = recent_reactions_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, list.join("\n"));
}

/// Chats open in a tab somewhere (notifications for them are skipped while the app is active).
static OPEN_CHATS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Messages Claude asked to send (whatsapp_send), waiting to land in their chat's composer.
static DRAFTS: std::sync::Mutex<Vec<whatsapp::ipc::SendRequest>> = std::sync::Mutex::new(Vec::new());

pub fn queue_draft(req: whatsapp::ipc::SendRequest) {
    if let Ok(mut v) = DRAFTS.lock() {
        v.retain(|r| r.chat != req.chat);
        v.push(req);
    }
}

pub fn is_chat_open(jid: &str) -> bool {
    OPEN_CHATS.lock().map(|v| v.iter().any(|j| j == jid)).unwrap_or(false)
}

pub enum ChatEvent {
    /// Open a downloaded file (image, video, document) in its own tab.
    OpenFile(PathBuf),
    /// Name/picture changed: the tab title should refresh.
    TitleChanged,
    /// First real action in the chat (typing, sending, recording, attaching, reacting, asking
    /// Claude): its preview tab becomes a normal tab. Reading, scrolling or clicking around doesn't.
    Interacted,
    /// "Pedir a Claude…" from the header: open a Claude Code session about this chat.
    AskClaude { jid: String, instruction: String },
    /// Open the 1:1 chat with someone (group member in the info sheet).
    OpenChat { jid: String, name: String },
}

#[derive(Clone)]
enum Item {
    Day(i64),
    Msg(Rc<ChatMessage>),
}

fn signature(m: &ChatMessage) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    m.id.hash(&mut h);
    m.text.hash(&mut h);
    m.kind.as_str().hash(&mut h);
    (m.status as u8).hash(&mut h);
    m.media.is_some().hash(&mut h);
    m.edited.hash(&mut h);
    m.forwarded.hash(&mut h);
    m.transcript.hash(&mut h);
    for r in &m.reactions {
        r.emoji.hash(&mut h);
        r.sender.hash(&mut h);
    }
    h.finish()
}

/// Popover for a phone number clicked in a message: checks WhatsApp and offers to open the chat.
struct PhonePop {
    position: Point<Pixels>,
    label: String,
    digits: String,
    state: PhoneState,
}

enum PhoneState {
    Checking,
    Found(String),
    Missing,
}

struct Menu {
    position: Point<Pixels>,
    msg: Rc<ChatMessage>,
    /// "Eliminar…" was picked: the menu shows the delete options instead.
    confirm_delete: bool,
}

/// "Reenviar…": pick the chats to forward a message to.
struct Forward {
    msg: Rc<ChatMessage>,
    input: Entity<TextInput>,
    chats: Vec<whatsapp::ChatSummary>,
    picked: Vec<String>,
    _sub: Subscription,
}

/// Text selected with the mouse inside one message bubble (byte offsets into `text`).
struct TextSel {
    id: String,
    text: String,
    layout: gpui::TextLayout,
    anchor: usize,
    head: usize,
}

impl TextSel {
    fn range(&self) -> std::ops::Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    fn selected(&self) -> Option<&str> {
        let r = self.range();
        (!r.is_empty()).then(|| &self.text[r])
    }

    fn index_at(&self, p: Point<Pixels>) -> usize {
        char_floor(&self.text, match self.layout.index_for_position(p) {
            Ok(i) | Err(i) => i,
        })
    }
}

pub struct ChatView {
    pub jid: String,
    pub name: String,
    pub avatar: Option<PathBuf>,
    /// Unread messages (shown on the tab while it's in the background).
    pub unread: u32,
    is_group: bool,
    focus: FocusHandle,
    items: Vec<Item>,
    sigs: Vec<u64>,
    list: ListState,
    oldest: Option<i64>,
    has_more: bool,
    composer: Entity<TextArea>,
    attachments: Vec<PathBuf>,
    reply_to: Option<Rc<ChatMessage>>,
    menu: Option<Menu>,
    forward: Option<Forward>,
    stickers_open: bool,
    picker_tab: PickerTab,
    typing: Option<(Option<String>, Instant)>,
    last_typing_sent: Option<Instant>,
    unread_pending: bool,
    /// Messages whose file should open once downloaded.
    open_after: HashSet<String>,
    /// Voice/audio to start once downloaded.
    play_after: Option<String>,
    player: Option<(String, crate::voice::Player)>,
    speed: f32,
    waveforms: HashMap<String, Vec<u8>>,
    /// Waveforms being computed from downloaded audio.
    wave_pending: HashSet<String>,
    /// Voice notes whose long transcription is shown in full.
    transcript_open: HashSet<String>,
    /// Claude is writing a suggested reply (see `wa_reply`).
    drafting: Option<Task<()>>,
    /// Message under the mouse (shows its reaction button).
    hovered_msg: Option<String>,
    /// Media already queued with `WhatsApp::prefetch` (once per message).
    prefetched: HashSet<String>,
    /// Text selected in a bubble (⌘C / "Copiar selección").
    sel: Option<TextSel>,
    /// The left button is down and dragging extends `sel`.
    selecting: bool,
    /// Swipe-to-reply in progress: (message id, how far the bubble moved right).
    swipe: Option<(String, f32)>,
    /// Mouse drag that may become a swipe: (message id, x where it started).
    drag: Option<(Rc<ChatMessage>, Pixels)>,
    /// Ends a trackpad swipe when the gesture stops sending events (no "ended" phase).
    _swipe_end: Option<Task<()>>,
    /// Quick-reactions bar opened from a message's reaction button.
    react_bar: Option<(Rc<ChatMessage>, Point<Pixels>)>,
    phone_pop: Option<PhonePop>,
    /// The quick-reactions bar shows the full emoji grid ("+").
    react_more: bool,
    /// Acted on (see `ChatEvent::Interacted`).
    interacted: bool,
    thumbs: HashMap<String, Option<PathBuf>>,
    /// Photos as shown in bubbles: the original when small, else a ≤720 px copy (see `preview`).
    previews: HashMap<String, Option<PathBuf>>,
    /// This chat's decoded images (RAM + GPU atlas), freed when the chat closes. gpui's global
    /// cache never evicts, so photos seen in old chats would pile up for the whole session.
    images: Entity<gpui::RetainAllImageCache>,
    /// Measured height of the floating composer (bottom padding of the message list).
    composer_h: Rc<std::cell::Cell<f32>>,
    recorder: Option<crate::voice::Recorder>,
    error: Option<SharedString>,
    reload_pending: bool,
    search: Option<Search>,
    info: Option<Info>,
    /// Profile picture shown full screen.
    zoom: Option<PathBuf>,
    /// "Pedir a Claude…" popover: instruction input.
    ask: Option<(Entity<TextInput>, Subscription)>,
    _listen: Task<()>,
    _tick: Option<Task<()>>,
    _reload: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ChatEvent> for ChatView {}

impl Drop for ChatView {
    fn drop(&mut self) {
        if let Ok(mut v) = OPEN_CHATS.lock()
            && let Some(i) = v.iter().position(|j| *j == self.jid)
        {
            v.remove(i);
        }
    }
}

impl ChatView {
    pub fn new(jid: String, name: String, cx: &mut Context<Self>) -> Self {
        let wa = WhatsApp::global();
        let mut rx = wa.subscribe();
        let listen = cx.spawn(async move |this, cx| {
            while let Some(u) = rx.next().await {
                if this.update(cx, |v, cx| v.on_update(u, cx)).is_err() {
                    break;
                }
            }
        });
        let composer = cx.new(|cx| {
            let mut t = TextArea::new("Escribe un mensaje", 1, 8, cx);
            t.submit_on_enter = true;
            t
        });
        let sub = cx.subscribe(&composer, |this, _, e: &TextAreaEvent, cx| match e {
            TextAreaEvent::Submit => this.send(cx),
            TextAreaEvent::Changed => this.composing(cx),
            TextAreaEvent::Cancel => {
                this.info = None;
                this.reply_to = None;
                this.stickers_open = false;
                cx.notify();
            }
            TextAreaEvent::PastedImage(p) => {
                this.attachments.push(p.clone());
                cx.notify();
            }
        });
        let list = ListState::new(0, ListAlignment::Bottom, px(600.));
        let weak = cx.entity().downgrade();
        list.set_scroll_handler(move |ev, _, cx| {
            if ev.visible_range.start <= 2 {
                // Called while the list state is borrowed: splice after this event.
                let weak = weak.clone();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| this.load_older(cx));
                });
            }
        });
        if let Ok(mut v) = OPEN_CHATS.lock() {
            v.push(jid.clone());
        }
        let summary = wa.store().chat(&jid);
        let mut this = Self {
            avatar: summary.as_ref().and_then(|s| s.avatar.clone()),
            unread: summary.as_ref().map(|s| s.unread).unwrap_or(0),
            is_group: jid.ends_with("@g.us"),
            name: summary.map(|s| s.name).unwrap_or(name),
            jid,
            focus: cx.focus_handle(),
            items: Vec::new(),
            sigs: Vec::new(),
            list,
            oldest: None,
            has_more: true,
            composer,
            attachments: Vec::new(),
            reply_to: None,
            menu: None,
            forward: None,
            stickers_open: false,
            picker_tab: PickerTab::Emojis,
            typing: None,
            last_typing_sent: None,
            unread_pending: true,
            open_after: HashSet::new(),
            play_after: None,
            player: None,
            speed: 1.0,
            waveforms: HashMap::new(),
            wave_pending: HashSet::new(),
            transcript_open: HashSet::new(),
            drafting: None,
            interacted: false,
            hovered_msg: None,
            sel: None,
            selecting: false,
            prefetched: HashSet::new(),
            swipe: None,
            drag: None,
            _swipe_end: None,
            react_bar: None,
            phone_pop: None,
            react_more: false,
            thumbs: HashMap::new(),
            previews: HashMap::new(),
            images: gpui::RetainAllImageCache::new(cx),
            composer_h: Rc::new(std::cell::Cell::new(64.)),
            recorder: None,
            error: None,
            reload_pending: false,
            search: None,
            info: None,
            zoom: None,
            ask: None,
            _listen: listen,
            _tick: None,
            _reload: None,
            _subs: vec![sub],
        };
        this.reload(cx);
        this
    }

    /// Move a queued Claude draft for this chat into the composer (text, files, quoted message).
    pub fn take_draft(&mut self, cx: &mut Context<Self>) {
        let Some(req) = DRAFTS.lock().ok().and_then(|mut v| v.iter().position(|r| r.chat == self.jid).map(|i| v.remove(i))) else { return };
        self.composer.update(cx, |c, cx| c.set_text(&req.text, cx));
        self.attachments = req.files;
        if let Some(q) = req.quote {
            self.reply_to = self
                .items
                .iter()
                .find_map(|i| match i {
                    Item::Msg(m) if m.id == q => Some(m.clone()),
                    _ => None,
                })
                .or_else(|| WhatsApp::global().store().message(&self.jid, &q).map(Rc::new));
        }
        self.stickers_open = false;
        cx.notify();
    }

    pub fn composer_focus(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }

    fn on_update(&mut self, u: Update, cx: &mut Context<Self>) {
        match u {
            Update::Messages(c) if c == self.jid => {
                self.unread_pending = true;
                // The info sheet refreshes with the (debounced) reload: a prefetch of many photos
                // would otherwise re-query it once per download.
                self.schedule_reload(cx);
            }
            Update::SendFailed { chat, error } if chat == self.jid => {
                self.error = Some(format!("No se pudo enviar: {error}").into());
                cx.notify();
            }
            Update::Typing { chat, who } if chat == self.jid => {
                self.typing = who.map(|w| (Some(w).filter(|w| !w.is_empty()), Instant::now()));
                cx.notify();
            }
            Update::Avatar(c) if c == self.jid => {
                self.avatar = WhatsApp::global().store().chat(&self.jid).and_then(|s| s.avatar);
                let full = WhatsApp::global().full_avatar_path(&self.jid);
                if self.zoom.is_some() && full.exists() {
                    self.zoom = Some(full);
                }
                cx.emit(ChatEvent::TitleChanged);
                cx.notify();
            }
            Update::Chats => {
                if let Some(s) = WhatsApp::global().store().chat(&self.jid)
                    && (s.name != self.name || s.unread != self.unread || s.avatar != self.avatar)
                {
                    self.name = s.name;
                    self.unread = s.unread;
                    self.avatar = s.avatar;
                    cx.emit(ChatEvent::TitleChanged);
                    cx.notify();
                }
            }
            Update::Group(c) if c == self.jid => {
                if let Some(info) = &mut self.info {
                    info.members = WhatsApp::global().group_members(&self.jid);
                    cx.notify();
                }
            }
            // A group participant's (or our own) picture: used next to voice notes.
            Update::Avatar(_) => cx.notify(),
            _ => {}
        }
    }

    fn schedule_reload(&mut self, cx: &mut Context<Self>) {
        if self.reload_pending {
            return;
        }
        self.reload_pending = true;
        self._reload = Some(cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(60)).await;
            let _ = this.update(cx, |this, cx| {
                this.reload_pending = false;
                this.reload(cx);
            });
        }));
    }

    fn build_items(msgs: &[ChatMessage]) -> Vec<Item> {
        let mut items = Vec::with_capacity(msgs.len() + 8);
        let mut last: Option<i64> = None;
        for m in msgs {
            if last.is_none_or(|l| !wa_ui::same_day(l, m.ts)) {
                items.push(Item::Day(m.ts));
            }
            last = Some(m.ts);
            items.push(Item::Msg(Rc::new(m.clone())));
        }
        items
    }

    fn item_sig(i: &Item) -> u64 {
        match i {
            Item::Day(ts) => *ts as u64 ^ 0xDA7,
            Item::Msg(m) => signature(m),
        }
    }

    /// Reload everything from the oldest loaded message on; only changed rows are re-measured.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let store = WhatsApp::global().store();
        let msgs = match self.oldest {
            Some(oldest) => store.messages_from(&self.jid, oldest),
            None => store.messages(&self.jid, None, PAGE),
        };
        if msgs.len() < PAGE && self.oldest.is_none() {
            self.has_more = false;
        }
        self.oldest = msgs.first().map(|m| m.ts);
        let items = Self::build_items(&msgs);
        let sigs: Vec<u64> = items.iter().map(Self::item_sig).collect();
        let first_diff = self.sigs.iter().zip(&sigs).position(|(a, b)| a != b).unwrap_or(self.sigs.len().min(sigs.len()));
        let old_len = self.sigs.len();
        if old_len == 0 {
            self.list.reset(items.len());
        } else if first_diff < old_len || sigs.len() != old_len {
            self.list.splice(first_diff..old_len, sigs.len() - first_diff);
        }
        self.items = items;
        self.sigs = sigs;
        if self.info.is_some() {
            self.load_info(cx);
        }
        self.prefetch_media();
        // Files the user asked to open as soon as they're downloaded.
        let ready: Vec<PathBuf> = self
            .open_after
            .iter()
            .filter_map(|id| self.find(id).and_then(|m| m.media.clone()))
            .collect();
        if !ready.is_empty() {
            let pending: HashSet<String> = self.open_after.iter().filter(|id| self.find(id).is_none_or(|m| m.media.is_none())).cloned().collect();
            self.open_after = pending;
            for p in ready {
                cx.emit(ChatEvent::OpenFile(p));
            }
        }
        if let Some(id) = self.play_after.clone()
            && let Some(m) = self.find(&id).filter(|m| m.media.is_some())
        {
            self.play_after = None;
            self.toggle_play(&m, cx);
        }
        cx.notify();
    }

    /// Queue the photos/stickers of the loaded messages that aren't downloaded yet, newest
    /// first, so they're there before they scroll into view.
    fn prefetch_media(&mut self) {
        let ids: Vec<String> = self
            .items
            .iter()
            .rev()
            .filter_map(|i| match i {
                Item::Msg(m) if m.media.is_none() && matches!(m.kind, MsgKind::Image | MsgKind::Sticker) => Some(m.id.clone()),
                _ => None,
            })
            .filter(|id| self.prefetched.insert(id.clone()))
            .collect();
        if !ids.is_empty() {
            WhatsApp::global().prefetch(&self.jid, ids);
        }
    }

    fn find(&self, id: &str) -> Option<Rc<ChatMessage>> {
        self.items.iter().rev().find_map(|i| match i {
            Item::Msg(m) if m.id == id => Some(m.clone()),
            _ => None,
        })
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        if !self.has_more {
            return;
        }
        let store = WhatsApp::global().store();
        let older = store.messages(&self.jid, self.oldest, PAGE);
        if older.len() < PAGE {
            self.has_more = false;
        }
        if older.is_empty() {
            return;
        }
        let top = self.list.logical_scroll_top();
        let before = self.items.len();
        self.oldest = older.first().map(|m| m.ts);
        self.reload(cx);
        let added = self.items.len().saturating_sub(before);
        self.list.scroll_to(ListOffset { item_ix: top.item_ix + added, offset_in_item: top.offset_in_item });
    }

    fn interacted(&mut self, cx: &mut Context<Self>) {
        if !self.interacted {
            self.interacted = true;
            cx.emit(ChatEvent::Interacted);
        }
    }

    fn composing(&mut self, cx: &mut Context<Self>) {
        let empty = self.composer.read(cx).content.is_empty();
        if !empty {
            self.interacted(cx);
        }
        if !empty && self.last_typing_sent.is_none_or(|t| t.elapsed() > Duration::from_secs(8)) {
            self.last_typing_sent = Some(Instant::now());
            WhatsApp::global().typing(&self.jid, true);
        }
        cx.notify();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        self.interacted(cx);
        let text = self.composer.read(cx).content.trim().to_string();
        let wa = WhatsApp::global();
        let quote = self.reply_to.as_ref().map(|m| m.id.clone());
        if !self.attachments.is_empty() {
            let files = std::mem::take(&mut self.attachments);
            wa.send_files(&self.jid, files, &text, quote.as_deref());
        } else if !text.is_empty() {
            wa.send_text(&self.jid, &text, quote.as_deref());
        } else {
            return;
        }
        if self.last_typing_sent.take().is_some() {
            wa.typing(&self.jid, false);
        }
        self.reply_to = None;
        self.composer.update(cx, |c, cx| c.set_text("", cx));
        self.list.scroll_to(ListOffset { item_ix: self.items.len(), offset_in_item: px(0.) });
        cx.notify();
    }

    fn attach(&mut self, cx: &mut Context<Self>) {
        self.interacted(cx);
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Adjuntar".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.attachments.extend(paths);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ---- search ----

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = &self.search {
            let input = s.input.clone();
            input.update(cx, |i, cx| i.select_all_text(cx));
            window.focus(&input.focus_handle(cx));
            return;
        }
        let input = cx.new(|cx| TextInput::new("Buscar en el chat", cx));
        let sub = cx.subscribe(&input, |this, _, e: &TextInputEvent, cx| match e {
            TextInputEvent::Changed => this.run_search(cx),
            TextInputEvent::Submit { shift } => this.step_search(!*shift, cx),
            TextInputEvent::Cancel => this.close_search(cx),
        });
        window.focus(&input.focus_handle(cx));
        self.search = Some(Search { input, query: String::new(), hits: Vec::new(), current: 0, _sub: sub });
        cx.notify();
    }

    fn toggle_ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ask.take().is_some() {
            cx.notify();
            return;
        }
        let input = cx.new(|cx| TextInput::new("¿Qué hago con este chat? (resumir, crear tarea, investigar…)", cx));
        let sub = cx.subscribe(&input, |this, input, e: &TextInputEvent, cx| match e {
            TextInputEvent::Submit { .. } => {
                let instruction = input.read(cx).content.trim().to_string();
                if !instruction.is_empty() {
                    this.interacted(cx);
                    cx.emit(ChatEvent::AskClaude { jid: this.jid.clone(), instruction });
                }
                this.ask = None;
                cx.notify();
            }
            TextInputEvent::Cancel => {
                this.ask = None;
                cx.notify();
            }
            TextInputEvent::Changed => {}
        });
        window.focus(&input.focus_handle(cx));
        self.ask = Some((input, sub));
        cx.notify();
    }

    fn open_forward(&mut self, msg: Rc<ChatMessage>, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new("Buscar chat", cx));
        let sub = cx.subscribe(&input, |this, _, e: &TextInputEvent, cx| match e {
            TextInputEvent::Changed => cx.notify(),
            TextInputEvent::Submit { .. } => this.send_forward(cx),
            TextInputEvent::Cancel => {
                this.forward = None;
                cx.notify();
            }
        });
        window.focus(&input.focus_handle(cx));
        let chats = WhatsApp::global().store().chats();
        self.forward = Some(Forward { msg, input, chats, picked: Vec::new(), _sub: sub });
        cx.notify();
    }

    fn send_forward(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.forward.take() else { return };
        if !f.picked.is_empty() {
            WhatsApp::global().forward(&f.msg.chat, &f.msg.id, f.picked);
            self.interacted(cx);
        }
        cx.notify();
    }

    fn render_forward(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.forward.as_ref()?;
        let q = f.input.read(cx).content.trim().to_lowercase();
        let rows: Vec<&whatsapp::ChatSummary> = f.chats.iter().filter(|c| q.is_empty() || c.name.to_lowercase().contains(&q) || c.jid.contains(&q)).take(80).collect();
        let n = f.picked.len();
        let list = div().id("fwd-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().children(rows.into_iter().enumerate().map(|(i, c)| {
            let on = f.picked.contains(&c.jid);
            let jid = c.jid.clone();
            div()
                .id(("fwd-chat", i))
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                .child(wa_ui::avatar(c.avatar.as_ref(), &c.name, 32.))
                .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).child(c.name.clone()))
                .child(
                    div()
                        .size(px(18.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_1()
                        .border_color(theme::rgb(if on { wa_ui::GREEN } else { theme::BORDER }))
                        .when(on, |d| d.bg(theme::rgb(wa_ui::GREEN)).child(wa_ui::icon("icons/wa-check.svg", 11., 0xffffff))),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(f) = this.forward.as_mut() {
                        match f.picked.iter().position(|j| *j == jid) {
                            Some(i) => {
                                f.picked.remove(i);
                            }
                            None => f.picked.push(jid.clone()),
                        }
                    }
                    cx.notify();
                }))
        }));
        Some(
            div()
                .id("fwd-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.forward = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("fwd-panel")
                        .w(px(380.))
                        .h(px(480.))
                        .p(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .rounded(px(10.))
                        .bg(theme::rgb(theme::SIDEBAR_BG))
                        .border_1()
                        .border_color(theme::rgb(theme::BORDER))
                        .shadow_lg()
                        // Clicks inside don't close it.
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(div().px(px(4.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("REENVIAR MENSAJE"))
                        .child(div().h(px(30.)).px(px(12.)).flex().items_center().rounded(px(15.)).glass().text_size(px(13.)).child(f.input.clone()))
                        .child(list)
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(8.))
                                .pt(px(4.))
                                .child(
                                    div()
                                        .id("fwd-cancel")
                                        .px(px(12.))
                                        .py(px(6.))
                                        .rounded(px(6.))
                                        .text_size(px(13.))
                                        .text_color(theme::rgb(theme::TEXT_MUTED))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                        .child("Cancelar")
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.forward = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("fwd-send")
                                        .px(px(12.))
                                        .py(px(6.))
                                        .rounded(px(6.))
                                        .text_size(px(13.))
                                        .text_color(gpui::white())
                                        .bg(theme::rgb(wa_ui::GREEN))
                                        .when(n == 0, |d| d.opacity(0.4))
                                        .when(n > 0, |d| d.cursor_pointer().hover(|s| s.opacity(0.9)))
                                        .child(if n > 1 { format!("Reenviar a {n}") } else { "Reenviar".to_string() })
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.send_forward(cx))),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_ask(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (input, _) = self.ask.as_ref()?;
        Some(
            div()
                .absolute()
                .top(px(64.))
                .right(px(16.))
                .w(px(420.))
                .p(px(8.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .rounded(px(10.))
                .bg(theme::rgb(theme::SIDEBAR_BG))
                .border_1()
                .border_color(theme::rgb(theme::BORDER))
                .shadow_lg()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.ask = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(4.))
                        .text_size(px(11.))
                        .text_color(theme::rgb(theme::TEXT_FAINT))
                        .child(wa_ui::icon("icons/wa-spark.svg", 12., 0xd97757))
                        .child("PEDIR A CLAUDE"),
                )
                .child(div().h(px(30.)).px(px(12.)).flex().items_center().rounded(px(15.)).glass().text_size(px(13.)).child(input.clone()))
                .child(div().px(px(4.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Enter abre Claude Code con este chat (y el proyecto de su etiqueta) como contexto"))
                .into_any_element(),
        )
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search = None;
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &mut self.search else { return };
        let q = s.input.read(cx).content.trim().to_string();
        if q == s.query {
            return;
        }
        s.query = q.clone();
        s.hits = if q.is_empty() { Vec::new() } else { WhatsApp::global().store().search(&q, Some(&self.jid), 1000) };
        s.current = 0;
        if !s.hits.is_empty() {
            self.jump_to_hit(cx);
        }
        cx.notify();
    }

    /// `older`: Enter / ↑ walks back in time; Shift-Enter / ↓ forward.
    fn step_search(&mut self, older: bool, cx: &mut Context<Self>) {
        let Some(s) = &mut self.search else { return };
        let n = s.hits.len();
        if n == 0 {
            return;
        }
        s.current = if older { (s.current + 1) % n } else { (s.current + n - 1) % n };
        self.jump_to_hit(cx);
        cx.notify();
    }

    fn jump_to_hit(&mut self, cx: &mut Context<Self>) {
        let Some(hit) = self.search.as_ref().and_then(|s| s.hits.get(s.current)).cloned() else { return };
        self.jump_to(&hit.id, hit.ts, cx);
    }

    /// Scroll to a message, loading older history first when it isn't loaded yet.
    fn jump_to(&mut self, id: &str, ts: i64, cx: &mut Context<Self>) {
        if self.oldest.is_none_or(|o| ts < o) {
            self.oldest = Some(ts);
            self.reload(cx);
        }
        if let Some(ix) = self.items.iter().position(|i| matches!(i, Item::Msg(m) if m.id == id)) {
            // A little context above the match.
            self.list.scroll_to(ListOffset { item_ix: ix.saturating_sub(1), offset_in_item: px(0.) });
        }
    }

    fn render_search(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.search.as_ref()?;
        let n = s.hits.len();
        let label = if s.query.is_empty() {
            String::new()
        } else if n == 0 {
            "Sin resultados".into()
        } else {
            format!("{} de {n}", s.current + 1)
        };
        let arrow = |id: &'static str, icon: &'static str, older: bool| {
            div()
                .id(id)
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|st| st.bg(theme::rgb(theme::HOVER)))
                .child(gpui::svg().path(icon).size(px(14.)).text_color(theme::rgb(if n > 0 { theme::TEXT } else { theme::TEXT_FAINT })))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.step_search(older, cx)))
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(14.))
                .py(px(6.))
                .flex_none()
                .bg(theme::rgb(theme::SIDEBAR_BG))
                .border_b_1()
                .border_color(theme::rgb(theme::BORDER))
                .child(gpui::svg().path("icons/search.svg").size(px(14.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                .child(div().flex_1().h(px(28.)).px(px(12.)).flex().items_center().rounded(px(14.)).glass().text_size(px(13.)).child(s.input.clone()))
                .child(div().flex_none().min_w(px(70.)).text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_MUTED)).child(label))
                .child(arrow("search-up", "icons/chevron-up.svg", true))
                .child(arrow("search-down", "icons/chevron-down.svg", false))
                .child(
                    div()
                        .id("search-close")
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|st| st.bg(theme::rgb(theme::HOVER)))
                        .child(gpui::svg().path("icons/close.svg").size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_search(cx))),
                )
                .into_any_element(),
        )
    }

    // ---- contact info ----

    fn toggle_info(&mut self, cx: &mut Context<Self>) {
        if self.info.take().is_none() {
            let wa = WhatsApp::global();
            let members = if self.is_group { wa.group_members(&self.jid) } else { None };
            let tab = if self.is_group { InfoTab::Members } else { InfoTab::Media };
            self.info = Some(Info { tab, media: Vec::new(), docs: Vec::new(), links: Vec::new(), members, loaded: false, copied: None });
            self.load_info(cx);
            if self.is_group {
                // Refreshed every time (people join/leave); the cached list shows meanwhile.
                wa.fetch_group_members(&self.jid);
            } else {
                wa.fetch_full_avatar(&self.jid);
            }
        }
        cx.notify();
    }

    fn load_info(&mut self, cx: &mut Context<Self>) {
        let jid = self.jid.clone();
        let task = cx.background_executor().spawn(async move {
            let store = WhatsApp::global().store();
            (
                store.chat_media(&jid, &[MsgKind::Image, MsgKind::Video], 300),
                store.chat_media(&jid, &[MsgKind::Document, MsgKind::Audio], 300),
                store.chat_links(&jid, 300),
            )
        });
        cx.spawn(async move |this, cx| {
            let (media, docs, links) = task.await;
            let wrap = |v: Vec<ChatMessage>| v.into_iter().map(Rc::new).collect::<Vec<_>>();
            let (media, docs, links) = (wrap(media), wrap(docs), wrap(links));
            let _ = this.update(cx, |this, cx| {
                let missing: Vec<String> = media.iter().filter(|m| m.media.is_none() && m.kind == MsgKind::Image).map(|m| m.id.clone()).filter(|id| this.prefetched.insert(id.clone())).collect();
                if !missing.is_empty() {
                    WhatsApp::global().prefetch(&this.jid, missing);
                }
                if let Some(info) = &mut this.info {
                    info.media = media;
                    info.docs = docs;
                    info.links = links;
                    info.loaded = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Phone number (or WhatsApp id) of a 1:1 chat.
    fn phone(&self) -> Option<String> {
        if self.is_group {
            return None;
        }
        let canon = WhatsApp::global().store().canonical(&self.jid);
        Some(whatsapp::store::phone_label(&canon))
    }

    fn render_info(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let info = self.info.as_ref()?;
        let tab = info.tab;
        let copied = info.copied.is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
        let (n_media, n_docs, n_links, loaded) = (info.media.len(), info.docs.len(), info.links.len(), info.loaded);
        let members = info.members.clone();
        let n_members = members.as_ref().map_or(0, Vec::len);
        let items: Vec<Rc<ChatMessage>> = match tab {
            InfoTab::Members => Vec::new(),
            InfoTab::Media => info.media.clone(),
            InfoTab::Docs => info.docs.clone(),
            InfoTab::Links => info.links.clone(),
        };
        let wa = WhatsApp::global();
        let full = wa.full_avatar_path(&self.jid);
        let big = if full.exists() { Some(full) } else { self.avatar.clone() };
        let (contact, push) = if self.is_group { (None, None) } else { wa.store().contact_names(&wa.store().canonical(&self.jid)) };
        let phone = self.phone();
        let is_lid = self.jid.ends_with("@lid") && wa.store().canonical(&self.jid).ends_with("@lid");
        let push_label = push.filter(|p| contact.as_deref() != Some(p.as_str()) && self.name.trim_start_matches('~') != p.as_str());

        let photo = {
            let big2 = big.clone();
            div()
                .id("info-photo")
                .size(px(132.))
                .rounded_full()
                .overflow_hidden()
                .border_2()
                .border_color(crate::glass::edge())
                .shadow(crate::glass::shadow())
                .when(big.is_some(), |d| d.cursor_pointer().hover(|s| s.opacity(0.9)))
                .child(wa_ui::avatar(big.as_ref(), &self.name, 132.))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(p) = big2.clone() {
                        this.zoom = Some(p);
                        cx.notify();
                    }
                }))
        };
        let copy_row = phone.map(|ph| {
            let ph2 = ph.clone();
            div()
                .id("info-copy")
                .flex()
                .items_center()
                .gap(px(8.))
                .mt(px(2.))
                .px(px(12.))
                .py(px(4.))
                .rounded_full()
                .cursor_pointer()
                .glass_button()
                .hover(|s| s.glass_hover())
                .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(if is_lid { format!("Usuario de WhatsApp · {ph}") } else { ph }))
                .child(if copied {
                    div().text_size(px(11.5)).text_color(theme::rgb(wa_ui::GREEN)).child("Copiado").into_any_element()
                } else {
                    gpui::svg().path("icons/copy.svg").size(px(14.)).text_color(theme::rgb(theme::TEXT_FAINT)).into_any_element()
                })
                .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Copiar")).into())
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(ph2.clone()));
                    if let Some(i) = &mut this.info {
                        i.copied = Some(Instant::now());
                    }
                    cx.notify();
                    cx.spawn(async move |this, cx| {
                        Timer::after(Duration::from_millis(1600)).await;
                        let _ = this.update(cx, |_, cx| cx.notify());
                    })
                    .detach();
                }))
        });
        // Resolved up front: the grid's closure can't borrow `self`/`cx` mutably.
        let media_srcs: Vec<Option<PathBuf>> = if tab == InfoTab::Media {
            items.iter().map(|m| if m.kind == MsgKind::Image { self.preview(m, cx) } else { None }.or_else(|| self.thumb(m))).collect()
        } else {
            Vec::new()
        };
        let tab_btn = |id: &'static str, label: &'static str, n: usize, t: InfoTab| {
            let active = tab == t;
            div()
                .id(id)
                .flex_1()
                .flex()
                .justify_center()
                .gap(px(5.))
                .py(px(5.))
                .rounded_full()
                .text_size(px(12.5))
                .cursor_pointer()
                .text_color(theme::rgb(if active { theme::TEXT } else { theme::TEXT_MUTED }))
                .when(active, |d| d.glass_button())
                .when(!active, |d| d.hover(|s| s.text_color(theme::rgb(theme::TEXT))))
                .child(label)
                .when(n > 0, |d| d.child(div().text_color(theme::rgb(theme::TEXT_FAINT)).child(n.to_string())))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(i) = &mut this.info {
                        i.tab = t;
                    }
                    cx.notify();
                }))
        };
        let empty = |text: &'static str| div().py(px(28.)).text_center().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(text).into_any_element();
        let body: AnyElement = if tab == InfoTab::Members {
            match members {
                None => empty("Cargando integrantes…"),
                Some(ms) => div()
                    .flex()
                    .flex_col()
                    .py(px(4.))
                    .children(ms.into_iter().enumerate().map(|(i, m)| {
                        let avatar = if m.me { wa.person_avatar(whatsapp::service::ME) } else { wa.person_avatar(&m.jid) };
                        let title = if m.me { "Tú".to_string() } else { m.name.trim_start_matches('~').to_string() };
                        let sub = if m.me || m.name == m.phone { None } else { Some(if m.name.starts_with('~') { format!("{} · {}", m.phone, m.name) } else { m.phone.clone() }) };
                        let (jid, name) = (m.jid.clone(), m.name.clone());
                        div()
                            .id(("info-member", i))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(14.))
                            .py(px(7.))
                            .when(!m.me, |d| d.cursor_pointer().hover(|s| s.bg(theme::rgb(theme::HOVER))))
                            .child(wa_ui::avatar(avatar.as_ref(), &title, 36.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().text_size(px(13.5)).text_color(theme::rgb(theme::TEXT)).child(title))
                                    .children(sub.map(|s| div().truncate().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(s))),
                            )
                            .when(m.admin, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .px(px(6.))
                                        .py(px(1.))
                                        .rounded(px(4.))
                                        .text_size(px(10.5))
                                        .text_color(theme::rgb(wa_ui::GREEN))
                                        .bg(theme::rgb(wa_ui::GREEN).opacity(0.12))
                                        .child("Admin"),
                                )
                            })
                            .when(!m.me, |d| {
                                d.tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Abrir chat")).into())
                                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(ChatEvent::OpenChat { jid: jid.clone(), name: name.clone() })))
                            })
                    }))
                    .into_any_element(),
            }
        } else if !loaded {
            empty("Cargando…")
        } else {
            match tab {
                InfoTab::Members => unreachable!(),
                InfoTab::Media if items.is_empty() => empty("No hay fotos ni videos."),
                InfoTab::Docs if items.is_empty() => empty("No hay documentos."),
                InfoTab::Links if items.is_empty() => empty("No hay enlaces."),
                InfoTab::Media => div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .px(px(16.))
                    .py(px(10.))
                    .children(items.iter().enumerate().map(|(i, m)| {
                        let src = media_srcs.get(i).cloned().flatten();
                        let msg = m.clone();
                        div()
                            .id(("info-media", i))
                            .relative()
                            .size(px(124.))
                            .rounded(px(10.))
                            .overflow_hidden()
                            .bg(theme::rgb(theme::SELECTED))
                            .cursor_pointer()
                            .children(src.map(|p| gpui::img(p).size_full().object_fit(gpui::ObjectFit::Cover)))
                            .when(m.kind == MsgKind::Video, |d| {
                                d.child(
                                    div()
                                        .absolute()
                                        .bottom(px(4.))
                                        .left(px(5.))
                                        .flex()
                                        .items_center()
                                        .gap(px(3.))
                                        .text_size(px(10.5))
                                        .text_color(gpui::white())
                                        .child(gpui::svg().path("icons/play.svg").size(px(10.)).text_color(gpui::white()))
                                        .children(m.duration.map(wa_ui::duration)),
                                )
                            })
                            .hover(|s| s.opacity(0.85))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_media(&msg, cx)))
                    }))
                    .into_any_element(),
                InfoTab::Docs => div()
                    .flex()
                    .flex_col()
                    .children(items.iter().enumerate().map(|(i, m)| {
                        let name = m.file_name.clone().unwrap_or_else(|| if m.text.is_empty() { m.kind.label().to_string() } else { m.text.clone() });
                        let ext = std::path::Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase();
                        let size = m.media.as_ref().map(|p| wa_ui::file_size(p));
                        let detail = [Some(ext).filter(|e| !e.is_empty()), size, Some(wa_ui::list_time(m.ts))].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                        let msg = m.clone();
                        let (jump_id, jump_ts) = (m.id.clone(), m.ts);
                        div()
                            .id(("info-doc", i))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(14.))
                            .py(px(8.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                            .child(
                                div()
                                    .size(px(36.))
                                    .flex_none()
                                    .rounded(px(6.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(theme::rgb(theme::SELECTED))
                                    .child(gpui::svg().path(if m.kind == MsgKind::Audio { "icons/play.svg" } else { "icons/doc-file.svg" }).size(px(18.)).text_color(theme::rgb(theme::TEXT_MUTED))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).child(name))
                                    .child(div().truncate().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(detail)),
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_media(&msg, cx)))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    this.jump_to(&jump_id, jump_ts, cx);
                                    cx.notify();
                                }),
                            )
                    }))
                    .into_any_element(),
                InfoTab::Links => div()
                    .flex()
                    .flex_col()
                    .children(items.iter().enumerate().flat_map(|(i, m)| {
                        let urls: Vec<String> = find_links(&m.text).into_iter().map(|r| m.text[r].to_string()).collect();
                        let text = m.text.replace('\n', " ");
                        let ts = m.ts;
                        let (jump_id, jump_ts) = (m.id.clone(), m.ts);
                        let url = urls.first().cloned().unwrap_or_default();
                        let host = url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.").split('/').next().unwrap_or("").to_string();
                        Some(
                            div()
                                .id(("info-link", i))
                                .flex()
                                .gap(px(10.))
                                .px(px(14.))
                                .py(px(8.))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                .child(
                                    div()
                                        .size(px(36.))
                                        .flex_none()
                                        .rounded(px(6.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .bg(theme::rgb(theme::SELECTED))
                                        .child(gpui::svg().path("icons/link.svg").size(px(16.)).text_color(theme::rgb(theme::TEXT_MUTED))),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .gap(px(1.))
                                        .child(
                                            div()
                                                .flex()
                                                .gap(px(6.))
                                                .child(div().flex_1().min_w_0().truncate().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(host))
                                                .child(div().flex_none().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(wa_ui::list_time(ts))),
                                        )
                                        .child(div().truncate().text_size(px(11.5)).text_color(theme::rgb(theme::ACCENT)).child(url.clone()))
                                        .when(text.trim() != url, |d| d.child(div().text_size(px(11.5)).line_clamp(2).text_color(theme::rgb(theme::TEXT_MUTED)).child(text))),
                                )
                                .on_click(move |_: &ClickEvent, _, cx| {
                                    let u = if url.starts_with("http") { url.clone() } else { format!("https://{url}") };
                                    crate::music::open_url(&u, cx);
                                })
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                        this.jump_to(&jump_id, jump_ts, cx);
                                        cx.notify();
                                    }),
                                ),
                        )
                    }))
                    .into_any_element(),
            }
        };
        let title = if self.is_group { "Info. del grupo" } else { "Info. del contacto" };
        Some(
            div()
                .id("chat-info-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p(px(32.))
                .bg(gpui::hsla(0., 0., 0., 0.42))
                .occlude()
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.info = None;
                    cx.notify();
                }))
                .child(
                    // Solid-ish base so the chat doesn't bleed through, glass sheen on top.
                    div()
                        .id("chat-info")
                        .w(px(436.))
                        .max_w_full()
                        .h(px(640.))
                        .max_h_full()
                        .rounded(px(20.))
                        .overflow_hidden()
                        .bg(theme::rgb(theme::SIDEBAR_BG).opacity(0.96))
                        .border_1()
                        .border_color(crate::glass::edge())
                        .shadow(vec![
                            gpui::BoxShadow { color: gpui::hsla(0., 0., 0., 0.45), offset: gpui::point(px(0.), px(18.)), blur_radius: px(48.), spread_radius: px(0.) },
                            gpui::BoxShadow { color: gpui::hsla(0., 0., 0., 0.25), offset: gpui::point(px(0.), px(2.)), blur_radius: px(6.), spread_radius: px(0.) },
                        ])
                        // Clicks inside don't close it.
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .relative()
                                .size_full()
                                .flex()
                                .flex_col()
                                .bg(crate::glass::fill())
                                .child(
                                    div()
                                        .id("info-scroll")
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_y_scroll()
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .items_center()
                                                .gap(px(6.))
                                                .pt(px(40.))
                                                .pb(px(18.))
                                                .px(px(20.))
                                                .child(photo)
                                                .child(div().mt(px(10.)).text_center().text_size(px(20.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(self.name.trim_start_matches('~').to_string()))
                                                .children(copy_row)
                                                .children(push_label.map(|p| div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("~{p}"))))
                                                .when(self.is_group, |d| d.child(div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Grupo"))),
                                        )
                                        .child(
                                            div()
                                                .mx(px(16.))
                                                .p(px(3.))
                                                .flex()
                                                .gap(px(2.))
                                                .rounded_full()
                                                .bg(gpui::hsla(0., 0., 0., if theme::is_light() { 0.05 } else { 0.22 }))
                                                .border_1()
                                                .border_color(crate::glass::edge())
                                                .when(self.is_group, |d| d.child(tab_btn("info-tab-members", "Integrantes", n_members, InfoTab::Members)))
                                                .child(tab_btn("info-tab-media", "Multimedia", n_media, InfoTab::Media))
                                                .child(tab_btn("info-tab-docs", "Archivos", n_docs, InfoTab::Docs))
                                                .child(tab_btn("info-tab-links", "Enlaces", n_links, InfoTab::Links)),
                                        )
                                        .child(div().pt(px(4.)).pb(px(12.)).child(body)),
                                )
                                // Floating close button (top-right), over the scroll area.
                                .child(
                                    div()
                                        .id("info-close")
                                        .absolute()
                                        .top(px(12.))
                                        .right(px(12.))
                                        .size(px(30.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_full()
                                        .cursor_pointer()
                                        .glass_button()
                                        .hover(|s| s.glass_hover())
                                        .child(gpui::svg().path("icons/close.svg").size(px(11.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                                        .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip("Cerrar (Esc)")).into())
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.info = None;
                                            cx.notify();
                                        })),
                                )
                                .child(div().absolute().top(px(18.)).left(px(20.)).text_size(px(11.)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme::rgb(theme::TEXT_FAINT)).child(title.to_uppercase())),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_zoom(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.zoom.clone()?;
        Some(
            div()
                .id("avatar-zoom")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p(px(40.))
                .bg(gpui::black().opacity(0.88))
                .cursor_pointer()
                .occlude()
                .child(gpui::img(p).max_w_full().max_h_full().size(px(640.)).object_fit(gpui::ObjectFit::Contain))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.zoom = None;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    // ---- voice notes ----

    fn start_recording(&mut self, cx: &mut Context<Self>) {
        self.interacted(cx);
        match crate::voice::Recorder::start(&WhatsApp::global().dir().join("tmp")) {
            Ok(r) => {
                self.recorder = Some(r);
                self.error = None;
                WhatsApp::global().typing(&self.jid, true);
                self.start_tick(cx);
            }
            Err(e) => self.error = Some(e.into()),
        }
        cx.notify();
    }

    fn stop_recording(&mut self, send: bool, cx: &mut Context<Self>) {
        let Some(rec) = self.recorder.take() else { return };
        WhatsApp::global().typing(&self.jid, false);
        if !send {
            rec.cancel();
            cx.notify();
            return;
        }
        let out = WhatsApp::global().dir().join("tmp");
        match rec.finish(&out) {
            Ok((file, secs, wave)) => {
                let quote = self.reply_to.take().map(|m| m.id.clone());
                WhatsApp::global().send_voice(&self.jid, file, secs, wave, quote.as_deref());
            }
            Err(e) => self.error = Some(e.into()),
        }
        cx.notify();
    }

    /// Repaint while recording / playing (progress, timer).
    fn start_tick(&mut self, cx: &mut Context<Self>) {
        if self._tick.is_some() {
            return;
        }
        self._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(150)).await;
                let keep = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        let playing = this.player.as_ref().is_some_and(|(_, p)| p.is_playing());
                        if !playing && this.player.as_ref().is_some_and(|(_, p)| p.current() <= 0.01) {
                            // Finished: rewind to the start of the bubble.
                            this.player = None;
                        }
                        let keep = this.recorder.is_some() || playing;
                        if !keep {
                            this._tick = None;
                        }
                        keep
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    fn toggle_play(&mut self, m: &ChatMessage, cx: &mut Context<Self>) {
        if let Some((id, p)) = &self.player
            && *id == m.id
        {
            if p.is_playing() {
                p.pause();
            } else {
                p.play();
                self.start_tick(cx);
            }
            cx.notify();
            return;
        }
        let Some(media) = m.media.clone() else {
            self.play_after = Some(m.id.clone());
            WhatsApp::global().download(&m.chat, &m.id);
            cx.notify();
            return;
        };
        let id = m.id.clone();
        let task = cx.background_executor().spawn(async move {
            let wav = crate::voice::playable(&media)?;
            let wave = crate::voice::waveform_of(&wav, WAVE_BARS);
            Ok::<_, String>((wav, wave))
        });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            let _ = this.update(cx, |this, cx| match res {
                Ok((wav, wave)) => {
                    this.waveforms.insert(id.clone(), wave);
                    match crate::voice::Player::open(&wav) {
                        Some(p) => {
                            p.set_rate(this.speed);
                            p.play();
                            this.player = Some((id, p));
                            this.start_tick(cx);
                        }
                        None => this.error = Some("No se pudo reproducir el audio.".into()),
                    }
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(format!("No se pudo reproducir el audio: {e}").into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_media(&mut self, m: &ChatMessage, cx: &mut Context<Self>) {
        match &m.media {
            Some(p) => cx.emit(ChatEvent::OpenFile(p.clone())),
            None => {
                self.open_after.insert(m.id.clone());
                WhatsApp::global().download(&m.chat, &m.id);
                cx.notify();
            }
        }
    }

    /// Bubble-sized copy of a downloaded photo: the original if it's already small, else a
    /// ≤720 px JPEG made once in the background (a 12 MP photo decodes to ~48 MB otherwise).
    /// None while it's being made.
    fn preview(&mut self, m: &ChatMessage, cx: &mut Context<Self>) -> Option<PathBuf> {
        const MAX: u32 = 720;
        let media = m.media.clone()?;
        if let Some(p) = self.previews.get(&m.id) {
            return p.clone();
        }
        if imagesize::size(&media).is_ok_and(|s| s.width.max(s.height) <= MAX as usize) {
            self.previews.insert(m.id.clone(), Some(media.clone()));
            return Some(media);
        }
        let dest = WhatsApp::global().dir().join("thumbs").join(format!("{}-{MAX}.jpg", m.id.replace(['/', ':'], "_")));
        if dest.exists() {
            self.previews.insert(m.id.clone(), Some(dest.clone()));
            return Some(dest);
        }
        self.previews.insert(m.id.clone(), None);
        let id = m.id.clone();
        let task = cx.background_executor().spawn(async move {
            let img = image::open(&media).ok()?.thumbnail(MAX, MAX).to_rgb8();
            std::fs::create_dir_all(dest.parent()?).ok()?;
            let mut out = std::io::BufWriter::new(std::fs::File::create(&dest).ok()?);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img).ok()?;
            Some(dest)
        });
        cx.spawn(async move |this, cx| {
            let made = task.await;
            let _ = this.update(cx, |this, cx| {
                // Couldn't shrink it (format…): show the original.
                let src = made.or_else(|| this.items.iter().find_map(|i| match i {
                    Item::Msg(m) if m.id == id => m.media.clone(),
                    _ => None,
                }));
                this.previews.insert(id, src);
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// First page of a downloaded PDF (top part, bubble-sized), rendered once in the background
    /// and cached in `thumbs/`. None while it's being made or if it can't be rendered.
    fn doc_preview(&mut self, m: &ChatMessage, cx: &mut Context<Self>) -> Option<PathBuf> {
        let media = m.media.clone()?;
        if let Some(p) = self.previews.get(&m.id) {
            return p.clone();
        }
        let dest = WhatsApp::global().dir().join("thumbs").join(format!("{}-pdf.jpg", m.id.replace(['/', ':'], "_")));
        if dest.exists() {
            self.previews.insert(m.id.clone(), Some(dest.clone()));
            return Some(dest);
        }
        self.previews.insert(m.id.clone(), None);
        let id = m.id.clone();
        let task = cx.background_executor().spawn(async move {
            crate::pdf_view::first_page_jpeg(&media, &dest, (DOC_PREVIEW_W * 2.) as u32, DOC_PREVIEW_H / DOC_PREVIEW_W).map(|_| dest)
        });
        cx.spawn(async move |this, cx| {
            let made = task.await;
            let _ = this.update(cx, |this, cx| {
                this.previews.insert(id, made);
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// JPEG preview embedded in an image/video/document message, cached as a file.
    fn thumb(&mut self, m: &ChatMessage) -> Option<PathBuf> {
        if let Some(t) = self.thumbs.get(&m.id) {
            return t.clone();
        }
        let wa = WhatsApp::global();
        let dir = wa.dir().join("thumbs");
        let path = dir.join(format!("{}.jpg", m.id.replace(['/', ':'], "_")));
        let found = if path.exists() {
            Some(path)
        } else {
            wa.store()
                .proto(&m.chat, &m.id)
                .and_then(|p| whatsapp::convert::decode(&p))
                .and_then(|msg| {
                    msg.image_message
                        .as_option()
                        .and_then(|i| i.jpeg_thumbnail.clone())
                        .or_else(|| msg.video_message.as_option().and_then(|v| v.jpeg_thumbnail.clone()))
                        .or_else(|| msg.document_message.as_option().and_then(|d| d.jpeg_thumbnail.clone()))
                })
                .filter(|b| !b.is_empty())
                .and_then(|bytes| {
                    let _ = std::fs::create_dir_all(&dir);
                    std::fs::write(&path, bytes).ok().map(|_| path)
                })
        };
        self.thumbs.insert(m.id.clone(), found.clone());
        found
    }

    // ---- rendering ----

    fn render_item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.items.get(ix).cloned() else { return div().into_any_element() };
        let m = match item {
            Item::Day(ts) => {
                return div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .py(px(10.))
                    .child(
                        div()
                            .px(px(12.))
                            .py(px(4.))
                            .rounded(px(8.))
                            .bg(theme::rgb(0x1d2024))
                            .text_size(px(12.))
                            .text_color(theme::rgb(theme::TEXT_MUTED))
                            .child(wa_ui::day_label(ts)),
                    )
                    .into_any_element();
            }
            Item::Msg(m) => m,
        };
        let prev_same_sender = ix > 0
            && matches!(self.items.get(ix - 1), Some(Item::Msg(p)) if p.sender == m.sender && p.from_me == m.from_me && m.ts - p.ts < 300);
        let next_same_sender = matches!(self.items.get(ix + 1), Some(Item::Msg(n)) if n.sender == m.sender && n.from_me == m.from_me && n.ts - m.ts < 300);
        let mine = m.from_me;
        let sticker = m.kind == MsgKind::Sticker;
        let visual = matches!(m.kind, MsgKind::Image | MsgKind::Video);
        // Picture/video/sticker without caption: the time sits on top of the media.
        let overlay_time = sticker || (visual && m.text.is_empty());
        let inline_time = matches!(m.kind, MsgKind::Text | MsgKind::Revoked | MsgKind::Location | MsgKind::Contact | MsgKind::Poll | MsgKind::Other);
        let tail = !next_same_sender && !sticker;
        let bubble_bg = if mine { wa_ui::OWN_BUBBLE } else { wa_ui::OTHER_BUBBLE };
        let is_hit = self.search.as_ref().and_then(|s| s.hits.get(s.current)).is_some_and(|h| h.id == m.id);
        let audio = matches!(m.kind, MsgKind::Audio | MsgKind::Voice);
        let footer = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_end()
            .gap(px(4.))
            .text_size(px(10.5))
            .text_color(theme::rgb(if overlay_time { 0xffffff } else { theme::TEXT_MUTED }))
            .when(m.edited, |d| d.child("Editado"))
            .child(wa_ui::clock(m.ts))
            .when(mine, |d| d.children(wa_ui::ticks(m.status)));
        let quoted = m.quoted.as_ref().map(|q| {
            div()
                .flex()
                .mb(px(4.))
                .rounded(px(8.))
                .overflow_hidden()
                .bg(gpui::black().opacity(0.22))
                .child(div().w(px(4.)).flex_none().bg(theme::rgb(if q.sender_name == "Tú" { wa_ui::GREEN } else { 0x53bdeb })))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .px(px(8.))
                        .py(px(5.))
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme::rgb(if q.sender_name == "Tú" { wa_ui::GREEN } else { 0x53bdeb }))
                                .child(if q.sender_name.is_empty() { "Mensaje".to_string() } else { q.sender_name.trim_start_matches('~').to_string() }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .text_size(px(12.5))
                                .text_color(theme::rgb(theme::TEXT_MUTED))
                                .children(wa_ui::kind_icon(q.kind).map(|p| wa_ui::icon(p, 13., theme::TEXT_MUTED)))
                                .child(div().min_w_0().line_clamp(2).child(if q.text.is_empty() { q.kind.label().to_string() } else { q.text.clone() })),
                        ),
                )
        });
        let forwarded = (m.forwarded > 0 && m.kind != MsgKind::Revoked).then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .mb(px(2.))
                .text_size(px(12.))
                .italic()
                .text_color(theme::rgb(theme::TEXT_MUTED))
                .when(visual, |d| d.px(px(6.)).pt(px(2.)))
                .when(sticker, |d| d.px(px(8.)).py(px(2.)).rounded_full().bg(gpui::black().opacity(0.45)))
                .child(wa_ui::icon("icons/wa-forward.svg", 13., theme::TEXT_MUTED))
                .child(if m.forwarded >= 5 { "Reenviado muchas veces" } else { "Reenviado" })
        });
        let sender = (self.is_group && !mine && !prev_same_sender).then(|| {
            div()
                .text_size(px(12.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::rgb(sender_color(&m.sender)))
                .child(m.sender_name.trim_start_matches('~').to_string())
        });
        let reactions = (!m.reactions.is_empty()).then(|| {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for r in &m.reactions {
                match counts.iter_mut().find(|c| c.0 == r.emoji) {
                    Some(c) => c.1 += 1,
                    None => counts.push((r.emoji.clone(), 1)),
                }
            }
            let tip = m.reactions.iter().map(|r| format!("{} {}", r.emoji, r.sender_name.trim_start_matches('~'))).collect::<Vec<_>>().join("\n");
            div()
                .id(SharedString::from(format!("rx-{}", m.id)))
                .flex()
                .gap(px(2.))
                .mt(px(-6.))
                .px(px(6.))
                .py(px(1.))
                .rounded_full()
                .bg(theme::rgb(theme::SELECTED))
                .border_2()
                .border_color(theme::rgb(wa_ui::CHAT_BG))
                .text_size(px(12.))
                .children(counts.into_iter().map(|(e, n)| if n > 1 { format!("{e}{n}") } else { e }))
                .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(Box::leak(tip.clone().into_boxed_str()))).into())
        });
        let content: AnyElement = if audio {
            let player = self.render_audio(&m, footer, cx);
            self.with_transcript(&m, player, cx)
        } else if overlay_time {
            let body = self.render_body(&m, cx);
            div()
                .relative()
                .child(body)
                .child(
                    div()
                        .absolute()
                        .bottom(px(5.))
                        .right(px(6.))
                        .px(px(6.))
                        .py(px(1.))
                        .rounded_full()
                        .bg(gpui::black().opacity(0.45))
                        .child(footer),
                )
                .into_any_element()
        } else if inline_time {
            let body = self.render_body(&m, cx);
            div().flex().flex_wrap().items_end().justify_end().gap(px(8.)).child(div().flex_grow().min_w_0().child(body)).child(footer.mt(px(4.)).mb(px(-2.))).into_any_element()
        } else {
            let body = self.render_body(&m, cx);
            div().flex().flex_col().gap(px(3.)).child(body).child(footer).into_any_element()
        };
        let msg = m.clone();
        let radius = px(14.);
        // Liquid-glass bubble: own colors, a touch translucent, lit from the top.
        let base = theme::rgb(bubble_bg);
        let bubble_fill = gpui::linear_gradient(
            180.,
            gpui::linear_color_stop(gpui::Hsla { l: (base.l + 0.035).min(1.), a: 0.93, ..base }, 0.),
            gpui::linear_color_stop(gpui::Hsla { a: 0.93, ..base }, 1.),
        );
        let offset = self.swipe.as_ref().filter(|(i, _)| *i == m.id).map(|(_, o)| *o).unwrap_or(0.);
        let hovered = self.hovered_msg.as_deref() == Some(m.id.as_str()) && self.swipe.is_none() && m.kind != MsgKind::Revoked;
        let bubble = div()
            .id(SharedString::from(format!("msg-{}", m.id)))
            .relative()
            // Block, not flex_col: a fit-content flex column measures its height with the text
            // unwrapped (one line), so long messages overflowed the bubble and got clipped.
            .block()
            .max_w(px(if sticker { 200. } else { 420. }))
            .when(!sticker, |d| {
                d.min_w(px(70.))
                    .rounded(radius)
                    .when(tail && mine, |d| d.rounded_br(px(4.)))
                    .when(tail && !mine, |d| d.rounded_bl(px(4.)))
                    .bg(bubble_fill)
                    .shadow(vec![gpui::BoxShadow { color: gpui::hsla(0., 0., 0., if theme::is_light() { 0.08 } else { 0.22 }), offset: gpui::point(px(0.), px(1.)), blur_radius: px(3.), spread_radius: px(0.) }])
                    // Specular highlight along the top edge.
                    .child(div().absolute().top_0().left(radius).right(radius).h(px(1.)).bg(gpui::hsla(0., 0., 1., if theme::is_light() { 0.5 } else { 0.07 })))
                    .when(visual, |d| d.p(px(4.)))
                    .when(!visual, |d| d.px(px(11.)).py(px(7.)))
            })
            .when(is_hit, |d| d.child(div().absolute().inset_0().rounded(radius).border_2().border_color(theme::rgb(theme::MODIFIED))))
            .children(sender.map(|s| s.when(visual, |d| d.px(px(6.)).pt(px(2.)))))
            .children(forwarded)
            .children(quoted)
            .child(content)
            .when(tail, |d| {
                d.child(
                    gpui::svg()
                        .path(if mine { "icons/tail-out.svg" } else { "icons/tail-in.svg" })
                        .absolute()
                        .bottom_0()
                        .w(px(10.))
                        .h(px(16.))
                        .when(mine, |d| d.right(px(-7.)))
                        .when(!mine, |d| d.left(px(-7.)))
                        .text_color(gpui::Hsla { a: 0.93, ..base }),
                )
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.menu = Some(Menu { position: e.position, msg: msg.clone(), confirm_delete: false });
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        // Reaction button beside the bubble while hovered (like WhatsApp).
        let react_btn = hovered.then(|| {
            let m = m.clone();
            div()
                .id(SharedString::from(format!("react-btn-{}", m.id)))
                .size(px(28.))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .glass_button()
                .hover(|s| s.glass_hover())
                .child(wa_ui::icon("icons/smile.svg", 16., theme::TEXT_MUTED))
                .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Reaccionar")).into())
                .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                    let p = e.position();
                    this.react_bar = Some((m.clone(), gpui::point(p.x - px(120.), p.y - px(56.))));
                    this.react_more = false;
                    cx.notify();
                }))
        });
        // Forward button next to it, also only while hovered.
        let fwd_btn = hovered.then(|| {
            let m = m.clone();
            div()
                .id(SharedString::from(format!("fwd-btn-{}", m.id)))
                .size(px(28.))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .glass_button()
                .hover(|s| s.glass_hover())
                .child(wa_ui::icon("icons/wa-forward.svg", 15., theme::TEXT_MUTED))
                .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Reenviar")).into())
                .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                    this.open_forward(m.clone(), w, cx);
                    cx.stop_propagation();
                }))
        });
        let (btn_left, btn_right) = if mine {
            (fwd_btn.into_iter().chain(react_btn).collect::<Vec<_>>(), Vec::new())
        } else {
            (Vec::new(), react_btn.into_iter().chain(fwd_btn).collect::<Vec<_>>())
        };
        let id_hover = m.id.clone();
        let id_wheel = m.id.clone();
        let id_move = m.id.clone();
        let drag_msg = m.clone();
        div()
            .id(SharedString::from(format!("row-{}", m.id)))
            .relative()
            .w_full()
            .flex()
            .flex_col()
            .px(px(22.))
            .pt(px(if prev_same_sender { 2. } else { 10. }))
            .when(mine, |d| d.items_end())
            .when(!mine, |d| d.items_start())
            .on_hover(cx.listener(move |this, h: &bool, _, cx| {
                let was = this.hovered_msg.clone();
                if *h {
                    this.hovered_msg = Some(id_hover.clone());
                } else if this.hovered_msg.as_deref() == Some(id_hover.as_str()) {
                    this.hovered_msg = None;
                }
                if was != this.hovered_msg {
                    cx.notify();
                }
            }))
            // Two-finger swipe right on the trackpad: reply.
            .on_scroll_wheel(cx.listener(move |this, e: &gpui::ScrollWheelEvent, window, cx| {
                let d = match e.delta {
                    gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                    gpui::ScrollDelta::Lines(_) => return,
                };
                let swiping = this.swipe.as_ref().is_some_and(|(i, _)| *i == id_wheel);
                if matches!(e.touch_phase, gpui::TouchPhase::Ended) {
                    if swiping {
                        this.end_swipe(window, cx);
                    }
                    return;
                }
                if !swiping && (d.0 <= 0. || d.0.abs() < d.1.abs() * 1.5) {
                    return; // vertical scroll: leave it to the list
                }
                this.swipe_by(&id_wheel, d.0, cx);
                cx.stop_propagation();
                this._swipe_end = Some(cx.spawn_in(window, async move |this, cx| {
                    Timer::after(Duration::from_millis(180)).await;
                    let _ = this.update_in(cx, |this, window, cx| this.end_swipe(window, cx));
                }));
            }))
            // Or drag it with the mouse.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, _| {
                    this.drag = Some((drag_msg.clone(), e.position.x));
                }),
            )
            .on_mouse_move(cx.listener(move |this, e: &gpui::MouseMoveEvent, _, cx| {
                let Some((m, x0)) = &this.drag else { return };
                if e.pressed_button != Some(MouseButton::Left) || m.id != id_move {
                    return;
                }
                let dx = f32::from(e.position.x - *x0);
                if dx > 12. || this.swipe.is_some() {
                    let id = m.id.clone();
                    this.swipe = Some((id, (dx - 12.).clamp(0., SWIPE_MAX)));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, window, cx| {
                    this.drag = None;
                    this.end_swipe(window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, window, cx| {
                    if this.drag.take().is_some() {
                        this.end_swipe(window, cx);
                    }
                }),
            )
            // Reply arrow uncovered behind the bubble as it moves.
            .when(offset > 4., |d| {
                d.child(
                    div()
                        .absolute()
                        .left(px(18.))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .size(px(30.))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(theme::rgb(theme::SELECTED))
                                .opacity((offset / SWIPE_REPLY).min(1.))
                                .child(wa_ui::icon("icons/wa-reply.svg", 15., if offset >= SWIPE_REPLY { wa_ui::GREEN } else { theme::TEXT_MUTED })),
                        ),
                )
            })
            .child(
                div()
                    .relative()
                    .left(px(offset))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .children(btn_left)
                    .child(bubble)
                    .children(btn_right),
            )
            .children(reactions.map(|r| div().relative().left(px(offset)).px(px(10.)).child(r)))
            .into_any_element()
    }

    fn render_text(&self, id: &str, text: &str, color: u32, cx: &mut Context<Self>) -> AnyElement {
        let links = find_links(text);
        let phones: Vec<(std::ops::Range<usize>, String)> =
            find_phones(text, &own_country_code()).into_iter().filter(|(r, _)| !links.iter().any(|l| l.start < r.end && r.start < l.end)).collect();
        let matches = match &self.search {
            Some(s) if !s.query.is_empty() => find_matches(text, &s.query),
            _ => Vec::new(),
        };
        let link_style = HighlightStyle { color: Some(theme::rgb(0x53bdeb)), underline: Some(gpui::UnderlineStyle { thickness: px(1.), ..Default::default() }), ..Default::default() };
        let hit_style = HighlightStyle { background_color: Some(theme::rgb(theme::FIND_MATCH_CURRENT)), color: Some(gpui::white()), ..Default::default() };
        let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> =
            links.iter().chain(phones.iter().map(|(r, _)| r)).map(|r| (r.clone(), link_style)).collect();
        for r in matches {
            if !highlights.iter().any(|(l, _)| l.start < r.end && r.start < l.end) {
                highlights.push((r, hit_style));
            }
        }
        highlights.sort_by_key(|(r, _)| r.start);
        let sel_style = HighlightStyle { background_color: Some(theme::rgb(0x53bdeb).opacity(0.38)), ..Default::default() };
        let sel = self.sel.as_ref().filter(|s| s.id == id && s.text == text).map(|s| (s.range(), sel_style)).filter(|(r, _)| !r.is_empty());
        let highlights: Vec<_> = gpui::combine_highlights(highlights, sel).collect();
        let styled = StyledText::new(text.to_string()).with_highlights(highlights);
        let layout = styled.layout().clone();
        let (sel_id, sel_text) = (id.to_string(), text.to_string());
        // Click-drag selects, double-click a word, triple-click the whole message.
        let base = div()
            .text_size(px(14.5))
            .line_height(px(20.))
            .text_color(theme::rgb(color))
            .cursor(gpui::CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    let mut s = TextSel { id: sel_id.clone(), text: sel_text.clone(), layout: layout.clone(), anchor: 0, head: 0 };
                    let ix = s.index_at(e.position);
                    (s.anchor, s.head) = match e.click_count {
                        2 => word_at(&s.text, ix),
                        n if n >= 3 => (0, s.text.len()),
                        _ => (ix, ix),
                    };
                    this.sel = Some(s);
                    this.selecting = e.click_count < 2;
                    window.focus(&this.focus);
                    // Not a swipe-to-reply drag.
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        if links.is_empty() && phones.is_empty() {
            return base.child(styled).into_any_element();
        }
        let urls: Vec<String> = links.iter().map(|r| text[r.clone()].to_string()).collect();
        let phone_info: Vec<(String, String)> = phones.iter().map(|(r, d)| (text[r.clone()].to_string(), d.clone())).collect();
        let ranges: Vec<_> = links.into_iter().chain(phones.into_iter().map(|(r, _)| r)).collect();
        let view = cx.entity().downgrade();
        base.child(InteractiveText::new(SharedString::from(format!("txt-{id}")), styled).on_click(ranges, move |ix, window, cx| {
            if let Some(u) = urls.get(ix) {
                let u = if u.starts_with("http") { u.clone() } else { format!("https://{u}") };
                crate::music::open_url(&u, cx);
            } else if let Some((label, digits)) = phone_info.get(ix - urls.len()) {
                let position = window.mouse_position();
                let (label, digits) = (label.clone(), digits.clone());
                let _ = view.update(cx, |this, cx| this.open_phone(position, label, digits, cx));
            }
        }))
        .into_any_element()
    }

    fn open_phone(&mut self, position: Point<Pixels>, label: String, digits: String, cx: &mut Context<Self>) {
        self.sel = None;
        self.selecting = false;
        let rx = WhatsApp::global().check_number(&digits);
        self.phone_pop = Some(PhonePop { position, label, digits: digits.clone(), state: PhoneState::Checking });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let found = rx.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                if let Some(p) = this.phone_pop.as_mut().filter(|p| p.digits == digits) {
                    p.state = match found {
                        Some(jid) => PhoneState::Found(jid),
                        None => PhoneState::Missing,
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn render_phone_pop(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pop = self.phone_pop.as_ref()?;
        let item = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(5.))
                .rounded(px(4.))
                .text_size(px(13.))
                .text_color(theme::rgb(theme::TEXT))
                .cursor_pointer()
                .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                .child(label)
        };
        let note = |t: &'static str| div().px(px(12.)).py(px(5.)).text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(t);
        let title = format!("+{}", pop.digits);
        let mut list = div()
            .flex()
            .flex_col()
            .min_w(px(210.))
            .p(px(4.))
            .rounded(px(8.))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.phone_pop = None;
                cx.notify();
            }))
            .child(div().px(px(12.)).pt(px(5.)).pb(px(3.)).text_size(px(12.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_MUTED)).child(title));
        list = match &pop.state {
            PhoneState::Checking => list.child(note("Verificando WhatsApp…")),
            PhoneState::Missing => list.child(note("No tiene WhatsApp")),
            PhoneState::Found(jid) => {
                let jid = jid.clone();
                list.child(item("phone-chat", "Enviar mensaje").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let name = WhatsApp::global().store().display_name(&jid);
                    this.phone_pop = None;
                    cx.emit(ChatEvent::OpenChat { jid: jid.clone(), name });
                    cx.notify();
                })))
            }
        };
        let label = pop.label.clone();
        list = list.child(item("phone-copy", "Copiar número").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(label.clone()));
            this.phone_pop = None;
            cx.notify();
        })));
        Some(gpui::deferred(gpui::anchored().position(pop.position).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element())
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = self.sel.as_ref().and_then(|s| s.selected()) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(t.to_string()));
        }
    }

    fn render_body(&mut self, m: &Rc<ChatMessage>, cx: &mut Context<Self>) -> AnyElement {
        let wa = WhatsApp::global();
        let caption = (!m.text.is_empty()).then(|| self.render_text(&m.id, &m.text, theme::TEXT, cx));
        match m.kind {
            MsgKind::Text | MsgKind::Location | MsgKind::Contact | MsgKind::Poll | MsgKind::Other => {
                let label = (m.kind != MsgKind::Text).then(|| {
                    div().flex().items_center().gap(px(5.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).children(wa_ui::kind_icon(m.kind).map(|p| wa_ui::icon(p, 14., theme::TEXT_MUTED))).child(m.kind.label())
                });
                div().flex().flex_col().gap(px(2.)).children(label).children(caption).into_any_element()
            }
            MsgKind::Revoked => div()
                .flex()
                .items_center()
                .gap(px(5.))
                .text_size(px(13.))
                .italic()
                .text_color(theme::rgb(theme::TEXT_FAINT))
                .child(wa_ui::icon("icons/wa-revoked.svg", 14., theme::TEXT_FAINT))
                .child(MsgKind::Revoked.label())
                .into_any_element(),
            MsgKind::Image | MsgKind::Sticker => {
                let sticker = m.kind == MsgKind::Sticker;
                let (w, h) = fit(m.width, m.height, if sticker { 160. } else { 320. }, if sticker { 160. } else { 340. });
                if m.media.is_none() {
                    wa.download(&m.chat, &m.id);
                }
                let src = if sticker { m.media.clone() } else { self.preview(m, cx) }.or_else(|| self.thumb(m));
                let msg = m.clone();
                let img = match src {
                    Some(p) => gpui::img(p).w(px(w)).h(px(h)).rounded(px(if sticker { 0. } else { 11. })).object_fit(gpui::ObjectFit::Cover).into_any_element(),
                    None => div().w(px(w)).h(px(h)).rounded(px(11.)).bg(theme::rgb(theme::SELECTED)).into_any_element(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .id(SharedString::from(format!("img-{}", m.id)))
                            .cursor_pointer()
                            .child(img)
                            .when(!sticker, |d| d.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_media(&msg, cx)))),
                    )
                    .children(caption)
                    .into_any_element()
            }
            MsgKind::Video => {
                let (w, h) = fit(m.width, m.height, 320., 340.);
                let thumb = self.thumb(m);
                let msg = m.clone();
                let waiting = self.open_after.contains(&m.id);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .id(SharedString::from(format!("vid-{}", m.id)))
                            .relative()
                            .w(px(w))
                            .h(px(h))
                            .rounded(px(11.))
                            .overflow_hidden()
                            .bg(theme::rgb(0x000000))
                            .cursor_pointer()
                            .children(thumb.map(|t| gpui::img(t).size_full().object_fit(gpui::ObjectFit::Cover)))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .size(px(44.))
                                            .rounded_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .bg(gpui::black().opacity(0.55))
                                            .child(if waiting { div().text_color(gpui::white()).text_size(px(11.)).child("…").into_any_element() } else { gpui::svg().path("icons/play.svg").size(px(20.)).text_color(gpui::white()).into_any_element() }),
                                    ),
                            )
                            .children(m.duration.map(|d| div().absolute().bottom(px(6.)).left(px(8.)).text_size(px(11.)).text_color(gpui::white()).flex().items_center().gap(px(4.)).child(wa_ui::icon("icons/wa-video.svg", 13., 0xffffff)).child(wa_ui::duration(d))))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_media(&msg, cx))),
                    )
                    .children(caption)
                    .into_any_element()
            }
            MsgKind::Audio | MsgKind::Voice => {
                let audio = self.render_audio(m, div(), cx);
                self.with_transcript(m, audio, cx)
            }
            MsgKind::Document => {
                let msg = m.clone();
                let name = m.file_name.clone().unwrap_or_else(|| m.text.clone());
                let detail = match &m.media {
                    Some(p) => wa_ui::file_size(p),
                    None if self.open_after.contains(&m.id) => "Descargando…".into(),
                    None => "Toca para descargar".into(),
                };
                let ext = std::path::Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase();
                let has_caption = m.text != name;
                let is_pdf = ext == "PDF";
                // PDFs: first page once downloaded, else the thumbnail WhatsApp embeds (if any).
                let preview = if is_pdf { self.doc_preview(m, cx) } else { None }.or_else(|| self.thumb(m));
                let show_preview = preview.is_some() || (is_pdf && m.media.is_some());
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .id(SharedString::from(format!("doc-{}", m.id)))
                            .flex()
                            .flex_col()
                            .when(show_preview, |d| d.w(px(DOC_PREVIEW_W)))
                            .when(!show_preview, |d| d.min_w(px(220.)))
                            .rounded(px(6.))
                            .overflow_hidden()
                            .bg(gpui::black().opacity(0.18))
                            .cursor_pointer()
                            .when(show_preview, |d| {
                                d.child(
                                    div()
                                        .w_full()
                                        .h(px(DOC_PREVIEW_H))
                                        .flex_none()
                                        .bg(theme::rgb(0xffffff).opacity(if preview.is_some() { 1. } else { 0.06 }))
                                        .children(preview.map(|p| gpui::img(p).size_full().object_fit(gpui::ObjectFit::Cover))),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .p(px(8.))
                                    .child(gpui::svg().path("icons/doc-file.svg").size(px(26.)).flex_none().text_color(theme::rgb(theme::TEXT_MUTED)))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .min_w_0()
                                            .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).truncate().child(name))
                                            .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(if ext.is_empty() { detail } else { format!("{ext} · {detail}") })),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_media(&msg, cx))),
                    )
                    .when(has_caption, |d| d.children(caption))
                    .into_any_element()
            }
        }
    }

    /// Real waveform of a downloaded audio, computed once in the background.
    fn ensure_waveform(&mut self, m: &ChatMessage, cx: &mut Context<Self>) {
        let Some(media) = m.media.clone() else { return };
        if self.waveforms.contains_key(&m.id) || !self.wave_pending.insert(m.id.clone()) {
            return;
        }
        let id = m.id.clone();
        let task = cx.background_executor().spawn(async move { crate::voice::playable(&media).ok().map(|wav| crate::voice::waveform_of(&wav, WAVE_BARS)) });
        cx.spawn(async move |this, cx| {
            let wave = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(w) = wave {
                    this.waveforms.insert(id, w);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Voice note / audio like WhatsApp: ▶ · waveform with a round thumb · duration and time
    /// underneath · sender's picture with a mic (or the speed pill while playing).
    fn render_audio(&mut self, m: &Rc<ChatMessage>, footer: gpui::Div, cx: &mut Context<Self>) -> AnyElement {
        let (playing, progress, current) = match &self.player {
            Some((id, p)) if *id == m.id => {
                let d = p.duration().max(0.1);
                (p.is_playing(), (p.current() / d).clamp(0., 1.) as f32, p.current())
            }
            _ => (false, 0., 0.),
        };
        let started = playing || current > 0.;
        if m.kind == MsgKind::Voice && m.media.is_none() {
            WhatsApp::global().download(&m.chat, &m.id);
        }
        self.ensure_waveform(m, cx);
        let wave = self.waveforms.get(&m.id).cloned().unwrap_or_else(|| placeholder_wave(&m.id));
        let total = m.duration.unwrap_or(0);
        let label = if started { wa_ui::duration(current as u32) } else { wa_ui::duration(total) };
        let msg = m.clone();
        let speed = self.speed;
        let mine = m.from_me;
        let voice = m.kind == MsgKind::Voice;
        // Unplayed voice notes: green thumb/mic; once heard (or ours) they go blue/grey.
        let heard = mine && matches!(m.status, whatsapp::MsgStatus::Played);
        let accent = if mine { if heard { wa_ui::READ_BLUE } else { 0x8696a0 } } else { wa_ui::GREEN };
        let bars = wave.iter().enumerate().map(|(i, v)| {
            let done = started && (i as f32 + 0.5) / wave.len() as f32 <= progress;
            div()
                .w(px(3.))
                .flex_none()
                .h(px(4. + (*v as f32 / 100.) * 20.))
                .rounded_full()
                .bg(theme::rgb(if done { 0xd1d7db } else { 0x8696a0 }))
                .opacity(if done { 1. } else { 0.75 })
        });
        let waiting = self.play_after.as_deref() == Some(m.id.as_str());
        let avatar = if voice || !mine {
            let who = if mine { whatsapp::service::ME.to_string() } else if self.is_group { m.sender.clone() } else { self.jid.clone() };
            let path = if !mine && !self.is_group { self.avatar.clone() } else { WhatsApp::global().person_avatar(&who) };
            let name = if mine { "Tú".to_string() } else if self.is_group { m.sender_name.clone() } else { self.name.clone() };
            Some((path, name))
        } else {
            None
        };
        let side: AnyElement = if playing {
            div()
                .id("speed")
                .w(px(46.))
                .flex_none()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(8.))
                        .py(px(3.))
                        .rounded_full()
                        .bg(gpui::white().opacity(0.18))
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::white())
                        .child(format!("{}×", if speed.fract() == 0. { format!("{speed:.0}") } else { format!("{speed:.1}") })),
                )
                .cursor_pointer()
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.speed = match this.speed {
                        s if s < 1.25 => 1.5,
                        s if s < 1.75 => 2.0,
                        _ => 1.0,
                    };
                    if let Some((_, p)) = &this.player {
                        p.set_rate(this.speed);
                    }
                    cx.notify();
                }))
                .into_any_element()
        } else {
            match avatar {
                Some((path, name)) => div()
                    .relative()
                    .flex_none()
                    .size(px(46.))
                    .child(wa_ui::avatar(path.as_ref(), &name, 46.))
                    .when(voice, |d| {
                        d.child(
                            div()
                                .absolute()
                                .bottom(px(-2.))
                                .left(px(-6.))
                                .child(gpui::svg().path("icons/mic.svg").size(px(18.)).text_color(theme::rgb(accent))),
                        )
                    })
                    .into_any_element(),
                None => div()
                    .size(px(46.))
                    .flex_none()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::rgb(0xd9822b))
                    .child(gpui::svg().path("icons/play.svg").size(px(18.)).text_color(gpui::white()))
                    .into_any_element(),
            }
        };
        let content = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .w(px(300.))
            .child(
                div()
                    .id(SharedString::from(format!("play-{}", m.id)))
                    .size(px(30.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(if waiting {
                        div().text_size(px(13.)).text_color(theme::rgb(0x8696a0)).child("…").into_any_element()
                    } else {
                        gpui::svg().path(if playing { "icons/pause.svg" } else { "icons/play.svg" }).size(px(22.)).text_color(theme::rgb(0xaebac1)).into_any_element()
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_play(&msg, cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .id(SharedString::from(format!("wave-{}", m.id)))
                            .relative()
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .pl(px(6.))
                            .children(bars)
                            // Thumb travels over the bars (its track is the width minus its size).
                            .child(
                                div().absolute().top_0().bottom_0().left_0().right(px(13.)).child(
                                    div()
                                        .absolute()
                                        .top(px(6.5))
                                        .left(gpui::relative(progress))
                                        .size(px(13.))
                                        .rounded_full()
                                        .bg(theme::rgb(accent)),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(theme::rgb(theme::TEXT_MUTED))
                            .child(label)
                            .child(footer),
                    ),
            );
        content.child(side).into_any_element()
    }

    /// The note's whisper transcription under the player (queued on first sight); long ones
    /// are cut with a "Ver más" toggle.
    fn with_transcript(&mut self, m: &Rc<ChatMessage>, audio: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        const SHORT: usize = 220;
        if m.kind != MsgKind::Voice || !whatsapp::transcribe::available() {
            return audio;
        }
        let text = match m.transcript.as_deref() {
            None => {
                WhatsApp::global().transcribe(&m.chat, &m.id);
                None
            }
            Some(t) => Some(t.trim().to_string()),
        };
        let body = match text {
            Some(t) if t.is_empty() => return audio,
            None => div().italic().text_color(theme::rgb(theme::TEXT_FAINT)).child("Transcribiendo…").into_any_element(),
            Some(t) => {
                let long = t.chars().count() > SHORT;
                let open = self.transcript_open.contains(&m.id);
                let shown = if long && !open {
                    let cut: String = t.chars().take(SHORT).collect();
                    let cut = cut.rsplit_once(' ').map(|(a, _)| a.to_string()).unwrap_or(cut);
                    format!("{}…", cut.trim_end_matches([',', '.', ';', ':']))
                } else {
                    t
                };
                let id = m.id.clone();
                div()
                    .child(shown)
                    .when(long, |d| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("tr-more-{}", m.id)))
                                .mt(px(2.))
                                .text_color(theme::rgb(wa_ui::READ_BLUE))
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .child(if open { "Ver menos" } else { "Ver más" })
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    if !this.transcript_open.remove(&id) {
                                        this.transcript_open.insert(id.clone());
                                    }
                                    // Row height changes: re-measure it.
                                    if let Some(ix) = this.items.iter().position(|i| matches!(i, Item::Msg(m) if m.id == id)) {
                                        this.list.splice(ix..ix + 1, 1);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(audio)
            .child(div().w(px(300.)).px(px(4.)).pt(px(6.)).border_t_1().border_color(gpui::white().opacity(0.08)).text_size(px(12.5)).line_height(px(17.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(body))
            .into_any_element()
    }

    /// Reply to `m` (swipe right / menu): quote it and focus the composer.
    fn start_reply(&mut self, m: Rc<ChatMessage>, window: &mut Window, cx: &mut Context<Self>) {
        if m.kind == MsgKind::Revoked {
            return;
        }
        self.interacted(cx);
        self.reply_to = Some(m);
        window.focus(&self.composer.focus_handle(cx));
        cx.notify();
    }

    /// Bubble offset while swiping; past `SWIPE_REPLY` letting go replies.
    fn swipe_by(&mut self, id: &str, dx: f32, cx: &mut Context<Self>) {
        let cur = self.swipe.as_ref().filter(|(i, _)| i == id).map(|(_, o)| *o).unwrap_or(0.);
        self.swipe = Some((id.to_string(), (cur + dx).clamp(0., SWIPE_MAX)));
        cx.notify();
    }

    fn end_swipe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, offset)) = self.swipe.take() else { return };
        if offset >= SWIPE_REPLY
            && let Some(m) = self.items.iter().find_map(|i| match i {
                Item::Msg(m) if m.id == id => Some(m.clone()),
                _ => None,
            })
        {
            self.start_reply(m, window, cx);
        }
        cx.notify();
    }

    /// WhatsApp-style quick reactions above a message (the current one highlighted; clicking it
    /// again removes it).
    fn render_react_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (m, pos) = self.react_bar.clone()?;
        let mine = m.reactions.iter().find(|r| r.from_me).map(|r| r.emoji.clone());
        let react = |m: &Rc<ChatMessage>, e: String, selected: bool| {
            let m = m.clone();
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx: &mut Context<Self>| {
                let emoji = if selected { "" } else { e.as_str() };
                WhatsApp::global().react(&m.chat, &m.id, emoji);
                note_reaction(emoji);
                this.interacted(cx);
                this.react_bar = None;
                this.react_more = false;
                cx.notify();
            })
        };
        let row = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .children(quick_reactions().into_iter().enumerate().map(|(i, e)| {
                let selected = mine.as_deref() == Some(e.as_str());
                div()
                    .id(("quick-react", i))
                    .size(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(21.))
                    .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                    .child(e.clone())
                    .on_click(react(&m, e, selected))
            }))
            .child(
                div()
                    .id("quick-react-more")
                    .size(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .when(self.react_more, |d| d.bg(theme::rgb(theme::SELECTED)))
                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                    .child(gpui::svg().path("icons/plus.svg").size(px(18.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.react_more = !this.react_more;
                        cx.notify();
                    })),
            );
        let grid = self.react_more.then(|| {
            let mut n = 0usize;
            div()
                .id("react-emoji-grid")
                .flex()
                .flex_col()
                .w(px(300.))
                .max_h(px(260.))
                .overflow_y_scroll()
                .children(EMOJIS.iter().map(|(cat, list)| {
                    div()
                        .flex()
                        .flex_col()
                        .child(div().px(px(4.)).pt(px(4.)).pb(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(*cat))
                        .child(div().flex().flex_wrap().children(list.split(' ').map(|e| {
                            n += 1;
                            let selected = mine.as_deref() == Some(e);
                            div()
                                .id(("react-emoji", n))
                                .size(px(34.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(20.))
                                .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                .child(e.to_string())
                                .on_click(react(&m, e.to_string(), selected))
                        })))
                }))
        });
        let bar = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(4.))
            .when(self.react_more, |d| d.rounded(px(14.)))
            .when(!self.react_more, |d| d.rounded_full())
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.react_bar = None;
                this.react_more = false;
                cx.notify();
            }))
            .child(row)
            .children(grid);
        Some(gpui::deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(bar)).with_priority(1).into_any_element())
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let m = menu.msg.clone();
        if menu.confirm_delete {
            return Some(self.render_delete_menu(menu.position, m, cx));
        }
        let item = |id: &'static str, label: String| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(5.))
                .rounded(px(4.))
                .text_size(px(13.))
                .text_color(theme::rgb(theme::TEXT))
                .cursor_pointer()
                .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                .child(label)
        };
        let my_reaction = m.reactions.iter().find(|r| r.from_me).map(|r| r.emoji.clone());
        let emojis = div().flex().gap(px(2.)).px(px(6.)).pb(px(4.)).children(quick_reactions().into_iter().enumerate().map(|(i, e)| {
            let (m2, e2) = (m.clone(), e.clone());
            let selected = my_reaction.as_deref() == Some(e.as_str());
            div()
                .id(("react", i))
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .text_size(px(18.))
                .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                .child(e.to_string())
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let emoji = if selected { "" } else { e2.as_str() };
                    WhatsApp::global().react(&m2.chat, &m2.id, emoji);
                    note_reaction(emoji);
                    this.interacted(cx);
                    this.menu = None;
                    cx.notify();
                }))
        }));
        let (m_reply, m_copy, m_fav, m_show, m_open, m_fwd) = (m.clone(), m.clone(), m.clone(), m.clone(), m.clone(), m.clone());
        let mut list = div()
            .flex()
            .flex_col()
            .min_w(px(190.))
            .p(px(4.))
            .rounded(px(8.))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }))
            .when(m.kind != MsgKind::Revoked, |d| d.child(emojis))
            .child(item("m-reply", "Responder".into()).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                this.reply_to = Some(m_reply.clone());
                this.menu = None;
                w.focus(&this.composer.focus_handle(cx));
                cx.notify();
            })));
        if m.kind != MsgKind::Revoked {
            list = list.child(item("m-forward", "Reenviar…".into()).on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                this.menu = None;
                this.open_forward(m_fwd.clone(), w, cx);
            })));
        }
        let selection = self.sel.as_ref().filter(|s| s.id == m.id).and_then(|s| s.selected()).map(str::to_string);
        if let Some(t) = selection {
            list = list.child(item("m-copy-sel", "Copiar selección".into()).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(t.clone()));
                this.menu = None;
                cx.notify();
            })));
        }
        if !m.text.is_empty() {
            list = list.child(item("m-copy", "Copiar texto".into()).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(m_copy.text.clone()));
                this.menu = None;
                cx.notify();
            })));
        }
        if m.kind == MsgKind::Sticker && m.media.is_some() {
            let fav = WhatsApp::global().is_favorite_sticker(&m.chat, &m.id);
            list = list.child(item("m-fav", if fav { "Quitar de favoritos".into() } else { "Añadir a favoritos".into() }).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                WhatsApp::global().toggle_favorite_sticker(&m_fav.chat, &m_fav.id);
                this.menu = None;
                cx.notify();
            })));
        }
        if m.kind.has_media() && m.kind != MsgKind::Sticker {
            list = list.child(item("m-open", "Abrir".into()).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.menu = None;
                this.open_media(&m_open, cx);
            })));
        }
        if let Some(p) = m.media.clone().filter(|_| m.kind.has_media()) {
            let _ = m_show;
            list = list.child(item("m-finder", "Mostrar en Finder".into()).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.reveal_path(&p);
                this.menu = None;
                cx.notify();
            })));
        }
        list = list.child(
            item("m-delete", "Eliminar…".into()).text_color(theme::rgb(theme::DELETED)).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                if let Some(menu) = this.menu.as_mut() {
                    menu.confirm_delete = true;
                }
                cx.notify();
            })),
        );
        Some(gpui::deferred(gpui::anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element())
    }

    /// Second step of "Eliminar…": for everyone (my messages, like WhatsApp within ~2 days)
    /// or only for me.
    fn render_delete_menu(&self, position: Point<Pixels>, m: Rc<ChatMessage>, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, label: &'static str, color: u32| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(5.))
                .rounded(px(4.))
                .text_size(px(13.))
                .text_color(theme::rgb(color))
                .cursor_pointer()
                .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                .child(label)
        };
        let for_all = m.from_me && m.kind != MsgKind::Revoked && whatsapp::store::now() - m.ts < 2 * 24 * 3600;
        let (m_all, m_me) = (m.clone(), m.clone());
        let list = div()
            .flex()
            .flex_col()
            .min_w(px(190.))
            .p(px(4.))
            .rounded(px(8.))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .border_1()
            .border_color(theme::rgb(theme::BORDER))
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }))
            .child(div().px(px(12.)).pt(px(4.)).pb(px(6.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child("¿Eliminar mensaje?"))
            .when(for_all, |d| {
                d.child(item("m-del-all", "Eliminar para todos", theme::DELETED).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    WhatsApp::global().delete_for_everyone(&m_all.chat, &m_all.id);
                    this.sel = None;
                    this.menu = None;
                    this.interacted(cx);
                    cx.notify();
                })))
            })
            .child(item("m-del-me", "Eliminar para mí", theme::DELETED).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                WhatsApp::global().delete_for_me(&m_me.chat, &m_me.id, m_me.ts);
                this.sel = None;
                this.menu = None;
                this.interacted(cx);
                cx.notify();
            })))
            .child(item("m-del-cancel", "Cancelar", theme::TEXT).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.menu = None;
                cx.notify();
            })));
        gpui::deferred(gpui::anchored().position(position).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element()
    }

    fn render_stickers(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.stickers_open {
            return None;
        }
        let tab = |id: &'static str, label: &'static str, t: PickerTab, active: bool| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(6.))
                .text_size(px(12.5))
                .cursor_pointer()
                .text_color(theme::rgb(if active { theme::TEXT } else { theme::TEXT_MUTED }))
                .border_b_2()
                .border_color(if active { theme::rgb(wa_ui::GREEN) } else { gpui::transparent_black() })
                .hover(|s| s.text_color(theme::rgb(theme::TEXT)))
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.picker_tab = t;
                    cx.notify();
                }))
        };
        let tabs = div()
            .flex()
            .px(px(6.))
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .child(tab("pick-emojis", "Emojis", PickerTab::Emojis, self.picker_tab == PickerTab::Emojis))
            .child(tab("pick-stickers", "Stickers", PickerTab::Stickers, self.picker_tab == PickerTab::Stickers));
        let body = if self.picker_tab == PickerTab::Emojis {
            let mut n = 0usize;
            div()
                .id("emoji-grid")
                .flex()
                .flex_col()
                .p(px(6.))
                .max_h(px(260.))
                .overflow_y_scroll()
                .children(EMOJIS.iter().map(|(cat, list)| {
                    div()
                        .flex()
                        .flex_col()
                        .child(div().px(px(4.)).pt(px(4.)).pb(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(*cat))
                        .child(div().flex().flex_wrap().children(list.split(' ').map(|e| {
                            n += 1;
                            let e = e.to_string();
                            div()
                                .id(("emoji", n))
                                .size(px(34.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(20.))
                                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                .child(e.clone())
                                .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                    this.composer.update(cx, |t, cx| t.insert(&e, cx));
                                    w.focus(&this.composer.focus_handle(cx));
                                }))
                        })))
                }))
                .into_any_element()
        } else {
            self.render_sticker_grid(cx)
        };
        Some(
            div()
                .mx(px(12.))
                .mb(px(6.))
                .rounded(px(8.))
                .bg(theme::rgb(theme::SIDEBAR_BG))
                .border_1()
                .border_color(theme::rgb(theme::BORDER))
                .child(tabs)
                .child(body)
                .into_any_element(),
        )
    }

    fn render_sticker_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        let favs = WhatsApp::global().store().stickers();
        if favs.is_empty() {
            div()
                .p(px(16.))
                .text_size(px(12.5))
                .text_color(theme::rgb(theme::TEXT_MUTED))
                .child("Aún no tienes stickers favoritos. Haz clic derecho sobre un sticker recibido → Añadir a favoritos.")
                .into_any_element()
        } else {
            div()
                .id("sticker-grid")
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .p(px(8.))
                .max_h(px(260.))
                .overflow_y_scroll()
                .children(favs.into_iter().enumerate().map(|(i, s)| {
                    let id = s.id.clone();
                    div()
                        .id(("fav", i))
                        .size(px(72.))
                        .p(px(4.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|st| st.bg(theme::rgb(theme::HOVER)))
                        .child(gpui::img(s.path).size_full().object_fit(gpui::ObjectFit::Contain))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            WhatsApp::global().send_sticker(&this.jid, &id);
                            this.interacted(cx);
                            this.stickers_open = false;
                            cx.notify();
                        }))
                        .on_mouse_down(MouseButton::Right, {
                            let id = s.id.clone();
                            cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                                WhatsApp::global().store().remove_sticker(&id);
                                cx.notify();
                            })
                        })
                }))
                .into_any_element()
        }
    }

    /// Ask Claude for a reply with the whole chat as context; it lands in the composer. What was
    /// typed there is passed as the instruction (or draft to improve).
    fn suggest_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafting.is_some() {
            return;
        }
        self.interacted(cx);
        let (jid, instruction) = (self.jid.clone(), self.composer.read(cx).content.trim().to_string());
        self.error = None;
        let task = cx.background_executor().spawn(async move { crate::wa_reply::suggest(&jid, &instruction) });
        self.drafting = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.drafting = None;
                match result {
                    Ok(text) => {
                        this.composer.update(cx, |c, cx| c.set_text(&text, cx));
                        window.focus(&this.composer.focus_handle(cx));
                    }
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_composer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let empty = self.composer.read(cx).content.trim().is_empty() && self.attachments.is_empty();
        let round = |id: &'static str, icon: &'static str, tip: &'static str| {
            div()
                .id(id)
                .size(px(34.))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.glass_hover())
                .child(gpui::svg().path(icon).size(px(18.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
        };
        if let Some(rec) = &self.recorder {
            let secs = rec.elapsed().as_secs() as u32;
            let level = rec.level();
            return div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(8.))
                .child(round("rec-cancel", "icons/trash.svg", "Descartar").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop_recording(false, cx))))
                .child(div().size(px(10.)).rounded_full().bg(theme::rgb(theme::DELETED)).opacity(0.4 + level * 0.6))
                .child(div().text_size(px(13.)).text_color(theme::rgb(theme::TEXT)).child(wa_ui::duration(secs)))
                .child(div().flex_1().h(px(4.)).rounded_full().bg(theme::rgb(theme::SELECTED)).child(div().h_full().rounded_full().w(gpui::relative(level)).bg(theme::rgb(wa_ui::GREEN))))
                .child(
                    div()
                        .id("rec-send")
                        .size(px(36.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .bg(theme::rgb(0x1f7a4d))
                        .child(gpui::svg().path("icons/send.svg").size(px(17.)).text_color(gpui::white()))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop_recording(true, cx))),
                )
                .into_any_element();
        }
        let reply = self.reply_to.as_ref().map(|r| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .mx(px(12.))
                .mb(px(6.))
                .rounded(px(6.))
                .overflow_hidden()
                .bg(theme::rgb(theme::SIDEBAR_BG))
                .child(div().w(px(3.)).h(px(40.)).bg(theme::rgb(wa_ui::GREEN)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_size(px(11.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(wa_ui::GREEN)).child(r.sender_name.trim_start_matches('~').to_string()))
                        .child(div().text_size(px(12.)).truncate().text_color(theme::rgb(theme::TEXT_MUTED)).child(r.preview())),
                )
                .child(round("reply-x", "icons/close.svg", "Cancelar respuesta").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.reply_to = None;
                    cx.notify();
                })))
        });
        // Pending attachments: a tray of thumbnails (pictures) and file cards, each with an ×.
        let chips = (!self.attachments.is_empty()).then(|| {
            div()
                .id("wa-attachments")
                .flex()
                .gap(px(8.))
                .mx(px(14.))
                .mb(px(8.))
                .p(px(8.))
                .rounded(px(12.))
                .bg(theme::rgb(theme::SIDEBAR_BG))
                .border_1()
                .border_color(theme::rgb(theme::BORDER))
                .overflow_x_scroll()
                .children(self.attachments.iter().enumerate().map(|(i, p)| {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let ext = p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).unwrap_or_default();
                    let is_img = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic");
                    let remove = div()
                        .id(("att-x", i))
                        .absolute()
                        .top(px(4.))
                        .right(px(4.))
                        .size(px(20.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .bg(gpui::black().opacity(0.6))
                        .hover(|s| s.bg(gpui::black().opacity(0.85)))
                        .child(gpui::svg().path("icons/close.svg").size(px(9.)).text_color(gpui::white()))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if i < this.attachments.len() {
                                this.attachments.remove(i);
                            }
                            cx.notify();
                        }));
                    let tile = div().relative().flex_none().h(px(84.)).rounded(px(8.)).overflow_hidden().bg(theme::rgb(theme::SELECTED));
                    if is_img {
                        tile.w(px(84.))
                            .child(gpui::img(p.clone()).size(px(84.)).object_fit(gpui::ObjectFit::Cover))
                            .child(remove)
                            .into_any_element()
                    } else {
                        tile.w(px(150.))
                            .flex()
                            .flex_col()
                            .justify_center()
                            .gap(px(4.))
                            .px(px(10.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(gpui::svg().path("icons/doc-file.svg").size(px(22.)).flex_none().text_color(theme::rgb(theme::TEXT_MUTED)))
                                    .child(div().text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child(ext.to_uppercase())),
                            )
                            .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT)).line_clamp(2).child(name))
                            .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(wa_ui::file_size(p)))
                            .child(remove)
                            .into_any_element()
                    }
                }))
        });
        div()
            .flex()
            .flex_col()
            .pt(px(6.))
            .children(self.render_stickers(cx))
            .children(reply)
            .children(chips)
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(10.))
                    .px(px(14.))
                    .pb(px(12.))
                    .child(
                        div()
                            .id("wa-attach")
                            .size(px(40.))
                            .flex_none()
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .glass_button()
                            .bg(frosted())
                            .hover(|s| s.glass_hover())
                            .child(gpui::svg().path("icons/plus.svg").size(px(18.)).text_color(theme::rgb(theme::TEXT)))
                            .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Adjuntar archivos")).into())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.attach(cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_end()
                            .gap(px(6.))
                            .min_h(px(40.))
                            .pl(px(16.))
                            .pr(px(4.))
                            .rounded(px(20.))
                            .glass()
                            .bg(frosted())
                            .child(div().flex_1().min_w_0().py(px(10.)).text_size(px(14.)).child(self.composer.clone()))
                            .child(div().pb(px(3.)).child(round("wa-stickers", "icons/smile.svg", "Emojis y stickers").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.stickers_open = !this.stickers_open;
                                cx.notify();
                            })))),
                    )
                    .child({
                        let busy = self.drafting.is_some();
                        div()
                            .id("wa-suggest")
                            .size(px(40.))
                            .flex_none()
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .glass_button()
                            .bg(frosted())
                            .child(if busy {
                                div().text_size(px(15.)).text_color(theme::rgb(0xd97757)).child("…").into_any_element()
                            } else {
                                wa_ui::icon("icons/wa-spark.svg", 18., 0xd97757).into_any_element()
                            })
                            .when(!busy, |d| d.cursor_pointer().hover(|s| s.glass_hover()))
                            .tooltip(move |_, cx| {
                                cx.new(|_| crate::git_panel::Tooltip(if busy { "Claude está escribiendo la respuesta…" } else { "Sugerir respuesta con Claude (usa lo escrito como indicación)" })).into()
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.suggest_reply(window, cx)))
                    })
                    .child({
                        let btn = div()
                            .id(if empty { "wa-mic" } else { "wa-send" })
                            .size(px(40.))
                            .flex_none()
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .bg(theme::rgb(wa_ui::GREEN))
                            .hover(|s| s.opacity(0.9))
                            .child(gpui::svg().path(if empty { "icons/mic.svg" } else { "icons/send.svg" }).size(px(18.)).text_color(theme::rgb(0x0b141a)));
                        if empty {
                            btn.tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Grabar nota de voz")).into()).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.start_recording(cx)))
                        } else {
                            btn.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.send(cx)))
                        }
                    }),
            )
            .into_any_element()
    }
}

const WAVE_BARS: usize = 36;

/// Stable fake waveform while the audio isn't downloaded yet.
fn placeholder_wave(id: &str) -> Vec<u8> {
    let mut h = id.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193));
    (0..WAVE_BARS)
        .map(|_| {
            h ^= h << 13;
            h ^= h >> 17;
            h ^= h << 5;
            (20 + h % 60) as u8
        })
        .collect()
}

/// Fit media into a box keeping its aspect ratio.
fn fit(w: Option<u32>, h: Option<u32>, max_w: f32, max_h: f32) -> (f32, f32) {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let (w, h) = (w as f32, h as f32);
            let s = (max_w / w).min(max_h / h).min(1.0);
            ((w * s).max(60.), (h * s).max(60.))
        }
        _ => (max_w * 0.8, max_h * 0.6),
    }
}

fn sender_color(jid: &str) -> u32 {
    const C: [u32; 8] = [0x53bdeb, 0xe5c07b, 0x98c379, 0xc678dd, 0xe06c75, 0x56b6c2, 0xd19a66, 0x7aa2f7];
    let h = jid.bytes().fold(5u32, |h, b| h.wrapping_mul(33).wrapping_add(b as u32));
    C[(h % C.len() as u32) as usize]
}

/// Byte ranges of URLs (http(s)://… or www.…) in `text`.
/// Largest char boundary ≤ `i` (clamped to the text).
fn char_floor(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte range of the word (letters, digits, `_`) around `ix`; empty if there's none.
fn word_at(text: &str, ix: usize) -> (usize, usize) {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..ix].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(ix, |(i, _)| i);
    let end = text[ix..].char_indices().find(|(_, c)| !is_word(*c)).map_or(text.len(), |(i, _)| ix + i);
    (start, end)
}

/// Country code of my own number (digits before the last 10), "57" when unknown.
pub(crate) fn own_country_code() -> String {
    let own = WhatsApp::try_global().and_then(|w| w.own_jid()).unwrap_or_default();
    let digits: String = own.split('@').next().unwrap_or("").chars().filter(char::is_ascii_digit).collect();
    if digits.len() > 10 { digits[..digits.len() - 10].to_string() } else { "57".into() }
}

/// Phone numbers in a message: byte range and digits with country code. `+` numbers take
/// 8–15 digits; bare ones must be 10 digits (local, `cc` is prepended) or `cc` + 10 digits.
pub(crate) fn find_phones(text: &str, cc: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        let prev_ok = i == 0 || !(chars[i - 1].1.is_alphanumeric() || "+-./_@$".contains(chars[i - 1].1));
        if !prev_ok || !(c.is_ascii_digit() || (c == '+' && chars.get(i + 1).is_some_and(|n| n.1.is_ascii_digit()))) {
            i += 1;
            continue;
        }
        let plus = c == '+';
        let mut j = if plus { i + 1 } else { i };
        let mut digits = String::new();
        let mut end = start;
        while j < chars.len() {
            let ch = chars[j].1;
            if ch.is_ascii_digit() {
                digits.push(ch);
                end = chars[j].0 + 1;
            } else {
                // Up to two separators in a row ("(415) 555"), and a digit must follow.
                let seps = chars[j..].iter().take_while(|n| " -.()".contains(n.1)).count();
                if seps == 0 || seps > 2 || !chars.get(j + seps).is_some_and(|n| n.1.is_ascii_digit()) {
                    break;
                }
                j += seps;
                continue;
            }
            j += 1;
        }
        // A letter or digit glued right after means it's a code or id, not a phone.
        let glued = chars.get(j).is_some_and(|n| n.1.is_alphanumeric());
        let full = if glued {
            None
        } else if plus {
            (8..=15).contains(&digits.len()).then(|| digits.clone())
        } else if digits.len() == 10 {
            Some(format!("{cc}{digits}"))
        } else if digits.len() == cc.len() + 10 && digits.starts_with(cc) {
            Some(digits.clone())
        } else {
            None
        };
        if let Some(d) = full {
            out.push((start..end, d));
        }
        i = j.max(i + 1);
    }
    out
}

fn find_links(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let start = ["https://", "http://", "www."].iter().filter_map(|p| rest.find(p)).min();
        let Some(s) = start else { break };
        let abs = i + s;
        // Must start a word.
        if abs > 0 && !text[..abs].ends_with(|c: char| c.is_whitespace() || "([<\"'".contains(c)) {
            i = abs + 1;
            while !text.is_char_boundary(i) {
                i += 1;
            }
            continue;
        }
        let end = text[abs..].find(|c: char| c.is_whitespace() || "<>\"".contains(c)).map(|e| abs + e).unwrap_or(text.len());
        let mut end = end;
        while end > abs && text[..end].ends_with(['.', ',', ')', ';', ':', '!', '?']) {
            end -= 1;
        }
        if end > abs + 4 {
            out.push(abs..end);
        }
        i = end.max(abs + 1);
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
    }
    out
}

/// Byte ranges where `query` appears in `text`, ignoring case.
fn find_matches(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let low = |c: char| c.to_lowercase().next().unwrap_or(c);
    let mut out = Vec::new();
    let mut i = 0;
    while i + q.len() <= chars.len() {
        if (0..q.len()).all(|k| low(chars[i + k].1) == q[k]) {
            let end = chars.get(i + q.len()).map(|c| c.0).unwrap_or(text.len());
            out.push(chars[i].0..end);
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

impl Focusable for ChatView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.unread_pending && window.is_window_active() {
            self.unread_pending = false;
            WhatsApp::global().mark_read(&self.jid);
        }
        let typing = self.typing.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(8)).map(|(w, _)| match w {
            Some(w) => format!("{} está escribiendo…", w.trim_start_matches('~')),
            None => "escribiendo…".to_string(),
        });
        let subtitle = typing.clone().unwrap_or_else(|| if self.is_group { "Grupo".into() } else { whatsapp::store::phone_label(&self.jid) });
        let weak = cx.entity().downgrade();
        let messages = list(self.list.clone(), move |ix, _, cx| weak.update(cx, |this, cx| this.render_item(ix, cx)).unwrap_or_else(|_| div().into_any_element()))
            .flex_1()
            .w_full();
        let error = self.error.clone();
        let search_bar = self.render_search(cx);
        let info = self.render_info(cx);
        let zoom = self.render_zoom(cx);
        let info_open = self.info.is_some();
        let pill_btn = |id: &'static str, icon: &'static str, tip: &'static str| {
            div()
                .id(id)
                .size(px(34.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .hover(|s| s.glass_hover())
                .child(gpui::svg().path(icon).size(px(16.)).text_color(theme::rgb(theme::TEXT)))
                .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip(tip)).into())
        };
        div()
            .image_cache(self.images.clone())
            .id("chat-view")
            .key_context("ChatView")
            .track_focus(&self.focus)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::rgb(wa_ui::CHAT_BG))
            .on_action(cx.listener(|this, _: &SearchInChat, w, cx| this.open_search(w, cx)))
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" && (this.info.is_some() || this.zoom.is_some()) {
                    if this.zoom.take().is_none() {
                        this.info = None;
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| this.copy_selection(cx)))
            // A click anywhere drops the text selection (a click on text starts a new one).
            .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, cx| {
                // With the menu open the click is for it ("Copiar selección" needs the selection).
                if e.button == MouseButton::Left && this.menu.is_none() && this.sel.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if !this.selecting || e.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                if let Some(s) = this.sel.as_mut() {
                    let h = s.index_at(e.position);
                    if h != s.head {
                        s.head = h;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &gpui::MouseUpEvent, _, _| this.selecting = false))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &gpui::MouseUpEvent, _, _| this.selecting = false))
            .drag_over::<gpui::ExternalPaths>(|s, _, _, _| s.opacity(0.85))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.attachments.extend(paths.paths().iter().cloned());
                cx.notify();
            }))
            .drag_over::<crate::workspace::DraggedFile>(|s, _, _, _| s.opacity(0.85))
            .on_drop(cx.listener(|this, f: &crate::workspace::DraggedFile, _, cx| {
                if !f.is_dir && !this.attachments.contains(&f.path) {
                    this.attachments.push(f.path.clone());
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(10.))
                    .h(px(62.))
                    .flex_none()
                    .bg(theme::rgb(theme::SIDEBAR_BG))
                    .border_b_1()
                    .border_color(crate::glass::edge())
                    .shadow(crate::glass::shadow_sm())
                    .child(
                        div()
                            .id("chat-profile")
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap(px(11.))
                            .pl(px(5.))
                            .pr(px(14.))
                            .py(px(5.))
                            .rounded_full()
                            .cursor_pointer()
                            .when(info_open, |d| d.glass_button())
                            .hover(|s| s.glass_hover())
                            .child(
                                div()
                                    .flex_none()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(crate::glass::edge())
                                    .shadow(crate::glass::shadow_sm())
                                    .child(wa_ui::avatar(self.avatar.as_ref(), &self.name, 40.)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .gap(px(1.))
                                    .child(div().text_size(px(14.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).truncate().child(self.name.trim_start_matches('~').to_string()))
                                    .child(div().text_size(px(11.5)).truncate().text_color(theme::rgb(if typing.is_some() { wa_ui::GREEN } else { theme::TEXT_FAINT })).child(subtitle)),
                            )
                            .tooltip(move |_, cx| cx.new(|_| crate::git_panel::Tooltip("Ver info. del contacto")).into())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_info(cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(2.))
                            .p(px(3.))
                            .rounded_full()
                            .glass()
                            .child(pill_btn("chat-ask-claude", "icons/wa-spark.svg", "Pedir a Claude…").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.toggle_ask(w, cx))))
                            .child(pill_btn("chat-search", "icons/search.svg", "Buscar en el chat (⌘F)").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.open_search(w, cx))))
                            .child(
                                pill_btn("chat-info-btn", "icons/more.svg", if info_open { "Cerrar info." } else { "Info. del contacto" })
                                    .when(info_open, |d| d.glass_button())
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_info(cx))),
                            ),
                    ),
            )
            .children(search_bar)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(wa_ui::wallpaper_layer())
                    // Room under the last message for the floating composer.
                    .child(messages.pb(px(self.composer_h.get() + 6.)))
                    .child({
                        let composer_h = self.composer_h.clone();
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .flex()
                            .flex_col()
                            // Fade so bubbles slide under the bar instead of being cut off.
                            .bg(gpui::linear_gradient(
                                180.,
                                gpui::linear_color_stop(gpui::Hsla { a: 0., ..theme::rgb(wa_ui::CHAT_BG) }, 0.),
                                gpui::linear_color_stop(gpui::Hsla { a: 0.55, ..theme::rgb(wa_ui::CHAT_BG) }, 1.),
                            ))
                            .child(
                                gpui::canvas(
                                    move |bounds, window, _| {
                                        let h = f32::from(bounds.size.height);
                                        if (composer_h.get() - h).abs() > 0.5 {
                                            composer_h.set(h);
                                            window.refresh();
                                        }
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            )
                            .children(error.map(|e| {
                        div()
                            .id("chat-error")
                            .mx(px(12.))
                            .mb(px(4.))
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(6.))
                            .bg(theme::rgb(0x3a1d22))
                            .text_size(px(12.))
                            .text_color(theme::rgb(theme::DELETED))
                            .cursor_pointer()
                            .child(e)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.error = None;
                                cx.notify();
                            }))
                    }))
                            .child(self.render_composer(cx))
                    }),
            )
            .children(info)
            .children(zoom)
            .children(self.render_ask(cx))
            .children(self.render_forward(cx))
            .children(self.render_menu(cx))
            .children(self.render_react_bar(cx))
            .children(self.render_phone_pop(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{find_links, find_matches, find_phones};

    #[test]
    fn phones() {
        let got = |t: &str| find_phones(t, "57").into_iter().map(|(r, d)| (t[r].to_string(), d)).collect::<Vec<_>>();
        assert_eq!(got("llama al 300 123 4567 ya"), vec![("300 123 4567".into(), "573001234567".into())]);
        assert_eq!(got("+57 310-555-1234."), vec![("+57 310-555-1234".into(), "573105551234".into())]);
        assert_eq!(got("+1 (415) 555-2671"), vec![("+1 (415) 555-2671".into(), "14155552671".into())]);
        assert_eq!(got("3001234567"), vec![("3001234567".into(), "573001234567".into())]);
        assert!(got("valor $1.200.000 y ref 12345").is_empty());
        assert!(got("pedido AB3001234567").is_empty());
        assert!(got("27 nov 2026").is_empty());
    }

    #[test]
    fn matches() {
        let t = "Hola ÁRBOL, hola árbol";
        let got: Vec<&str> = find_matches(t, "hola").into_iter().map(|r| &t[r]).collect();
        assert_eq!(got, vec!["Hola", "hola"]);
        let got: Vec<&str> = find_matches(t, "árbol").into_iter().map(|r| &t[r]).collect();
        assert_eq!(got, vec!["ÁRBOL", "árbol"]);
    }

    #[test]
    fn links() {
        let t = "mira https://ex.com/a?b=1, y www.foo.co. ok (http://x.io/z)";
        let got: Vec<&str> = find_links(t).into_iter().map(|r| &t[r]).collect();
        assert_eq!(got, vec!["https://ex.com/a?b=1", "www.foo.co", "http://x.io/z"]);
        assert!(find_links("correo@www.no").is_empty());
        assert!(find_links("ñandú http://á.com fin").len() == 1);
    }
}

/// Frosted surface for controls floating over the messages: mostly opaque so text stays
/// readable, with the glass top-light (GPUI can't blur what's behind an element).
fn frosted() -> gpui::Background {
    let base = theme::rgb(theme::SIDEBAR_BG);
    let top = if theme::is_light() { gpui::Hsla { l: (base.l + 0.02).min(1.), a: 0.9, ..base } } else { gpui::Hsla { l: (base.l + 0.05).min(1.), a: 0.88, ..base } };
    gpui::linear_gradient(180., gpui::linear_color_stop(top, 0.), gpui::linear_color_stop(gpui::Hsla { a: 0.84, ..base }, 1.))
}
