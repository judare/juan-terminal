//! Plugin "Bloqueo de sitios": adult, gambling, YouTube… sites blocked system-wide through
//! /etc/hosts and blanked in Brave (lists and schedules in settings.toml). Switching it or one
//! of its parts off takes 24 h (no impulse unblocking).

pub mod brave_guard;
pub mod website_block;

use super::{Manifest, builtin, spec};

pub fn manifest() -> Manifest {
    let mut m = builtin(
        super::WEBSITE_BLOCKER,
        "Bloqueo de sitios",
        "icons/lock.svg",
        "Bloquea sitios para adultos, apuestas, YouTube, etc. en todo el sistema (/etc/hosts) y en Brave. Las listas y horarios están en Settings (⌘,). Desactivarlo tarda 24 h.",
        vec![
            spec("system_hosts", "Bloqueo del sistema (/etc/hosts)", "blocked_websites: todas las apps y navegadores"),
            spec("brave", "Brave: sitios bloqueados, horarios y videos ocultos", "brave_blocked_websites, website_schedules, brave_no_video_websites"),
            spec("adult_filter", "Brave: filtro de páginas para adultos", "Bloquea páginas con mucho contenido adulto, sea cual sea la URL"),
        ],
    );
    m.delayed_off = true;
    m
}
