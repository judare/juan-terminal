# Plugins

Fragua is a core (projects, files, editor, terminals, Git, Claude, Tools, Ports, dev servers)
plus plugins. The **Plugins** view (plug icon in the sidebar's mode row, View › Plugins, or
⌘⇧P → "Plugins") lists them with an on/off switch; clicking one opens its **page** in a tab,
like an extension page in VS Code: description (Detalles) and its options (Configuración).

## Layout

```
crates/app/src/plugins/
  mod.rs              registry: manifests, on/off + settings state, external plugins
  panel.rs            the Plugins list (sidebar)
  detail.rs           a plugin's page (header, Detalles, Configuración)
  media/              Música, podcasts y videos (YouTube via yt-dlp)
  whatsapp/           WhatsApp (chats, notifications, Dock badge, MCP tools)
  google/             Google Calendar y Gmail (OAuth, agenda, Meet links, unread mail)
  http/               Cliente HTTP (Postman v2.1 collections / environments, curl)
  clipboard/          Portapapeles (history, pinned snippets, paste from the launcher)
  two_factor/         Códigos 2FA (TOTP)
  ssh/                Servidores SSH (client in crates/ssh_remote)
  ai_usage/           Métricas de uso de IA (⌘⇧U)
  website_blocker/    Bloqueo de sitios (adult, gambling, YouTube…: /etc/hosts + Brave)

~/Library/Application Support/juan-terminal/
  plugins.json        on/off and settings of every plugin
  plugins/<plugin>/   external plugins, one folder each, with a plugin.json
```

Each built-in plugin folder has a `mod.rs` with its `manifest()` (id, name, description,
settings), an `ABOUT` text (Markdown) for its page and the plugin's modules. Plugins with their
own settings UI (Google account, HTTP collections, clipboard, downloads) return it from
`plugins::config_view`.

Settings are on/off switches (`spec`, `spec_off`) or choices (`choice`: a few values shown as
pills, read with `plugins::choice_value` / `choice_number`). The host asks `plugins::enabled(id)` and
`plugins::setting(id, key)` at each integration point (sidebar, launcher, command palette,
MCP server, Dock badge, notifications, /etc/hosts).

## Built-in plugins

All installed and on by default; with everything on, the app behaves exactly as before (same
shortcuts, notifications and Dock badge). Their version is the app's: they update with Fragua.

| id | Plugin | Settings |
|---|---|---|
| `media` | Música, podcasts y videos | Atajo global ⌘⇧M · descargas en el launcher · seguir al terminar · descargar podcasts / Ver después · borrar al terminar · borrar descargas a los N días · caché de canciones · calidad de video |
| `whatsapp` | WhatsApp | Notificaciones · sin leer en el Dock |
| `google` | Google Calendar y Gmail | Aviso N min antes · próxima reunión en la barra lateral · launcher · notificar correos |
| `http` | Cliente HTTP | Seguir redirecciones · verificar TLS · tiempo máximo |
| `clipboard` | Portapapeles | Atajo global ⌃⌘V · pegar al elegir · entradas guardadas · días |
| `2fa` | Códigos 2FA | — |
| `ssh` | Servidores SSH | — |
| `ai-usage` | Métricas de uso de IA | Botón en la barra lateral |
| `website-blocker` | Bloqueo de sitios | /etc/hosts · Brave (bloqueados, horarios, sin video) · filtro adulto |

Switching off:

* **media**: stops playback, hides the card, theater, search and ⌘⇧M results. The global keys
  (⌃⌥Space, ⌃⌥←/→, ⌘⇧M) stay registered but do nothing until the app restarts.
* **whatsapp**: hides the tab, stops notifications and the Dock count, removes the WhatsApp MCP
  tools from new Claude sessions. The connection itself closes at the next restart.
* **2fa / ssh**: hide their sections of the Servers view (ssh also the "+" menu entries and the
  `list_servers` / `ssh_exec` tools).
* **website-blocker**: switching it (or one of its parts) off takes **24 hours**, like removing
  a site from the list: no impulse unblocking. Switching on is immediate. The lists and
  schedules stay in `settings.toml` (⌘,); project schedules are core and keep working.

### Google Calendar y Gmail

Read-only OAuth (Calendar + Gmail scopes) with an OAuth client of type *Desktop app* created
once in Google Cloud Console; the plugin page walks through it. Client secret and refresh token
are in the Keychain (`Fragua Google`), the account address in `google.json`. Synced every 2
minutes; launcher keywords `meet`, `agenda`, `mail`; tab ⌘⇧P › "Google: Agenda y correo".

### Cliente HTTP

Collections and environments are Postman v2.1 JSON in `<state>/http/` (fields Fragua doesn't
edit are kept). Dropping a `*.postman_collection.json` / `*.postman_environment.json` (or any
JSON that looks like one) on the window imports it and switches the plugin on. Requests go
through `/usr/bin/curl`; `{{variables}}` come from the active environment and the collection,
auth is inherited from folders / collection. Pre-request and test scripts aren't run.

### Portapapeles

Polls the pasteboard's change count twice a second; text only, concealed / transient copies
(password managers) skipped. History in `clipboard.json`. Picking an entry in the launcher
pastes it with a synthetic ⌘V when Fragua has the Accessibility permission.

## External plugins

A plugin is a folder (usually a git repository) with a `plugin.json`:

```json
{
  "id": "hello",
  "name": "Hello",
  "description": "Ejemplo de plugin para Fragua",
  "version": "0.1.0",
  "author": "Tu nombre",
  "default_enabled": true,
  "settings": [
    { "key": "loud", "label": "Gritar", "help": "Saludo en mayúsculas", "default": false }
  ],
  "commands": [
    { "title": "Saludar", "run": "if [ \"$FRAGUA_SETTING_LOUD\" = 1 ]; then echo HOLA; else echo hola; fi" }
  ],
  "mcp": {
    "command": "./server.js",
    "args": [],
    "env": { "LOG_LEVEL": "info" }
  }
}
```

* `id`: letters, digits, `-`, `_`; unique (built-in ids are taken).
* `settings`: on/off switches shown in the Plugins view.
* `commands`: listed in ⌘⇧P as "<name>: <title>". `run` goes to `zsh -c` in a new terminal tab
  (cwd: the project folder) and the tab stays open afterwards.
* `mcp` (optional): a stdio MCP server added to the Claude sessions started in Fragua (key
  `plugin-<id>`). A command starting with `./` is relative to the plugin folder, which is also
  its working directory.
* Commands and the MCP server get `FRAGUA_PLUGIN_ID`, `FRAGUA_PLUGIN_DIR` and
  `FRAGUA_SETTING_<KEY>` (`1`/`0`) in their environment.

Install: Plugins view › **+** › paste the git URL (`git clone` into `plugins/<name>`), or copy
the folder there by hand and press reload. **Actualizar** runs `git pull --ff-only`; **Quitar**
moves the folder to the Trash.

External plugins run with your user's permissions: only install plugins you trust.
