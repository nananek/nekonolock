//! sudo 用の PAM モジュール `pam_nekonolock.so`。
//!
//! ロック画面の認証ヘルパと同じ `nekonolock-gpg` の署名・検証を使うが、
//! 権限境界を越えるためユーザーのヘルパ socket には接続しない。
//!
//! - 署名: 実 uid のユーザーとして `gpg` を実行し、その gpg-agent / scdaemon
//!   にカードを使わせる。
//! - 検証: root 側の一時 keyring に取り込んだ `/etc/nekonolock/sudo-key.asc`
//!   だけを使う。
//! - 設定: `/etc/nekonolock/sudo.toml`（root 所有）。ユーザーの
//!   `~/.config/nekonolock/config.toml` は読まない。
//!
//! 成功したときだけ `PAM_SUCCESS` を返す。カードが無い・PIN が違う・
//! ヘルパが使えない場合は `PAM_IGNORE` を返し、PAM スタックの
//! パスワード認証へフォールバックする。

#![deny(unsafe_code)]

mod auth;
mod config;
#[allow(unsafe_code)]
mod module;
#[allow(unsafe_code)]
mod pam_ffi;
#[allow(unsafe_code)]
mod user;

pub use config::{ConfigError, DEFAULT_CONFIG_PATH, SudoConfig, SudoFile};
