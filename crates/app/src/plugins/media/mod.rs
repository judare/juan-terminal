//! Plugin "Música, podcasts y videos": YouTube search and playback through yt-dlp, offline
//! downloads, the sidebar "now playing" card, the theater view and the global media keys.

pub mod music;
pub mod music_engine;
pub mod music_proxy;
pub mod music_ui;

use gpui::{ClickEvent, Context, Render, Window, div, prelude::*, px};

use super::detail::{button, section_title};
use super::{Manifest, builtin, choice, spec};
use crate::theme;

pub const ABOUT: &str = "## Qué hace
Busca y reproduce música, podcasts y videos de YouTube sin abrir el navegador (usa `yt-dlp`).

- **⌘⇧M** abre la búsqueda (también desde otras apps si el atajo global está activo).
- **⌃⌥Space** pausa / reproduce, **⌃⌥← / →** canción anterior / siguiente, desde cualquier app.
- Tarjeta \"sonando ahora\" al pie de la barra lateral y vista grande para videos.
- **Ver después** guarda podcasts y videos y los descarga para verlos sin conexión.
- Las canciones que suenan quedan en caché para repetirlas sin red.
- Los links de YouTube y YouTube Music se reproducen aquí: los que abres en Fragua y los
  que llegan a Brave (la pestaña se cierra). YouTube Music se puede navegar en Brave aunque
  YouTube esté bloqueado: al dar play, la canción suena aquí.
- Las descargas aparecen en el launcher (⌘Space): escribe `musica`, `podcast` o `video`.

## Descargas
En **Configuración** eliges qué se descarga, la calidad del video y cuándo se borran: al
terminar de verlos, a los N días, o a mano con **Borrar descargas**.";

pub fn manifest() -> Manifest {
    builtin(
        super::MEDIA,
        "Música, podcasts y videos",
        "icons/music.svg",
        "Busca y reproduce música, podcasts y videos de YouTube (yt-dlp), con descargas offline, tarjeta en la barra lateral y controles globales (⌃⌥Space, ⌃⌥←/→).",
        vec![
            spec("open_links", "Abrir links de YouTube aquí", "Los links de YouTube y YouTube Music (desde Fragua o Brave) suenan en el reproductor"),
            spec("global_hotkey", "Atajo global ⌘⇧M", "Abre la búsqueda también desde otras apps"),
            spec("launcher_results", "Descargas en el launcher (⌘Space)", "Canciones, podcasts y videos descargados como resultados"),
            spec("autoplay", "Seguir reproduciendo al terminar", "Cuando se acaba la cola suena una sugerencia o algo del mismo artista / canal"),
            spec("podcast_autodownload", "Descargar podcasts al reproducirlos", "Para seguirlos sin conexión donde quedaste"),
            spec("later_download", "Descargar lo que guardas en Ver después", "Podcasts y videos guardados quedan disponibles offline"),
            spec("delete_finished", "Borrar al terminar", "Borra el podcast o video descargado cuando lo terminas de ver o escuchar"),
            choice(
                "download_days",
                "Borrar descargas viejas",
                "Podcasts y videos descargados hace más de N días (no toca Ver después). Se revisa al abrir la app.",
                &[("never", "Nunca"), ("7", "7 días"), ("14", "14 días"), ("30", "30 días"), ("60", "60 días")],
                "never",
            ),
            choice(
                "song_cache_days",
                "Caché de canciones",
                "Las canciones que suenan se guardan para repetirlas sin red; se borran si no suenan en ese tiempo",
                &[("0", "Sin caché"), ("7", "7 días"), ("30", "30 días"), ("90", "90 días")],
                "30",
            ),
            choice("video_quality", "Calidad de video", "Para reproducir y descargar; más calidad ocupa más espacio", &[("360", "360p"), ("480", "480p"), ("720", "720p"), ("1080", "1080p")], "480"),
        ],
    )
}

/// Page section: disk used by downloads, delete them, open the folder.
pub struct MediaConfig;

impl MediaConfig {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Some(m) = music::global(cx) {
            cx.observe(&m, |_, _, cx| cx.notify()).detach();
        }
        Self
    }
}

impl Render for MediaConfig {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let music = music::global(cx);
        let usage = match music.as_ref().map(|m| m.read(cx).disk_usage()) {
            Some(Some(b)) => format!("Ocupan {}", crate::hover_preview::human_size(b)),
            Some(None) => "Midiendo…".to_string(),
            None => "Activa el plugin para ver las descargas.".to_string(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(section_title("Descargas en este Mac"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(13.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(usage))
                    .when_some(music, |d, m| {
                        d.child(button("media-clear", "icons/trash.svg", "Borrar descargas").on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            m.update(cx, |m, cx| m.clear_downloads(cx));
                        })))
                    })
                    .child(button("media-folder", "icons/finder.svg", "Abrir carpeta").on_click(|_, _, _| {
                        let dir = music::music_dir();
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("/usr/bin/open").arg(dir).spawn();
                    })),
            )
            .child(div().text_size(px(11.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("Borrar descargas conserva lo que está en Ver después."))
    }
}
