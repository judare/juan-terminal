//! WhatsApp (multi-device, linked by QR) for juan-terminal: connection, local cache, and the
//! socket the MCP tools use to send through the app's connection.

pub mod convert;
pub mod ipc;
pub mod model;
pub mod service;
pub mod store;
pub mod transcribe;

pub use model::*;
pub use service::{SendAnswer, WhatsApp, data_dir};
pub use store::Store;
