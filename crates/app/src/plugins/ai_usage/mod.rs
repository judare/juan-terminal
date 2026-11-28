//! Plugin "Métricas de uso de IA": the agent usage dashboard (⌘⇧U): tokens, tasks, models.

pub mod usage_stats;
pub mod usage_view;

pub const ABOUT: &str = r#"## Qué hace
Panel de uso de los agentes de IA (**⌘⇧U**).

- Tokens de entrada, salida y caché por día y por modelo.
- Tareas y sesiones por proyecto.
- Lee los registros locales de Claude Code; no envía nada a ningún servicio."#;

use super::{Manifest, builtin, spec};

pub fn manifest() -> Manifest {
    builtin(
        super::AI_USAGE,
        "Métricas de uso de IA",
        "icons/chart.svg",
        "Panel de uso de los agentes: tokens, tareas y modelos (⌘⇧U).",
        vec![spec("sidebar_button", "Botón en la barra lateral", "Ícono de métricas junto a PROJECTS")],
    )
}
