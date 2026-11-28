//! Plugin "Códigos 2FA": TOTP accounts with live codes in the Servers view (secrets in the
//! Keychain). The UI section lives in `servers_panel` and is shown only while this is on.

pub mod totp;

pub const ABOUT: &str = r#"## Qué hace
Códigos de verificación en dos pasos (TOTP) sin el celular.

- Las cuentas están en la vista **Servers**, con el código en vivo y el tiempo restante.
- Haz clic en un código para copiarlo.
- Agrega cuentas escaneando el QR desde una imagen o pegando el secreto.
- Los secretos se guardan en el **Llavero** de macOS, nunca en archivos."#;

use super::{Manifest, builtin};

pub fn manifest() -> Manifest {
    builtin(super::TWO_FACTOR, "Códigos 2FA", "icons/key.svg", "Cuentas TOTP con códigos en vivo en la vista Servers (secretos en el Llavero).", Vec::new())
}
