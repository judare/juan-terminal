//! Plugin "WhatsApp": chats in the sidebar and as tabs, voice notes, replies drafted with
//! Claude, macOS notifications, the unread count on the Dock and the WhatsApp MCP tools.
//! The protocol client itself is the `whatsapp` crate.

pub mod chat_view;
pub mod voice;
pub mod wa_bridge;
pub mod wa_gate;
pub mod wa_mcp;
pub mod wa_reply;
pub mod wa_ui;
pub mod whatsapp_panel;

pub const ABOUT: &str = r#"## Qué hace
Tus chats de WhatsApp dentro de Fragua, sin abrir otra app.

- Pestaña **WhatsApp** en la barra lateral con los chats y el número de no leídos.
- Cada chat se abre como una pestaña: fotos, audios, videos, documentos y notas de voz.
- **Pedir a Claude…** en un chat: Claude lee la conversación y te deja un borrador.
- Claude también puede leer, buscar y preparar mensajes con las herramientas MCP
  (`whatsapp_list_chats`, `whatsapp_read_chat`, `whatsapp_search`, `whatsapp_send`).
  Los mensajes de Claude quedan en el compositor para que tú los envíes.

## Conexión
La primera vez escaneas un código QR desde WhatsApp › Dispositivos vinculados."#;

use super::{Manifest, builtin, spec};

pub fn manifest() -> Manifest {
    builtin(
        super::WHATSAPP,
        "WhatsApp",
        "icons/whatsapp.svg",
        "Chats de WhatsApp en la barra lateral, respuestas con Claude y herramientas de WhatsApp para Claude (MCP).",
        vec![
            spec("notifications", "Notificaciones de macOS", "Aviso por cada mensaje nuevo"),
            spec("dock_badge", "Sin leer en el Dock", "Suma los chats sin leer al badge del ícono"),
        ],
    )
}
