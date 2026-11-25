//! Sidebar "WhatsApp" mode: link a device by QR, then the chat list (newest first, pinned on
//! top, no archived chats) with search over names and cached messages.

use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{ClickEvent, Context, Entity, EventEmitter, Focusable, Render, SharedString, Subscription, Task, Timer, Window, div, prelude::*, px, uniform_list};
use whatsapp::{ChatMessage, ChatSummary, Label, MsgKind, Status, Update, WhatsApp};

use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;
use crate::wa_ui;

pub enum WhatsAppPanelEvent {
    /// Open a conversation (optionally scrolled to a message from a search hit).
    OpenChat { jid: String, name: String },
    /// "Pedir a Claude…": open a Claude Code session about this chat with the instruction.
    AskClaude { jid: String, instruction: String },
}

/// Label-chip value for "Sin etiqueta" (chats without any label).
const NO_LABEL: i64 = -1;

/// Without a filter or search, the list only shows chats active in this window.
const RECENT_SECS: i64 = 30 * 24 * 3600;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Favorites,
    Unread,
    Groups,
    Label(i64),
}

impl Filter {
    const ALL: [(Filter, &'static str); 4] = [(Filter::All, "Todos"), (Filter::Favorites, "Favoritos"), (Filter::Unread, "No leídos"), (Filter::Groups, "Grupos")];

    fn keep(self, c: &ChatSummary) -> bool {
        match self {
            Filter::All => true,
            Filter::Favorites => c.important,
            Filter::Unread => c.unread > 0,
            Filter::Groups => c.is_group,
            Filter::Label(id) => c.labels.contains(&id),
        }
    }
}

pub struct WhatsAppPanel {
    status: Status,
    /// Every chat (matching the search), before the chip filter.
    all: Rc<Vec<ChatSummary>>,
    /// What the list shows.
    chats: Rc<Vec<ChatSummary>>,
    /// Kind chip (Favoritos / No leídos / Grupos; `All` = none).
    filter: Filter,
    /// Label chip, combined (AND) with the kind chip.
    label: Option<i64>,
    /// Search box non-empty at the last reload (filtering → show every chat).
    searching: bool,
    /// "Ver más" clicked: show chats older than [`RECENT_SECS`] in the unfiltered list.
    show_old: bool,
    /// Chats hidden from the unfiltered list for being old.
    hidden_old: usize,
    /// Showing the archived chats instead of the inbox.
    archived_view: bool,
    archived_count: usize,
    /// Unread in important, non-archived chats (badge), whatever the view.
    unread_total: usize,
    hits: Rc<Vec<ChatMessage>>,
    query: Entity<TextInput>,
    qr: Option<std::path::PathBuf>,
    reload_pending: bool,
    scroll: gpui::UniformListScrollHandle,
    /// Typing indicators by chat (cleared after a few seconds).
    typing: std::collections::HashMap<String, (Option<String>, std::time::Instant)>,
    /// Right-click menu on a chat row: (position, jid, currently important).
    /// Right-click menu on chat rows: (position, the chats it acts on).
    menu: Option<(gpui::Point<gpui::Pixels>, Vec<String>)>,
    /// Chats picked with ⌘-click / ⇧-click for bulk actions (archive, labels, favourites).
    selected: std::collections::HashSet<String>,
    /// Last ⌘/plain-clicked row: ⇧-click selects the range from it.
    select_anchor: Option<usize>,
    /// Chat shown in the foreground tab (highlighted in the list).
    active_chat: Option<String>,
    /// Right-click menu on a label chip: (position, label id).
    label_menu: Option<(gpui::Point<gpui::Pixels>, i64)>,
    /// Projects (id, name) offered in the label menu, read when it opens.
    menu_projects: Vec<(String, String)>,
    labels: Rc<Vec<Label>>,
    /// Name of a new label, typed inside the chat menu.
    label_input: Entity<TextInput>,
    creating_label: bool,
    /// Instruction for Claude about the chat, typed inside the chat menu.
    ask_input: Entity<TextInput>,
    asking: bool,
    _ask_sub: Subscription,
    _listen: Task<()>,
    _reload: Option<Task<()>>,
    _sub: Subscription,
    _label_sub: Subscription,
}

impl EventEmitter<WhatsAppPanelEvent> for WhatsAppPanel {}

impl WhatsAppPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let wa = WhatsApp::global();
        let mut rx = wa.subscribe();
        let listen = cx.spawn(async move |this, cx| {
            while let Some(u) = rx.next().await {
                if this.update(cx, |p, cx| p.on_update(u, cx)).is_err() {
                    break;
                }
            }
        });
        let query = cx.new(|cx| TextInput::new("Buscar chat o mensaje", cx));
        let sub = cx.subscribe(&query, |this, _, e: &TextInputEvent, cx| match e {
            TextInputEvent::Changed => this.schedule_reload(cx),
            TextInputEvent::Cancel => {
                this.query.update(cx, |q, cx| q.set_text("", cx));
            }
            TextInputEvent::Submit { .. } => {
                if let Some(c) = this.chats.first().cloned() {
                    cx.emit(WhatsAppPanelEvent::OpenChat { jid: c.jid, name: c.name });
                }
            }
        });
        let ask_input = cx.new(|cx| TextInput::new("¿Qué hago con este chat?", cx));
        let ask_sub = cx.subscribe(&ask_input, |this, input, e: &TextInputEvent, cx| match e {
            TextInputEvent::Submit { .. } => {
                let instruction = input.read(cx).content.trim().to_string();
                if let (Some((_, targets)), false) = (this.menu.clone(), instruction.is_empty())
                    && let Some(jid) = targets.first()
                {
                    cx.emit(WhatsAppPanelEvent::AskClaude { jid: jid.clone(), instruction });
                }
                input.update(cx, |i, cx| i.set_text("", cx));
                this.asking = false;
                this.menu = None;
                cx.notify();
            }
            TextInputEvent::Cancel => {
                input.update(cx, |i, cx| i.set_text("", cx));
                this.asking = false;
                cx.notify();
            }
            TextInputEvent::Changed => {}
        });
        let label_input = cx.new(|cx| TextInput::new("Nueva etiqueta", cx));
        let label_sub = cx.subscribe(&label_input, |this, input, e: &TextInputEvent, cx| match e {
            TextInputEvent::Submit { .. } => {
                let name = input.read(cx).content.trim().to_string();
                if let (Some((_, targets)), false) = (this.menu.clone(), name.is_empty()) {
                    let wa = WhatsApp::global();
                    if let Some(id) = wa.create_label(&name) {
                        for jid in &targets {
                            wa.set_chat_label(jid, id, true);
                        }
                    }
                }
                input.update(cx, |i, cx| i.set_text("", cx));
                this.creating_label = false;
                this.menu = None;
                cx.notify();
            }
            TextInputEvent::Cancel => {
                input.update(cx, |i, cx| i.set_text("", cx));
                this.creating_label = false;
                cx.notify();
            }
            TextInputEvent::Changed => {}
        });
        let mut this = Self {
            status: wa.status(),
            all: Rc::new(Vec::new()),
            chats: Rc::new(Vec::new()),
            filter: Filter::All,
            label: None,
            searching: false,
            show_old: false,
            hidden_old: 0,
            archived_view: false,
            archived_count: 0,
            unread_total: 0,
            hits: Rc::new(Vec::new()),
            query,
            qr: None,
            reload_pending: false,
            scroll: gpui::UniformListScrollHandle::new(),
            typing: Default::default(),
            menu: None,
            selected: Default::default(),
            select_anchor: None,
            active_chat: None,
            label_menu: None,
            menu_projects: Vec::new(),
            labels: Rc::new(Vec::new()),
            label_input,
            creating_label: false,
            ask_input,
            asking: false,
            _ask_sub: ask_sub,
            _listen: listen,
            _reload: None,
            _sub: sub,
            _label_sub: label_sub,
        };
        this.update_qr();
        this.reload(cx);
        this
    }

