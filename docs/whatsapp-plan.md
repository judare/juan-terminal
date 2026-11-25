# WhatsApp en juan-terminal

Cliente de WhatsApp dentro de juan-terminal usando `whatsapp-rust` 0.7.0 (cliente no oficial de
WhatsApp Web multi-device; MIT; compila en Rust estable sin la feature `simd`).

## Alcance

Incluido:
- Vincular dispositivo por QR (cualquier teléfono que lo escanee). Desvincular.
- Pestaña "WhatsApp" en la barra lateral (junto a Files/Search/Git…): lista de chats con foto
  pequeña, nombre, último mensaje, hora y contador de no leídos; ordenada por fecha. Fijados arriba.
  Sin archivados. Búsqueda (nombre y texto de mensajes, sobre la caché local).
- Etiquetas locales por chat (clic derecho → Etiquetas; varias por chat) y filtro por etiqueta en
  los chips bajo el buscador (clic derecho en el chip → eliminar).
- Pestañas de chat con la foto de la persona y contador de no leídos.
- Click en un chat → se abre como pestaña en el área del editor (cerrable, igual que un archivo).
- Conversación: texto, imágenes, videos, audios/notas de voz (escuchar), stickers (ver, enviar
  favoritos), documentos, respuestas citadas, reacciones (ver/enviar), estados de entrega/lectura.
- Enviar: texto, adjuntos (imagen, video, documento), notas de voz grabadas, stickers favoritos,
  responder citando, reaccionar. Marca como leído al abrir. "escribiendo…".
- Notificaciones macOS + badge en el Dock (no para chats silenciados).
- Caché local persistente (SQLite + archivos de media) para no volver a descargar todo.
- Herramientas MCP para Claude Code: `whatsapp_list_chats`, `whatsapp_read_chat`,
  `whatsapp_search`, `whatsapp_send` (texto + adjuntos). Cada envío de Claude pide confirmación
  en la app (destinatario, texto, adjuntos) además del permiso normal de Claude Code.

Excluido: estados/historias, comunidades, canales (newsletters), llamadas, chats archivados.

## Arquitectura

```
crates/whatsapp/            backend, sin GPUI
  store.rs   caché propia (rusqlite): chats, messages, contacts, favorite_stickers
  service.rs hilo con runtime tokio: Bot + SqliteStore de sesión, eventos → caché → Update
  model.rs   tipos planos para la UI (ChatSummary, ChatMessage, MediaRef…)
  ipc.rs     socket unix para que el proceso MCP pida envíos al proceso de la app
crates/app/src/
  whatsapp_panel.rs   modo de barra lateral: QR / lista de chats / búsqueda
  chat_view.rs        contenido de pestaña de editor para una conversación
  voice.rs            reproducir/grabar notas de voz (Ogg/Opus)
```

Datos en `~/Library/Application Support/juan-terminal/whatsapp/`:
- `session.db` (claves y sesión de whatsapp-rust), `cache.db` (chats y mensajes), `media/`,
  `avatars/`, `stickers/`.

Una sola conexión: la sesión vive en el proceso de la app. El proceso MCP (`--mcp-servers`, que
lanza Claude) lee `cache.db` directamente (solo lectura) y pide los envíos por
`whatsapp.sock`; si la app no está abierta, el envío falla con un mensaje claro.

Mensajes guardados con el proto original (`wa::Message` serializado) para poder descargar la
media bajo demanda (la URL del CDN caduca; se reintenta con `media_reupload`).

## Medidas contra bloqueos

- Se identifica como dispositivo vinculado de escritorio, comportamiento tipo WhatsApp Web.
- Envíos de Claude: confirmación en la app y límite de ritmo (mín. 3 s entre mensajes).
- `TemporaryBan` / `LoggedOut`: se desconecta y se avisa; sin reintentos agresivos.

## Fases

1. Backend: crate, caché, servicio, QR, historial, recepción en vivo, envío de texto.
2. UI: modo de barra lateral (QR + lista), pestaña de chat con texto, leído, enviar.
3. Media: imágenes, stickers, videos, documentos, audios; adjuntar y enviar; favoritos.
4. Citas, reacciones, notas de voz grabadas, notificaciones y badge.
5. MCP + socket + confirmación en la app.

## Estado (2026-11-24)

Implementadas las 5 fases. Verificado:
- `cargo test -p whatsapp`: caché (no leídos, orden, archivados/estados/canales ocultos, fusión
  LID→teléfono, búsqueda, estados de entrega, reacciones, edición/borrado, paginación, lectura
  solo-lectura) y conversión de mensajes (texto, citas, reacciones, revocados, media).
- `cargo test -p juan-terminal`: Opus codificar→decodificar (nota de voz), detección de enlaces.
- Conexión real: el backend llega a WhatsApp y recibe el QR (`cargo run -p whatsapp --example qr_check`
  con `JUAN_TERMINAL_STATE_DIR` temporal).
- MCP: las 5 herramientas aparecen en `tools/list` y responden sin vincular.

No verificado aún: flujo completo con un teléfono vinculado (historial, envío, media, notas de voz)
y el aspecto visual de la pestaña y del chat.

Limitaciones conocidas:
- Solo hay el historial que el teléfono sincroniza al vincular (más todo lo que llegue después);
  no se pide historial más antiguo al teléfono.
- La búsqueda ignora mayúsculas solo en ASCII ("Ó" ≠ "ó").
- Los videos enviados no llevan miniatura.
- Notificaciones vía `osascript` (aparecen como "Editor de Scripts"), igual que las de Claude.
