//! Plugin "Códigos 2FA": TOTP accounts with live codes in the Servers view (secrets in the
//! Keychain). The UI section lives in `servers_panel` and is shown only while this is on.

pub mod totp;

use super::{Manifest, builtin};

pub fn manifest() -> Manifest {
    builtin(super::TWO_FACTOR, "Códigos 2FA", "icons/key.svg", "Cuentas TOTP con códigos en vivo en la vista Servers (secretos en el Llavero).", Vec::new())
}
