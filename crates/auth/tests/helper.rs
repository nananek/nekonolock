//! 認証ヘルパの結合テスト。
//!
//! `mock-pam` / `mock-card` を付けたデバッグビルドのバイナリを起動し、
//! 実際の Unix socket 越しに要求を送る。

#![cfg(all(feature = "mock-card", feature = "mock-pam"))]

use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use nekonolock_proto::{AuthKind, AuthMethod, Reason, Request, Response};
use nix::sys::socket::{
    AddressFamily, MsgFlags, SockFlag, SockType, UnixAddr, connect, recv, send, setsockopt, socket,
    sockopt,
};
use nix::sys::time::TimeVal;
use tempfile::TempDir;

struct Helper {
    child: Option<Child>,
    socket: PathBuf,
    dir: TempDir,
}

impl Helper {
    fn spawn(config: &str, envs: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // tempfile の既定モードに頼らず、認証ヘルパが要求する 0700 にする。
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, config).expect("config");
        let socket = dir.path().join("auth.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_nekonolock-auth"));
        command
            .arg("--config")
            .arg(&config_path)
            .arg("--socket")
            .arg(&socket)
            .env_clear()
            .envs(envs.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // HOME / XDG_RUNTIME_DIR は使わない（--config と --socket を明示している）。
        let mut child = command.spawn().expect("ヘルパを起動できる");

        let started = Instant::now();
        while !socket.exists() {
            if started.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let output = child.wait_with_output().expect("stderr");
                panic!(
                    "socket ができなかった: {} (stderr: {})",
                    socket.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if let Some(status) = child.try_wait().expect("try_wait") {
                let output = child.wait_with_output().expect("stderr");
                panic!(
                    "ヘルパが終了した ({status}): stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        Self {
            child: Some(child),
            socket,
            dir,
        }
    }

    fn request(&self, request: &Request) -> Response {
        request_bytes(&self.socket, &serde_json::to_vec(request).unwrap())
    }

    fn raw_request(&self, bytes: &[u8]) -> Response {
        request_bytes(&self.socket, bytes)
    }

    fn finish(mut self) -> Output {
        let mut child = self.child.take().expect("child");
        let _ = child.kill();
        child.wait_with_output().expect("出力を回収できる")
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn request_bytes(socket_path: &Path, bytes: &[u8]) -> Response {
    let fd = connect_seqpacket(socket_path);
    send(fd.as_raw_fd(), bytes, MsgFlags::empty()).expect("要求を送れる");
    let mut buf = vec![0u8; 8192];
    let len = recv(fd.as_raw_fd(), &mut buf, MsgFlags::empty()).expect("応答を読める");
    assert!(len > 0, "応答が空だった");
    serde_json::from_slice(&buf[..len]).expect("応答が JSON として解釈できる")
}

fn connect_seqpacket(path: &Path) -> OwnedFd {
    let fd = socket(
        AddressFamily::Unix,
        SockType::SeqPacket,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .expect("socket");
    let addr = UnixAddr::new(path).expect("addr");
    connect(fd.as_raw_fd(), &addr).expect("connect");
    fd
}

fn config(openpgp: bool, password: bool, timeout_sec: u64) -> String {
    format!(
        r#"
[auth]
openpgp = {openpgp}
password = {password}
key = "0123456789ABCDEF!"
challenge_bytes = 32
timeout_sec = {timeout_sec}

[pam]
service = "nekonolock-test"
"#
    )
}

#[test]
fn status_reports_card_and_methods() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    let response = helper.request(&Request::status());
    assert_eq!(
        response,
        Response::status(true, [AuthKind::Pin, AuthKind::Password])
    );
}

#[test]
fn pin_unlocks_with_openpgp() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    let response = helper.request(&Request::unlock(AuthKind::Pin, "123456"));
    assert_eq!(response, Response::unlocked(AuthMethod::Openpgp));
}

#[test]
fn wrong_pin_stays_locked() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    let response = helper.request(&Request::unlock(AuthKind::Pin, "000000"));
    assert_eq!(response.reason(), Some(Reason::AuthFailed));
}

#[test]
fn password_unlocks_with_pam() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    let response = helper.request(&Request::unlock(AuthKind::Password, "test-password"));
    assert_eq!(response, Response::unlocked(AuthMethod::Password));
}

#[test]
fn wrong_password_stays_locked() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    let response = helper.request(&Request::unlock(AuthKind::Password, "wrong-password"));
    assert_eq!(response.reason(), Some(Reason::AuthFailed));
}

