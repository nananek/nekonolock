//! `nekonolock` のロック画面（UI）で使う部品。
//!
//! Wayland への依存を `main.rs` に閉じ込め、ここには次の純粋な処理を置く。
//!
//! - `input`: 入力欄とモード切替の状態
//! - `render`: `wl_shm` のバッファへ描くソフトウェアレンダラ
//! - `auth`: 認証ヘルパとの 1 要求 1 応答
//! - `clock`: 時計の書式検証と整形

#![deny(unsafe_code)]

pub mod auth;
pub mod clock;
pub mod input;
pub mod render;
