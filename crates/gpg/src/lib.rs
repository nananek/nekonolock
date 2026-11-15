//! OpenPGP applet によるチャレンジ署名と、その検証。
//!
//! ロック画面の認証ヘルパと sudo 用 PAM モジュールが同じ実装を使う。
//! カードを握るのは scdaemon だけにしたいので、ここでは `gpg` を子プロセスと
//! して呼ぶ。PIN は argv ではなく stdin（`--passphrase-fd 0`）で渡す。

pub mod card;

pub use card::{CardError, CardSigner, GpgCard, Identity};