#[test]
fn disabled_methods_are_rejected() {
    let pin_only = Helper::spawn(&config(true, false, 5), &[]);
    assert_eq!(
        pin_only
            .request(&Request::unlock(AuthKind::Password, "test-password"))
            .reason(),
        Some(Reason::PasswordDisabled)
    );
    assert_eq!(
        pin_only.request(&Request::status()).body,
        nekonolock_proto::ResponseBody::Status {
            ok: true,
            card: true,
            methods: vec![AuthKind::Pin],
        }
    );

    let password_only = Helper::spawn(&config(false, true, 5), &[]);
    assert_eq!(
        password_only
            .request(&Request::unlock(AuthKind::Pin, "123456"))
            .reason(),
        Some(Reason::PinDisabled)
    );
}

#[test]
fn absent_card_is_reported_and_pin_is_refused() {
    let helper = Helper::spawn(
        &config(true, true, 5),
        &[("NEKONOLOCK_MOCK_CARD", "absent")],
    );
    assert_eq!(
        helper.request(&Request::status()),
        Response::status(false, [AuthKind::Pin, AuthKind::Password])
    );
    assert_eq!(
        helper
            .request(&Request::unlock(AuthKind::Pin, "123456"))
            .reason(),
        Some(Reason::NoCard)
    );
}

#[test]
fn pam_failure_is_reported_as_pam_error() {
    let helper = Helper::spawn(&config(true, true, 5), &[("NEKONOLOCK_MOCK_PAM", "error")]);
    assert_eq!(
        helper
            .request(&Request::unlock(AuthKind::Password, "test-password"))
            .reason(),
        Some(Reason::PamError)
    );
}

#[test]
fn malformed_and_wrong_version_are_bad_requests() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    assert_eq!(
        helper.raw_request(b"not json").reason(),
        Some(Reason::BadRequest)
    );
    assert_eq!(
        helper.raw_request(br#"{"v":99,"op":"status"}"#).reason(),
        Some(Reason::BadRequest)
    );
}

#[test]
fn empty_request_times_out_and_closes() {
    let helper = Helper::spawn(&config(true, true, 1), &[]);
    let fd = connect_seqpacket(&helper.socket);

    // 何も送らずに待つ。ヘルパは 1 秒で接続を閉じる。
    setsockopt(&fd, sockopt::ReceiveTimeout, &TimeVal::new(5, 0)).unwrap();
    let mut buf = [0u8; 64];
    let len = recv(fd.as_raw_fd(), &mut buf, MsgFlags::empty()).expect("recv");
    assert_eq!(len, 0, "タイムアウト時は応答せず切断する");
}

#[test]
fn socket_is_private() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    let dir_mode = fs::metadata(helper.dir.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let socket_mode = fs::metadata(&helper.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(socket_mode, 0o600);
}

#[test]
fn secrets_are_not_logged() {
    let helper = Helper::spawn(&config(true, true, 5), &[]);
    let response = helper.request(&Request::unlock(AuthKind::Password, "hunter2-super-secret"));
    assert_eq!(response.reason(), Some(Reason::AuthFailed));

    let output = helper.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("hunter2-super-secret"), "stderr: {stderr}");
    assert!(stderr.contains("auth-failed"), "stderr: {stderr}");
}

#[test]
fn invalid_config_exits_with_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = dir.path().join("config.toml");
    fs::write(&config_path, config(false, false, 5)).unwrap();
    let socket = dir.path().join("auth.sock");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nekonolock-auth"))
        .arg("--config")
        .arg(&config_path)
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            panic!("設定エラーで終了しなかった");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!status.success());
    assert!(stderr.contains("設定"), "stderr: {stderr}");
}
