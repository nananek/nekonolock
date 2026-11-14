//! `nekonolock-auth`: UI から要求を受けて認証するヘルパ。
//!
//! - `status`: カードの有無と有効な方式を返す。
//! - `unlock` `pin`: OpenPGP applet のチャレンジ署名を検証する。
//! - `unlock` `password`: PAM で認証する。
//!
//! 秘密（PIN / パスワード）はログ・argv・環境変数に出さない。

#[cfg_attr(feature = "mock-card", allow(dead_code))]
mod card;
mod engine;
#[cfg(any(feature = "mock-card", feature = "mock-pam"))]
mod mock;
#[cfg_attr(feature = "mock-pam", allow(dead_code))]
mod pam;
mod server;

use std::env;
use std::ffi::OsString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nekonolock_proto::{Config, Listener, default_socket_path};

#[cfg(not(feature = "mock-card"))]
use crate::card::GpgCard;
use crate::engine::Engine;
#[cfg(not(feature = "mock-pam"))]
use crate::pam::PamAuth;

// モックを有効にしたまま配布ビルドを作れてしまうと、既知の PIN で解除できる
// バイナリができてしまう。テスト用 feature は debug ビルドに限定する。
#[cfg(all(
    any(feature = "mock-card", feature = "mock-pam"),
    not(debug_assertions)
))]
compile_error!("mock-card / mock-pam はテスト専用。release ビルドでは有効にしない");

struct Args {
    config: Option<PathBuf>,
    socket: Option<PathBuf>,
    once: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("nekonolock-auth: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };

    let config = Config::load_auth(args.config.as_deref()).context("設定を読み込めない")?;
    let timeout = Duration::from_secs(config.auth.timeout_sec);

    // PAM は実行ユーザーの名前で認証する。socket のピア uid は同じであることを
    // 接続時に確認済みなので、ここでは実効 uid のユーザーを使う。
    let user = if config.auth.password {
        current_user()?
    } else {
        String::new()
    };

    let listener = match systemd_listener()? {
        Some(listener) => {
            eprintln!("nekonolock-auth: systemd socket activation で起動した");
            listener
        }
        None => {
            let path = match args.socket {
                Some(path) => path,
                None => match env::var_os("NEKONOLOCK_SOCKET") {
                    Some(path) => PathBuf::from(path),
                    None => default_socket_path()?,
                },
            };
            eprintln!("nekonolock-auth: listening on {}", path.display());
            Listener::bind(&path)
                .with_context(|| format!("socket を作れない: {}", path.display()))?
        }
    };

    let card = make_card(&config);
    let pam = make_pam(&config);
    let engine = Engine::new(config.auth, user, card, pam);

    server::serve(&listener, &engine, timeout, args.once)
}

fn current_user() -> Result<String> {
    let uid = nix::unistd::geteuid();
    let user = nix::unistd::User::from_uid(uid)?
        .ok_or_else(|| anyhow::anyhow!("uid {uid} のユーザーが /etc/passwd に無い"))?;
    Ok(user.name)
}

#[cfg(not(feature = "mock-card"))]
fn make_card(config: &Config) -> GpgCard {
    GpgCard::new(
        config.auth.key.clone(),
        Duration::from_secs(config.auth.timeout_sec),
    )
}

#[cfg(feature = "mock-card")]
fn make_card(_config: &Config) -> mock::MockCard {
    mock::MockCard::new()
}

#[cfg(not(feature = "mock-pam"))]
fn make_pam(config: &Config) -> PamAuth {
    PamAuth::new(config.pam.service.clone())
}

#[cfg(feature = "mock-pam")]
fn make_pam(_config: &Config) -> mock::MockPam {
    mock::MockPam::new()
}

/// systemd から fd 3 を渡された場合だけ引き取る。
///
/// fd 3 を `OwnedFd` にするため unsafe が要るが、LISTEN_PID が自分で
/// LISTEN_FDS が 1 のときだけ実行する。
#[allow(unsafe_code)]
fn systemd_listener() -> Result<Option<Listener>> {
    let pid: u32 = match env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        Some(pid) => pid,
        None => return Ok(None),
    };
    let current = nix::unistd::getpid().as_raw() as u32;
    if pid != current {
        // 親から引き継いだ古い環境変数。systemd からの起動ではない。
        return Ok(None);
    }

    let fds: i32 = match env::var("LISTEN_FDS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        Some(fds) => fds,
        None => return Ok(None),
    };
    if fds != 1 {
        bail!("socket activation は 1 個の fd だけを想定している（LISTEN_FDS={fds}）");
    }

    // SAFETY: systemd が fd 3 に listen socket を渡している。ここで所有権を
    // 受け取り、以降は他で close しない。
    let fd = unsafe { OwnedFd::from_raw_fd(3) };
    // SAFETY: 起動直後で他スレッドはまだ env を読んでいない。
    unsafe {
        env::remove_var("LISTEN_PID");
        env::remove_var("LISTEN_FDS");
    }
    Ok(Some(Listener::from_owned_fd(fd)))
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = env::args_os();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("nekonolock-auth"));

    let mut config = None;
    let mut socket = None;
    let mut once = false;

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--config") => {
                config = Some(next_path(&mut args, "--config")?);
            }
            Some("--socket") => {
                socket = Some(next_path(&mut args, "--socket")?);
            }
            Some("--once") => once = true,
            Some("-h" | "--help") => {
                println!(
                    "使い方: {} [--config PATH] [--socket PATH] [--once]\n\n\
                     --config  UI と同じ config.toml（既定: $XDG_CONFIG_HOME/nekonolock/config.toml）\n\
                     --socket  socket のパス（既定: $XDG_RUNTIME_DIR/nekonolock/auth.sock）\n\
                     --once    1 接続だけ処理して終了する",
                    PathBuf::from(program).display()
                );
                return Ok(None);
            }
            Some(other) => bail!("知らない引数: {other}"),
            None => bail!("引数を UTF-8 として解釈できない"),
        }
    }

    Ok(Some(Args {
        config,
        socket,
        once,
    }))
}

fn next_path(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<PathBuf> {
    let value = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("{name} にパスが続かない"))?;
    Ok(PathBuf::from(value))
}
