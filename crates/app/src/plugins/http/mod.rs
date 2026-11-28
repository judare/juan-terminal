//! Plugin "Cliente HTTP": Postman-style collections and environments (stored as Postman v2.1
//! JSON, `store.rs`), a request editor / response viewer tab (`page.rs`) and its settings
//! section (`config.rs`). Dropping a Postman export on the window imports it and turns the
//! plugin on.

pub mod config;
pub mod page;
pub mod store;

use super::{Manifest, builtin, choice, spec};

pub const ABOUT: &str = r#"## Qué hace
Un cliente HTTP tipo Postman dentro de Fragua.

- **Colecciones** con carpetas y requests: método, URL, headers, body (JSON, texto, form,
  multipart con archivos, GraphQL) y auth (Bearer, Basic, API key).
- **Entornos** con variables: escribe `{{base_url}}` o `{{token}}` y se reemplazan al enviar.
  También `{{$guid}}`, `{{$timestamp}}`, `{{$isoTimestamp}}` y `{{$randomInt}}`.
- La auth se hereda de la carpeta o la colección, como en Postman.
- Respuesta con estado, tiempo, tamaño, headers y JSON formateado; **Abrir en editor** la
  abre como archivo y **Copiar cURL** copia el comando equivalente.
- **⌘Enter** envía el request.

## Importar de Postman
Arrastra a la ventana un archivo exportado de Postman (`*.postman_collection.json` o
`*.postman_environment.json`): se importa y el plugin se activa solo. También desde
**Configuración › Importar**.

Se guardan en formato Postman v2.1, así que puedes exportarlas de vuelta. Lo que Fragua no
edita (scripts, ejemplos, documentación) se conserva intacto; los scripts de Postman no se
ejecutan.

Abre el cliente con ⌘⇧P › "HTTP" o escribe `http` en el launcher."#;

pub fn manifest() -> Manifest {
    builtin(
        super::HTTP,
        "Cliente HTTP",
        "icons/send.svg",
        "Colecciones y entornos tipo Postman: requests con variables, auth y respuestas formateadas. Arrastra un export de Postman para importarlo.",
        vec![
            spec("follow_redirects", "Seguir redirecciones", "Como Postman: 301/302 llevan a la URL final"),
            spec("verify_tls", "Verificar certificados TLS", "Apágalo para servidores con certificados propios"),
            choice("timeout", "Tiempo máximo por request", "", &[("10", "10 s"), ("30", "30 s"), ("60", "60 s"), ("300", "5 min")], "60"),
        ],
    )
}
