//! `nekonolock` の UI と認証ヘルパの間でやりとりするプロトコル。
//!
//! - 要求・応答の型（[`protocol`]）
//! - `SOCK_SEQPACKET` の socket ヘルパ（[`socket`]）
//! - 共有する設定（[`config`]）

#![deny(unsafe_code)]

pub mod config;
pub mod protocol;
pub mod socket;

pub use config::{AuthConfig, Config, ConfigError, PamConfig, UiConfig};
pub use protocol::{
    AuthKind, AuthMethod, MAX_MESSAGE_BYTES, MAX_SECRET_BYTES, PROTOCOL_VERSION, Reason, Request,
    RequestBody, Response, ResponseBody, Secret,
};
pub use socket::{Connection, Listener, SocketError, connect, default_socket_path};