    /// Only chats marked important count (the rest would just be noise).
    pub fn total_unread(&self) -> usize {
        self.unread_total
    }

    fn on_update(&mut self, u: Update, cx: &mut Context<Self>) {
        match u {
            Update::Status(s) => {
                self.status = s;
                self.update_qr();
                self.schedule_reload(cx);
                cx.notify();
            }
            Update::Chats | Update::Avatar(_) => self.schedule_reload(cx),
            Update::Typing { chat, who } => {
                match who {
                    Some(w) => {
                        self.typing.insert(chat, (Some(w).filter(|w| !w.is_empty()), std::time::Instant::now()));
                    }
                    None => {
                        self.typing.remove(&chat);
                    }
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn update_qr(&mut self) {
        self.qr = match &self.status {
            Status::Qr(code) => wa_ui::qr_png(code, WhatsApp::global().dir()),
            _ => None,
        };
    }

    /// Coalesce bursts (history sync sends many updates) into one reload.
    fn schedule_reload(&mut self, cx: &mut Context<Self>) {
        if self.reload_pending {
            return;
        }
        self.reload_pending = true;
        self._reload = Some(cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(200)).await;
            let _ = this.update(cx, |this, cx| {
                this.reload_pending = false;
                this.reload(cx);
            });
        }));
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let q = self.query.read(cx).content.trim().to_string();
        let searching = !q.is_empty();
        let archived = self.archived_view;
        let task = cx.background_executor().spawn(async move {
            let store = WhatsApp::global().store();
            let mut chats = if archived { store.archived_chats() } else { store.chats() };
            let unread = if archived { store.total_unread() as usize } else { chats.iter().filter(|c| c.important).map(|c| c.unread as usize).sum() };
            let archived_count = store.archived_count();
            let mut hits = Vec::new();
            if !q.is_empty() {
                let lower = q.to_lowercase();
                chats.retain(|c| c.name.to_lowercase().contains(&lower) || c.jid.contains(&lower));
                if q.chars().count() >= 2 {
                    hits = store.search(&q, None, 50);
                }
            }
            (chats, hits, store.labels(), unread, archived_count)
        });
        cx.spawn(async move |this, cx| {
            let (chats, hits, labels, unread, archived_count) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.archived_view != archived {
                    return; // view switched meanwhile; a newer reload is coming
                }
                this.unread_total = unread;
                this.archived_count = archived_count;
                if archived && archived_count == 0 {
                    this.archived_view = false;
                    this.reload(cx);
                }
                if let Some(id) = this.label
                    && id != NO_LABEL
                    && !labels.iter().any(|l| l.id == id)
                {
                    this.label = None;
                }
                this.labels = Rc::new(labels);
                this.all = Rc::new(chats);
                // Selection survives reloads but drops chats that left the list (archived…).
                let all = this.all.clone();
                this.selected.retain(|j| all.iter().any(|c| &c.jid == j));
                this.searching = searching;
                this.apply_filter();
                this.hits = Rc::new(hits);
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_filter(&mut self) {
        let (f, label) = (self.filter, self.label);
        let mut chats: Vec<ChatSummary> = self.all.iter().filter(|c| f.keep(c) && label.is_none_or(|l| if l == NO_LABEL { c.labels.is_empty() } else { c.labels.contains(&l) })).cloned().collect();
        self.hidden_old = 0;
        if f == Filter::All && label.is_none() && !self.searching && !self.show_old && !self.archived_view {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            let before = chats.len();
            chats.retain(|c| c.pinned || c.unread > 0 || c.last_ts >= now - RECENT_SECS);
            self.hidden_old = before - chats.len();
        }
        self.chats = Rc::new(chats);
    }

    /// The workspace tells us which chat is in the foreground tab (None: none / not a chat).
    pub fn set_active_chat(&mut self, jid: Option<String>, cx: &mut Context<Self>) {
        if self.active_chat != jid {
            self.active_chat = jid;
            cx.notify();
        }
    }

    fn archive(&mut self, jids: &[String], archive: bool, cx: &mut Context<Self>) {
        let wa = WhatsApp::global();
        for j in jids {
            wa.set_archived(j, archive);
            self.selected.remove(j);
        }
        self.menu = None;
        cx.notify();
    }

    /// Bar shown while chats are selected: count + bulk actions.
    fn render_selection_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.selected.is_empty() {
            return None;
        }
        let n = self.selected.len();
        let btn = |id: &'static str, icon: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.))
                .px(px(8.))
                .h(px(26.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(theme::rgb(theme::TEXT))
                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                .child(wa_ui::icon(icon, 14., theme::TEXT_MUTED))
                .child(label)
        };
        let archive = !self.archived_view;
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(2.))
                .mx(px(8.))
                .mb(px(6.))
                .px(px(6.))
                .h(px(34.))
                .rounded(px(8.))
                .bg(theme::rgb(theme::SELECTED))
                .border_1()
                .border_color(theme::rgb(theme::BORDER))
                .text_size(px(12.))
                .child(div().flex_1().min_w_0().truncate().pl(px(4.)).font_weight(gpui::FontWeight::SEMIBOLD).child(format!("{n} seleccionado{}", if n == 1 { "" } else { "s" })))
                .child(btn("wa-sel-archive", "icons/archive.svg", if archive { "Archivar" } else { "Desarchivar" }).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let jids: Vec<String> = this.selected.iter().cloned().collect();
                    this.archive(&jids, archive, cx);
                })))
                .child(btn("wa-sel-label", "icons/tag.svg", "Etiqueta").on_click(cx.listener(|this, e: &ClickEvent, _, cx| {
                    let targets: Vec<String> = this.chats.iter().filter(|c| this.selected.contains(&c.jid)).map(|c| c.jid.clone()).collect();
                    let pos = e.position();
                    this.menu = Some((gpui::point(pos.x - px(110.), pos.y + px(14.)), targets));
                    this.creating_label = false;
                    cx.notify();
                })))
                .child(
                    div()
                        .id("wa-sel-clear")
                        .size(px(26.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                        .child(wa_ui::icon("icons/close.svg", 12., theme::TEXT_MUTED))
                        .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Quitar selección")).into())
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.selected.clear();
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn set_archived_view(&mut self, on: bool, cx: &mut Context<Self>) {
        self.selected.clear();
        self.archived_view = on;
        self.filter = Filter::All;
        self.label = None;
        self.all = Rc::new(Vec::new());
        self.chats = Rc::new(Vec::new());
        self.hidden_old = 0;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        self.reload(cx);
        cx.notify();
    }

    /// "Archivados (N)" entry in the inbox, or the way back from the archived list.
    fn render_archive_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let row = |id: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(10.))
                .mx(px(4.))
                .mb(px(4.))
                .px(px(10.))
                .h(px(32.))
                .rounded(px(6.))
                .flex_none()
                .cursor_pointer()
                .text_size(px(12.5))
                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
        };
        if self.archived_view {
            return Some(
                row("wa-archived-back")
                    .child(gpui::svg().path("icons/chevron-right.svg").size(px(14.)).text_color(theme::rgb(theme::TEXT_MUTED)).with_transformation(gpui::Transformation::rotate(gpui::radians(std::f32::consts::PI))))
                    .child(div().flex_1().font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child("Archivados"))
                    .child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(self.archived_count.to_string()))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_archived_view(false, cx)))
                    .into_any_element(),
            );
        }
        if self.archived_count == 0 {
            return None;
        }
        Some(
            row("wa-archived")
                .child(gpui::svg().path("icons/archive.svg").size(px(15.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                .child(div().flex_1().text_color(theme::rgb(theme::TEXT_MUTED)).child("Archivados"))
                .child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(self.archived_count.to_string()))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_archived_view(true, cx)))
                .into_any_element(),
        )
    }

    fn render_chips(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fixed = Filter::ALL.iter().map(|(f, l)| (*f, SharedString::from(*l), None));
        let none = (!self.labels.is_empty()).then(|| (Filter::Label(NO_LABEL), SharedString::from("Sin etiqueta"), None));
        let custom = self.labels.iter().map(|l| (Filter::Label(l.id), SharedString::from(l.name.clone()), Some(l.color)));
        let all: Vec<(Filter, SharedString, Option<u32>)> = fixed.chain(none).chain(custom).collect();
        // One row that scrolls sideways (trackpad / shift+wheel) instead of wrapping.
        div()
            .id("wa-chips")
            .flex()
            .gap(px(6.))
            .px(px(10.))
            .pb(px(8.))
            .flex_none()
            .w_full()
            .overflow_x_scroll()
            .children(all.into_iter().enumerate().map(|(i, (f, label, color))| {
                let active = match f {
                    Filter::All => self.filter == Filter::All && self.label.is_none(),
                    Filter::Label(id) => self.label == Some(id),
                    _ => self.filter == f,
                };
                let count = match f {
                    Filter::Unread => self.all.iter().filter(|c| c.unread > 0).count(),
                    _ => 0,
                };
                div()
                    .id(("wa-chip", i))
                    .flex_none()
                    .whitespace_nowrap()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(10.))
                    .h(px(24.))
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(active, |d| d.bg(theme::rgb(wa_ui::CHIP_ACTIVE_BG)).text_color(theme::rgb(wa_ui::CHIP_ACTIVE_TEXT)))
                    .when(!active, |d| d.bg(theme::rgb(theme::SELECTED)).text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT))))
                    .children(color.map(|c| div().size(px(7.)).rounded_full().bg(theme::rgb(c))))
                    .child(label)
                    .when(count > 0, |d| d.child(div().text_size(px(10.5)).opacity(0.8).child(count.to_string())))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        // Kind and label chips combine; clicking an active one turns it off.
                        match f {
                            Filter::All => {
                                this.filter = Filter::All;
                                this.label = None;
                            }
                            Filter::Label(id) => this.label = if active { None } else { Some(id) },
                            _ => this.filter = if active { Filter::All } else { f },
                        }
                        this.apply_filter();
                        this.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                        cx.notify();
                    }))
                    .when_some(match f {
                        Filter::Label(id) if id != NO_LABEL => Some(id),
                        _ => None,
                    }, |d, id| {
                        d.on_mouse_down(
                            gpui::MouseButton::Right,
                            cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                                this.label_menu = Some((e.position, id));
                                this.menu_projects = workspace_state::StateStore::default_location().load().projects.into_iter().map(|p| (p.id, p.name)).collect();
                                cx.notify();
                            }),
                        )
                    })
            }))
            .into_any_element()
    }

    /// Offline with a linked session: the cached chats stay usable; what's written is queued.
    fn offline_banner(&self) -> bool {
        !crate::net::online() && !matches!(self.status, Status::Idle | Status::Qr(_) | Status::QrExpired | Status::LoggedOut(_))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let link_button = |label: &'static str| {
            div()
                .id("wa-link")
                .mt(px(10.))
                .px(px(14.))
                .py(px(7.))
                .rounded(px(6.))
                .cursor_pointer()
                .bg(theme::rgb(0x1f7a4d))
                .hover(|s| s.bg(theme::rgb(0x23905a)))
                .text_size(px(13.))
                .text_color(gpui::white())
                .child(label)
                .on_click(|_: &ClickEvent, _, _| WhatsApp::global().start())
        };
        let card = |title: SharedString, body: SharedString| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.))
                .px(px(18.))
                .py(px(24.))
                .text_center()
                .child(gpui::svg().path("icons/whatsapp.svg").size(px(36.)).text_color(theme::rgb(wa_ui::GREEN)))
                .child(div().text_size(px(14.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(title))
                .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(body))
        };
        let has_chats = !self.all.is_empty();
        if self.offline_banner() {
            return Some(if has_chats {
                banner("Sin internet: puedes leer y escribir; los mensajes se envían al reconectar.", theme::TEXT_MUTED).into_any_element()
            } else {
                card("Sin conexión a internet".into(), "WhatsApp se conectará solo cuando vuelva la red.".into()).into_any_element()
            });
        }
        let el = match &self.status {
            Status::Connected => return None,
            Status::Connecting if has_chats => {
                return Some(banner("Conectando…", theme::TEXT_MUTED).into_any_element());
            }
            Status::Connecting => card("Conectando…".into(), "Recuperando la sesión de WhatsApp.".into()).into_any_element(),
            Status::Starting => card("Generando código…".into(), "".into()).into_any_element(),
            Status::Qr(_) => {
                let qr = self.qr.clone();
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(18.))
                    .child(div().text_size(px(14.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child("Vincular dispositivo"))
                    .children(qr.map(|p| div().p(px(6.)).bg(gpui::white()).rounded(px(8.)).child(gpui::img(p).size(px(220.)))))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::rgb(theme::TEXT_MUTED))
                            .text_center()
                            .child("En tu teléfono abre WhatsApp → Configuración → Dispositivos vinculados → Vincular un dispositivo, y escanea este código."),
                    )
                    .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("El código se renueva solo."))
                    .into_any_element()
            }
            Status::QrExpired => card("El código expiró".into(), "No se escaneó a tiempo.".into()).child(link_button("Generar otro código")).into_any_element(),
            Status::Idle => card(
                "WhatsApp".into(),
                "Vincula tu WhatsApp para ver y responder tus chats desde aquí. Las conversaciones se guardan en este Mac.".into(),
            )
            .child(link_button("Vincular dispositivo"))
            .into_any_element(),
            Status::LoggedOut(why) => card("Sesión cerrada".into(), why.clone().into()).child(link_button("Vincular de nuevo")).into_any_element(),
            Status::Blocked(why) => card("Desconectado".into(), why.clone().into()).child(link_button("Reintentar")).into_any_element(),
            Status::Error(e) if has_chats => {
                return Some(banner(e, theme::DELETED).child(div().id("wa-retry").cursor_pointer().text_color(theme::rgb(theme::ACCENT)).child("Reintentar").on_click(|_: &ClickEvent, _, _| WhatsApp::global().start())).into_any_element());
            }
            Status::Error(e) => card("Error".into(), e.clone().into()).child(link_button("Reintentar")).into_any_element(),
        };
        let _ = cx;
        Some(el)
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        if ix == self.chats.len() && self.hidden_old > 0 {
            return div()
                .id("wa-more")
                .w_full()
                .h(px(60.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(theme::rgb(theme::TEXT_MUTED))
                .hover(|s| s.bg(theme::rgb(theme::HOVER)).text_color(theme::rgb(theme::TEXT)))
                .child(format!("Ver más ({} chats con más de 30 días)", self.hidden_old))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_old = true;
                    this.apply_filter();
                    cx.notify();
                }))
                .into_any_element();
        }
        let Some(c) = self.chats.get(ix).cloned() else { return div().into_any_element() };
        let typing = self.typing.get(&c.jid).filter(|(_, t)| t.elapsed() < Duration::from_secs(8)).map(|(w, _)| match w {
            Some(w) => format!("{} está escribiendo…", w.trim_start_matches('~')),
            None => "escribiendo…".to_string(),
        });
        let unread = c.unread > 0;
        let (jid, name) = (c.jid.clone(), c.name.clone());
        let menu_jid = c.jid.clone();
        let picked = self.selected.contains(&c.jid);
        let current = self.active_chat.as_deref() == Some(c.jid.as_str());
        div()
            .id(("wa-chat", ix))
            .w_full()
            .when(current && !picked, |d| d.bg(theme::rgb(theme::SELECTED)))
            .when(picked, |d| d.bg(theme::rgb(wa_ui::CHIP_ACTIVE_BG)))
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(60.))
            .px(px(12.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
            .child(wa_ui::avatar(c.avatar.as_ref(), &c.name, 40.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.5))
                                    .text_color(theme::rgb(theme::TEXT))
                                    .when(unread, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                                    .child(c.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .text_color(theme::rgb(if unread { wa_ui::GREEN } else { theme::TEXT_FAINT }))
                                    .child(wa_ui::list_time(c.last_ts)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .when(c.last_from_me && typing.is_none(), |d| d.children(wa_ui::ticks(c.last_status)))
                            .child(match typing {
                                Some(t) => div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(theme::rgb(wa_ui::GREEN)).child(t),
                                None => div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(3.))
                                    .text_size(px(12.))
                                    .text_color(theme::rgb(theme::TEXT_MUTED))
                                    .when(!c.last_sender.is_empty(), |d| d.child(div().flex_none().max_w(px(110.)).truncate().child(format!("{}:", c.last_sender))))
                                    .children(wa_ui::kind_icon(c.last_kind).map(|p| wa_ui::icon(p, 13., theme::TEXT_MUTED)))
                                    .child(div().flex_1().min_w_0().truncate().when(c.last_kind == MsgKind::Revoked, |d| d.italic()).child(c.last_text.clone())),
                            })
                            .when(c.muted, |d| d.child(wa_ui::icon("icons/wa-muted.svg", 13., theme::TEXT_FAINT)))
                            .when(c.pinned, |d| d.child(wa_ui::icon("icons/wa-pin.svg", 13., theme::TEXT_FAINT)))
                            .when(c.important, |d| d.child(wa_ui::icon("icons/wa-star.svg", 12., 0xe5c07b)))
                            .when(unread, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .min_w(px(18.))
                                        .h(px(18.))
                                        .px(px(5.))
                                        .rounded_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .bg(theme::rgb(if c.important { wa_ui::GREEN } else { theme::TEXT_FAINT }))
                                        .text_size(px(10.5))
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .text_color(theme::rgb(0x0b141a))
                                        .child(if c.unread > 999 { "999+".to_string() } else { c.unread.to_string() }),
                                )
                            }),
                    ),
            )
            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                let m = e.modifiers();
                if m.platform || m.control {
                    // ⌘-click toggles the row in the selection.
                    if !this.selected.remove(&jid) {
                        this.selected.insert(jid.clone());
                    }
                    this.select_anchor = Some(ix);
                    cx.notify();
                } else if m.shift {
                    let from = this.select_anchor.unwrap_or(ix);
                    let (a, b) = (from.min(ix), from.max(ix));
                    for c in this.chats.iter().skip(a).take(b - a + 1) {
                        this.selected.insert(c.jid.clone());
                    }
                    cx.notify();
                } else if !this.selected.is_empty() {
                    // While selecting, a plain click keeps adding instead of opening.
                    if !this.selected.remove(&jid) {
                        this.selected.insert(jid.clone());
                    }
                    this.select_anchor = Some(ix);
                    cx.notify();
                } else {
                    this.select_anchor = Some(ix);
                    cx.emit(WhatsAppPanelEvent::OpenChat { jid: jid.clone(), name: name.clone() });
                }
            }))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                    // On a selected row the menu acts on the whole selection.
                    let targets = if this.selected.contains(&menu_jid) {
                        this.chats.iter().filter(|c| this.selected.contains(&c.jid)).map(|c| c.jid.clone()).collect()
                    } else {
                        vec![menu_jid.clone()]
                    };
                    this.menu = Some((e.position, targets));
                    this.creating_label = false;
                    this.asking = false;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn render_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if let Some((pos, id)) = self.label_menu {
            let label = self.labels.iter().find(|l| l.id == id);
            let name = label.map(|l| l.name.clone()).unwrap_or_default();
            let linked = label.and_then(|l| l.project.clone());
            let mut list = menu_box(cx.listener(|this, _, _, cx| {
                this.label_menu = None;
                cx.notify();
            }))
            .child(div().px(px(12.)).py(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("PROYECTO (CONTEXTO PARA CLAUDE)"));
            let options = std::iter::once((None, "Ninguno".to_string())).chain(self.menu_projects.iter().map(|(pid, pname)| (Some(pid.clone()), pname.clone())));
            for (i, (pid, pname)) in options.enumerate() {
                let on = pid == linked;
                list = list.child(
                    menu_row(("wa-label-project", i))
                        .child(div().flex_1().min_w_0().truncate().child(pname))
                        .child(div().w(px(14.)).flex_none().flex().justify_center().when(on, |d| d.child(wa_ui::icon("icons/wa-check.svg", 13., wa_ui::GREEN))))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            WhatsApp::global().set_label_project(id, pid.as_deref());
                            this.label_menu = None;
                            cx.notify();
                        })),
                );
            }
            let list = list
            .child(div().h(px(1.)).my(px(4.)).bg(theme::rgb(theme::BORDER)))
            .child(menu_item("wa-del-label", format!("Eliminar etiqueta “{name}”")).text_color(theme::rgb(theme::DELETED)).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                WhatsApp::global().delete_label(id);
                this.label_menu = None;
                cx.notify();
            })));
            return Some(gpui::deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element());
        }
        let (pos, targets) = self.menu.clone()?;
        let chats: Vec<&ChatSummary> = targets.iter().filter_map(|j| self.all.iter().find(|c| &c.jid == j)).collect();
        let n = targets.len();
        let all_important = !chats.is_empty() && chats.iter().all(|c| c.important);
        let label = match (all_important, n) {
            (true, 1) => "Quitar de favoritos".to_string(),
            (true, _) => format!("Quitar {n} chats de favoritos"),
            (false, 1) => "Marcar como favorito".to_string(),
            (false, _) => format!("Marcar {n} chats como favoritos"),
        };
        let archive = !self.archived_view;
        let archive_label = match (archive, n) {
            (true, 1) => "Archivar chat".to_string(),
            (true, _) => format!("Archivar {n} chats"),
            (false, 1) => "Desarchivar chat".to_string(),
            (false, _) => format!("Desarchivar {n} chats"),
        };
        let t = targets.clone();
        let mut list = menu_box(cx.listener(|this, _, _, cx| {
            this.menu = None;
            this.creating_label = false;
            this.asking = false;
            cx.notify();
        }));
        if self.asking {
            let name = chats.first().map(|c| c.name.clone()).unwrap_or_default();
            list = list
                .min_w(px(320.))
                .child(div().px(px(12.)).py(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("PEDIR A CLAUDE · {}", name.to_uppercase())))
                .child(
                    div()
                        .mx(px(6.))
                        .my(px(3.))
                        .h(px(28.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .bg(theme::rgb(theme::SELECTED))
                        .text_size(px(12.5))
                        .child(self.ask_input.clone()),
                )
                .child(div().px(px(12.)).py(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Enter abre Claude Code con el chat (y su proyecto) como contexto"));
            return Some(gpui::deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element());
        }
        if n == 1 {
            list = list.child(
                menu_row("wa-ask-claude")
                    .child(wa_ui::icon("icons/wa-spark.svg", 14., 0xd97757))
                    .child("Pedir a Claude…")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.asking = true;
                        this.creating_label = false;
                        let f = this.ask_input.focus_handle(cx);
                        window.focus(&f);
                        cx.notify();
                    })),
            );
        }
        let mut list = list
        .child(menu_item("wa-important", label).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            for j in &t {
                WhatsApp::global().set_important(j, !all_important);
            }
            this.menu = None;
            cx.notify();
        })))
        .child({
            let t = targets.clone();
            menu_item("wa-archive", archive_label).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.archive(&t, archive, cx);
            }))
        })
        .child(div().h(px(1.)).my(px(4.)).bg(theme::rgb(theme::BORDER)))
        .child(div().px(px(12.)).py(px(2.)).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(if n > 1 { format!("ETIQUETAS ({n} CHATS)") } else { "ETIQUETAS".to_string() }));
        for (i, l) in self.labels.iter().enumerate() {
            let on = !chats.is_empty() && chats.iter().all(|c| c.labels.contains(&l.id));
            let some = chats.iter().any(|c| c.labels.contains(&l.id));
            let (t, id) = (targets.clone(), l.id);
            list = list.child(
                menu_row(("wa-label", i))
                    .child(div().size(px(9.)).flex_none().rounded_full().bg(theme::rgb(l.color)))
                    .child(div().flex_1().min_w_0().truncate().child(l.name.clone()))
                    .child(div().w(px(14.)).flex_none().flex().justify_center().map(|d| {
                        if on {
                            d.child(wa_ui::icon("icons/wa-check.svg", 13., wa_ui::GREEN))
                        } else if some {
                            d.child(div().w(px(8.)).h(px(1.5)).rounded_full().bg(theme::rgb(wa_ui::GREEN)))
                        } else {
                            d
                        }
                    }))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        for j in &t {
                            WhatsApp::global().set_chat_label(j, id, !on);
                        }
                        cx.notify();
                    })),
            );
        }
        list = if self.creating_label {
            list.child(
                div()
                    .mx(px(6.))
                    .my(px(3.))
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .bg(theme::rgb(theme::SELECTED))
                    .text_size(px(12.5))
                    .child(self.label_input.clone()),
            )
        } else {
            list.child(menu_item("wa-new-label", "＋ Nueva etiqueta…".to_string()).text_color(theme::rgb(theme::TEXT_MUTED)).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.creating_label = true;
                let f = this.label_input.focus_handle(cx);
                window.focus(&f);
                cx.notify();
            })))
        };
        let _ = window;
        Some(gpui::deferred(gpui::anchored().position(pos).snap_to_window_with_margin(px(8.)).child(list)).with_priority(1).into_any_element())
    }
}

