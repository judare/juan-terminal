//! Plugin "Servidores SSH": per-project SSH servers (terminals, SFTP browser) and the
//! `list_servers` / `ssh_exec` MCP tools. The client is the `ssh_remote` crate; the UI is the
//! "SERVERS" section of `servers_panel` and the SSH entries of the "+" tab menu.

pub const ABOUT: &str = r#"## Qué hace
Servidores SSH por proyecto.

- Agrega servidores en la vista **Servers** (usuario, host, puerto, llave).
- Abre terminales SSH desde el menú **+** de las pestañas o desde el launcher (`ssh <nombre>`).
- Explorador de archivos remoto (SFTP): abre, edita y guarda archivos del servidor.
- Claude puede listar los servidores y ejecutar comandos con `list_servers` y `ssh_exec`."#;

use super::{Manifest, builtin};

pub fn manifest() -> Manifest {
    builtin(
        super::SSH,
        "Servidores SSH",
        "icons/server.svg",
        "Servidores SSH por proyecto: terminales SSH, explorador de archivos remoto (SFTP) y las herramientas list_servers / ssh_exec para Claude.",
        Vec::new(),
    )
}
