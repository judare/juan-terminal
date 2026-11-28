//! Google plugin section of its page: the OAuth client (one-time setup), connect / disconnect
//! and the sync status.

use gpui::{ClickEvent, Context, Entity, Render, SharedString, Window, div, prelude::*, px};

use super::{auth, sync};
use crate::plugins::detail::{button, markdown, primary_button, section_title};
use crate::text_input::TextInput;
use crate::theme;

const STEPS: &str = r#"### Configuración (una sola vez)
1. Abre **Google Cloud Console** y crea un proyecto (o usa uno existente).
2. En **APIs y servicios › Biblioteca** activa **Google Calendar API** y **Gmail API**.
3. En **Pantalla de consentimiento de OAuth** elige *Interno* (cuentas de tu Workspace) o
   *Externo* y agrega tu correo como usuario de prueba.
4. En **Credenciales › Crear credenciales › ID de cliente de OAuth** elige el tipo
   **App de escritorio**.
5. Copia el **Client ID** y el **Client secret** aquí abajo y presiona **Conectar**.

Fragua solo pide permisos de lectura (calendario y correo). El Client secret y el token
quedan en el Llavero de macOS."#;

pub struct GoogleConfig {
    client_id: Entity<TextInput>,
    secret: Entity<TextInput>,
    busy: bool,
    message: Option<(SharedString, bool)>,
}

impl GoogleConfig {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let client_id = cx.new(|cx| {
            let mut i = TextInput::new("123456789-abc.apps.googleusercontent.com", cx);
            if let Some(id) = auth::client_id() {
                i.set_text(&id, cx);
            }
            i
        });
        let secret = cx.new(|cx| TextInput::new(if auth::has_client_secret() { "•••••••• (guardado; escribe para cambiarlo)" } else { "GOCSPX-…" }, cx));
        if let Some(g) = sync::global(cx) {
            cx.observe(&g, |_, _, cx| cx.notify()).detach();
        }
        Self { client_id, secret, busy: false, message: None }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            auth::cancel_login();
            return;
        }
        let id = self.client_id.read(cx).content.clone();
        let secret = self.secret.read(cx).content.clone();
        if let Err(e) = auth::save_client(&id, &secret) {
            self.message = Some((format!("{e:#}").into(), true));
            cx.notify();
            return;
        }
        self.secret.update(cx, |i, cx| {
            i.set_text("", cx);
            i.set_placeholder("•••••••• (guardado; escribe para cambiarlo)", cx);
        });
        self.busy = true;
        self.message = Some(("Se abrió el navegador: elige tu cuenta y acepta los permisos…".into(), false));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async { auth::login() }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.message = Some(match res {
                    Ok(email) => (format!("Conectado como {email}").into(), false),
                    Err(e) => (format!("{e:#}").into(), true),
                });
                cx.notify();
                if let Some(g) = sync::global(cx) {
                    g.update(cx, |g, cx| g.sync(cx));
                }
            });
        })
        .detach();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().spawn(async { auth::disconnect() }).await;
            let _ = this.update(cx, |this, cx| {
                this.message = Some(("Cuenta desconectada".into(), false));
                if let Some(g) = sync::global(cx) {
                    g.update(cx, |g, cx| g.clear(cx));
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for GoogleConfig {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = auth::connected();
        let g = sync::global(cx);
        let status = g.as_ref().map(|g| {
            let g = g.read(cx);
            match (&g.error, g.syncing, g.last_sync) {
                (Some(e), _, _) => (e.clone(), true),
                (None, true, _) => ("Sincronizando…".to_string(), false),
                (None, false, Some(t)) => (format!("{} eventos · {} sin leer · actualizado {}", g.events.len(), g.unread, crate::plugins::clipboard::history::ago(t)), false),
                (None, false, None) => ("Esperando la primera sincronización".to_string(), false),
            }
        });
        let field = |input: Entity<TextInput>| div().h(px(30.)).px(px(10.)).flex().items_center().rounded(px(6.)).border_1().border_color(theme::rgb(theme::BORDER)).bg(theme::rgb(theme::EDITOR_BG)).font_family(theme::MONO_FONT).text_size(px(12.)).child(input);
        let mut col = div().flex().flex_col().gap(px(10.)).child(section_title("Cuenta de Google"));
        if connected {
            let email = auth::email().unwrap_or_else(|| "cuenta conectada".into());
            col = col
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .p(px(12.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme::rgb(theme::BORDER))
                        .child(div().size(px(8.)).rounded_full().bg(theme::rgb(theme::ADDED)))
                        .child(div().flex_1().flex().flex_col().child(div().text_size(px(13.)).child(email)).when_some(status, |d, (s, err)| d.child(div().text_size(px(11.5)).text_color(theme::rgb(if err { theme::MODIFIED } else { theme::TEXT_FAINT })).child(s))))
                        .when_some(g, |d, g| d.child(button("g-sync", "icons/fetch.svg", "Sincronizar").on_click(move |_, _, cx| g.update(cx, |g, cx| g.sync(cx)))))
                        .child(button("g-disconnect", "icons/close.svg", "Desconectar").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.disconnect(cx)))),
                );
        } else {
            col = col
                .children(markdown(STEPS))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(button("g-console", "icons/external.svg", "Abrir Google Cloud Console").on_click(|_, _, _| sync::open("https://console.cloud.google.com/apis/credentials")))
                        .child(button("g-apis", "icons/external.svg", "Activar las APIs").on_click(|_, _, _| sync::open("https://console.cloud.google.com/flows/enableapi?apiid=calendar-json.googleapis.com,gmail.googleapis.com"))),
                )
                .child(div().pt(px(4.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child("Client ID"))
                .child(field(self.client_id.clone()))
                .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT_MUTED)).child("Client secret"))
                .child(field(self.secret.clone()))
                .child(div().flex().child(primary_button("g-connect", if self.busy { "Cancelar" } else { "Conectar con Google" }).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.connect(cx)))));
        }
        if let Some((m, err)) = self.message.clone() {
            col = col.child(div().text_size(px(12.)).text_color(theme::rgb(if err { 0xf0b3b8 } else { theme::TEXT_MUTED })).child(m));
        }
        col
    }
}
