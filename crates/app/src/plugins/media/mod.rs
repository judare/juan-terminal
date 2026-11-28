//! Plugin "Música, podcasts y videos": YouTube search and playback through yt-dlp, offline
//! downloads, the sidebar "now playing" card, the theater view and the global media keys.

pub mod music;
pub mod music_engine;
pub mod music_proxy;
pub mod music_ui;

use super::{Manifest, builtin, spec};

pub fn manifest() -> Manifest {
    builtin(
        super::MEDIA,
        "Música, podcasts y videos",
        "icons/music.svg",
        "Busca y reproduce música, podcasts y videos de YouTube (yt-dlp), con descargas offline, tarjeta en la barra lateral y controles globales (⌃⌥Space, ⌃⌥←/→).",
        vec![
            spec("global_hotkey", "Atajo global ⌘⇧M", "Abre la búsqueda también desde otras apps"),
            spec("launcher_results", "Descargas en el launcher (⌘Space)", "Canciones, podcasts y videos descargados como resultados"),
        ],
    )
}