fn menu_box(on_out: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut gpui::App) + 'static) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .min_w(px(220.))
        .p(px(4.))
        .rounded(px(8.))
        .bg(theme::rgb(theme::SIDEBAR_BG))
        .border_1()
        .border_color(theme::rgb(theme::BORDER))
        .shadow_lg()
        .on_mouse_down_out(on_out)
}

fn menu_item(id: impl Into<gpui::ElementId>, label: String) -> gpui::Stateful<gpui::Div> {
    menu_row(id).child(label)
}

fn menu_row(id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .py(px(5.))
        .rounded(px(4.))
        .text_size(px(13.))
        .text_color(theme::rgb(theme::TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
}

fn banner(text: &str, color: u32) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .py(px(6.))
        .text_size(px(11.5))
        .text_color(theme::rgb(color))
        .border_b_1()
        .border_color(theme::rgb(theme::BORDER))
        .child(div().flex_1().min_w_0().child(text.to_string()))
}

impl Render for WhatsAppPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let status = self.render_status(cx);
        let linked = matches!(self.status, Status::Connected | Status::Connecting | Status::Error(_)) || !self.all.is_empty();
        let full_card = status.is_some() && (self.all.is_empty() || (!self.offline_banner() && !matches!(self.status, Status::Connecting | Status::Error(_))));
        let count = self.chats.len();
        let rows = count + (self.hidden_old > 0) as usize;
        let hits = self.hits.clone();
        let searching = !self.query.read(cx).content.trim().is_empty();
        let menu = self.render_menu(window, cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .children(menu)
            .when(linked && !full_card, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(10.))
                        .pb(px(6.))
                        .flex_none()
                        .child(
                            div()
                                .flex_1()
                                .h(px(28.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .rounded(px(6.))
                                .bg(theme::rgb(theme::SELECTED))
                                .text_size(px(12.5))
                                .child(self.query.clone()),
                        ),
                )
                .children(self.render_archive_bar(cx))
                .when(!self.archived_view, |d| d.child(self.render_chips(cx)))
                .children(self.render_selection_bar(cx))
            })
            .children(status)
            .when(!full_card && count == 0 && (self.filter != Filter::All || self.label.is_some()) && !self.all.is_empty(), |d| {
                d.child(
                    div().px(px(16.)).py(px(18.)).text_center().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(match (self.filter, self.label) {
                        (Filter::All, Some(NO_LABEL)) => "Todos los chats tienen alguna etiqueta.",
                        (Filter::All, Some(_)) => "Ningún chat tiene esta etiqueta. Clic derecho en un chat → Etiquetas.",
                        (_, Some(_)) => "Ningún chat cumple los dos filtros.",
                        (Filter::Favorites, _) => "No hay chats favoritos. Clic derecho en un chat → Marcar como favorito.",
                        (Filter::Unread, _) => "No hay chats sin leer.",
                        _ => "No hay grupos.",
                    }),
                )
            })
            .when(!full_card, |d| {
                d.child(
                    uniform_list("wa-chats", rows, cx.processor(|this, range: std::ops::Range<usize>, _, cx| range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()))
                        .track_scroll(self.scroll.clone())
                        .when(!searching || hits.is_empty(), |d| d.flex_1())
                        .when(searching && !hits.is_empty(), |d| d.h(px((count.min(4) as f32) * 60.)))
                        .w_full(),
                )
                .when(searching && !hits.is_empty(), |d| {
                    d.child(
                        div()
                            .px(px(12.))
                            .py(px(6.))
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::rgb(theme::TEXT_FAINT))
                            .child("MENSAJES"),
                    )
                    .child(
                        div().id("wa-hits").flex_1().min_h_0().overflow_y_scroll().children(hits.iter().enumerate().map(|(i, m)| {
                            let (jid, chat_name) = (m.chat.clone(), WhatsApp::global().store().display_name(&m.chat));
                            let name = chat_name.clone();
                            div()
                                .id(("wa-hit", i))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .px(px(12.))
                                .py(px(7.))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .text_size(px(12.5))
                                        .child(div().truncate().text_color(theme::rgb(theme::TEXT)).child(chat_name))
                                        .child(div().flex_none().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(wa_ui::list_time(m.ts))),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(theme::rgb(theme::TEXT_MUTED))
                                        .line_clamp(2)
                                        .child(format!("{}{}", if m.from_me { "Tú: " } else { "" }, m.preview())),
                                )
                                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                    cx.emit(WhatsAppPanelEvent::OpenChat { jid: jid.clone(), name: name.clone() });
                                }))
                        })),
                    )
                })
            })
    }
}
