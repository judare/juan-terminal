//! "HTTP" tab: collections tree on the left, the selected request (method, URL, headers, body,
//! auth) on the right, the response below. Edits are saved to the collection file shortly
//! after typing stops.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use gpui::{
    ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight, PromptLevel, Render, SharedString, Subscription, Task, WeakEntity, Window, div,
    prelude::*, px,
};

use super::store::{self, Auth, BodyMode, Collection, Environment, Form, Response};
use crate::text_area::{TextArea, TextAreaEvent};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;
use crate::workspace::Workspace;

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
/// Response lines rendered (the rest: "Abrir en editor").
const MAX_LINES: usize = 3000;

pub fn method_color(m: &str) -> u32 {
    match m {
        "GET" => theme::ADDED,
        "POST" => theme::MODIFIED,
        "PUT" => theme::INFO,
        "PATCH" => 0xc678dd,
        "DELETE" => theme::DELETED,
        _ => theme::TEXT_MUTED,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ReqTab {
    Headers,
    Body,
    Auth,
}

#[derive(Clone, Copy, PartialEq)]
enum RespTab {
    Body,
    Headers,
}

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Method,
    Env,
}

#[derive(Clone, Copy, PartialEq)]
enum AuthKind {
    Inherit,
    None,
    Bearer,
    Basic,
    ApiKey,
    Other,
}

const AUTH_KINDS: [(AuthKind, &str); 5] = [(AuthKind::Inherit, "Heredada"), (AuthKind::None, "Sin auth"), (AuthKind::Bearer, "Bearer"), (AuthKind::Basic, "Basic"), (AuthKind::ApiKey, "API key")];

#[derive(Clone)]
enum Row {
    Collection(usize),
    Folder(usize, Vec<usize>, usize),
    Request(usize, Vec<usize>, usize),
}

type Key = (PathBuf, Vec<usize>);

pub struct HttpPage {
    ws: WeakEntity<Workspace>,
    collections: Vec<Collection>,
    envs: Vec<Environment>,
    active_env: Option<String>,
    expanded: HashSet<Key>,
    selected: Option<Key>,
    /// The form as last loaded / saved (edits are compared against it).
    loaded: Option<Form>,
    other_auth: Option<String>,
    name: Entity<TextInput>,
    url: Entity<TextInput>,
    headers: Entity<TextArea>,
    body: Entity<TextArea>,
    auth_a: Entity<TextInput>,
    auth_b: Entity<TextInput>,
    method: String,
    body_mode: BodyMode,
    auth_kind: AuthKind,
    apikey_query: bool,
    tab: ReqTab,
    menu: Option<Menu>,
    save_task: Option<Task<()>>,
    sending: Option<Arc<AtomicU32>>,
    send_task: Option<Task<()>>,
    response: Option<(Response, String)>,
    resp_tab: RespTab,
    message: Option<(SharedString, bool)>,
    pub focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl HttpPage {
    pub fn new(ws: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new("Nombre del request", cx));
        let url = cx.new(|cx| TextInput::new("{{base_url}}/recurso", cx));
        let headers = cx.new(|cx| TextArea::new("Content-Type: application/json\nX-Api-Key: {{api_key}}", 4, 14, cx));
        let body = cx.new(|cx| TextArea::new("{\n  \"clave\": \"valor\"\n}", 6, 18, cx));
        let auth_a = cx.new(|cx| TextInput::new("", cx));
        let auth_b = cx.new(|cx| TextInput::new("", cx));
        let mut subs = Vec::new();
        for input in [&name, &url, &auth_a, &auth_b] {
            let is_url = input == &url;
            subs.push(cx.subscribe_in(input, window, move |this, _, e, _, cx| match e {
                TextInputEvent::Changed => this.edited(cx),
                TextInputEvent::Submit { .. } if is_url => this.send(cx),
                _ => {}
            }));
        }
        for area in [&headers, &body] {
            subs.push(cx.subscribe_in(area, window, |this, _, e, _, cx| match e {
                TextAreaEvent::Changed => this.edited(cx),
                TextAreaEvent::Submit => this.send(cx),
                _ => {}
            }));
        }
        let mut this = Self {
            ws,
            collections: Vec::new(),
            envs: Vec::new(),
            active_env: store::active_environment(),
            expanded: HashSet::new(),
            selected: None,
            loaded: None,
            other_auth: None,
            name,
            url,
            headers,
            body,
            auth_a,
            auth_b,
            method: "GET".into(),
            body_mode: BodyMode::None,
            auth_kind: AuthKind::Inherit,
            apikey_query: false,
            tab: ReqTab::Headers,
            menu: None,
            save_task: None,
            sending: None,
            send_task: None,
            response: None,
            resp_tab: RespTab::Body,
            message: None,
            focus: cx.focus_handle(),
            _subs: subs,
        };
        this.reload(cx);
        // Open the first request of the first collection.
        if let Some(c) = this.collections.first() {
            this.expanded.insert((c.file.clone(), Vec::new()));
            if let Some(path) = first_request(c, &[]) {
                let file = c.file.clone();
                this.select(file, path, window, cx);
            }
        }
        this
    }

    /// Re-read collections and environments from disk (after an import elsewhere).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.flush_save(cx);
        self.collections = store::load_collections();
        self.envs = store::load_environments();
        self.active_env = store::active_environment().filter(|id| self.envs.iter().any(|e| &e.id() == id));
        if let Some((file, _)) = &self.selected {
            if !self.collections.iter().any(|c| &c.file == file) {
                self.selected = None;
                self.loaded = None;
            }
        }
        cx.notify();
    }

    /// Show a collection just imported (expanded, first request open).
    pub fn reveal_collection(&mut self, file: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
        let Some(c) = self.collections.iter().find(|c| c.file == file) else { return };
        self.expanded.insert((c.file.clone(), Vec::new()));
        if let Some(path) = first_request(c, &[]) {
            let f = c.file.clone();
            self.select(f, path, window, cx);
        }
    }

    pub fn set_active_env_by_file(&mut self, file: &std::path::Path, cx: &mut Context<Self>) {
        self.reload(cx);
        if let Some(e) = self.envs.iter().find(|e| e.file == file) {
            let id = e.id();
            store::set_active_environment(Some(&id));
            self.active_env = Some(id);
        }
        cx.notify();
    }

    fn coll_ix(&self, file: &std::path::Path) -> Option<usize> {
        self.collections.iter().position(|c| c.file == file)
    }

    fn env(&self) -> Option<&Environment> {
        let id = self.active_env.as_ref()?;
        self.envs.iter().find(|e| &e.id() == id)
    }

    // ---- form <-> inputs ------------------------------------------------------------------

    fn select(&mut self, file: PathBuf, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_save(cx);
        let Some(ci) = self.coll_ix(&file) else { return };
        let Some(item) = self.collections[ci].item(&path) else { return };
        let form = Form::from_item(item);
        // Expand the folders above it.
        for depth in 0..path.len() {
            self.expanded.insert((file.clone(), path[..depth].to_vec()));
        }
        self.selected = Some((file, path));
        self.fill(&form, cx);
        self.loaded = Some(form);
        self.response = None;
        self.message = None;
        self.menu = None;
        window.focus(&self.url.focus_handle(cx));
        cx.notify();
    }

    fn fill(&mut self, f: &Form, cx: &mut Context<Self>) {
        self.method = f.method.clone();
        self.body_mode = f.body_mode;
        self.other_auth = None;
        let (kind, a, b) = match &f.auth {
            Auth::Inherit => (AuthKind::Inherit, String::new(), String::new()),
            Auth::None => (AuthKind::None, String::new(), String::new()),
            Auth::Bearer(t) => (AuthKind::Bearer, t.clone(), String::new()),
            Auth::Basic(u, p) => (AuthKind::Basic, u.clone(), p.clone()),
            Auth::ApiKey { key, value, in_query } => {
                self.apikey_query = *in_query;
                (AuthKind::ApiKey, key.clone(), value.clone())
            }
            Auth::Other(t) => {
                self.other_auth = Some(t.clone());
                (AuthKind::Other, String::new(), String::new())
            }
        };
        self.auth_kind = kind;
        self.set_auth_placeholders(cx);
        self.name.update(cx, |i, cx| i.set_text(&f.name, cx));
        self.url.update(cx, |i, cx| {
            i.set_text(&f.url, cx);
            i.move_end(cx);
        });
        self.headers.update(cx, |i, cx| i.set_text(&f.headers, cx));
        self.body.update(cx, |i, cx| i.set_text(&f.body, cx));
        self.auth_a.update(cx, |i, cx| i.set_text(&a, cx));
        self.auth_b.update(cx, |i, cx| i.set_text(&b, cx));
        if f.body_mode != BodyMode::None && f.headers.trim().is_empty() {
            self.tab = ReqTab::Body;
        }
    }

    fn set_auth_placeholders(&mut self, cx: &mut Context<Self>) {
        let (a, b) = match self.auth_kind {
            AuthKind::Bearer => ("Token  (p. ej. {{token}})", ""),
            AuthKind::Basic => ("Usuario", "Contraseña"),
            AuthKind::ApiKey => ("Nombre (p. ej. X-Api-Key)", "Valor  (p. ej. {{api_key}})"),
            _ => ("", ""),
        };
        self.auth_a.update(cx, |i, cx| i.set_placeholder(a, cx));
        self.auth_b.update(cx, |i, cx| i.set_placeholder(b, cx));
    }

    fn form(&self, cx: &gpui::App) -> Form {
        let a = self.auth_a.read(cx).content.clone();
        let b = self.auth_b.read(cx).content.clone();
        Form {
            name: self.name.read(cx).content.trim().to_string(),
            method: self.method.clone(),
            url: self.url.read(cx).content.clone(),
            headers: self.headers.read(cx).content.clone(),
            body_mode: self.body_mode,
            body: self.body.read(cx).content.clone(),
            auth: match self.auth_kind {
                AuthKind::Inherit => Auth::Inherit,
                AuthKind::None => Auth::None,
                AuthKind::Bearer => Auth::Bearer(a),
                AuthKind::Basic => Auth::Basic(a, b),
                AuthKind::ApiKey => Auth::ApiKey { key: a, value: b, in_query: self.apikey_query },
                AuthKind::Other => Auth::Other(self.other_auth.clone().unwrap_or_default()),
            },
        }
    }

    /// An input changed: save soon if the request differs from what's on disk.
    fn edited(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_none() || self.loaded.is_none() {
            return;
        }
        let form = self.form(cx);
        if Some(&form) == self.loaded.as_ref() {
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            let _ = this.update(cx, |this, cx| {
                this.save_task = None;
                this.save_now(cx);
            });
        }));
        cx.notify();
    }

    /// Write a pending edit now (before switching request or re-reading the files).
    fn flush_save(&mut self, cx: &mut Context<Self>) {
        if self.save_task.take().is_some() {
            self.save_now(cx);
        }
    }

    fn save_now(&mut self, cx: &mut Context<Self>) {
        let form = self.form(cx);
        if Some(&form) != self.loaded.as_ref() {
            self.write_form(&form);
            cx.notify();
        }
    }

    fn write_form(&mut self, form: &Form) {
        let Some((file, path)) = self.selected.clone() else { return };
        let Some(ci) = self.coll_ix(&file) else { return };
        let c = &mut self.collections[ci];
        let Some(item) = c.item_mut(&path) else { return };
        form.apply(item);
        match c.save() {
            Ok(()) => self.loaded = Some(form.clone()),
            Err(e) => self.message = Some((format!("No se pudo guardar: {e:#}").into(), true)),
        }
    }

    // ---- sending ---------------------------------------------------------------------------

    fn vars(&self, ci: usize) -> Vec<(String, String)> {
        let mut v = self.collections[ci].variables();
        if let Some(e) = self.env() {
            v.extend(e.variables());
        }
        v
    }

    fn effective_auth(&self, form: &Form) -> Auth {
        match (&form.auth, &self.selected) {
            (Auth::Inherit, Some((file, path))) => self.coll_ix(file).map(|ci| self.collections[ci].inherited_auth(path)).unwrap_or(Auth::None),
            (a, _) => a.clone(),
        }
    }

    fn prepared(&self, cx: &gpui::App) -> Option<store::Prepared> {
        let (file, _) = self.selected.as_ref()?;
        let ci = self.coll_ix(file)?;
        let form = self.form(cx);
        let auth = self.effective_auth(&form);
        Some(store::prepare(&form, &auth, &self.vars(ci)))
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        if let Some(pid) = self.sending.take() {
            store::cancel(&pid);
            return;
        }
        self.save_now(cx);
        let Some(p) = self.prepared(cx) else { return };
        if p.url.trim().is_empty() {
            self.message = Some(("Escribe la URL".into(), true));
            cx.notify();
            return;
        }
        let pid = Arc::new(AtomicU32::new(0));
        self.sending = Some(pid.clone());
        self.message = None;
        cx.notify();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let resp = cx
                .background_executor()
                .spawn(async move {
                    let r = store::send(&p, pid);
                    let pretty = r.pretty_body();
                    (r, pretty)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.sending = None;
                this.response = Some(resp);
                cx.notify();
            });
        }));
    }

    fn copy_curl(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.prepared(cx) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(store::as_curl(&p)));
            self.message = Some(("Comando cURL copiado".into(), false));
            cx.notify();
        }
    }

    fn open_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((r, pretty)) = &self.response else { return };
        let name: String = self.name.read(cx).content.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        let name = if name.trim_matches('-').is_empty() { "respuesta".to_string() } else { name };
        let path = store::dir().join("responses").join(format!("{name}.{}", r.extension()));
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        let text_ok = std::str::from_utf8(&r.body).is_ok();
        let res = if text_ok { std::fs::write(&path, pretty) } else { std::fs::write(&path, &r.body) };
        if let Err(e) = res {
            self.message = Some((format!("No se pudo guardar la respuesta: {e}").into(), true));
            cx.notify();
            return;
        }
        if let Some(ws) = self.ws.upgrade() {
            ws.update(cx, |w, cx| w.open_file(&path, true, window, cx));
        }
    }

    // ---- tree actions -----------------------------------------------------------------------

    fn new_request(&mut self, ci: Option<usize>, parent: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.save_now(cx);
        let ci = match ci {
            Some(ci) => ci,
            None => match self.collections.iter().position(|c| c.name() == "Mis requests") {
                Some(ci) => ci,
                None => match store::new_collection("Mis requests") {
                    Ok(c) => {
                        self.collections.push(c);
                        self.collections.len() - 1
                    }
                    Err(e) => {
                        self.message = Some((format!("{e:#}").into(), true));
                        cx.notify();
                        return;
                    }
                },
            },
        };
        let c = &mut self.collections[ci];
        let Some(holder) = c.item_mut(&parent) else { return };
        if !holder["item"].is_array() {
            holder["item"] = serde_json::json!([]);
        }
        let arr = holder["item"].as_array_mut().expect("array");
        arr.push(store::new_request_json("Nuevo request"));
        let mut path = parent.clone();
        path.push(arr.len() - 1);
        if let Err(e) = c.save() {
            self.message = Some((format!("{e:#}").into(), true));
        }
        let file = c.file.clone();
        self.expanded.insert((file.clone(), parent));
        self.select(file, path, window, cx);
        window.focus(&self.name.focus_handle(cx));
        self.name.update(cx, |i, cx| i.select_all_text(cx));
    }

    fn duplicate(&mut self, ci: usize, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.save_now(cx);
        let Some((&last, parent)) = path.split_last() else { return };
        let c = &mut self.collections[ci];
        let Some(mut copy) = c.item(&path).cloned() else { return };
        copy["name"] = serde_json::json!(format!("{} (copia)", copy["name"].as_str().unwrap_or("Request")));
        if let Some(o) = copy.as_object_mut() {
            o.remove("id");
        }
        let Some(arr) = c.item_mut(parent).and_then(|h| h["item"].as_array_mut()) else { return };
        arr.insert(last + 1, copy);
        let _ = c.save();
        let file = c.file.clone();
        let mut new_path = parent.to_vec();
        new_path.push(last + 1);
        self.select(file, new_path, window, cx);
    }

    fn delete_item(&mut self, ci: usize, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.collections[ci].item(&path).and_then(|i| i["name"].as_str()).unwrap_or("").to_string();
        let answer = window.prompt(PromptLevel::Warning, &format!("¿Borrar “{name}”?"), None, &["Borrar", "Cancelar"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.save_task = None;
                let Some(c) = this.collections.get_mut(ci) else { return };
                let Some((&last, parent)) = path.split_last() else { return };
                let Some(arr) = c.item_mut(parent).and_then(|h| h["item"].as_array_mut()) else { return };
                if last < arr.len() {
                    arr.remove(last);
                }
                let _ = c.save();
                // Indexes after it shifted: drop the selection if it was there or below.
                if this.selected.as_ref().is_some_and(|(f, p)| *f == c.file && p.starts_with(parent)) {
                    this.selected = None;
                    this.loaded = None;
                    this.response = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_collection(&mut self, ci: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.collections.get(ci).cloned() else { return };
        let answer = window.prompt(PromptLevel::Warning, &format!("¿Borrar la colección “{}”?", c.name()), Some("Va a la Papelera."), &["Borrar", "Cancelar"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.save_task = None;
                let _ = super::config::trash(&c.file);
                this.reload(cx);
            });
        })
        .detach();
    }

    fn set_env(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        store::set_active_environment(id.as_deref());
        self.active_env = id;
        self.menu = None;
        cx.notify();
    }

    fn new_env(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        match store::new_environment("Nuevo entorno") {
            Ok(e) => {
                let file = e.file.clone();
                self.set_active_env_by_file(&file, cx);
                if let Some(ws) = self.ws.upgrade() {
                    ws.update(cx, |w, cx| w.open_file(&file, true, window, cx));
                }
            }
            Err(e) => self.message = Some((format!("{e:#}").into(), true)),
        }
        cx.notify();
    }

    fn edit_env(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let Some(file) = self.env().map(|e| e.file.clone()) else { return };
        if let Some(ws) = self.ws.upgrade() {
            ws.update(cx, |w, cx| w.open_file(&file, true, window, cx));
        }
        cx.notify();
    }

    fn toggle(&mut self, key: Key, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

    // ---- rendering ---------------------------------------------------------------------------

    fn rows(&self) -> Vec<Row> {
        fn walk(c: &Collection, ci: usize, items: &serde_json::Value, path: &mut Vec<usize>, depth: usize, expanded: &HashSet<Key>, out: &mut Vec<Row>) {
            let Some(arr) = items.as_array() else { return };
            for (i, it) in arr.iter().enumerate() {
                path.push(i);
                if it.get("request").is_some() {
                    out.push(Row::Request(ci, path.clone(), depth));
                } else {
                    out.push(Row::Folder(ci, path.clone(), depth));
                    if expanded.contains(&(c.file.clone(), path.clone())) {
                        walk(c, ci, &it["item"], path, depth + 1, expanded, out);
                    }
                }
                path.pop();
            }
        }
        let mut out = Vec::new();
        for (ci, c) in self.collections.iter().enumerate() {
            out.push(Row::Collection(ci));
            if self.expanded.contains(&(c.file.clone(), Vec::new())) {
                walk(c, ci, &c.json["item"], &mut Vec::new(), 1, &self.expanded, &mut out);
            }
        }
        out
    }

    fn render_row(&self, ix: usize, row: &Row, cx: &mut Context<Self>) -> gpui::AnyElement {
        let hover_btn = |id: (&'static str, usize), icon: &'static str| {
            div()
                .id(id)
                .flex_none()
                .p(px(3.))
                .rounded(px(4.))
                .invisible()
                .group_hover("http-row", |s| s.visible())
                .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
                .child(gpui::svg().path(icon).size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)))
        };
        let base = |depth: usize| {
            div()
                .id(("http-row", ix))
                .group("http-row")
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .pl(px(8. + depth as f32 * 12.))
                .pr(px(6.))
                .mx(px(4.))
                .rounded(px(5.))
                .cursor_pointer()
        };
        match row.clone() {
            Row::Collection(ci) => {
                let c = &self.collections[ci];
                let key = (c.file.clone(), Vec::new());
                let open = self.expanded.contains(&key);
                base(0)
                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                    .child(gpui::svg().path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                    .child(gpui::svg().path("icons/folder.svg").size(px(13.)).text_color(theme::rgb(theme::ACCENT)))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).child(c.name()))
                    .child(hover_btn(("http-c-add", ix), "icons/plus.svg").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.new_request(Some(ci), Vec::new(), w, cx);
                    })))
                    .child(hover_btn(("http-c-del", ix), "icons/trash.svg").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.delete_collection(ci, w, cx);
                    })))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(key.clone(), cx)))
                    .into_any_element()
            }
            Row::Folder(ci, path, depth) => {
                let c = &self.collections[ci];
                let key = (c.file.clone(), path.clone());
                let open = self.expanded.contains(&key);
                let name = c.item(&path).and_then(|i| i["name"].as_str()).unwrap_or("Carpeta").to_string();
                let p2 = path.clone();
                base(depth)
                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                    .child(gpui::svg().path(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }).size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                    .child(gpui::svg().path(if open { "icons/folder-open.svg" } else { "icons/folder.svg" }).size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).child(name))
                    .child(hover_btn(("http-f-add", ix), "icons/plus.svg").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.new_request(Some(ci), p2.clone(), w, cx);
                    })))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(key.clone(), cx)))
                    .into_any_element()
            }
            Row::Request(ci, path, depth) => {
                let c = &self.collections[ci];
                let item = c.item(&path);
                let f = item.map(Form::from_item);
                let (method, name) = f.map(|f| (f.method, f.name)).unwrap_or_default();
                let selected = self.selected.as_ref().is_some_and(|(file, p)| *file == c.file && *p == path);
                let file = c.file.clone();
                let (p1, p2, p3) = (path.clone(), path.clone(), path.clone());
                base(depth)
                    .when(selected, |d| d.bg(theme::rgb(theme::SELECTED)))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::rgb(theme::HOVER))))
                    .child(div().flex_none().w(px(40.)).font_family(theme::MONO_FONT).text_size(px(9.5)).font_weight(FontWeight::BOLD).text_color(theme::rgb(method_color(&method))).child(if method == "DELETE" { "DEL".to_string() } else if method == "OPTIONS" { "OPT".to_string() } else { method.clone() }))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(name))
                    .child(hover_btn(("http-r-dup", ix), "icons/copy.svg").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.duplicate(ci, p1.clone(), w, cx);
                    })))
                    .child(hover_btn(("http-r-del", ix), "icons/trash.svg").on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                        cx.stop_propagation();
                        this.delete_item(ci, p2.clone(), w, cx);
                    })))
                    .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.select(file.clone(), p3.clone(), w, cx)))
                    .into_any_element()
            }
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let env_name = self.env().map(|e| e.name()).unwrap_or_else(|| "Sin entorno".into());
        let rows = self.rows();
        let mut list = div().id("http-tree").flex_1().min_h_0().overflow_y_scroll().pb(px(16.)).flex().flex_col();
        if self.collections.is_empty() {
            list = list.child(
                div()
                    .p(px(14.))
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(theme::rgb(theme::TEXT_FAINT))
                    .child("Sin colecciones. Arrastra a la ventana un export de Postman (.json), o crea un request con +."),
            );
        }
        for (ix, row) in rows.iter().enumerate() {
            list = list.child(self.render_row(ix, row, cx));
        }
        let env_menu = (self.menu == Some(Menu::Env)).then(|| {
            let mut m = popover().top(px(40.)).left(px(8.)).w(px(204.));
            m = m.child(menu_item("env-none", "Sin entorno", self.active_env.is_none()).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_env(None, cx))));
            for (i, e) in self.envs.iter().enumerate() {
                let id = e.id();
                let on = self.active_env.as_ref() == Some(&id);
                m = m.child(menu_item(("env", i), e.name(), on).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_env(Some(id.clone()), cx))));
            }
            m = m.child(div().h(px(1.)).my(px(4.)).bg(theme::rgb(theme::BORDER)));
            if self.env().is_some() {
                m = m.child(menu_item("env-edit", "Editar variables del entorno…", false).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.edit_env(w, cx))));
            }
            m.child(menu_item("env-new", "Nuevo entorno…", false).on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.new_env(w, cx))))
        });
        div()
            .relative()
            .flex()
            .flex_col()
            .w(px(220.))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(theme::rgb(theme::BORDER))
            .bg(theme::rgb(theme::SIDEBAR_BG))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .p(px(8.))
                    .child(
                        div()
                            .id("http-env")
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap(px(6.))
                            .px(px(8.))
                            .h(px(26.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme::rgb(theme::BORDER))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                            .child(div().size(px(7.)).rounded_full().bg(theme::rgb(if self.active_env.is_some() { theme::ADDED } else { theme::TEXT_FAINT })))
                            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).child(env_name))
                            .child(gpui::svg().path("icons/chevron-down.svg").size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.menu = if this.menu == Some(Menu::Env) { None } else { Some(Menu::Env) };
                                cx.notify();
                            })),
                    )
                    .child(
                        crate::git_panel::icon_action("http-new", "icons/plus.svg", "Nuevo request").on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                            let ci = this.selected.as_ref().and_then(|(f, _)| this.coll_ix(f));
                            this.new_request(ci, Vec::new(), w, cx);
                        })),
                    )
                    .child(crate::git_panel::icon_action("http-import", "icons/pull.svg", "Importar de Postman…").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(ws) = this.ws.upgrade() {
                            super::config::pick_and_import(ws, cx);
                        }
                    }))),
            )
            .child(list)
            .children(env_menu)
    }

    fn render_request(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some((file, path)) = self.selected.clone() else {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .text_color(theme::rgb(theme::TEXT_FAINT))
                .child(gpui::svg().path("icons/send.svg").size(px(32.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                .child(div().text_size(px(14.)).child("Elige un request o crea uno con +"))
                .child(div().text_size(px(12.)).child("Arrastra un export de Postman a la ventana para importarlo"))
                .into_any_element();
        };
        let ci = self.coll_ix(&file);
        let sending = self.sending.is_some();
        let dirty = self.save_task.is_some();
        let vars = ci.map(|ci| self.vars(ci)).unwrap_or_default();
        let form = self.form(cx);
        let mut missing = store::missing_vars(&form.url, &vars);
        for m in store::missing_vars(&form.headers, &vars) {
            if !missing.contains(&m) {
                missing.push(m);
            }
        }
        let resolved = store::resolve(&form.url, &vars);
        let crumbs = ci
            .map(|ci| {
                let c = &self.collections[ci];
                let mut parts = vec![c.name()];
                for d in 1..path.len() {
                    if let Some(n) = c.item(&path[..d]).and_then(|i| i["name"].as_str()) {
                        parts.push(n.to_string());
                    }
                }
                parts.join(" / ")
            })
            .unwrap_or_default();

        let method_menu = (self.menu == Some(Menu::Method)).then(|| {
            let mut m = popover().top(px(36.)).left(px(0.)).w(px(130.));
            for (i, meth) in METHODS.iter().enumerate() {
                let meth = meth.to_string();
                let on = meth == self.method;
                m = m.child(
                    menu_item(("meth", i), meth.clone(), on)
                        .font_family(theme::MONO_FONT)
                        .text_color(theme::rgb(method_color(&meth)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.method = meth.clone();
                            this.menu = None;
                            this.edited(cx);
                            cx.notify();
                        })),
                );
            }
            m
        });

        let tab = |id: &'static str, label: String, t: ReqTab, current: ReqTab| {
            let on = t == current;
            div()
                .id(id)
                .px(px(2.))
                .mr(px(16.))
                .h(px(30.))
                .flex()
                .items_center()
                .cursor_pointer()
                .border_b_2()
                .border_color(theme::rgb(if on { theme::ACCENT } else { theme::APP_BG }))
                .text_size(px(12.))
                .text_color(theme::rgb(if on { theme::TEXT } else { theme::TEXT_MUTED }))
                .child(label)
        };
        let header_count = form.headers.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).count();
        let tabs = div()
            .flex()
            .border_b_1()
            .border_color(theme::rgb(theme::BORDER))
            .child(tab("http-tab-h", if header_count > 0 { format!("Headers ({header_count})") } else { "Headers".into() }, ReqTab::Headers, self.tab).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.tab = ReqTab::Headers;
                cx.notify();
            })))
            .child(tab("http-tab-b", if self.body_mode == BodyMode::None { "Body".into() } else { format!("Body · {}", self.body_mode.label()) }, ReqTab::Body, self.tab).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.tab = ReqTab::Body;
                cx.notify();
            })))
            .child(tab("http-tab-a", format!("Auth · {}", self.auth_label(&form)), ReqTab::Auth, self.tab).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.tab = ReqTab::Auth;
                cx.notify();
            })));

        let tab_body: gpui::AnyElement = match self.tab {
            ReqTab::Headers => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(field(self.headers.clone().into_any_element()).p(px(8.)).font_family(theme::MONO_FONT).text_size(px(12.)))
                .child(hint("Una línea por header: Nombre: valor  ·  # al inicio lo desactiva  ·  {{variables}} permitidas"))
                .into_any_element(),
            ReqTab::Body => {
                let mut pills = div().flex().flex_wrap().gap(px(4.));
                for (i, mode) in BodyMode::ALL.iter().enumerate() {
                    let mode = *mode;
                    pills = pills.child(pill(("bm", i), mode.label(), mode == self.body_mode).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.body_mode = mode;
                        this.edited(cx);
                        cx.notify();
                    })));
                }
                let mut col = div().flex().flex_col().gap(px(8.)).child(pills);
                if self.body_mode != BodyMode::None {
                    col = col.child(field(self.body.clone().into_any_element()).p(px(8.)).font_family(theme::MONO_FONT).text_size(px(12.)));
                    let h = self.body_mode.hint();
                    if !h.is_empty() {
                        col = col.child(hint(h));
                    }
                    if self.body_mode == BodyMode::Json {
                        let text = store::resolve(&self.body.read(cx).content, &vars);
                        if !text.trim().is_empty() && serde_json::from_str::<serde_json::Value>(&text).is_err() && !text.contains("{{") {
                            col = col.child(div().text_size(px(11.5)).text_color(theme::rgb(theme::MODIFIED)).child("El body no es JSON válido"));
                        }
                    }
                }
                col.into_any_element()
            }
            ReqTab::Auth => self.render_auth(&form, cx),
        };

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .flex_none()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_none().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(crumbs))
                    .child(div().flex_none().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("/"))
                    .child(div().flex_1().min_w_0().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(self.name.clone()))
                    .child(div().flex_none().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(if dirty { "Guardando…" } else { "Guardado" })),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("http-method")
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_between()
                            .gap(px(6.))
                            .w(px(104.))
                            .px(px(10.))
                            .h(px(32.))
                            .rounded(px(7.))
                            .border_1()
                            .border_color(theme::rgb(theme::BORDER))
                            .bg(theme::rgb(theme::HOVER))
                            .cursor_pointer()
                            .font_family(theme::MONO_FONT)
                            .text_size(px(12.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::rgb(method_color(&self.method)))
                            .child(self.method.clone())
                            .child(gpui::svg().path("icons/chevron-down.svg").size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.menu = if this.menu == Some(Menu::Method) { None } else { Some(Menu::Method) };
                                cx.notify();
                            })),
                    )
                    .child(field(self.url.clone().into_any_element()).flex_1().min_w_0().h(px(32.)).px(px(10.)).flex().items_center().font_family(theme::MONO_FONT).text_size(px(12.5)))
                    .child(
                        crate::plugins::detail::primary_button("http-send", if sending { "Cancelar" } else { "Enviar" })
                            .h(px(32.))
                            .w(px(92.))
                            .tooltip(|_, cx| cx.new(|_| crate::git_panel::Tooltip("Enviar (⌘Enter / Enter en la URL)")).into())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.send(cx))),
                    )
                    .children(method_menu),
            )
            .when(resolved != form.url && !resolved.is_empty(), |d| d.child(div().font_family(theme::MONO_FONT).text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(format!("→ {resolved}"))))
            .when(!missing.is_empty(), |d| {
                let env = self.env().map(|e| format!("el entorno “{}”", e.name())).unwrap_or_else(|| "ningún entorno (elige uno arriba a la izquierda)".into());
                d.child(div().text_size(px(11.5)).text_color(theme::rgb(theme::MODIFIED)).child(format!("Sin valor en {env}: {}", missing.iter().map(|m| format!("{{{{{m}}}}}")).collect::<Vec<_>>().join(", "))))
            })
            .child(tabs)
            .child(tab_body)
            .into_any_element()
    }

    fn auth_label(&self, form: &Form) -> String {
        match &form.auth {
            Auth::Inherit => format!("heredada ({})", self.effective_auth(form).label()),
            a => a.label().to_string(),
        }
    }

    fn render_auth(&self, form: &Form, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut pills = div().flex().flex_wrap().gap(px(4.));
        for (i, (kind, label)) in AUTH_KINDS.iter().enumerate() {
            let kind = *kind;
            pills = pills.child(pill(("ak", i), label, kind == self.auth_kind).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.auth_kind = kind;
                this.set_auth_placeholders(cx);
                this.edited(cx);
                cx.notify();
            })));
        }
        let mut col = div().flex().flex_col().gap(px(8.)).child(pills);
        match self.auth_kind {
            AuthKind::Inherit => {
                col = col.child(hint_owned(format!("Usa la auth de la carpeta o la colección: {}.", self.effective_auth(form).label())));
            }
            AuthKind::None => col = col.child(hint("No se envía ninguna autenticación.")),
            AuthKind::Bearer => col = col.child(field(self.auth_a.clone().into_any_element()).h(px(30.)).px(px(10.)).flex().items_center().font_family(theme::MONO_FONT).text_size(px(12.))),
            AuthKind::Basic | AuthKind::ApiKey => {
                col = col
                    .child(field(self.auth_a.clone().into_any_element()).h(px(30.)).px(px(10.)).flex().items_center().font_family(theme::MONO_FONT).text_size(px(12.)))
                    .child(field(self.auth_b.clone().into_any_element()).h(px(30.)).px(px(10.)).flex().items_center().font_family(theme::MONO_FONT).text_size(px(12.)));
                if self.auth_kind == AuthKind::ApiKey {
                    let q = self.apikey_query;
                    col = col.child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .child(pill("ak-h", "En header", !q).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.apikey_query = false;
                                this.edited(cx);
                                cx.notify();
                            })))
                            .child(pill("ak-q", "En la URL (query)", q).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.apikey_query = true;
                                this.edited(cx);
                                cx.notify();
                            }))),
                    );
                }
            }
            AuthKind::Other => {
                col = col.child(hint_owned(format!("Auth “{}” de Postman: se conserva en la colección pero Fragua no la envía. Elige otra para cambiarla.", self.other_auth.clone().unwrap_or_default())));
            }
        }
        col.into_any_element()
    }

    fn render_response(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.sending.is_some() {
            return div().flex_1().flex().items_center().justify_center().text_size(px(13.)).text_color(theme::rgb(theme::ACCENT)).child("Enviando…").into_any_element();
        }
        let Some((r, pretty)) = &self.response else {
            return div().flex_1().into_any_element();
        };
        if let Some(err) = &r.error {
            if r.status == 0 {
                return div()
                    .flex_1()
                    .p(px(16.))
                    .child(div().p(px(12.)).rounded(px(8.)).bg(theme::rgb(0x3a1d22)).text_size(px(13.)).text_color(theme::rgb(0xf0b3b8)).child(err.clone()))
                    .into_any_element();
            }
        }
        let color = match r.status {
            200..=299 => theme::ADDED,
            300..=399 => theme::INFO,
            400..=499 => theme::MODIFIED,
            _ => theme::DELETED,
        };
        let size = crate::hover_preview::human_size(r.size.max(r.body.len() as u64));
        let tab = |id: &'static str, label: String, t: RespTab, current: RespTab| {
            let on = t == current;
            div()
                .id(id)
                .px(px(2.))
                .mr(px(14.))
                .h(px(28.))
                .flex()
                .items_center()
                .cursor_pointer()
                .border_b_2()
                .border_color(theme::rgb(if on { theme::ACCENT } else { theme::APP_BG }))
                .text_size(px(12.))
                .text_color(theme::rgb(if on { theme::TEXT } else { theme::TEXT_MUTED }))
                .child(label)
        };
        let body: gpui::AnyElement = match self.resp_tab {
            RespTab::Body => {
                let binary = std::str::from_utf8(&r.body).is_err();
                if binary {
                    hint_owned(format!("Respuesta binaria ({}, {size}). Ábrela en el editor.", r.content_type())).into_any_element()
                } else if pretty.is_empty() {
                    hint("(sin cuerpo)").into_any_element()
                } else {
                    let total = pretty.lines().count();
                    let mut col = div().flex().flex_col().font_family(theme::MONO_FONT).text_size(px(12.)).line_height(px(17.)).text_color(theme::rgb(theme::TEXT));
                    for line in pretty.lines().take(MAX_LINES) {
                        col = col.child(div().child(if line.is_empty() { " ".to_string() } else { line.to_string() }));
                    }
                    if total > MAX_LINES {
                        col = col.child(hint_owned(format!("… {} líneas más. Ábrela en el editor para verla completa.", total - MAX_LINES)));
                    }
                    col.into_any_element()
                }
            }
            RespTab::Headers => {
                let mut col = div().flex().flex_col().gap(px(2.)).font_family(theme::MONO_FONT).text_size(px(12.));
                for (k, v) in &r.headers {
                    col = col.child(div().flex().gap(px(8.)).child(div().flex_none().text_color(theme::rgb(theme::INFO)).child(format!("{k}:"))).child(div().flex_1().min_w_0().text_color(theme::rgb(theme::TEXT)).child(v.clone())));
                }
                col.into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .border_t_1()
            .border_color(theme::rgb(theme::BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .pt(px(10.))
                    .child(div().px(px(8.)).py(px(2.)).rounded(px(5.)).bg(theme::rgb(color)).text_size(px(12.)).font_weight(FontWeight::BOLD).text_color(theme::rgb(0x111111)).child(format!("{} {}", r.status, r.reason)))
                    .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{} ms", r.millis)))
                    .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(size))
                    .when_some(r.error.clone(), |d, e| d.child(div().text_size(px(12.)).text_color(theme::rgb(theme::MODIFIED)).child(e)))
                    .child(div().flex_1())
                    .child(crate::git_panel::icon_action("http-copy", "icons/copy.svg", "Copiar respuesta").child("Copiar").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some((_, p)) = &this.response {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(p.clone()));
                            this.message = Some(("Respuesta copiada".into(), false));
                            cx.notify();
                        }
                    })))
                    .child(crate::git_panel::icon_action("http-curl", "icons/terminal.svg", "Copiar como cURL").child("cURL").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_curl(cx))))
                    .child(crate::git_panel::icon_action("http-open", "icons/external.svg", "Abrir la respuesta en el editor").child("Editor").on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.open_response(w, cx)))),
            )
            .child(
                div()
                    .flex()
                    .px(px(16.))
                    .border_b_1()
                    .border_color(theme::rgb(theme::BORDER))
                    .child(tab("http-rt-b", "Body".into(), RespTab::Body, self.resp_tab).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.resp_tab = RespTab::Body;
                        cx.notify();
                    })))
                    .child(tab("http-rt-h", format!("Headers ({})", r.headers.len()), RespTab::Headers, self.resp_tab).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.resp_tab = RespTab::Headers;
                        cx.notify();
                    }))),
            )
            .child(div().id("http-resp").flex_1().min_h_0().overflow_y_scroll().p(px(16.)).child(body))
            .into_any_element()
    }
}

