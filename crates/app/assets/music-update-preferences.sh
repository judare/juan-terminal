#!/bin/zsh -l
# Weekly (LaunchAgent com.juan-terminal.music-preferences) or from juan-terminal's music search
# ("Renovar con Claude"): Claude refreshes the search topics in preferences.json.
# Written by juan-terminal; edits here are overwritten by the next app launch.
set -u
DIR="$HOME/Library/Application Support/juan-terminal/music"
cd "$DIR" || exit 1
CLAUDE="${CLAUDE_BIN:-$HOME/.local/bin/claude}"
[ -x "$CLAUDE" ] || CLAUDE="$(command -v claude)"
[ -n "$CLAUDE" ] || { echo "claude CLI not found" >&2; exit 1; }
# Not a child session of whatever Claude Code may have launched the app.
unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT CLAUDE_PID 2>/dev/null

echo "== $(date '+%Y-%m-%d %H:%M') refreshing preferences" >> update.log

read -r -d '' PROMPT <<'EOF'
Estás en la carpeta de datos del reproductor de música de juan-terminal. Actualiza preferences.json, que define los temas de búsqueda de YouTube con los que la app sugiere contenido en tres modos: "music", "podcast" y "videos".

Lee primero:
- preferences.json: temas actuales por modo y "used" (temas ya usados en semanas anteriores).
- history.json (si existe): lo último que se reprodujo, con su modo, título y canal.
- progress.json (si existe): podcasts y videos con su avance. finished=true significa que se terminó (gustó). Si pos es mucho menor que dur, se abandonó (interesó menos).

Luego reescribe preferences.json con exactamente el mismo esquema:
- Gustos de base: podcasts EN ESPAÑOL de negocios, producto y entrevistas a fundadores y dueños de empresas grandes ("gente muy crack"); videos EN ESPAÑOL estilo Farid Dieck, midudev, divulgación, tecnología e IA. Respeta "language".
- "podcast" y "videos": unos 12 temas NUEVOS por modo, en la línea de los gustos que se ven (más de lo que se terminó, menos de lo que se abandonó, nada repetido). Antes de reemplazarlos, mueve los temas actuales de esos modos a used.podcast y used.videos. Nunca propongas un tema que ya esté en "used" ni en la lista actual, aunque se escriba distinto.
- "music": conserva los artistas y géneros que siguen encajando, quita los que no, y agrega unos 6 artistas parecidos. En música sí se puede repetir; no hace falta moverlos a "used".
- "avoid": géneros o temas que NO gustan (p. ej. vallenato). Nunca propongas nada de esa lista y consérvala tal cual.
- "updated": la fecha de hoy (YYYY-MM-DD). Deja "note" como está.
- Cada tema debe ser una búsqueda corta que funcione en YouTube (un artista, un programa, un canal o un tema concreto), en el idioma en que se consume.

Si dudas de que un tema dé resultados, compruébalo con: yt-dlp --flat-playlist -J "ytsearch3:<tema>".
Escribe solo preferences.json, con JSON válido. No toques ningún otro archivo.
EOF

"$CLAUDE" -p "$PROMPT" --allowedTools "Read" "Write" "Edit" "Bash(yt-dlp:*)" --permission-mode acceptEdits >> update.log 2>&1
status=$?
echo "== exit $status" >> update.log
exit $status