/// First request in a collection (depth first) under `under`.
fn first_request(c: &Collection, under: &[usize]) -> Option<Vec<usize>> {
    let holder = c.item(under)?;
    for (i, it) in holder["item"].as_array()?.iter().enumerate() {
        let mut p = under.to_vec();
        p.push(i);
        if it.get("request").is_some() {
            return Some(p);
        }
        if let Some(found) = first_request(c, &p) {
            return Some(found);
        }
    }
    None
}

fn field(child: gpui::AnyElement) -> gpui::Div {
    div().overflow_hidden().rounded(px(7.)).border_1().border_color(theme::rgb(theme::BORDER)).bg(theme::rgb(theme::EDITOR_BG)).child(child)
}

fn hint(text: &'static str) -> gpui::Div {
    div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(text)
}

fn hint_owned(text: String) -> gpui::Div {
    div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(text)
}

fn pill(id: impl Into<gpui::ElementId>, label: &'static str, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(10.))
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(12.))
        .cursor_pointer()
        .text_size(px(11.5))
        .border_1()
        .when(on, |d| d.bg(theme::rgb(theme::BRAND_DEEP)).border_color(theme::rgb(theme::BRAND_EDGE)).text_color(theme::rgb(0xffffff)))
        .when(!on, |d| d.border_color(theme::rgb(theme::BORDER)).text_color(theme::rgb(theme::TEXT_MUTED)).hover(|s| s.bg(theme::rgb(theme::HOVER))))
        .child(label)
}

fn popover() -> gpui::Div {
    div()
        .absolute()
        .flex()
        .flex_col()
        .p(px(4.))
        .rounded(px(8.))
        .bg(theme::rgb(0x202227))
        .border_1()
        .border_color(theme::rgb(0x33363d))
        .shadow_lg()
        .occlude()
}

fn menu_item(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(8.))
        .h(px(26.))
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(12.))
        .hover(|s| s.bg(theme::rgb(theme::SELECTED)))
        .when(on, |d| d.bg(theme::rgb(theme::HOVER)))
        .child(label.into())
}

impl Focusable for HttpPage {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for HttpPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let message = self.message.clone();
        div()
            .id("http-page")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .bg(theme::rgb(theme::APP_BG))
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(div().id("http-req-scroll").flex_none().max_h(gpui::relative(0.6)).overflow_y_scroll().child(self.render_request(cx)))
                    .when_some(message, |d, (m, err)| d.child(div().px(px(16.)).pb(px(6.)).text_size(px(12.)).text_color(theme::rgb(if err { 0xf0b3b8 } else { theme::TEXT_MUTED })).child(m)))
                    .child(self.render_response(cx)),
            )
    }
}
